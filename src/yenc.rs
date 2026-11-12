//! yEnc article bodies. NNTP dot-stuffing is already removed by [`crate::nntp`].

use std::fmt;

/// One decoded article: a part of a posted file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Part {
    /// The name the poster gave the file. Often obfuscated.
    pub name: String,
    /// Size of the whole posted file.
    pub file_size: u64,
    /// Offset of this part within the posted file.
    pub begin: u64,
    pub data: Vec<u8>,
}

impl Part {
    pub fn end(&self) -> u64 {
        self.begin + self.data.len() as u64
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum YencError {
    NoHeader,
    Header(String),
    Length { name: String, expected: u64, actual: u64 },
    Crc { name: String, begin: u64 },
}

impl fmt::Display for YencError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHeader => f.write_str("the article has no =ybegin line, so it is not yEnc"),
            Self::Header(line) => write!(f, "malformed yEnc header: {line}"),
            Self::Length { name, expected, actual } => {
                write!(f, "yEnc data for {name:?} decoded to {actual} bytes, expected {expected}")
            }
            Self::Crc { name, begin } => {
                write!(f, "yEnc CRC mismatch for {name:?} at byte {begin}; the article is corrupt")
            }
        }
    }
}

impl std::error::Error for YencError {}

fn line_at(buf: &[u8], from: usize) -> (&[u8], usize) {
    let end = buf[from..].iter().position(|&b| b == b'\n').map_or(buf.len(), |i| from + i);
    let line = &buf[from..end];
    (line.strip_suffix(b"\r").unwrap_or(line), end + 1)
}

/// `key=value` pairs; `name=` runs to the end of the line.
fn fields(line: &str) -> Vec<(&str, &str)> {
    let (head, name) = match line.find(" name=") {
        Some(at) => (&line[..at], Some(&line[at + " name=".len()..])),
        None => (line, None),
    };
    let mut out: Vec<(&str, &str)> = head.split_whitespace().filter_map(|w| w.split_once('=')).collect();
    out.extend(name.map(|n| ("name", n)));
    out
}

fn get<'a>(fields: &[(&str, &'a str)], key: &str) -> Option<&'a str> {
    fields.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

fn int(fields: &[(&str, &str)], key: &str, line: &str) -> Result<u64, YencError> {
    get(fields, key)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| YencError::Header(format!("missing a valid {key:?}: {line}")))
}

/// Decodes one article body and checks its length and, when the poster sent one, its CRC32.
pub fn decode(body: &[u8]) -> Result<Part, YencError> {
    let mut pos = 0;
    let header = loop {
        if pos >= body.len() {
            return Err(YencError::NoHeader);
        }
        let (line, next) = line_at(body, pos);
        pos = next;
        if line.starts_with(b"=ybegin ") {
            break String::from_utf8_lossy(line).into_owned();
        }
    };
    let head = fields(&header);
    let file_size = int(&head, "size", &header)?;
    let name = get(&head, "name").unwrap_or_default().to_string();
    let multipart = get(&head, "part").is_some();
    let (begin, end) = if multipart {
        let (line, next) = line_at(body, pos.min(body.len()));
        pos = next;
        let line = String::from_utf8_lossy(line);
        if !line.starts_with("=ypart ") {
            return Err(YencError::Header(format!("multipart body has no =ypart line after: {header}")));
        }
        let part = fields(&line);
        let begin = int(&part, "begin", &line)?;
        if begin == 0 {
            return Err(YencError::Header(format!("=ypart begins at 0, but offsets start at 1: {line}")));
        }
        (begin - 1, int(&part, "end", &line)?)
    } else {
        (0, file_size)
    };
    if begin >= file_size.max(1) {
        return Err(YencError::Header(format!("part begins at {begin}, past the end of a {file_size}-byte file")));
    }
    // Some posters write a wrong `end` on the last part. Like other decoders, trust `begin`
    // and the decoded length then, as long as the part stays inside the file.
    let declared = end.checked_sub(begin).filter(|_| end <= file_size);
    let mut data = Vec::with_capacity(declared.unwrap_or(file_size - begin).min(4 << 20) as usize);
    let mut at_line_start = true;
    let mut trailer: Option<&[u8]> = None;
    let mut i = pos;
    while i < body.len() {
        let b = body[i];
        match b {
            b'\n' => {
                at_line_start = true;
                i += 1;
                continue;
            }
            b'\r' => {
                i += 1;
                continue;
            }
            b'=' if at_line_start && body.get(i + 1) == Some(&b'y') => {
                trailer = Some(line_at(body, i).0);
                break;
            }
            b'=' => {
                i += 1;
                data.push(body.get(i).copied().unwrap_or(0).wrapping_sub(106));
            }
            _ => data.push(b.wrapping_sub(42)),
        }
        at_line_start = false;
        i += 1;
    }
    let actual = data.len() as u64;
    match declared {
        Some(expected) if actual != expected => return Err(YencError::Length { name, expected, actual }),
        None if begin + actual > file_size => {
            return Err(YencError::Length { name, expected: file_size - begin, actual });
        }
        _ => {}
    }
    if let Some(trailer) = trailer {
        let trailer = String::from_utf8_lossy(trailer);
        let tail = fields(&trailer);
        let crc = get(&tail, "pcrc32").or(if multipart { None } else { get(&tail, "crc32") });
        if let Some(crc) = crc.and_then(|c| u32::from_str_radix(c, 16).ok())
            && crc != crc32fast::hash(&data)
        {
            return Err(YencError::Crc { name, begin });
        }
    }
    Ok(Part { name, file_size, begin, data })
}

#[cfg(test)]
pub(crate) fn encode(data: &[u8], name: &str, file_size: u64, begin: u64, part: Option<u32>) -> Vec<u8> {
    let mut out = Vec::new();
    match part {
        Some(p) => {
            out.extend(format!("=ybegin part={p} line=128 size={file_size} name={name}\r\n").bytes());
            out.extend(format!("=ypart begin={} end={}\r\n", begin + 1, begin + data.len() as u64).bytes());
        }
        None => out.extend(format!("=ybegin line=128 size={file_size} name={name}\r\n").bytes()),
    }
    let mut col = 0;
    for &b in data {
        let v = b.wrapping_add(42);
        if matches!(v, 0 | b'\n' | b'\r' | b'=') || (col == 0 && v == b'.') {
            out.extend([b'=', v.wrapping_add(64)]);
            col += 2;
        } else {
            out.push(v);
            col += 1;
        }
        if col >= 128 {
            out.extend(b"\r\n");
            col = 0;
        }
    }
    out.extend(
        format!("\r\n=yend size={} part={} pcrc32={:08x}\r\n", data.len(), part.unwrap_or(1), crc32fast::hash(data))
            .bytes(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_byte_value() {
        let data: Vec<u8> = (0..2048u32).map(|i| i as u8).collect();
        let part = decode(&encode(&data, "a file.mkv", 9000, 4096, Some(3))).unwrap();
        assert_eq!(part.name, "a file.mkv");
        assert_eq!((part.file_size, part.begin, part.end()), (9000, 4096, 6144));
        assert_eq!(part.data, data);
    }

    #[test]
    fn a_single_part_covers_the_whole_file() {
        let part = decode(&encode(b"hello", "x", 5, 0, None)).unwrap();
        assert_eq!((part.begin, part.data.as_slice()), (0, &b"hello"[..]));
    }

    #[test]
    fn a_wrong_crc_is_corruption() {
        let mut body = encode(b"some bytes", "x", 10, 0, Some(1));
        let at = body.windows(7).position(|w| w == b"pcrc32=").unwrap() + 7;
        body[at..at + 8].copy_from_slice(b"00000001");
        assert_eq!(decode(&body), Err(YencError::Crc { name: "x".into(), begin: 0 }));
    }

    #[test]
    fn a_wrong_end_on_the_last_part_is_tolerated() {
        // Seen in the wild: `=ypart begin=6144001 end=1536000` on the last part of a 6579419-byte file.
        let data = vec![7u8; 435_419];
        let mut body = encode(&data, "x.part18.rar", 6_579_419, 6_144_000, Some(9));
        let at = body.windows(11).position(|w| w == b"end=6579419").unwrap();
        body.splice(at..at + 11, *b"end=1536000");
        let part = decode(&body).unwrap();
        assert_eq!((part.begin, part.end()), (6_144_000, 6_579_419));
    }

    #[test]
    fn a_short_body_is_reported() {
        let body = b"=ybegin part=1 line=128 size=10 name=x\r\n=ypart begin=1 end=10\r\nab\r\n=yend size=10\r\n";
        assert!(matches!(decode(body), Err(YencError::Length { expected: 10, actual: 2, .. })));
    }
}
