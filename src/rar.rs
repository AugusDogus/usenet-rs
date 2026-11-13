//! RAR4 and RAR5 volume headers: where each stored file's bytes sit in each volume.
//! Nothing here decompresses; compressed and encrypted entries are reported as such.

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Rar4,
    Rar5,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub name: String,
    pub stored: bool,
    pub encrypted: bool,
    /// Offset of this entry's data within the volume.
    pub data_offset: u64,
    /// Bytes of this entry's data inside this volume.
    pub packed_size: u64,
    pub unpacked_size: u64,
    pub split_before: bool,
    pub split_after: bool,
    /// CRC32 the archive records for this volume's chunk (RAR4) or the whole file (RAR5).
    pub crc32: Option<u32>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Volume {
    pub format: Format,
    /// RAR5 numbers its volumes from 0. RAR4 only marks the first.
    pub number: Option<u64>,
    pub first: Option<bool>,
    pub entries: Vec<Entry>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RarError<E> {
    NotRar,
    Encrypted,
    Malformed(String),
    Read(E),
}

impl<E: fmt::Display> fmt::Display for RarError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRar => f.write_str("not a RAR volume"),
            Self::Encrypted => f.write_str("the archive headers are encrypted, so it needs a password"),
            Self::Malformed(why) => write!(f, "malformed RAR headers: {why}"),
            Self::Read(e) => write!(f, "{e}"),
        }
    }
}

const RAR4_SIG: &[u8] = b"Rar!\x1a\x07\x00";
const RAR5_SIG: &[u8] = b"Rar!\x1a\x07\x01\x00";
/// Bytes read when a header lies past what has been read so far.
const WINDOW: usize = 64 * 1024;

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
    Truncated,
    Malformed(String),
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn need(&self, n: usize) -> Result<(), Stop> {
        if self.pos + n > self.b.len() { Err(Stop::Truncated) } else { Ok(()) }
    }
    fn u8(&mut self) -> Result<u8, Stop> {
        self.need(1)?;
        self.pos += 1;
        Ok(self.b[self.pos - 1])
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
        Err(Stop::Malformed("a variable-length integer is longer than 64 bits".into()))
    }
    fn bytes(&mut self, n: usize) -> Result<&[u8], Stop> {
        self.need(n)?;
        self.pos += n;
        Ok(&self.b[self.pos - n..self.pos])
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
    usize::try_from(v).map_err(|_| Stop::Malformed(format!("size {v} is too large")))
}

/// Parses the block at `at` in `b`. Offsets in the result are relative to `b`.
fn block5(b: &[u8], at: usize) -> Result<(Block, u64), Stop> {
    let mut r = Reader { b, pos: at + 4 }; // header CRC32
    let header_size = to_usize(r.vint()?)?;
    let header_end = r.pos + header_size;
    r.need(header_size)?;
    let kind = r.vint()?;
    let flags = r.vint()?;
    let extra_size = if flags & 0x1 != 0 { to_usize(r.vint()?)? } else { 0 };
    let data_size = if flags & 0x2 != 0 { r.vint()? } else { 0 };
    let next = header_end as u64 + data_size;
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
            let mut encrypted = false;
            let mut extra = Reader { b, pos: header_end.saturating_sub(extra_size) };
            while extra.pos < header_end {
                let size = to_usize(extra.vint()?)?;
                let record_end = extra.pos + size;
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
                data_offset: header_end as u64,
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
    Ok((block, next))
}

fn block4(b: &[u8], at: usize) -> Result<(Block, u64), Stop> {
    let mut r = Reader { b, pos: at + 2 }; // header CRC16
    let kind = r.u8()?;
    let flags = r.u16()?;
    let size = usize::from(r.u16()?);
    let add_size = if flags & 0x8000 != 0 { u64::from(r.u32()?) } else { 0 };
    if size < 7 {
        return Err(Stop::Malformed(format!("a block declares an impossible size {size}")));
    }
    Reader { b, pos: at }.need(size)?;
    let next = (at + size) as u64 + add_size;
    let block = match kind {
        0x73 => Block::Main { number: None, first: Some(flags & 0x0100 != 0), encrypted: flags & 0x0080 != 0 },
        0x74 => {
            let mut f = Reader { b, pos: at + 7 };
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
            // Unicode names are stored as "ascii\0encoded"; the ASCII part is enough here.
            let raw = f.bytes(name_size)?;
            let raw = raw.split(|&c| c == 0).next().unwrap_or(raw);
            let name = raw.iter().map(|&c| char::from(c)).collect();
            let directory = flags & 0xe0 == 0xe0 || attributes & 0x10 != 0;
            Block::File((!directory).then_some(Entry {
                name,
                stored: method == 0x30,
                encrypted: flags & 0x4 != 0,
                data_offset: (at + size) as u64,
                packed_size: u64::from(pack_high) << 32 | u64::from(pack_low),
                unpacked_size: u64::from(unp_high) << 32 | u64::from(unp_low),
                split_before: flags & 0x1 != 0,
                split_after: flags & 0x2 != 0,
                crc32: Some(crc32),
            }))
        }
        0x7b => Block::End,
        _ => Block::Other,
    };
    Ok((block, next))
}

/// Walks the block headers of a volume of `size` bytes. `head` is its start, usually the
/// first article; headers past it, such as a file that starts mid-volume after a sample,
/// are read through `read` in small windows, so file data is never fetched.
pub async fn scan<E, F, Fut>(head: Vec<u8>, size: u64, mut read: F) -> Result<Volume, RarError<E>>
where
    F: FnMut(u64, usize) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, E>>,
{
    let format = format_of(&head).ok_or(RarError::NotRar)?;
    let parse = match format {
        Format::Rar5 => block5,
        Format::Rar4 => block4,
    };
    let (mut window_at, mut window) = (0u64, head);
    let mut pos = match format {
        Format::Rar5 => RAR5_SIG.len(),
        Format::Rar4 => RAR4_SIG.len(),
    } as u64;
    let mut volume = Volume { format, number: None, first: None, entries: Vec::new() };
    while pos < size {
        let relative = (pos - window_at) as usize;
        let (block, next) = match (relative < window.len()).then(|| parse(&window, relative)) {
            Some(Ok(parsed)) => parsed,
            Some(Err(Stop::Malformed(why))) => return Err(RarError::Malformed(format!("at byte {pos}: {why}"))),
            Some(Err(Stop::Truncated)) | None => {
                let want = WINDOW.min((size - pos) as usize);
                if window_at == pos && window.len() >= want {
                    return Err(RarError::Malformed(format!("the header at byte {pos} is over {WINDOW} bytes")));
                }
                window = read(pos, want).await.map_err(RarError::Read)?;
                window_at = pos;
                continue;
            }
        };
        match block {
            Block::Main { encrypted: true, .. } | Block::Encryption => return Err(RarError::Encrypted),
            Block::Main { number, first, .. } => {
                volume.number = if format == Format::Rar5 { number } else { None };
                volume.first = first;
            }
            Block::File(Some(mut entry)) => {
                entry.data_offset += window_at;
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
        let next = next + window_at;
        if next <= pos {
            return Err(RarError::Malformed(format!("the block at byte {pos} does not advance")));
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
        [vec![0; 4], vint(body.len() as u64), body].concat()
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
        out.extend([0, 0, 0x73]);
        out.extend((0x0001u16 | if first { 0x0100 } else { 0 }).to_le_bytes());
        out.extend(13u16.to_le_bytes());
        out.extend([0; 6]);
        for f in files {
            let name = f.name.as_bytes();
            out.extend([0, 0, 0x74]);
            out.extend((0x8000u16 | u16::from(f.split)).to_le_bytes());
            out.extend((32 + name.len() as u16).to_le_bytes());
            out.extend((f.data as u32).to_le_bytes());
            out.extend((f.unpacked as u32).to_le_bytes());
            out.push(2);
            out.extend(0x1234_5678_u32.to_le_bytes());
            out.extend([0; 4]);
            out.extend([29, f.method]);
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0x20u32.to_le_bytes());
            out.extend(name);
            out.extend(std::iter::repeat_n(fill, f.data as usize));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{File, rar4, rar5};
    use super::*;

    async fn scan_all(volume: &[u8], head: usize) -> (Result<Volume, RarError<()>>, Vec<u64>) {
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
    async fn rar4_encrypted_headers_need_a_password() {
        let mut volume = rar4(true, &[], 0);
        volume[10] |= 0x80;
        assert_eq!(scan_all(&volume, volume.len()).await.0, Err(RarError::Encrypted));
    }
}
