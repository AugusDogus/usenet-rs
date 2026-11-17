//! Reading NNTP replies from bytes as they arrive, without a socket: status lines, and the
//! dot-stuffed multiline body that follows `222`. Pipelined replies arrive back to back, so
//! the parser keeps what it has not used for the next reply.

use std::fmt;

use memchr::memmem;

/// Longest status line accepted, CRLF included. RFC 3977 allows 512 octets.
pub(crate) const MAX_LINE: usize = 2048;
/// Largest body accepted. Usenet servers cap articles at a few megabytes.
pub(crate) const MAX_BODY: usize = 16 * 1024 * 1024;
/// Bytes read from the socket at a time.
pub(crate) const READ_CHUNK: usize = 256 * 1024;
/// Room reserved for a body up front, about one typical article.
const BODY_CAPACITY: usize = 800 * 1024;

/// A way a server broke the protocol, which leaves the connection unusable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Violation {
    /// A status line ran past `limit` bytes without ending.
    LineTooLong {
        /// The longest line accepted, in bytes.
        limit: usize,
    },
    /// An article body ran past `limit` bytes without its terminating dot.
    BodyTooLarge {
        /// The largest body accepted, in bytes.
        limit: usize,
    },
    /// The server sent bytes while no command was waiting for a reply.
    Unsolicited,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LineTooLong { limit } => write!(f, "a reply line ran past {limit} bytes without ending"),
            Self::BodyTooLarge { limit } => write!(f, "an article ran past {limit} bytes without ending"),
            Self::Unsolicited => f.write_str("the server sent data no command asked for"),
        }
    }
}

/// What the command a reply answers can be answered with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Expect {
    /// One status line.
    Line,
    /// One status line, followed by a multiline body when its code is 222.
    Body,
}

/// A status line: its code, when it starts with one, and its text for messages.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Status {
    pub code: Option<u16>,
    pub text: String,
}

impl Status {
    fn parse(line: &[u8]) -> Self {
        let text = String::from_utf8_lossy(line).trim_end().to_string();
        let code = match line {
            [a, b, c, rest @ ..]
                if [a, b, c].iter().all(|d| d.is_ascii_digit())
                    && rest.first().is_none_or(|&s| matches!(s, b' ' | b'\r' | b'\n')) =>
            {
                Some(u16::from(a - b'0') * 100 + u16::from(b - b'0') * 10 + u16::from(c - b'0'))
            }
            _ => None,
        };
        Self { code, text }
    }
}

/// One whole reply.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Reply {
    /// A single-line reply, including any status a body was expected with but did not get.
    Line(Status),
    /// The body after a 222 status, with dot-stuffing removed and lines kept with their CRLF.
    Body(Vec<u8>),
}

/// The bytes received and not yet used, and the body being read when a 222 has arrived.
#[derive(Debug)]
pub(crate) struct Replies {
    buffer: Vec<u8>,
    /// Where the unused bytes in `buffer` start.
    start: usize,
    body: Option<Body>,
    max_body: usize,
}

#[derive(Debug)]
struct Body {
    data: Vec<u8>,
    /// The next byte starts a line, so a dot there is stuffing or the terminator.
    line_start: bool,
}

impl Replies {
    pub fn new(max_body: usize) -> Self {
        Self { buffer: Vec::new(), start: 0, body: None, max_body }
    }

    /// The buffer to read more bytes into: unused bytes moved to the front, with at least
    /// [`READ_CHUNK`] bytes of room after them.
    pub fn buffer(&mut self) -> &mut Vec<u8> {
        if self.start > 0 {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        self.buffer.reserve(READ_CHUNK);
        &mut self.buffer
    }

    #[cfg(any(test, fuzzing))]
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buffer().extend_from_slice(bytes);
    }

    /// True when no byte of a reply is waiting: nothing buffered, and no body half read.
    pub fn is_idle(&self) -> bool {
        self.start == self.buffer.len() && self.body.is_none()
    }

    /// Bytes held for replies not yet whole.
    #[cfg(any(test, fuzzing))]
    pub fn buffered(&self) -> usize {
        self.buffer.len() - self.start + self.body.as_ref().map_or(0, |b| b.data.len())
    }

    /// The next whole reply, or None until more bytes arrive. `expect` must stay the same
    /// until the reply it is for comes back.
    pub fn next(&mut self, expect: Expect) -> Result<Option<Reply>, Violation> {
        if self.body.is_none() {
            let rest = &self.buffer[self.start..];
            let Some(end) = memchr::memchr(b'\n', rest) else {
                return if rest.len() >= MAX_LINE { Err(Violation::LineTooLong { limit: MAX_LINE }) } else { Ok(None) };
            };
            if end >= MAX_LINE {
                return Err(Violation::LineTooLong { limit: MAX_LINE });
            }
            let status = Status::parse(&rest[..=end]);
            self.start += end + 1;
            if expect == Expect::Line || status.code != Some(222) {
                return Ok(Some(Reply::Line(status)));
            }
            let capacity = BODY_CAPACITY.min(self.max_body);
            self.body = Some(Body { data: Vec::with_capacity(capacity), line_start: true });
        }
        self.read_body()
    }

    fn read_body(&mut self) -> Result<Option<Reply>, Violation> {
        let Some(body) = self.body.as_mut() else { return Ok(None) };
        loop {
            let rest = &self.buffer[self.start..];
            if body.line_start {
                match rest {
                    [] | [b'.'] | [b'.', b'\r'] => return Ok(None),
                    [b'.', b'\n', ..] | [b'.', b'\r', b'\n', ..] => {
                        self.start += if rest[1] == b'\n' { 2 } else { 3 };
                        let done = self.body.take().map(|b| b.data).unwrap_or_default();
                        return Ok(Some(Reply::Body(done)));
                    }
                    // A stuffed dot: the line's own text starts after it.
                    [b'.', ..] => self.start += 1,
                    _ => {}
                }
                body.line_start = false;
                continue;
            }
            // Copy up to the next line that starts with a dot; other lines go across whole.
            let (take, line_start) = match memmem::find(rest, b"\n.") {
                Some(at) => (at + 1, true),
                None => (rest.len(), rest.last() == Some(&b'\n')),
            };
            if body.data.len() + take > self.max_body {
                return Err(Violation::BodyTooLarge { limit: self.max_body });
            }
            body.data.extend_from_slice(&rest[..take]);
            self.start += take;
            body.line_start = line_start;
            if self.start == self.buffer.len() {
                return Ok(None);
            }
        }
    }
}

/// Drives the parser from bytes alone, for the fuzz target. Not part of the API.
#[cfg(fuzzing)]
pub mod fuzzing {
    pub use super::Violation;
    use super::{Expect, MAX_LINE, READ_CHUNK, Replies, Reply};

    /// A reply as the parser read it.
    #[derive(Clone, PartialEq, Eq, Debug)]
    pub enum Parsed {
        /// A status line.
        Line {
            /// Its code, when it starts with one.
            code: Option<u16>,
            /// Its text.
            text: String,
        },
        /// A body after 222.
        Body(Vec<u8>),
        /// The parser gave up.
        Broken(Violation),
    }

    /// Feeds `input` in pieces of `chunks` sizes, cycling, with each reply in turn read as the
    /// answer to BODY when `bodies` says so, cycling, and to a one-line command otherwise.
    /// Returns every whole reply in order, stopping at the first violation.
    ///
    /// # Panics
    ///
    /// When the parser holds more bytes than its caps allow.
    pub fn parse(input: &[u8], chunks: &[usize], bodies: &[bool], max_body: usize) -> Vec<Parsed> {
        let mut parser = Replies::new(max_body);
        let mut out = Vec::new();
        let (mut sizes, mut kinds) = (chunks.iter().cycle(), bodies.iter().cycle());
        let mut expect = || if kinds.next().copied().unwrap_or(false) { Expect::Body } else { Expect::Line };
        let mut next = expect();
        let mut rest = input;
        while !rest.is_empty() {
            let size = sizes.next().copied().unwrap_or(rest.len()).clamp(1, READ_CHUNK);
            let (piece, after) = rest.split_at(size.min(rest.len()));
            rest = after;
            parser.feed(piece);
            loop {
                match parser.next(next) {
                    Ok(Some(Reply::Line(status))) => out.push(Parsed::Line { code: status.code, text: status.text }),
                    Ok(Some(Reply::Body(body))) => out.push(Parsed::Body(body)),
                    Ok(None) => break,
                    Err(violation) => {
                        out.push(Parsed::Broken(violation));
                        return out;
                    }
                }
                next = expect();
            }
            assert!(parser.buffered() <= max_body + MAX_LINE + READ_CHUNK, "the parser holds {}", parser.buffered());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(text: &str) -> Reply {
        Reply::Line(Status::parse(text.as_bytes()))
    }

    /// Every reply in `input`, fed `chunk` bytes at a time, until it runs out or breaks.
    fn replies(input: &[u8], expect: &[Expect], chunk: usize, max_body: usize) -> Vec<Result<Reply, Violation>> {
        let mut parser = Replies::new(max_body);
        let mut out = Vec::new();
        let mut expect = expect.iter().copied();
        let Some(mut next) = expect.next() else { return out };
        for piece in input.chunks(chunk.max(1)) {
            parser.feed(piece);
            loop {
                match parser.next(next) {
                    Ok(Some(reply)) => {
                        out.push(Ok(reply));
                        match expect.next() {
                            Some(e) => next = e,
                            None => return out,
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        out.push(Err(e));
                        return out;
                    }
                }
            }
            assert!(parser.buffered() <= max_body + MAX_LINE + chunk, "{}", parser.buffered());
        }
        out
    }

    #[test]
    fn status_codes_need_three_digits_and_then_a_space_or_the_end() {
        let codes: Vec<Option<u16>> =
            ["222 0 <a@b>\r\n", "430\r\n", "4301 x\r\n", "2a2 x\r\n", "22\r\n", "", "22\u{20ac}x\r\n", "999 what"]
                .iter()
                .map(|l| Status::parse(l.as_bytes()).code)
                .collect();
        assert_eq!(codes, [Some(222), Some(430), None, None, None, None, None, Some(999)]);
    }

    #[test]
    fn a_body_is_unstuffed_and_ends_at_a_lone_dot_in_any_chunking() {
        let input = b"222 0 <a@b>\r\n..dot\r\nplain\r\n.\r\n.\r\n223 0 <c@d>\r\n222 1\n.x\n.\n";
        let expected = vec![
            Ok(Reply::Body(b".dot\r\nplain\r\n".to_vec())),
            Ok(status(".")),
            Ok(status("223 0 <c@d>")),
            Ok(Reply::Body(b"x\n".to_vec())),
        ];
        let expect = [Expect::Body, Expect::Line, Expect::Line, Expect::Body];
        for chunk in 1..=input.len() {
            assert_eq!(replies(input, &expect, chunk, MAX_BODY), expected, "chunk {chunk}");
        }
    }

    #[test]
    fn a_222_answer_to_a_line_command_is_only_a_line() {
        let got = replies(b"222 0\r\nbody\r\n", &[Expect::Line, Expect::Line], 4, MAX_BODY);
        assert_eq!(got, [Ok(status("222 0")), Ok(status("body"))]);
    }

    #[test]
    fn an_empty_body_and_a_missing_article_are_told_apart() {
        let got = replies(b"222 0\r\n.\r\n430 no such article\r\n", &[Expect::Body, Expect::Body], 3, MAX_BODY);
        assert_eq!(got, [Ok(Reply::Body(Vec::new())), Ok(status("430 no such article"))]);
    }

    #[test]
    fn an_endless_line_or_body_is_cut_off_at_its_cap() {
        let long = vec![b'x'; MAX_LINE * 3];
        assert_eq!(replies(&long, &[Expect::Line], 100, 64), [Err(Violation::LineTooLong { limit: MAX_LINE })]);
        let mut late = vec![b'x'; MAX_LINE + 10];
        late.extend_from_slice(b"\r\n");
        assert_eq!(replies(&late, &[Expect::Line], late.len(), 64), [Err(Violation::LineTooLong { limit: MAX_LINE })]);
        let mut body = b"222 0\r\n".to_vec();
        body.extend(std::iter::repeat_n(b"0123456789\r\n".as_slice(), 100).flatten());
        assert_eq!(replies(&body, &[Expect::Body], 7, 64), [Err(Violation::BodyTooLarge { limit: 64 })]);
    }

    #[test]
    fn bytes_that_are_not_text_make_a_reply_without_a_code() {
        let got = replies(b"\xff\xfe\x00\r\n\r\n", &[Expect::Body, Expect::Body], 2, MAX_BODY);
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|r| matches!(r, Ok(Reply::Line(Status { code: None, .. })))), "{got:?}");
    }

    /// A small deterministic generator, so random tests replay exactly.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// The parser agrees with reading line by line, on random replies made of the pieces
    /// that matter: dots, CRs, LFs, status codes, and text.
    #[test]
    fn random_replies_parse_as_reading_line_by_line_does() {
        const PIECES: [&[u8]; 9] = [b".", b"\r", b"\n", b"\r\n", b"222 ", b"430 ", b".\r\n", b"ab", b"\xe2\x82"];
        let mut rng = XorShift(0x5eed_1234_abcd_ef01);
        for _ in 0..3000 {
            let mut input = Vec::new();
            for _ in 0..rng.next() % 40 {
                input.extend_from_slice(PIECES[(rng.next() % PIECES.len() as u64) as usize]);
            }
            let expect: Vec<Expect> =
                (0..8).map(|_| if rng.next().is_multiple_of(2) { Expect::Body } else { Expect::Line }).collect();
            let chunk = (rng.next() % 9 + 1) as usize;
            let got = replies(&input, &expect, chunk, MAX_BODY);
            assert_eq!(got, line_by_line(&input, &expect), "{:?} {expect:?}", String::from_utf8_lossy(&input));
        }
    }

    /// The reference: whole lines, as `read_until(b'\n')` gives them.
    fn line_by_line(input: &[u8], expect: &[Expect]) -> Vec<Result<Reply, Violation>> {
        let mut lines = input.split_inclusive(|&b| b == b'\n').filter(|l| l.ends_with(b"\n"));
        let mut out = Vec::new();
        for &e in expect {
            let Some(line) = lines.next() else { break };
            let status = Status::parse(line);
            if e == Expect::Line || status.code != Some(222) {
                out.push(Ok(Reply::Line(status)));
                continue;
            }
            let mut body = Vec::new();
            let done = loop {
                let Some(line) = lines.next() else { break false };
                if line == b".\r\n" || line == b".\n" {
                    break true;
                }
                body.extend_from_slice(line.strip_prefix(b".").unwrap_or(line));
            };
            if !done {
                break;
            }
            out.push(Ok(Reply::Body(body)));
        }
        out
    }
}
