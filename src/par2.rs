//! PAR2 2.0 recovery sets: checking files against them, and rebuilding damaged or missing
//! slices from recovery blocks, as SABnzbd does when an article is gone from every server.
//!
//! A recovery set cuts its files into slices of one size and stores an MD5 and CRC32 of each.
//! Its recovery blocks are Reed-Solomon sums over GF(2^16) of every slice in the set, so any
//! `n` damaged slices can be rebuilt from any `n` recovery blocks, given every other slice of
//! the set. That last part is the price: rebuilding one slice reads the whole set.
//!
//! For a release being streamed:
//!
//! 1. [`Par2Files::find`] names the NZB's PAR2 files. Fetch the index and parse it with
//!    [`RecoverySet::parse`].
//! 2. [`RecoverySet::file_named`] and [`RecoverySet::slices_in`] say which slices an article
//!    that no server has falls in.
//! 3. [`Par2Files::choose`] orders the volumes to fetch for enough recovery blocks. Push
//!    their bytes through a [`Scanner`] until it holds enough.
//! 4. Feed every other slice to a [`Repair`] as it lands, then [`finish`](Repair::finish) it
//!    once [`Repair::waiting`] is empty. Or, with the files on disk, [`RecoverySet::repair`]
//!    does all of it through a reader and a writer.
//!
//! ```
//! use usenet::par2::{RecoverySet, Repair, Scanner};
//!
//! # fn run(index: &[u8], volume: &[u8], file: &mut Vec<u8>) -> Result<(), Box<dyn std::error::Error>> {
//! let set = RecoverySet::parse(index)?;
//! let movie = set.file_named("movie.part01.rar").ok_or("not in the set")?;
//! // Bytes 1,000,000 to 1,768,000 of the file are gone from every server.
//! let damaged: Vec<u32> = set.slices_in(movie, 1_000_000..1_768_000).collect();
//! let mut scanner = Scanner::new(&set);
//! scanner.push(volume);
//! let mut repair = Repair::new(&set, &damaged, scanner.into_blocks())?;
//! for index in repair.waiting().collect::<Vec<_>>() {
//!     let slice = set.slice(index).ok_or("no such slice")?;
//!     let bytes = file.get(slice.offset as usize..).unwrap_or_default();
//!     repair.feed(index, &bytes[..bytes.len().min(slice.length as usize)])?;
//! }
//! for rebuilt in repair.finish()? {
//!     let at = rebuilt.slice.offset as usize;
//!     file[at..at + rebuilt.data.len()].copy_from_slice(&rebuilt.data);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! This reads one recovery set at a time. Files a set lists without slice checksums (its
//! "non-recovery" files) are not covered.

mod error;
mod gf;
mod kernel;
mod packet;
mod plan;
mod repair;
#[cfg(any(test, feature = "fake"))]
pub(crate) mod write;

use std::collections::{HashMap, HashSet};
use std::io;
use std::ops::Range;

use md5::{Digest, Md5};

pub use error::{Defect, Lacking, Operation, Par2Error, RepairError};
pub use plan::{Par2Files, Volume};
pub use repair::{Feed, Rebuilt, Repair};

use packet::{FILE_DESC, Header, IFSC, MAIN, Packets, RECOVERY};

/// The largest slice size this module accepts, 256 MiB. PAR2 allows more, but a set with
/// larger slices could not be repaired in memory, and real ones use a few MiB at most.
pub const MAX_SLICE: u64 = 256 << 20;

/// The MD5 and CRC32 of one slice, padded with zeros to the slice size if it is a file's
/// short last slice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SliceChecksum {
    /// MD5 of the padded slice.
    pub md5: [u8; 16],
    /// CRC32 of the padded slice.
    pub crc32: u32,
}

/// One file of a recovery set.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct File {
    /// The file's ID within the set.
    pub id: [u8; 16],
    /// The name the file was protected under. PAR2 does not say how names are encoded; bytes
    /// that are not UTF-8 are replaced.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// MD5 of the whole file.
    pub md5: [u8; 16],
    /// MD5 of the first 16 KiB, or of the whole file if smaller: enough to recognize a file
    /// posted under another name.
    pub md5_16k: [u8; 16],
    /// The set-wide index of the file's first slice.
    pub first_slice: u32,
    /// How many slices the file has: its size divided by the slice size, rounded up.
    pub slices: u32,
}

/// Where one slice lies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slice {
    /// The set-wide index: slices run through the files in the set's order.
    pub index: u32,
    /// The file's position in [`RecoverySet::files`].
    pub file: usize,
    /// Offset of the slice in the file.
    pub offset: u64,
    /// Bytes of the file in the slice: the slice size, or less for a file's last slice.
    pub length: u32,
}

/// One recovery block: a sum of every slice in the set, weighted by powers of `exponent`.
#[derive(Clone, PartialEq, Eq)]
pub struct RecoveryBlock {
    /// Which sum this is. Blocks with different exponents are independent.
    pub exponent: u16,
    /// One slice's worth of bytes.
    pub data: Vec<u8>,
}

impl std::fmt::Debug for RecoveryBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RecoveryBlock({}, {} bytes)", self.exponent, self.data.len())
    }
}

/// A recovery set's description: its files, slice size and slice checksums. Any PAR2 file of
/// the set carries it, and the index (the `.par2` file without `.vol` in its name) carries
/// nothing else, so it is the one to fetch first.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RecoverySet {
    id: [u8; 16],
    slice_size: u64,
    files: Vec<File>,
    checksums: Vec<SliceChecksum>,
}

struct Description {
    name: String,
    size: u64,
    md5: [u8; 16],
    md5_16k: [u8; 16],
}

impl RecoverySet {
    /// Reads the recovery set described in a PAR2 file, or in any bytes that hold its packets
    /// in order. Damaged packets are skipped, so a file with some damage still parses when an
    /// intact copy of each packet it needs is left. Recovery blocks are ignored; push them
    /// through a [`Scanner`].
    ///
    /// When the bytes describe more than one set, the one whose main packet comes first wins.
    pub fn parse(bytes: &[u8]) -> Result<Self, Par2Error> {
        let mut any = false;
        let mut main: Option<(Header, &[u8])> = None;
        let mut descriptions: HashMap<([u8; 16], [u8; 16]), Description> = HashMap::new();
        let mut checksums: HashMap<([u8; 16], [u8; 16]), &[u8]> = HashMap::new();
        for (header, body) in Packets::new(bytes, |kind| kind != RECOVERY) {
            any = true;
            match &header.kind {
                MAIN if main.is_none() => main = Some((header, body)),
                FILE_DESC => {
                    if let Some((id, d)) = description(body) {
                        descriptions.entry((header.set, id)).or_insert(d);
                    }
                }
                IFSC => {
                    if let Some((id, rest)) = body.split_first_chunk::<16>() {
                        checksums.entry((header.set, *id)).or_insert(rest);
                    }
                }
                _ => {}
            }
        }
        let Some((header, body)) = main else {
            return Err(if any { Par2Error::NoMain } else { Par2Error::NotPar2 });
        };
        let (slice_size, rest) = body.split_first_chunk::<8>().ok_or(Par2Error::Malformed(Defect::FileList))?;
        let (count, ids) = rest.split_first_chunk::<4>().ok_or(Par2Error::Malformed(Defect::FileList))?;
        let slice_size = u64::from_le_bytes(*slice_size);
        if slice_size == 0 || !slice_size.is_multiple_of(4) || slice_size > MAX_SLICE {
            return Err(Par2Error::Malformed(Defect::SliceSize { size: slice_size }));
        }
        let count = u32::from_le_bytes(*count) as usize;
        let ids = ids.get(..count.saturating_mul(16)).ok_or(Par2Error::Malformed(Defect::FileList))?;
        let ids = ids.as_chunks::<16>().0;
        if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
            return Err(Par2Error::Malformed(Defect::FileList));
        }
        let (mut files, mut sums) = (Vec::with_capacity(count), Vec::new());
        let mut slices: u64 = 0;
        for &id in ids {
            let key = (header.set, id);
            let d = descriptions
                .remove(&key)
                .ok_or(Par2Error::Incomplete { file_id: id, lacking: Lacking::Description })?;
            let list = checksums.get(&key).ok_or(Par2Error::Incomplete { file_id: id, lacking: Lacking::Checksums })?;
            let expected = d.size.div_ceil(slice_size);
            let found = (list.len() / 20) as u64;
            if found != expected {
                return Err(Par2Error::Malformed(Defect::ChecksumCount { file_id: id, expected, found }));
            }
            let first_slice = slices;
            slices += expected;
            if slices > gf::MAX_INPUTS as u64 {
                return Err(Par2Error::Malformed(Defect::TooManySlices { slices }));
            }
            for entry in list.as_chunks::<20>().0 {
                if let Some((md5, crc)) = entry.split_first_chunk::<16>()
                    && let Some(crc) = crc.first_chunk::<4>()
                {
                    sums.push(SliceChecksum { md5: *md5, crc32: u32::from_le_bytes(*crc) });
                }
            }
            files.push(File {
                id,
                name: d.name,
                size: d.size,
                md5: d.md5,
                md5_16k: d.md5_16k,
                // Both fit: the total is at most MAX_INPUTS.
                first_slice: first_slice as u32,
                slices: expected as u32,
            });
        }
        Ok(Self { id: header.set, slice_size, files, checksums: sums })
    }

    /// The set's ID, which every packet of the set carries.
    pub fn id(&self) -> [u8; 16] {
        self.id
    }

    /// Bytes per slice, a multiple of 4.
    pub fn slice_size(&self) -> u64 {
        self.slice_size
    }

    /// The files, in the set's order, which is the order of their slices.
    pub fn files(&self) -> &[File] {
        &self.files
    }

    /// Slices across all files.
    pub fn slice_count(&self) -> u32 {
        self.checksums.len() as u32
    }

    /// The position of the file protected under `name`, compared without regard to ASCII
    /// case.
    pub fn file_named(&self, name: &str) -> Option<usize> {
        self.files.iter().position(|f| f.name.eq_ignore_ascii_case(name))
    }

    /// The position of the file of `size` bytes whose first 16 KiB (or whole self, if
    /// smaller) are `head`. Posters often rename files, or obfuscate their names, so this is
    /// how to match a posted file to the set: by the file size and the bytes of its first
    /// article.
    pub fn file_starting(&self, size: u64, head: &[u8]) -> Option<usize> {
        let head = &head[..head.len().min(16 * 1024)];
        if (head.len() as u64) < size.min(16 * 1024) {
            return None;
        }
        let md5: [u8; 16] = Md5::digest(head).into();
        self.files.iter().position(|f| f.size == size && f.md5_16k == md5)
    }

    /// Where slice `index` lies, or None past the last slice.
    pub fn slice(&self, index: u32) -> Option<Slice> {
        let file = self.files.partition_point(|f| f.first_slice + f.slices <= index);
        let f = self.files.get(file)?;
        let k = u64::from(index - f.first_slice);
        let offset = k * self.slice_size;
        // At most the slice size, which fits.
        let length = (f.size - offset).min(self.slice_size) as u32;
        Some(Slice { index, file, offset, length })
    }

    /// The slices of file `file` that hold any byte of `bytes`, which is clipped to the file.
    /// Empty for an empty or out-of-range clip, or no such file.
    pub fn slices_in(&self, file: usize, bytes: Range<u64>) -> Range<u32> {
        let Some(f) = self.files.get(file) else { return 0..0 };
        let (start, end) = (bytes.start, bytes.end.min(f.size));
        if start >= end {
            return 0..0;
        }
        // Both fit: they are at most the file's slice count.
        let first = (start / self.slice_size) as u32;
        let last = end.div_ceil(self.slice_size) as u32;
        f.first_slice + first..f.first_slice + last
    }

    /// The checksum recorded for slice `index`.
    pub fn checksum(&self, index: u32) -> Option<SliceChecksum> {
        self.checksums.get(index as usize).copied()
    }

    /// True when `data` is slice `index` as it was protected: its full length, matching its
    /// CRC32 and MD5.
    pub fn check(&self, index: u32, data: &[u8]) -> bool {
        let (Some(slice), Some(sum)) = (self.slice(index), self.checksum(index)) else { return false };
        data.len() == slice.length as usize && padded_crc32(data, self.slice_size) == sum.crc32 && {
            padded_md5(data, self.slice_size) == sum.md5
        }
    }

    /// True when `data` has slice `index`'s length and CRC32. A repair checks only this
    /// before adding a slice in, which is far cheaper than an MD5, and checks what it rebuilds
    /// against both: a wrong slice that slipped past its CRC32 shows up there.
    pub(crate) fn crc_matches(&self, index: u32, data: &[u8]) -> bool {
        let (Some(slice), Some(sum)) = (self.slice(index), self.checksum(index)) else { return false };
        data.len() == slice.length as usize && padded_crc32(data, self.slice_size) == sum.crc32
    }

    /// Checks every slice and returns the indices of those that are damaged or missing.
    ///
    /// `read` fills the buffer, which is the slice's length, with the slice's bytes and says
    /// how many it wrote; fewer than the buffer holds means the rest is missing. An error ends
    /// the check.
    pub fn verify<R>(&self, mut read: R) -> io::Result<Vec<u32>>
    where
        R: FnMut(Slice, &mut [u8]) -> io::Result<usize>,
    {
        let mut buf = vec![0u8; self.slice_size as usize];
        let mut damaged = Vec::new();
        for index in 0..self.slice_count() {
            let Some(slice) = self.slice(index) else { continue };
            let want = &mut buf[..slice.length as usize];
            let got = read(slice, want)?.min(want.len());
            if !self.check(index, &want[..got]) {
                damaged.push(index);
            }
        }
        Ok(damaged)
    }

    /// Rebuilds the `damaged` slices from `blocks` and every other slice of the set, which
    /// `read` supplies as in [`verify`](Self::verify), and hands each rebuilt slice to
    /// `write`. A slice that `read` supplies damaged or short is rebuilt too, when the blocks
    /// allow. Returns the slices written.
    ///
    /// Every slice is read before any is written, so a failure writes nothing.
    pub fn repair<R, W>(
        &self,
        damaged: &[u32],
        blocks: Vec<RecoveryBlock>,
        mut read: R,
        mut write: W,
    ) -> Result<Vec<Slice>, RepairError>
    where
        R: FnMut(Slice, &mut [u8]) -> io::Result<usize>,
        W: FnMut(Slice, &[u8]) -> io::Result<()>,
    {
        let mut repair = Repair::new(self, damaged, blocks)?;
        let mut buf = vec![0u8; self.slice_size as usize];
        for index in 0..self.slice_count() {
            let Some(slice) = self.slice(index) else { continue };
            if !repair.is_waiting(index) {
                continue;
            }
            let want = &mut buf[..slice.length as usize];
            let got = read(slice, want).map_err(|e| RepairError::io(index, Operation::Read, &e))?.min(want.len());
            // A slice read damaged joins the ones to rebuild, or fails when the blocks cannot cover it.
            let _: Feed = repair.feed(index, &want[..got])?;
        }
        let rebuilt = repair.finish()?;
        for r in &rebuilt {
            write(r.slice, &r.data).map_err(|e| RepairError::io(r.slice.index, Operation::Write, &e))?;
        }
        Ok(rebuilt.into_iter().map(|r| r.slice).collect())
    }
}

/// A file description packet's body: the file's ID and what it says of the file.
fn description(body: &[u8]) -> Option<([u8; 16], Description)> {
    let (id, rest) = body.split_first_chunk::<16>()?;
    let (md5, rest) = rest.split_first_chunk::<16>()?;
    let (md5_16k, rest) = rest.split_first_chunk::<16>()?;
    let (size, name) = rest.split_first_chunk::<8>()?;
    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
    let d = Description {
        name: String::from_utf8_lossy(name).into_owned(),
        size: u64::from_le_bytes(*size),
        md5: *md5,
        md5_16k: *md5_16k,
    };
    Some((*id, d))
}

const ZEROS: [u8; 4096] = [0; 4096];

/// Feeds `data` and then zeros up to `size` bytes to `update`.
fn padded(data: &[u8], size: u64, mut update: impl FnMut(&[u8])) {
    update(data);
    let mut pad = size.saturating_sub(data.len() as u64);
    while pad > 0 {
        let n = pad.min(ZEROS.len() as u64) as usize;
        update(&ZEROS[..n]);
        pad -= n as u64;
    }
}

fn padded_crc32(data: &[u8], size: u64) -> u32 {
    let mut h = crc32fast::Hasher::new();
    padded(data, size, |b| h.update(b));
    h.finalize()
}

fn padded_md5(data: &[u8], size: u64) -> [u8; 16] {
    let mut h = Md5::new();
    padded(data, size, |b| h.update(b));
    h.finalize().into()
}

/// Collects a recovery set's recovery blocks from PAR2 volumes as their bytes arrive, a few
/// articles at a time, keeping at most one packet's worth of bytes beyond the blocks found.
///
/// Packets of other sets, damaged packets, and bytes between packets are passed over, so the
/// articles of a volume can be pushed as they are fetched, and a volume with damage still
/// yields its intact blocks. A block whose exponent was found before is not kept twice.
#[derive(Debug)]
pub struct Scanner {
    set: [u8; 16],
    slice_size: u64,
    pending: Vec<u8>,
    blocks: Vec<RecoveryBlock>,
    exponents: HashSet<u16>,
    damaged: u32,
}

impl Scanner {
    /// A scanner for the blocks of `set`.
    pub fn new(set: &RecoverySet) -> Self {
        Self {
            set: set.id,
            slice_size: set.slice_size,
            pending: Vec::new(),
            blocks: Vec::new(),
            exponents: HashSet::new(),
            damaged: 0,
        }
    }

    /// Reads the next bytes of a volume. Start each volume where the last one ended, or after
    /// a gap: a packet cut by a gap is lost, and the rest are found.
    pub fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        let packet = packet::HEADER as u64 + 4 + self.slice_size;
        let mut at = 0;
        loop {
            let Some(found) = memchr::memmem::find(&self.pending[at..], packet::MAGIC) else {
                // Keep what could be the start of a magic cut off by the end of the bytes.
                at = at.max(self.pending.len().saturating_sub(packet::MAGIC.len() - 1));
                break;
            };
            let start = at + found;
            let bytes = &self.pending[start..];
            if bytes.len() < packet::HEADER {
                at = start;
                break;
            }
            at = start + packet::MAGIC.len();
            let Some(header) = Header::read(bytes) else { continue };
            if header.kind != *RECOVERY || header.set != self.set {
                continue;
            }
            if header.length != packet {
                self.damaged += 1;
                continue;
            }
            // The length is at most HEADER + 4 + MAX_SLICE, which fits.
            let Some(whole) = bytes.get(..packet as usize) else {
                at = start;
                break;
            };
            let Some((exponent, data)) = whole[packet::HEADER..].split_first_chunk::<4>() else { continue };
            let exponent = u32::from_le_bytes(*exponent);
            match u16::try_from(exponent) {
                Ok(exponent) if header.intact(whole) => {
                    if self.exponents.insert(exponent) {
                        self.blocks.push(RecoveryBlock { exponent, data: data.to_vec() });
                    }
                    at = start + whole.len();
                }
                _ => self.damaged += 1,
            }
        }
        self.pending.drain(..at);
    }

    /// The distinct blocks found so far.
    pub fn blocks(&self) -> &[RecoveryBlock] {
        &self.blocks
    }

    /// Recovery packets of this set passed over because they were damaged.
    pub fn damaged(&self) -> u32 {
        self.damaged
    }

    /// The blocks found, in the order found.
    pub fn into_blocks(self) -> Vec<RecoveryBlock> {
        self.blocks
    }
}

#[cfg(test)]
mod tests;
