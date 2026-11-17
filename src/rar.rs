//! RAR4 and RAR5 volume headers: where each stored file's bytes sit in each volume.
//!
//! Nothing here decompresses or decrypts; compressed and encrypted entries are reported as
//! such. Every header's checksum is verified, so a damaged volume is reported as damaged
//! instead of being misread. Sizes read from headers are checked against the volume's real
//! size and never used to size an allocation.

use std::fmt;

/// The archive format of a volume.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// RAR 2.9 to 4.x, which `rar -ma4` still writes.
    Rar4,
    /// RAR 5.0 and later.
    Rar5,
}

/// One file's part in one volume: its header and the data after it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// The path inside the archive, as the archiver wrote it. It may use `\` and may try to
    /// escape the extraction folder; sanitize it before using it as a path.
    pub name: String,
    /// The data is stored as it is, not compressed, so it can be read in place.
    pub stored: bool,
    /// The data is encrypted, so it needs a password.
    pub encrypted: bool,
    /// Offset of this entry's data within the volume.
    pub data_offset: u64,
    /// Bytes of this entry's data inside this volume.
    pub packed_size: u64,
    /// Size of the whole file once every volume's part is joined and unpacked.
    pub unpacked_size: u64,
    /// The file began in an earlier volume.
    pub split_before: bool,
    /// The file continues in the next volume.
    pub split_after: bool,
    /// CRC32 of this volume's part of the data when the file continues in the next volume,
    /// otherwise of the whole file. RAR5 records a keyed checksum for encrypted files.
    pub crc32: Option<u32>,
}

/// The headers of one volume.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Volume {
    /// The archive format.
    pub format: Format,
    /// RAR5 numbers its volumes from 0. RAR4 only marks the first.
    pub number: Option<u64>,
    /// Whether this is the first volume of its set, when the archive says.
    pub first: Option<bool>,
    /// The files in this volume, directories left out, in header order.
    pub entries: Vec<Entry>,
}

/// Why a volume's headers could not be read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RarError {
    /// The data does not start with a RAR4 or RAR5 signature.
    NotRar,
    /// The headers themselves are encrypted, so even the file list needs a password.
    Encrypted,
    /// A header's checksum does not match its bytes: the volume is damaged.
    Corrupt {
        /// Offset of the header within the volume.
        at: u64,
        /// The checksum the header records. RAR4 records only 16 bits.
        stored: u32,
        /// The checksum of the header's bytes as read.
        computed: u32,
    },
    /// A header is internally inconsistent or contradicts the volume's size.
    Malformed {
        /// Offset of the header within the volume.
        at: u64,
        /// What is wrong with it.
        defect: Defect,
    },
    /// Reading the bytes of a header returned fewer than the volume's size implies.
    ShortRead {
        /// Offset of the read within the volume.
        at: u64,
        /// Bytes asked for.
        wanted: usize,
        /// Bytes returned.
        got: usize,
    },
}

/// What makes a header malformed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Defect {
    /// A RAR5 variable-length integer runs past 64 bits.
    LongInteger,
    /// A RAR4 header declares fewer bytes than the 7 every header has.
    TinyHeader {
        /// The declared header size.
        size: u16,
    },
    /// A header's fields run past the size the header declares for itself.
    Overrun,
    /// The volume ends inside a header.
    HeaderPastEnd,
    /// A header is larger than the 64 KiB read for one.
    HeaderTooLarge,
    /// A size or offset is too large to address.
    TooLarge,
    /// A file's data runs past the end of the volume.
    DataPastEnd {
        /// Where the header says the data ends.
        end: u64,
        /// The volume's size.
        size: u64,
    },
    /// A header points the next one at or before itself.
    NoProgress,
}

/// Why [`scan`] failed: reading the volume failed, or what was read is not usable.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ScanError<E> {
    /// The read function failed; its error is passed through.
    Read(E),
    /// The bytes read are not a usable RAR volume.
    Rar(RarError),
}

impl fmt::Display for Defect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LongInteger => f.write_str("a variable-length integer runs past 64 bits"),
            Self::TinyHeader { size } => write!(f, "it declares {size} bytes, fewer than the 7 every header has"),
            Self::Overrun => f.write_str("its fields run past the size it declares for itself"),
            Self::HeaderPastEnd => f.write_str("the volume ends inside it"),
            Self::HeaderTooLarge => f.write_str("it is larger than the 64 KiB read for a header"),
            Self::TooLarge => f.write_str("a size or offset in it is too large to address"),
            Self::DataPastEnd { end, size } => {
                write!(f, "its data runs to byte {end}, past the end of the {size}-byte volume")
            }
            Self::NoProgress => f.write_str("it points the next header at or before itself"),
        }
    }
}

impl fmt::Display for RarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRar => f.write_str("the data does not start with a RAR signature, so it is not a RAR volume"),
            Self::Encrypted => f.write_str(
                "the RAR headers are encrypted, so even the file list needs a password, which this crate does not take",
            ),
            Self::Corrupt { at, stored, computed } => write!(
                f,
                "the RAR header at byte {at} fails its checksum (it records {stored:#x}, its bytes give \
                 {computed:#x}), so the volume is damaged; repair it with PAR2 or use another posting"
            ),
            Self::Malformed { at, defect } => write!(
                f,
                "the RAR header at byte {at} is malformed: {defect}; the volume is damaged or not a RAR \
                 archive this crate can read"
            ),
            Self::ShortRead { at, wanted, got } => write!(
                f,
                "reading the RAR header at byte {at} returned {got} of {wanted} bytes, so the posting holds \
                 less of the volume than its size says"
            ),
        }
    }
}

impl std::error::Error for RarError {}

impl<E: fmt::Display> fmt::Display for ScanError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(e) => write!(f, "{e}"),
            Self::Rar(e) => write!(f, "{e}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for ScanError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(e) => Some(e),
            Self::Rar(e) => Some(e),
        }
    }
}

const RAR4_SIG: &[u8] = b"Rar!\x1a\x07\x00";
const RAR5_SIG: &[u8] = b"Rar!\x1a\x07\x01\x00";
/// Bytes read when a header lies past what has been read so far.
const WINDOW: usize = 64 * 1024;

/// The format whose signature `head` starts with.
pub fn format_of(head: &[u8]) -> Option<Format> {
    if head.starts_with(RAR5_SIG) {
        Some(Format::Rar5)
    } else if head.starts_with(RAR4_SIG) {
        Some(Format::Rar4)
    } else {
        None
    }
}

enum Stop {
    /// The header runs past the bytes read so far.
    Truncated,
    Malformed(Defect),
    Corrupt {
        stored: u32,
        computed: u32,
    },
}

impl Stop {
    /// Inside a header that is wholly present, running out of bytes is a defect, not a cue
    /// to read more.
    fn within_header(self) -> Self {
        match self {
            Self::Truncated => Self::Malformed(Defect::Overrun),
            other => other,
        }
    }
}

/// Whether header checksums are verified. Fuzzing skips them to reach the parsers.
#[derive(Clone, Copy)]
enum Checksums {
    Verify,
    #[cfg_attr(not(any(test, fuzzing)), allow(dead_code))]
    Skip,
}

impl Checksums {
    const DEFAULT: Self = if cfg!(fuzzing) { Self::Skip } else { Self::Verify };

    fn check(self, stored: u32, computed: u32) -> Result<(), Stop> {
        match self {
            Self::Verify if stored != computed => Err(Stop::Corrupt { stored, computed }),
            _ => Ok(()),
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn need(&self, n: usize) -> Result<(), Stop> {
        match self.pos.checked_add(n) {
            Some(end) if end <= self.b.len() => Ok(()),
            _ => Err(Stop::Truncated),
        }
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], Stop> {
        self.need(n)?;
        let start = self.pos;
        self.pos += n;
        Ok(&self.b[start..self.pos])
    }
    fn u8(&mut self) -> Result<u8, Stop> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, Stop> {
        Ok(u16::from(self.u8()?) | u16::from(self.u8()?) << 8)
    }
    fn u32(&mut self) -> Result<u32, Stop> {
        Ok(u32::from(self.u16()?) | u32::from(self.u16()?) << 16)
    }
    fn vint(&mut self) -> Result<u64, Stop> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.u8()?;
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(Stop::Malformed(Defect::LongInteger))
    }
}

enum Block {
    Main { number: Option<u64>, first: Option<bool>, encrypted: bool },
    File(Option<Entry>),
    Encryption,
    End,
    Other,
}

fn to_usize(v: u64) -> Result<usize, Stop> {
    usize::try_from(v).map_err(|_| Stop::Malformed(Defect::TooLarge))
}

fn add(a: u64, b: u64) -> Result<u64, Stop> {
    a.checked_add(b).ok_or(Stop::Malformed(Defect::TooLarge))
}

/// A parser for one block at an offset in a buffer: the block, and where the next starts.
/// Offsets in the result are relative to the buffer.
type Parse = fn(&[u8], usize, Checksums) -> Result<(Block, u64), Stop>;

fn block5(b: &[u8], at: usize, checksums: Checksums) -> Result<(Block, u64), Stop> {
    let mut r = Reader { b, pos: at };
    let stored = r.u32()?;
    let header_size = to_usize(r.vint()?)?;
    let start = r.pos;
    r.need(header_size)?;
    let end = start + header_size;
    // The CRC covers the header from its size field to its end.
    checksums.check(stored, crc32fast::hash(&b[at + 4..end]))?;
    let (block, data_size) = fields5(Reader { b: &b[..end], pos: start }).map_err(Stop::within_header)?;
    Ok((block, add(end as u64, data_size)?))
}

/// The fields of a RAR5 header that `r` holds exactly, and the size of the data after it.
fn fields5(mut r: Reader<'_>) -> Result<(Block, u64), Stop> {
    let end = r.b.len();
    let kind = r.vint()?;
    let flags = r.vint()?;
    let extra_size = if flags & 0x1 != 0 { to_usize(r.vint()?)? } else { 0 };
    let data_size = if flags & 0x2 != 0 { r.vint()? } else { 0 };
    let block = match kind {
        1 => {
            let archive_flags = r.vint()?;
            let number =
                if archive_flags & 0x2 != 0 { Some(r.vint()?) } else { (archive_flags & 0x1 != 0).then_some(0) };
            Block::Main { number, first: number.map(|n| n == 0), encrypted: false }
        }
        2 => {
            let file_flags = r.vint()?;
            let unpacked_size = r.vint()?;
            r.vint()?; // attributes
            if file_flags & 0x2 != 0 {
                r.u32()?; // mtime
            }
            let crc32 = if file_flags & 0x4 != 0 { Some(r.u32()?) } else { None };
            let compression = r.vint()?;
            r.vint()?; // host OS
            let name_len = to_usize(r.vint()?)?;
            let name = String::from_utf8_lossy(r.bytes(name_len)?).into_owned();
            let extra_start = end.checked_sub(extra_size).filter(|&s| s >= r.pos).ok_or(Stop::Truncated)?;
            let mut extra = Reader { b: r.b, pos: extra_start };
            let mut encrypted = false;
            // Each record's start is past the previous one's size field, so this ends.
            while extra.pos < end {
                let size = to_usize(extra.vint()?)?;
                let record_end = extra.pos.checked_add(size).ok_or(Stop::Truncated)?;
                if extra.vint()? == 0x01 {
                    encrypted = true;
                }
                extra.pos = record_end;
            }
            let directory = file_flags & 0x1 != 0;
            Block::File((!directory).then_some(Entry {
                name,
                stored: (compression >> 7) & 0x7 == 0,
                encrypted,
                data_offset: end as u64,
                packed_size: data_size,
                unpacked_size,
                split_before: flags & 0x8 != 0,
                split_after: flags & 0x10 != 0,
                crc32,
            }))
        }
        4 => Block::Encryption,
        5 => Block::End,
        _ => Block::Other,
    };
    Ok((block, data_size))
}

/// The bytes a RAR4 header's CRC covers, as unrar checks them, or None where it does not:
/// old authenticity and signature blocks, and the old Unix owner block, whose names sit
/// outside its declared size. A RAR 2.x comment embedded in a main or comment header is
/// left out; the CRC covers the 13 bytes before it.
fn crc4_span(header: &[u8], kind: u8, flags: u16) -> Option<&[u8]> {
    let embedded_comment = (kind == 0x73 && flags & 0x0002 != 0) || kind == 0x75;
    match kind {
        0x76 | 0x79 => None,
        0x77 if header.get(11..13) == Some(&[0x01, 0x01]) => None,
        _ if embedded_comment => Some(&header[2..header.len().min(13)]),
        _ => Some(&header[2..]),
    }
}

fn block4(b: &[u8], at: usize, checksums: Checksums) -> Result<(Block, u64), Stop> {
    let mut r = Reader { b, pos: at };
    let stored = r.u16()?;
    let kind = r.u8()?;
    let flags = r.u16()?;
    let size = r.u16()?;
    if size < 7 {
        return Err(Stop::Malformed(Defect::TinyHeader { size }));
    }
    let header = Reader { b, pos: at }.bytes(usize::from(size))?;
    if let Some(span) = crc4_span(header, kind, flags) {
        // The low 16 bits of a CRC32.
        checksums.check(u32::from(stored), crc32fast::hash(span) & 0xffff)?;
    }
    let end = (at + header.len()) as u64;
    let (block, data_size) = fields4(Reader { b: header, pos: 7 }, kind, flags, end).map_err(Stop::within_header)?;
    Ok((block, add(end, data_size)?))
}

/// The fields of a RAR4 header that `r` holds exactly, and the size of the data after it.
/// `end` is the header's end within the buffer.
fn fields4(mut r: Reader<'_>, kind: u8, flags: u16, end: u64) -> Result<(Block, u64), Stop> {
    let add_size = if flags & 0x8000 != 0 { u64::from(r.u32()?) } else { 0 };
    let block = match kind {
        0x73 => Block::Main { number: None, first: Some(flags & 0x0100 != 0), encrypted: flags & 0x0080 != 0 },
        0x74 => {
            let mut f = Reader { b: r.b, pos: 7 };
            let pack_low = f.u32()?;
            let unp_low = f.u32()?;
            f.u8()?; // host OS
            let crc32 = f.u32()?;
            f.u32()?; // time
            f.u8()?; // version
            let method = f.u8()?;
            let name_size = usize::from(f.u16()?);
            let attributes = f.u32()?;
            let (pack_high, unp_high) = if flags & 0x100 != 0 { (f.u32()?, f.u32()?) } else { (0, 0) };
            let name = name4(f.bytes(name_size)?, flags & 0x200 != 0);
            let directory = flags & 0xe0 == 0xe0 || attributes & 0x10 != 0;
            let packed_size = u64::from(pack_high) << 32 | u64::from(pack_low);
            let entry = Entry {
                name,
                stored: method == 0x30,
                encrypted: flags & 0x4 != 0,
                data_offset: end,
                packed_size,
                unpacked_size: u64::from(unp_high) << 32 | u64::from(unp_low),
                split_before: flags & 0x1 != 0,
                split_after: flags & 0x2 != 0,
                crc32: Some(crc32),
            };
            // ADD_SIZE holds only the low 32 bits of a file over 4 GiB.
            return Ok((Block::File((!directory).then_some(entry)), packed_size));
        }
        0x7b => Block::End,
        _ => Block::Other,
    };
    Ok((block, add_size))
}

/// A RAR4 file name. With the Unicode flag it is either UTF-8, or a legacy byte name, a NUL
/// and the Unicode name encoded against those bytes. Without it, the archiver's code page,
/// which is read as UTF-8 when it is valid and as Latin-1 otherwise.
fn name4(raw: &[u8], unicode: bool) -> String {
    let legacy = |bytes: &[u8]| match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes.iter().map(|&c| char::from(c)).collect(),
    };
    match raw.iter().position(|&c| c == 0) {
        Some(nul) if unicode => decode_name4(&raw[..nul], &raw[nul + 1..]).unwrap_or_else(|| legacy(&raw[..nul])),
        Some(nul) => legacy(&raw[..nul]),
        None => legacy(raw),
    }
}

/// The RAR 2.9 to 4.x Unicode name encoding: UTF-16 units, coded two bits at a time as a
/// literal low byte, a low byte under a shared high byte, a full unit, or a run copied from
/// the legacy name. None when the encoding is cut short. A copied run never reads past the
/// legacy name, so the output is bounded by the input.
fn decode_name4(legacy: &[u8], encoded: &[u8]) -> Option<String> {
    let mut units: Vec<u16> = Vec::with_capacity(legacy.len());
    let mut bytes = encoded.iter().copied();
    let high = u16::from(bytes.next()?) << 8;
    let (mut flags, mut flag_bits) = (0u8, 0);
    while let Some(b) = bytes.next() {
        if flag_bits == 0 {
            (flags, flag_bits) = (b, 8);
            continue;
        }
        match flags >> 6 {
            0 => units.push(u16::from(b)),
            1 => units.push(high | u16::from(b)),
            2 => units.push(u16::from(b) | u16::from(bytes.next()?) << 8),
            _ => {
                let run = usize::from(b & 0x7f) + 2;
                let correction = if b & 0x80 != 0 { Some(bytes.next()?) } else { None };
                for _ in 0..run {
                    let c = *legacy.get(units.len())?;
                    units.push(match correction {
                        Some(k) => high | u16::from(c.wrapping_add(k)),
                        None => u16::from(c),
                    });
                }
            }
        }
        flags <<= 2;
        flag_bits -= 2;
    }
    Some(String::from_utf16_lossy(&units))
}

/// Walks the block headers of a volume of `size` bytes. `head` is its start, usually the
/// first article; headers past it, such as a file that starts mid-volume after a sample,
/// are read through `read(offset, length)` in windows of at most 64 KiB, so file data is
/// never fetched. `read` must return the `length` bytes at `offset`; fewer is an error.
///
/// Every header's checksum is verified. Scanning stops at the end-of-archive header, or at
/// a file that continues in the next volume, since its data fills the rest of this one.
pub async fn scan<E, F, Fut>(head: Vec<u8>, size: u64, read: F) -> Result<Volume, ScanError<E>>
where
    F: FnMut(u64, usize) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, E>>,
{
    walk(head, size, read, Checksums::DEFAULT).await
}

async fn walk<E, F, Fut>(head: Vec<u8>, size: u64, mut read: F, checksums: Checksums) -> Result<Volume, ScanError<E>>
where
    F: FnMut(u64, usize) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, E>>,
{
    let format = format_of(&head).ok_or(ScanError::Rar(RarError::NotRar))?;
    let (parse, signature): (Parse, &[u8]) = match format {
        Format::Rar5 => (block5, RAR5_SIG),
        Format::Rar4 => (block4, RAR4_SIG),
    };
    let malformed = |at, defect| ScanError::Rar(RarError::Malformed { at, defect });
    let (mut window_at, mut window) = (0u64, head);
    let mut pos = signature.len() as u64;
    let mut volume = Volume { format, number: None, first: None, entries: Vec::new() };
    // Each pass either moves `pos` forward past a header or reads the window at `pos`, and
    // a window read at `pos` is parsed before any other read, so this ends.
    while pos < size {
        let relative = usize::try_from(pos - window_at).ok().filter(|&r| r < window.len());
        let (block, next) = match relative.map(|r| parse(&window, r, checksums)) {
            Some(Ok(parsed)) => parsed,
            Some(Err(Stop::Malformed(defect))) => return Err(malformed(pos, defect)),
            Some(Err(Stop::Corrupt { stored, computed })) => {
                return Err(ScanError::Rar(RarError::Corrupt { at: pos, stored, computed }));
            }
            Some(Err(Stop::Truncated)) | None => {
                let want = usize::try_from(size - pos).map_or(WINDOW, |left| left.min(WINDOW));
                if window_at == pos && window.len() >= want {
                    let defect = if want < WINDOW { Defect::HeaderPastEnd } else { Defect::HeaderTooLarge };
                    return Err(malformed(pos, defect));
                }
                window = read(pos, want).await.map_err(ScanError::Read)?;
                window_at = pos;
                if window.len() < want {
                    return Err(ScanError::Rar(RarError::ShortRead { at: pos, wanted: want, got: window.len() }));
                }
                continue;
            }
        };
        let next = next.checked_add(window_at).ok_or(malformed(pos, Defect::TooLarge))?;
        match block {
            Block::Main { encrypted: true, .. } | Block::Encryption => return Err(ScanError::Rar(RarError::Encrypted)),
            Block::Main { number, first, .. } => {
                volume.number = if format == Format::Rar5 { number } else { None };
                volume.first = first;
            }
            Block::File(Some(mut entry)) => {
                entry.data_offset = entry.data_offset.checked_add(window_at).ok_or(malformed(pos, Defect::TooLarge))?;
                if next > size {
                    return Err(malformed(pos, Defect::DataPastEnd { end: next, size }));
                }
                let split_after = entry.split_after;
                volume.entries.push(entry);
                // Data that continues in the next volume fills the rest of this one.
                if split_after {
                    break;
                }
            }
            Block::End => break,
            Block::File(None) | Block::Other => {}
        }
        if next <= pos {
            return Err(malformed(pos, Defect::NoProgress));
        }
        pos = next;
    }
    Ok(volume)
}

#[cfg(any(test, feature = "fake"))]
pub(crate) mod fixtures {
    pub struct File<'a> {
        pub name: &'a str,
        pub data: u64,
        pub split: u8,
        pub method: u8,
        pub unpacked: u64,
    }

    fn vint(mut n: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut b = (n & 0x7f) as u8;
            n >>= 7;
            if n > 0 {
                b |= 0x80;
            }
            out.push(b);
            if n == 0 {
                return out;
            }
        }
    }

    fn block5(body: Vec<u8>) -> Vec<u8> {
        let header = [vint(body.len() as u64), body].concat();
        [crc32fast::hash(&header).to_le_bytes().to_vec(), header].concat()
    }

    /// Fills in a RAR4 header's CRC16.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn seal4(header: &mut [u8]) {
        let crc = (crc32fast::hash(&header[2..]) & 0xffff) as u16;
        header[..2].copy_from_slice(&crc.to_le_bytes());
    }

    /// `split` uses RAR5 header flags: 0x8 before, 0x10 after. `method` is the compression method.
    pub fn rar5(number: u64, files: &[File<'_>], fill: u8) -> Vec<u8> {
        let mut out = b"Rar!\x1a\x07\x01\x00".to_vec();
        let main = if number == 0 { vint(0x1) } else { [vint(0x3), vint(number)].concat() };
        out.extend(block5([vec![1, 0], main].concat()));
        for f in files {
            let name = f.name.as_bytes();
            let body = [
                vec![2],
                vint(0x2 | u64::from(f.split)),
                vint(f.data),
                vint(0x4),
                vint(f.unpacked),
                vec![0],
                0xdead_beef_u32.to_le_bytes().to_vec(),
                vint(u64::from(f.method) << 7),
                vec![0],
                vint(name.len() as u64),
                name.to_vec(),
            ]
            .concat();
            out.extend(block5(body));
            out.extend(std::iter::repeat_n(fill, f.data as usize));
        }
        out
    }

    /// `split` uses RAR4 flags: 0x1 before, 0x2 after. `method` 0x30 is stored.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn rar4(first: bool, files: &[File<'_>], fill: u8) -> Vec<u8> {
        let mut out = b"Rar!\x1a\x07\x00".to_vec();
        let mut main = vec![0, 0, 0x73];
        main.extend((0x0001u16 | if first { 0x0100 } else { 0 }).to_le_bytes());
        main.extend(13u16.to_le_bytes());
        main.extend([0; 6]);
        seal4(&mut main);
        out.extend(main);
        for f in files {
            let name = f.name.as_bytes();
            let mut header = vec![0, 0, 0x74];
            header.extend((0x8000u16 | u16::from(f.split)).to_le_bytes());
            header.extend((32 + name.len() as u16).to_le_bytes());
            header.extend((f.data as u32).to_le_bytes());
            header.extend((f.unpacked as u32).to_le_bytes());
            header.push(2);
            header.extend(0x1234_5678_u32.to_le_bytes());
            header.extend([0; 4]);
            header.extend([29, f.method]);
            header.extend((name.len() as u16).to_le_bytes());
            header.extend(0x20u32.to_le_bytes());
            header.extend(name);
            seal4(&mut header);
            out.extend(header);
            out.extend(std::iter::repeat_n(fill, f.data as usize));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{File, rar4, rar5, seal4};
    use super::*;

    async fn scan_all(volume: &[u8], head: usize) -> (Result<Volume, ScanError<()>>, Vec<u64>) {
        let mut reads = Vec::new();
        let r = scan(volume[..head].to_vec(), volume.len() as u64, |at: u64, len: usize| {
            reads.push(at);
            std::future::ready(Ok::<_, ()>(volume[at as usize..at as usize + len].to_vec()))
        })
        .await;
        (r, reads)
    }

    #[tokio::test]
    async fn rar5_finds_a_file_that_starts_mid_volume() {
        let volume = rar5(
            4,
            &[
                File { name: "!Sample.mkv", data: 300, split: 0x8, method: 0, unpacked: 1000 },
                File { name: "Movie.mkv", data: 500, split: 0x10, method: 0, unpacked: 90_000 },
            ],
            0xaa,
        );
        let (v, reads) = scan_all(&volume, 64).await;
        let v = v.unwrap();
        assert_eq!(v.number, Some(4));
        let summary: Vec<_> = v
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.stored, e.split_before, e.split_after, e.packed_size))
            .collect();
        assert_eq!(summary, [("!Sample.mkv", true, true, false, 300), ("Movie.mkv", true, false, true, 500)]);
        let movie = &v.entries[1];
        assert_eq!(&volume[movie.data_offset as usize - 9..movie.data_offset as usize], b"Movie.mkv");
        assert_eq!(reads.len(), 1, "only the second header is read, not the sample's data");
    }

    #[tokio::test]
    async fn rar5_reports_compression() {
        let volume = rar5(0, &[File { name: "a.mkv", data: 10, split: 0, method: 3, unpacked: 50 }], 0);
        let v = scan_all(&volume, volume.len()).await.0.unwrap();
        assert!(!v.entries[0].stored);
        assert_eq!(v.first, Some(true));
    }

    #[tokio::test]
    async fn rar4_reads_store_method_offsets_and_crc() {
        let volume = rar4(
            true,
            &[
                File { name: "!Sample.mkv", data: 200, split: 0, method: 0x30, unpacked: 200 },
                File { name: "Movie.mkv", data: 300, split: 0x2, method: 0x30, unpacked: 9000 },
            ],
            0xbb,
        );
        let v = scan_all(&volume, 80).await.0.unwrap();
        assert_eq!(v.first, Some(true));
        let movie = &v.entries[1];
        assert_eq!((movie.stored, movie.packed_size, movie.crc32), (true, 300, Some(0x1234_5678)));
        assert_eq!(volume.len() as u64 - movie.data_offset, 300);
        assert_eq!(volume[movie.data_offset as usize], 0xbb);
    }

    #[tokio::test]
    async fn a_short_read_is_an_error_not_a_retry() {
        let volume = rar5(0, &[File { name: "a.mkv", data: 300, split: 0, method: 0, unpacked: 300 }], 0);
        let mut reads = 0;
        let r = scan(volume[..20].to_vec(), volume.len() as u64, |_, _| {
            reads += 1;
            std::future::ready(Ok::<_, ()>(vec![0; 4]))
        })
        .await;
        assert_eq!(r, Err(ScanError::Rar(RarError::ShortRead { at: 16, wanted: volume.len() - 16, got: 4 })));
        assert_eq!(reads, 1);
    }

    #[test]
    fn rar4_unicode_names_decode() {
        // Written by rar 6.24 for "Subs\\Ünïcödé 日本語.srt".
        let raw = [
            &b"Subs\\\xc3\x9cn\xc3\xafc\xc3\xb6d\xc3\xa9 \xe6\x97\xa5\xe6\x9c\xac\xe8\xaa\x9e.srt\0"[..],
            b"e\xc0\x03\xdcn\xef\x00c\xf6d\xe9\x1a \xe5,g\x9e\x8a\x00.srt",
        ]
        .concat();
        assert_eq!(name4(&raw, true), "Subs\\Ünïcödé 日本語.srt");
        assert_eq!(name4("Ünï.srt".as_bytes(), true), "Ünï.srt");
        assert_eq!(name4(b"caf\xe9.srt", false), "café.srt");
    }

    #[tokio::test]
    async fn rar4_finds_the_block_after_a_file_over_4_gib() {
        let packed = (1u64 << 32) + 100;
        let mut head = b"Rar!\x1a\x07\x00".to_vec();
        let mut main = vec![0, 0, 0x73, 0x01, 0x01, 13, 0];
        main.extend([0; 6]);
        seal4(&mut main);
        head.extend(main);
        let name = b"big.mkv";
        let mut file = vec![0, 0, 0x74];
        file.extend(0x8100u16.to_le_bytes());
        file.extend((40 + name.len() as u16).to_le_bytes());
        file.extend((packed as u32).to_le_bytes());
        file.extend((packed as u32).to_le_bytes());
        file.push(2);
        file.extend([0; 8]);
        file.extend([29, 0x30]);
        file.extend((name.len() as u16).to_le_bytes());
        file.extend(0x20u32.to_le_bytes());
        file.extend(((packed >> 32) as u32).to_le_bytes());
        file.extend(((packed >> 32) as u32).to_le_bytes());
        file.extend(name);
        seal4(&mut file);
        head.extend(file);
        let data_at = head.len() as u64;
        let tail = rar4(false, &[File { name: "after.nfo", data: 10, split: 0, method: 0x30, unpacked: 10 }], 1);
        let tail = &tail[20..]; // its file block, without signature and main block
        let size = data_at + packed + tail.len() as u64;
        let r = scan(head, size, |at: u64, len: usize| {
            let from = (at - (data_at + packed)) as usize;
            std::future::ready(Ok::<_, ()>(tail[from..from + len].to_vec()))
        })
        .await
        .unwrap();
        let entries: Vec<_> = r.entries.iter().map(|e| (e.name.as_str(), e.packed_size, e.data_offset)).collect();
        assert_eq!(entries, [("big.mkv", packed, data_at), ("after.nfo", 10, size - 10)]);
    }

    #[tokio::test]
    async fn rar4_encrypted_headers_need_a_password() {
        let mut volume = rar4(true, &[], 0);
        volume[10] |= 0x80;
        seal4(&mut volume[7..20]);
        assert_eq!(scan_all(&volume, volume.len()).await.0, Err(ScanError::Rar(RarError::Encrypted)));
    }

    #[tokio::test]
    async fn a_damaged_header_fails_its_checksum() {
        let mut volume = rar5(0, &[File { name: "a.mkv", data: 10, split: 0, method: 0, unpacked: 10 }], 0);
        let at = volume.len() - 12; // a byte of the file name
        volume[at] ^= 0x20;
        let r = scan_all(&volume, volume.len()).await.0;
        assert!(matches!(r, Err(ScanError::Rar(RarError::Corrupt { at: 16, .. }))), "{r:?}");

        let mut volume = rar4(true, &[File { name: "a.mkv", data: 10, split: 0, method: 0x30, unpacked: 10 }], 0);
        let at = volume.len() - 12;
        volume[at] ^= 0x20;
        let r = scan_all(&volume, volume.len()).await.0;
        assert!(matches!(r, Err(ScanError::Rar(RarError::Corrupt { at: 20, .. }))), "{r:?}");
    }

    #[tokio::test]
    async fn an_old_comment_in_a_rar4_main_header_is_outside_its_checksum() {
        let mut volume = b"Rar!\x1a\x07\x00".to_vec();
        let mut main = vec![0, 0, 0x73, 0x03, 0x01, 20, 0];
        main.extend([0; 6]);
        main.extend(b"comment"); // the old comment block, inside the main header
        let crc = (crc32fast::hash(&main[2..13]) & 0xffff) as u16;
        main[..2].copy_from_slice(&crc.to_le_bytes());
        volume.extend(&main);
        assert_eq!(scan_all(&volume, volume.len()).await.0.map(|v| v.first), Ok(Some(true)));
        volume[7 + 15] ^= 1;
        assert!(scan_all(&volume, volume.len()).await.0.is_ok(), "the comment is not covered");
        volume[7 + 10] ^= 1;
        let r = scan_all(&volume, volume.len()).await.0;
        assert!(matches!(r, Err(ScanError::Rar(RarError::Corrupt { at: 7, .. }))), "{r:?}");
    }

    #[tokio::test]
    async fn data_past_the_end_of_the_volume_is_malformed() {
        let volume = rar5(0, &[File { name: "a.mkv", data: 10, split: 0, method: 0, unpacked: 10 }], 0);
        let cut = &volume[..volume.len() - 1];
        let r = scan_all(cut, cut.len()).await.0;
        let size = cut.len() as u64;
        assert_eq!(
            r,
            Err(ScanError::Rar(RarError::Malformed { at: 16, defect: Defect::DataPastEnd { end: size + 1, size } }))
        );
    }

    #[tokio::test]
    async fn a_header_whose_fields_overrun_it_is_malformed() {
        // A RAR4 file header that declares 20 bytes, too few for its fixed fields.
        let mut volume = rar4(true, &[], 0);
        let mut file = vec![0, 0, 0x74, 0, 0, 20, 0];
        file.extend([0; 13]);
        seal4(&mut file);
        volume.extend(file);
        let r = scan_all(&volume, volume.len()).await.0;
        assert_eq!(r, Err(ScanError::Rar(RarError::Malformed { at: 20, defect: Defect::Overrun })));
    }

    /// xorshift64*, so the mutation test is the same on every run.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    fn mutate(rng: &mut Rng, bytes: &mut Vec<u8>) {
        let at = rng.below(bytes.len());
        match rng.below(5) {
            0 => {
                if let Some(b) = bytes.get_mut(at) {
                    *b ^= 1 << rng.below(8);
                }
            }
            1 => {
                if let Some(b) = bytes.get_mut(at) {
                    *b = [0x00, 0x01, 0x7f, 0x80, 0xff][rng.below(5)];
                }
            }
            2 => bytes.truncate(at),
            3 => {
                let chunk: Vec<u8> = bytes.iter().skip(rng.below(bytes.len())).take(rng.below(64)).copied().collect();
                bytes.splice(at..at, chunk);
            }
            _ => {
                let value = [u32::MAX, 0x8000_0000, 0xffff, 0x7fff_ffff][rng.below(4)].to_le_bytes();
                for (b, v) in bytes.iter_mut().skip(at).zip(value) {
                    *b = v;
                }
            }
        }
    }

    /// Mutations of real volumes, with checksums verified and skipped so the parsers behind
    /// them are reached: no panic, no overflow, and reads only ever move forward through the
    /// bytes that exist, so every scan ends.
    #[tokio::test]
    async fn mutated_volumes_never_panic_or_loop() {
        let fixtures: [&[u8]; 6] = [
            include_bytes!("../tests/data/rar/rar4/multi_dirs_unicode/arc.rar"),
            include_bytes!("../tests/data/rar/rar4/comment_rr/arc.part1.rar"),
            include_bytes!("../tests/data/rar/rar4/enc_headers/arc.part2.rar"),
            include_bytes!("../tests/data/rar/rar5/late_header/arc.rar"),
            include_bytes!("../tests/data/rar/rar5/quickopen/arc.part3.rar"),
            include_bytes!("../tests/data/rar/rar5/enc_data/arc.part1.rar"),
        ];
        let mut outcomes = [0usize; 2];
        for (k, original) in fixtures.iter().enumerate() {
            let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ k as u64);
            for i in 0..500 {
                let mut bytes = original.to_vec();
                for _ in 0..=rng.below(3) {
                    mutate(&mut rng, &mut bytes);
                }
                let size = match rng.below(4) {
                    0 => bytes.len() as u64 + rng.below(1 << 20) as u64,
                    1 => u64::MAX,
                    _ => bytes.len() as u64,
                };
                let head = bytes[..bytes.len().min(rng.below(3000))].to_vec();
                let checksums = if i % 2 == 0 { Checksums::Skip } else { Checksums::Verify };
                let mut reads = Vec::new();
                let r = walk(
                    head,
                    size,
                    |at: u64, len: usize| {
                        assert!(len <= WINDOW);
                        reads.push(at);
                        let from = usize::try_from(at).unwrap_or(usize::MAX).min(bytes.len());
                        std::future::ready(Ok::<_, ()>(bytes[from..bytes.len().min(from + len)].to_vec()))
                    },
                    checksums,
                )
                .await;
                assert!(reads.windows(2).all(|w| w[0] < w[1]), "fixture {k}, run {i}: reads {reads:?}");
                assert!(reads.len() <= bytes.len() + 1, "fixture {k}, run {i}");
                if let Ok(volume) = r {
                    for e in &volume.entries {
                        assert!(e.data_offset.checked_add(e.packed_size).is_some_and(|end| end <= size));
                    }
                }
                outcomes[i % 2] += 1;
            }
        }
        assert_eq!(outcomes, [1500, 1500]);
    }
}
