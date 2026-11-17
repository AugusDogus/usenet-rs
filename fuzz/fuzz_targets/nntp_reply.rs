//! The NNTP reply parser, fed arbitrary bytes in arbitrary pieces: it never panics, never
//! holds more than its caps, and reads the same replies as reading whole lines would.

#![no_main]

use libfuzzer_sys::fuzz_target;
use usenet::nntp::fuzzing::{Parsed, Violation, parse};

/// A small cap, so the fuzzer reaches it.
const MAX_BODY: usize = 4096;

fuzz_target!(|data: &[u8]| {
    // The first bytes pick piece sizes and which replies answer BODY; the rest is the stream.
    let Some((&header, rest)) = data.split_first() else { return };
    let split = usize::from(header % 8).min(rest.len());
    let (control, input) = rest.split_at(split);
    let chunks: Vec<usize> = control.iter().map(|&b| usize::from(b % 64) + 1).collect();
    let bodies: Vec<bool> = control.iter().map(|&b| b & 0x80 != 0).collect();
    let bodies = if bodies.is_empty() { vec![true] } else { bodies };
    let got = parse(input, &chunks, &bodies, MAX_BODY);
    let whole = parse(input, &[input.len().max(1)], &bodies, MAX_BODY);
    // How the bytes arrive changes nothing, up to where a cap was hit.
    if !got.iter().chain(&whole).any(|p| matches!(p, Parsed::Broken(_))) {
        assert_eq!(got, whole);
        assert_eq!(got, line_by_line(input, &bodies));
    }
    for parsed in &got {
        match parsed {
            Parsed::Body(body) => assert!(body.len() <= MAX_BODY),
            Parsed::Line { text, .. } => assert!(text.len() <= 3 * 2048),
            Parsed::Broken(Violation::LineTooLong { .. } | Violation::BodyTooLarge { .. }) => {}
            Parsed::Broken(v) => panic!("the parser alone never reports {v:?}"),
        }
    }
});

/// The reference: whole lines, as `read_until(b'\n')` gives them.
fn line_by_line(input: &[u8], bodies: &[bool]) -> Vec<Parsed> {
    let mut lines = input.split_inclusive(|&b| b == b'\n').filter(|l| l.ends_with(b"\n"));
    let mut out = Vec::new();
    for &body in bodies.iter().cycle() {
        let Some(line) = lines.next() else { break };
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
        if !body || code != Some(222) {
            out.push(Parsed::Line { code, text });
            continue;
        }
        let mut data = Vec::new();
        let done = loop {
            let Some(line) = lines.next() else { break false };
            if line == b".\r\n" || line == b".\n" {
                break true;
            }
            data.extend_from_slice(line.strip_prefix(b".").unwrap_or(line));
        };
        if !done {
            break;
        }
        out.push(Parsed::Body(data));
    }
    out
}
