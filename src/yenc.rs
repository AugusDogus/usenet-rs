//! yEnc article bodies. NNTP dot-stuffing is already removed by [`crate::nntp`].
//!
//! [`decode`] reads one article: its `=ybegin` (and, for a multipart post, `=ypart`) line,
//! the encoded data, and the `=yend` trailer, and checks the length and CRC32 the poster
//! declared. The data loop runs on SIMD where the CPU has it (SSE2 or AVX2 on x86_64, NEON
//! on aarch64) and on a scalar loop elsewhere; every path decodes exactly the same bytes.

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
    /// The decoded bytes, which belong at `begin..begin + data.len()` of the posted file.
    pub data: Vec<u8>,
}

impl Part {
    /// Offset just past this part within the posted file.
    pub fn end(&self) -> u64 {
        self.begin.saturating_add(self.data.len() as u64)
    }
}

/// A yEnc control line, which carries `key=value` fields.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ControlLine {
    /// The `=ybegin` line that opens an article.
    Begin,
    /// The `=ypart` line that places a multipart article within its file.
    Part,
}

impl fmt::Display for ControlLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Begin => "=ybegin",
            Self::Part => "=ypart",
        })
    }
}

/// Why an article body could not be decoded.
///
/// Header problems ([`NoHeader`](Self::NoHeader) through [`BeginPastEnd`](Self::BeginPastEnd))
/// come from the poster and are usually the same in every copy. Data problems
/// ([`Length`](Self::Length), [`Crc`](Self::Crc), [`Truncated`](Self::Truncated)) mean this copy
/// was damaged or cut short, so another copy of the article may decode.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum YencError {
    /// No line starts with `=ybegin `, so the body is not yEnc.
    NoHeader,
    /// A control line lacks a field the decoder needs.
    MissingField {
        /// The line that lacks it.
        line: ControlLine,
        /// The missing field, such as `size` or `begin`.
        field: &'static str,
        /// The line as read, lossily converted to UTF-8.
        text: String,
    },
    /// A control line has a field whose value is not a decimal number.
    InvalidField {
        /// The line that holds it.
        line: ControlLine,
        /// The field, such as `size` or `begin`.
        field: &'static str,
        /// The value as read.
        value: String,
    },
    /// The `=ybegin` line has a `part` field, but the next line is not `=ypart`.
    MissingPartLine {
        /// The file name from the `=ybegin` line.
        name: String,
    },
    /// The `=ypart` line says `begin=0`, but yEnc offsets start at 1.
    PartBeginsAtZero {
        /// The file name from the `=ybegin` line.
        name: String,
    },
    /// The part starts at or past the end of its file.
    BeginPastEnd {
        /// The file name from the `=ybegin` line.
        name: String,
        /// Zero-based offset where the part claims to start.
        begin: u64,
        /// Size of the whole file from the `=ybegin` line.
        file_size: u64,
    },
    /// The data decoded to a different length than the header declares.
    Length {
        /// The file name from the `=ybegin` line.
        name: String,
        /// Bytes the header declares for this part.
        expected: u64,
        /// Bytes the data decoded to.
        actual: u64,
    },
    /// The data's CRC32 differs from the one in the `=yend` line.
    Crc {
        /// The file name from the `=ybegin` line.
        name: String,
        /// Zero-based offset of the part within its file.
        begin: u64,
        /// The CRC32 the trailer declares.
        expected: u32,
        /// The CRC32 of the decoded data.
        actual: u32,
    },
    /// The body ends before its `=yend` line, so data may be missing.
    Truncated {
        /// The file name from the `=ybegin` line.
        name: String,
        /// Zero-based offset of the part within its file.
        begin: u64,
        /// Bytes decoded before the body ended.
        decoded: u64,
    },
}

impl fmt::Display for YencError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHeader => f.write_str("the article has no =ybegin line, so it is not yEnc and cannot be decoded"),
            Self::MissingField { line, field, text } => {
                write!(f, "the yEnc {line} line has no {field} field ({text:?}), so the article cannot be decoded")
            }
            Self::InvalidField { line, field, value } => write!(
                f,
                "the yEnc {line} line has {field}={value:?}, which is not a decimal number, so the article cannot be decoded"
            ),
            Self::MissingPartLine { name } => write!(
                f,
                "the yEnc article for {name:?} is one part of several but has no =ypart line after =ybegin, \
                 so its place in the file is unknown"
            ),
            Self::PartBeginsAtZero { name } => write!(
                f,
                "the yEnc =ypart line for {name:?} says begin=0, but offsets start at 1, \
                 so its place in the file is unknown"
            ),
            Self::BeginPastEnd { name, begin, file_size } => write!(
                f,
                "the yEnc part of {name:?} begins at byte {begin}, past the end of the {file_size}-byte file, \
                 so it cannot be placed"
            ),
            Self::Length { name, expected, actual } => write!(
                f,
                "yEnc data for {name:?} decoded to {actual} bytes, but its header declares {expected}; \
                 the article is damaged, so fetch another copy"
            ),
            Self::Crc { name, begin, expected, actual } => write!(
                f,
                "yEnc data for {name:?} at byte {begin} has CRC32 {actual:08x}, but its trailer declares \
                 {expected:08x}; the article is corrupt, so fetch another copy"
            ),
            Self::Truncated { name, begin, decoded } => write!(
                f,
                "yEnc data for {name:?} at byte {begin} ends after {decoded} bytes without its =yend line; \
                 the article is cut short, so fetch another copy"
            ),
        }
    }
}

impl std::error::Error for YencError {}

fn line_at(buf: &[u8], from: usize) -> (&[u8], usize) {
    let rest = buf.get(from..).unwrap_or_default();
    let len = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
    let line = &rest[..len];
    (line.strip_suffix(b"\r").unwrap_or(line), from + len + 1)
}

/// `key=value` pairs; `name=` runs to the end of the line.
fn fields(line: &str) -> Vec<(&str, &str)> {
    let (head, name) = match line.split_once(" name=") {
        Some((head, name)) => (head, Some(name)),
        None => (line, None),
    };
    let mut out: Vec<(&str, &str)> = head.split_whitespace().filter_map(|w| w.split_once('=')).collect();
    out.extend(name.map(|n| ("name", n)));
    out
}

fn get<'a>(fields: &[(&str, &'a str)], key: &str) -> Option<&'a str> {
    fields.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

fn int(fields: &[(&str, &str)], field: &'static str, line: ControlLine, text: &str) -> Result<u64, YencError> {
    let Some(value) = get(fields, field) else {
        return Err(YencError::MissingField { line, field, text: text.to_owned() });
    };
    value.parse().map_err(|_| YencError::InvalidField { line, field, value: value.to_owned() })
}

/// Decodes one article body and checks its length and, when the poster sent one, its CRC32.
///
/// `body` is the article body with NNTP dot-stuffing already removed. Lines before
/// `=ybegin` are skipped. A wrong `end` on a multipart article's `=ypart` line is
/// tolerated as long as the decoded data stays inside the file, since some posters write
/// one on the last part.
///
/// ```
/// let body = b"=ybegin line=128 size=5 name=hello.txt\r\n\x92\x8f\x96\x96\x99\r\n=yend size=5 crc32=3610a686\r\n";
/// let part = usenet::yenc::decode(body)?;
/// assert_eq!((part.name.as_str(), part.begin, part.data.as_slice()), ("hello.txt", 0, &b"hello"[..]));
///
/// // Cut before its trailer, the same article is reported as truncated.
/// let cut = &body[..body.len() - 30];
/// assert!(matches!(usenet::yenc::decode(cut), Err(usenet::yenc::YencError::Truncated { .. })));
/// # Ok::<(), usenet::yenc::YencError>(())
/// ```
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
    let file_size = int(&head, "size", ControlLine::Begin, &header)?;
    let name = get(&head, "name").unwrap_or_default().to_string();
    let multipart = get(&head, "part").is_some();
    let (begin, end) = if multipart {
        let (line, next) = line_at(body, pos);
        pos = next;
        let line = String::from_utf8_lossy(line);
        if !line.starts_with("=ypart ") {
            return Err(YencError::MissingPartLine { name });
        }
        let part = fields(&line);
        let begin = int(&part, "begin", ControlLine::Part, &line)?;
        let Some(begin) = begin.checked_sub(1) else {
            return Err(YencError::PartBeginsAtZero { name });
        };
        (begin, int(&part, "end", ControlLine::Part, &line)?)
    } else {
        (0, file_size)
    };
    if begin >= file_size.max(1) {
        return Err(YencError::BeginPastEnd { name, begin, file_size });
    }
    // Some posters write a wrong `end` on the last part. Like other decoders, trust `begin`
    // and the decoded length then, as long as the part stays inside the file.
    let declared = end.checked_sub(begin).filter(|_| end <= file_size);
    let Decoded { data, trailer } = data::decode(data::Kernel::best(), body, pos);
    let actual = data.len() as u64;
    let room = file_size - begin;
    match declared {
        Some(expected) if actual != expected => return Err(YencError::Length { name, expected, actual }),
        None if actual > room => return Err(YencError::Length { name, expected: room, actual }),
        _ => {}
    }
    let Some(trailer) = trailer else {
        return Err(YencError::Truncated { name, begin, decoded: actual });
    };
    let trailer = String::from_utf8_lossy(line_at(body, trailer).0);
    let tail = fields(&trailer);
    // A multipart `crc32` covers the whole file, so only `pcrc32` checks this part. A single
    // part is the whole file.
    let crc = if multipart { get(&tail, "pcrc32") } else { get(&tail, "crc32").or(get(&tail, "pcrc32")) };
    if let Some(expected) = crc.and_then(|c| u32::from_str_radix(c, 16).ok()) {
        let actual = crc32fast::hash(&data);
        if expected != actual {
            return Err(YencError::Crc { name, begin, expected, actual });
        }
    }
    Ok(Part { name, file_size, begin, data })
}

/// The data of an article and where its trailer starts.
struct Decoded {
    data: Vec<u8>,
    /// Index of the `=` that opens the `=y` trailer line, if the body has one.
    trailer: Option<usize>,
}

/// The data loop: everything between the header lines and the `=y` trailer.
///
/// The scalar loop defines the semantics: line endings are dropped, `=` escapes the next
/// byte whatever it is (a lone `=` at the very end decodes as if a 0 followed), and `=y`
/// ends the data only at the start of a line. A line starts after an unescaped `\n`, and
/// unescaped `\r`s don't end it.
///
/// The SIMD kernels classify 64 bytes at a time into bit masks of `=`, `\r`, `\n` and `y`,
/// then decode and compact the chunk with plain integer code. A chunk that may hold a
/// trailer or an escaped `=` goes to the scalar loop instead; neither happens in the data
/// of a well-formed article, so that costs one scalar chunk per article.
mod data {
    use super::Decoded;

    const CHUNK: usize = 64;

    /// Above this many skipped bytes in a chunk, the scalar loop is faster.
    const MAX_SIMD_SKIPS: u32 = 24;

    /// The decoder's state between bytes.
    #[derive(Clone, Copy)]
    struct State {
        /// Only `\r`s have been read since the last unescaped `\n`, or since the data began.
        line_start: bool,
        /// The last byte read was an `=` whose escaped byte comes next.
        escape: bool,
    }

    /// One way to run the data loop.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(super) enum Kernel {
        Scalar,
        #[cfg(target_arch = "x86_64")]
        Sse2,
        #[cfg(target_arch = "x86_64")]
        Avx2,
        #[cfg(target_arch = "aarch64")]
        Neon,
    }

    impl Kernel {
        /// The fastest kernel this CPU runs.
        pub(super) fn best() -> Self {
            #[cfg(target_arch = "x86_64")]
            if std::arch::is_x86_feature_detected!("avx2") {
                return Self::Avx2;
            } else {
                return Self::Sse2;
            }
            #[cfg(target_arch = "aarch64")]
            return Self::Neon;
            #[allow(unreachable_code)]
            Self::Scalar
        }

        /// Every kernel this CPU runs, for tests.
        #[cfg(test)]
        pub(super) fn available() -> Vec<Self> {
            let mut out = vec![Self::Scalar];
            #[cfg(target_arch = "x86_64")]
            {
                out.push(Self::Sse2);
                if std::arch::is_x86_feature_detected!("avx2") {
                    out.push(Self::Avx2);
                }
            }
            #[cfg(target_arch = "aarch64")]
            out.push(Self::Neon);
            out
        }
    }

    /// Decodes `body[start..]` up to the trailer.
    pub(super) fn decode(kernel: Kernel, body: &[u8], start: usize) -> Decoded {
        match kernel {
            Kernel::Scalar => run::<Scalar>(body, start),
            #[cfg(target_arch = "x86_64")]
            Kernel::Sse2 => run::<x86::Sse2>(body, start),
            // SAFETY: `Kernel::Avx2` is only chosen after detecting AVX2.
            #[cfg(target_arch = "x86_64")]
            Kernel::Avx2 => unsafe { x86::run_avx2(body, start) },
            #[cfg(target_arch = "aarch64")]
            Kernel::Neon => run::<neon::Neon>(body, start),
        }
    }

    /// Bit `k` of each mask is set when byte `k` of a chunk is that character.
    #[derive(Clone, Copy, Default)]
    pub(super) struct Masks {
        pub(super) eq: u64,
        pub(super) cr: u64,
        pub(super) lf: u64,
        pub(super) y: u64,
    }

    /// Classifies a chunk and writes each byte minus 42 to `out[..64]`.
    pub(super) trait Classify {
        /// False for the scalar kernel, which skips the chunk loop.
        const SIMD: bool;
        fn classify(chunk: &[u8; CHUNK], out: &mut [u8; 2 * CHUNK]) -> Masks;
    }

    struct Scalar;

    impl Classify for Scalar {
        const SIMD: bool = false;
        fn classify(_: &[u8; CHUNK], _: &mut [u8; 2 * CHUNK]) -> Masks {
            Masks::default()
        }
    }

    #[inline(always)]
    pub(super) fn run<C: Classify>(body: &[u8], start: usize) -> Decoded {
        // The data never decodes to more bytes than it has, and a chunk appends `CHUNK` bytes
        // before it cuts back to those it keeps, so this never reallocates.
        let mut out = Vec::with_capacity(body.len().saturating_sub(start) + CHUNK);
        let mut state = State { line_start: true, escape: false };
        let mut i = start;
        let mut scratch = [0u8; 2 * CHUNK];
        if C::SIMD {
            // A chunk also peeks at the byte after it, so it needs `CHUNK + 1` bytes.
            while let Some(window) = body.get(i..i + CHUNK + 1) {
                let (chunk, next) = window.split_at(CHUNK);
                let Ok(chunk) = <&[u8; CHUNK]>::try_from(chunk) else { break };
                let masks = C::classify(chunk, &mut scratch);
                if chunk_step(masks, next.first().copied(), &mut state, &mut scratch, &mut out) {
                    i += CHUNK;
                    continue;
                }
                let (next, trailer) = scalar(body, i, i + CHUNK, &mut state, &mut out);
                if trailer.is_some() {
                    return Decoded { data: out, trailer };
                }
                i = next;
            }
        }
        let (_, trailer) = scalar(body, i, body.len(), &mut state, &mut out);
        Decoded { data: out, trailer }
    }

    /// The reference loop. Reads `body[i..stop]`, finishing an escape that straddles `stop`,
    /// and returns where it stopped and where the trailer starts if it reached one.
    fn scalar(body: &[u8], mut i: usize, stop: usize, state: &mut State, out: &mut Vec<u8>) -> (usize, Option<usize>) {
        if state.escape {
            out.push(body.get(i).copied().unwrap_or(0).wrapping_sub(106));
            state.escape = false;
            state.line_start = false;
            i += 1;
        }
        while i < stop {
            let Some(&b) = body.get(i) else { break };
            match b {
                b'\n' => {
                    state.line_start = true;
                    i += 1;
                    continue;
                }
                b'\r' => {
                    i += 1;
                    continue;
                }
                b'=' if state.line_start && body.get(i + 1) == Some(&b'y') => return (i, Some(i)),
                b'=' => {
                    i += 1;
                    out.push(body.get(i).copied().unwrap_or(0).wrapping_sub(106));
                }
                _ => out.push(b.wrapping_sub(42)),
            }
            state.line_start = false;
            i += 1;
        }
        (i, None)
    }

    /// Decodes one classified chunk into `out`, or returns false, changing nothing, when the
    /// scalar loop must read it: when it may hold a trailer, or an `=` escapes another `=`.
    /// `next` is the byte after the chunk.
    #[inline(always)]
    fn chunk_step(
        m: Masks,
        next: Option<u8>,
        state: &mut State,
        minus42: &mut [u8; 2 * CHUNK],
        out: &mut Vec<u8>,
    ) -> bool {
        let pending = u64::from(state.escape);
        if m.eq & ((m.eq << 1) | pending) != 0 {
            return false;
        }
        // An `=` right after a line break (or at a chunk that starts a line) followed by `y`.
        let after_break = ((m.cr | m.lf) << 1) | u64::from(state.line_start);
        let before_y = (m.y >> 1) | (u64::from(next == Some(b'y')) << 63);
        if m.eq & after_break & before_y != 0 {
            return false;
        }
        // No `=` escapes another, so every `=` is an escape and escapes the byte after it.
        let escaped = (m.eq << 1) | pending;
        let cr = m.cr & !escaped;
        let lf = m.lf & !escaped;
        let skip = m.eq | cr | lf;
        // Each skipped byte costs a copy below, so a chunk of mostly escapes, which real
        // articles never are, is quicker byte by byte.
        if skip.count_ones() > MAX_SIMD_SKIPS {
            return false;
        }
        let mut e = escaped;
        while e != 0 {
            let at = e.trailing_zeros() as usize;
            minus42[at] = minus42[at].wrapping_sub(64);
            e &= e - 1;
        }
        // Copy the runs between skipped bytes. A fixed-size copy is much faster than one of the
        // run's length, so each appends a whole chunk's worth and then cuts back to the run.
        let mut from = 0;
        let mut rest = skip;
        loop {
            let to = if rest == 0 { CHUNK } else { rest.trailing_zeros() as usize };
            let kept = out.len() + (to - from);
            out.extend_from_slice(&minus42[from..from + CHUNK]);
            out.truncate(kept);
            if rest == 0 {
                break;
            }
            from = to + 1;
            rest &= rest - 1;
        }
        state.escape = m.eq >> 63 != 0;
        // The last byte that is not a dropped `\r` decides whether the next chunk starts a line.
        let kept = !cr;
        if kept != 0 {
            let last = 63 - kept.leading_zeros();
            state.line_start = (lf >> last) & 1 != 0;
        }
        true
    }

    #[cfg(target_arch = "x86_64")]
    mod x86 {
        use std::arch::x86_64::{
            __m128i, __m256i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8, _mm_storeu_si128,
            _mm_sub_epi8, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi8,
            _mm256_storeu_si256, _mm256_sub_epi8,
        };

        use super::{CHUNK, Classify, Decoded, Masks, run};

        pub(in super::super) struct Sse2;

        impl Classify for Sse2 {
            const SIMD: bool = true;
            #[inline(always)]
            fn classify(chunk: &[u8; CHUNK], out: &mut [u8; 2 * CHUNK]) -> Masks {
                let mut m = Masks::default();
                for k in 0..CHUNK / 16 {
                    // SAFETY: `16 * k + 16 <= CHUNK`, so both 16-byte accesses are in bounds, and
                    // SSE2 is part of every x86_64 CPU.
                    unsafe {
                        let v = _mm_loadu_si128(chunk.as_ptr().add(16 * k).cast::<__m128i>());
                        let mask = |c: u8| {
                            let bits = _mm_movemask_epi8(_mm_cmpeq_epi8(v, _mm_set1_epi8(c as i8)));
                            u64::from(bits as u16) << (16 * k)
                        };
                        m.eq |= mask(b'=');
                        m.cr |= mask(b'\r');
                        m.lf |= mask(b'\n');
                        m.y |= mask(b'y');
                        let minus42 = _mm_sub_epi8(v, _mm_set1_epi8(42));
                        _mm_storeu_si128(out.as_mut_ptr().add(16 * k).cast::<__m128i>(), minus42);
                    }
                }
                m
            }
        }

        struct Avx2;

        impl Classify for Avx2 {
            const SIMD: bool = true;
            #[inline(always)]
            fn classify(chunk: &[u8; CHUNK], out: &mut [u8; 2 * CHUNK]) -> Masks {
                let mut m = Masks::default();
                for k in 0..CHUNK / 32 {
                    // SAFETY: `32 * k + 32 <= CHUNK`, so both 32-byte accesses are in bounds, and
                    // this is only reached through `run_avx2`, whose caller checked for AVX2.
                    unsafe {
                        let v = _mm256_loadu_si256(chunk.as_ptr().add(32 * k).cast::<__m256i>());
                        let mask = |c: u8| {
                            let bits = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c as i8)));
                            u64::from(bits as u32) << (32 * k)
                        };
                        m.eq |= mask(b'=');
                        m.cr |= mask(b'\r');
                        m.lf |= mask(b'\n');
                        m.y |= mask(b'y');
                        let minus42 = _mm256_sub_epi8(v, _mm256_set1_epi8(42));
                        _mm256_storeu_si256(out.as_mut_ptr().add(32 * k).cast::<__m256i>(), minus42);
                    }
                }
                m
            }
        }

        /// # Safety
        ///
        /// The CPU must support AVX2.
        #[target_feature(enable = "avx2")]
        pub(in super::super) unsafe fn run_avx2(body: &[u8], start: usize) -> Decoded {
            run::<Avx2>(body, start)
        }
    }

    #[cfg(target_arch = "aarch64")]
    mod neon {
        use std::arch::aarch64::{
            uint8x16_t, vandq_u8, vceqq_u8, vdupq_n_u8, vgetq_lane_u64, vld1q_u8, vpaddq_u8, vreinterpretq_u64_u8,
            vst1q_u8, vsubq_u8,
        };

        use super::{CHUNK, Classify, Masks};

        pub(in super::super) struct Neon;

        /// Bit `k` of the result is the top bit of byte `k` of the four vectors in turn.
        #[inline(always)]
        fn movemask(v: [uint8x16_t; 4]) -> u64 {
            const BITS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
            // SAFETY: `BITS` is 16 bytes, and NEON is part of every aarch64 CPU.
            unsafe {
                let bits = vld1q_u8(BITS.as_ptr());
                let [a, b, c, d] = v.map(|v| vandq_u8(v, bits));
                let sum = vpaddq_u8(vpaddq_u8(a, b), vpaddq_u8(c, d));
                let sum = vpaddq_u8(sum, sum);
                vgetq_lane_u64::<0>(vreinterpretq_u64_u8(sum))
            }
        }

        impl Classify for Neon {
            const SIMD: bool = true;
            #[inline(always)]
            fn classify(chunk: &[u8; CHUNK], out: &mut [u8; 2 * CHUNK]) -> Masks {
                // SAFETY: each load reads and each store writes 16 bytes at `16 * k` for `k < 4`,
                // inside the chunk and `out`, and NEON is part of every aarch64 CPU.
                unsafe {
                    let v: [uint8x16_t; 4] = std::array::from_fn(|k| vld1q_u8(chunk.as_ptr().add(16 * k)));
                    let mask = |c: u8| movemask(v.map(|v| vceqq_u8(v, vdupq_n_u8(c))));
                    let m = Masks { eq: mask(b'='), cr: mask(b'\r'), lf: mask(b'\n'), y: mask(b'y') };
                    for (k, v) in v.into_iter().enumerate() {
                        vst1q_u8(out.as_mut_ptr().add(16 * k), vsubq_u8(v, vdupq_n_u8(42)));
                    }
                    m
                }
            }
        }
    }
}

/// Encodes one article, for tests.
#[cfg(any(test, feature = "fake"))]
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
        let actual = crc32fast::hash(b"some bytes");
        assert_eq!(decode(&body), Err(YencError::Crc { name: "x".into(), begin: 0, expected: 1, actual }));
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
    fn a_body_cut_before_its_trailer_is_truncated() {
        let body = encode(&[7u8; 300], "x", 300, 0, Some(1));
        let at = body.windows(6).position(|w| w == b"=yend ").unwrap();
        assert_eq!(decode(&body[..at]), Err(YencError::Truncated { name: "x".into(), begin: 0, decoded: 300 }));
        // A wrong `end` on a last part is tolerated, so the trailer is what shows it is whole.
        let mut last = encode(&[7u8; 300], "x", 1300, 1000, Some(2));
        let at = last.windows(8).position(|w| w == b"end=1300").unwrap();
        last.splice(at..at + 8, *b"end=0500");
        let cut = last.windows(6).position(|w| w == b"=yend ").unwrap() - 100;
        assert!(matches!(decode(&last[..cut]), Err(YencError::Truncated { .. })));
    }

    #[test]
    fn a_single_part_checks_the_file_crc() {
        let mut body = encode(b"hello", "x", 5, 0, None);
        let crc = format!("{:08x}", crc32fast::hash(b"hello"));
        let at = body.windows(7).position(|w| w == b"pcrc32=").unwrap();
        body.splice(at.., format!("pcrc32=00000001 crc32={crc}\r\n").bytes());
        assert_eq!(decode(&body).unwrap().data, b"hello");
    }

    #[test]
    fn a_short_body_is_reported() {
        let body = b"=ybegin part=1 line=128 size=10 name=x\r\n=ypart begin=1 end=10\r\nab\r\n=yend size=10\r\n";
        assert!(matches!(decode(body), Err(YencError::Length { expected: 10, actual: 2, .. })));
    }

    /// A small deterministic generator, so the tests need no dependency and fail reproducibly.
    pub(super) struct Rng(u64);

    impl Rng {
        pub(super) fn new(seed: u64) -> Self {
            Self(seed)
        }

        pub(super) fn next(&mut self) -> u64 {
            // splitmix64
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        pub(super) fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }

        /// A byte that is often one the decoder treats specially.
        fn byte(&mut self) -> u8 {
            match self.below(20) {
                0..=1 => b'=',
                2 => b'\r',
                3 => b'\n',
                4 => b'y',
                _ => self.next() as u8,
            }
        }
    }

    /// Every kernel decodes `body[start..]` exactly as the scalar loop does.
    fn assert_kernels_agree(body: &[u8], start: usize) {
        let reference = data::decode(data::Kernel::Scalar, body, start);
        for kernel in data::Kernel::available() {
            let got = data::decode(kernel, body, start);
            assert!(
                got.data == reference.data && got.trailer == reference.trailer,
                "{kernel:?} differs from scalar from {start} in {body:?}"
            );
        }
    }

    #[test]
    fn kernels_agree_on_random_input() {
        let mut rng = Rng::new(1);
        for _ in 0..3000 {
            let len = rng.below(700);
            let body: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
            assert_kernels_agree(&body, rng.below(len + 2));
        }
    }

    #[test]
    fn kernels_agree_on_every_escape_and_line_break_position() {
        const PATTERNS: &[&[u8]] = &[
            b"=",
            b"==",
            b"===",
            b"=\r",
            b"=\n",
            b"=\r\n",
            b"=y",
            b"\n=y",
            b"\r=y",
            b"\r\n=y",
            b"\n\r\r=y",
            b"=\n=y",
            b"=\r\n=y",
            b"\n==y",
            b"\n=\ny",
            b"\r\n",
            b"\n",
            b"\r",
            b"\n=",
            b"\n=\n",
        ];
        let mut rng = Rng::new(2);
        let base: Vec<u8> = (0..260).map(|_| (rng.next() as u8) | 0x80).collect();
        for pattern in PATTERNS {
            for at in 0..200 {
                let mut body = base.clone();
                body.splice(at..at, pattern.iter().copied());
                for start in 0..3 {
                    assert_kernels_agree(&body, start);
                    // The same pattern at the very end of the data.
                    assert_kernels_agree(&body[..at + pattern.len()], start);
                }
            }
        }
    }

    #[test]
    fn kernels_agree_on_encoded_articles() {
        let mut rng = Rng::new(3);
        for _ in 0..50 {
            let data: Vec<u8> = (0..rng.below(20_000)).map(|_| rng.next() as u8).collect();
            let body = encode(&data, "x", data.len() as u64, 0, None);
            assert_kernels_agree(&body, 0);
            assert_eq!(decode(&body).map(|p| p.data), Ok(data));
        }
    }

    #[test]
    fn header_problems_name_the_line_and_field() {
        let decode_str = |s: &str| decode(s.as_bytes());
        assert_eq!(decode_str("hello\r\n"), Err(YencError::NoHeader));
        assert_eq!(
            decode_str("=ybegin line=128 name=x\r\n"),
            Err(YencError::MissingField {
                line: ControlLine::Begin,
                field: "size",
                text: "=ybegin line=128 name=x".into()
            })
        );
        assert_eq!(
            decode_str("=ybegin size=1k name=x\r\n"),
            Err(YencError::InvalidField { line: ControlLine::Begin, field: "size", value: "1k".into() })
        );
        assert_eq!(
            decode_str("=ybegin part=1 size=10 name=x\r\nabc\r\n"),
            Err(YencError::MissingPartLine { name: "x".into() })
        );
        assert_eq!(
            decode_str("=ybegin part=1 size=10 name=x\r\n=ypart begin=0 end=10\r\n"),
            Err(YencError::PartBeginsAtZero { name: "x".into() })
        );
        assert_eq!(
            decode_str("=ybegin part=1 size=10 name=x\r\n=ypart begin=1\r\n"),
            Err(YencError::MissingField { line: ControlLine::Part, field: "end", text: "=ypart begin=1".into() })
        );
        assert_eq!(
            decode_str("=ybegin part=2 size=10 name=x\r\n=ypart begin=11 end=12\r\n"),
            Err(YencError::BeginPastEnd { name: "x".into(), begin: 10, file_size: 10 })
        );
    }

    #[test]
    fn extreme_header_numbers_do_not_overflow() {
        let max = u64::MAX;
        let body = format!("=ybegin part=1 size={max} name=x\r\n=ypart begin={max} end={max}\r\nab\r\n=yend\r\n");
        assert!(matches!(decode(body.as_bytes()), Err(YencError::Length { .. })));
        let body = format!("=ybegin part=1 size={max} name=x\r\n=ypart begin=3 end=1\r\nab\r\n=yend\r\n");
        assert_eq!(decode(body.as_bytes()).map(|p| p.data), Ok(vec![55, 56]));
        assert_eq!(decode(b"=ybegin size=0 name=x\r\n=yend\r\n").map(|p| p.data), Ok(vec![]));
    }

    #[test]
    fn a_lone_escape_at_the_end_decodes_as_an_escaped_zero() {
        let reference = data::decode(data::Kernel::Scalar, b"ab=", 0);
        assert_eq!((reference.data, reference.trailer), (vec![55, 56, 150], None));
    }

    fn sample(name: &str) -> Vec<u8> {
        let path = format!("{}/tests/data/yenc/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    /// Articles written by `tests/data/yenc/generate.py`, an encoder independent of this crate.
    const SAMPLES: &[(&str, &str, u64, u64, usize, u32)] = &[
        ("multipart-first.yenc", "sample.part01.rar", 1_900_000, 0, 768_000, 0x7775_c6b2),
        ("multipart-last.yenc", "sample.part01.rar", 1_900_000, 1_536_000, 364_000, 0x6f32_2e54),
        ("wrong-end.yenc", "sample.part02.rar", 50_000, 40_000, 10_000, 0xded2_6014),
        ("single-part.yenc", "sample.vol00+01.par2", 40_432, 0, 40_432, 0xc1a3_da1a),
        ("single.yenc", "sample.nfo", 6_000, 0, 6_000, 0xf47f_128c),
        ("escapes.yenc", "sample.part03.rar", 40_000, 10_000, 30_000, 0x1404_f137),
    ];

    #[test]
    fn decodes_the_sample_articles() {
        for &(file, name, file_size, begin, len, crc) in SAMPLES {
            let body = sample(file);
            let part = decode(&body).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(
                (part.name.as_str(), part.file_size, part.begin, part.data.len()),
                (name, file_size, begin, len)
            );
            assert_eq!(crc32fast::hash(&part.data), crc, "{file}");
            assert_kernels_agree(&body, 0);

            let trailer = body.windows(6).rposition(|w| w == b"=yend ").unwrap();
            assert!(matches!(decode(&body[..trailer]), Err(YencError::Truncated { .. })), "{file}");
            let mut damaged = body.clone();
            let at = trailer - 10;
            damaged[at] = if damaged[at] == b'a' { b'b' } else { b'a' };
            assert!(matches!(decode(&damaged), Err(YencError::Crc { .. } | YencError::Length { .. })), "{file}");
        }
    }

    /// A bounded, deterministic stand-in for the fuzz target, so CI catches regressions on stable.
    #[test]
    fn mutated_articles_decode_without_panics_and_kernels_agree() {
        const INSERTS: &[&[u8]] = &[b"=", b"\r\n", b"\n", b"\r", b"=y", b"\r\n=yend size=1\r\n", b"==", b"y", b"\0"];
        let bodies: Vec<Vec<u8>> = SAMPLES
            .iter()
            .map(|(file, ..)| {
                let mut body = sample(file);
                // Keep the big samples small: their header and some data, then their trailer.
                if body.len() > 50_000 {
                    let trailer = body.windows(6).rposition(|w| w == b"=yend ").unwrap();
                    body.drain(20_000..trailer);
                }
                body
            })
            .collect();
        let mut rng = Rng::new(4);
        for _ in 0..2000 {
            let mut body = bodies[rng.below(bodies.len())].clone();
            for _ in 0..1 + rng.below(8) {
                let at = rng.below(body.len() + 1);
                match rng.below(5) {
                    0 => {
                        if let Some(b) = body.get_mut(at) {
                            *b ^= 1 << rng.below(8);
                        }
                    }
                    1 => drop(body.splice(at..at, INSERTS[rng.below(INSERTS.len())].iter().copied())),
                    2 => drop(body.drain(at..(at + rng.below(300)).min(body.len()))),
                    3 => body.truncate(at),
                    _ => {
                        let end = (at + rng.below(300)).min(body.len());
                        let copy = body[at..end].to_vec();
                        let to = rng.below(body.len() + 1);
                        body.splice(to..to, copy);
                    }
                }
            }
            if let Ok(part) = decode(&body) {
                assert!(part.end() <= part.file_size);
            }
            let start = rng.below(body.len().min(200) + 1);
            assert_kernels_agree(&body, start);
        }
    }
}
