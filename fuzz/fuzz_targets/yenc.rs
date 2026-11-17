//! Decodes arbitrary bodies, and arbitrary data behind a valid header so most inputs reach
//! the data loop. Seed with `tests/data/yenc`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use usenet::yenc::{Part, decode};

fn check(part: &Part, input: &[u8]) {
    assert!(part.end() <= part.file_size.max(1));
    assert!(part.data.len() <= input.len());
}

fuzz_target!(|input: &[u8]| {
    if let Ok(part) = decode(input) {
        check(&part, input);
    }
    let mut body = format!("=ybegin line=128 size={} name=f\r\n", input.len()).into_bytes();
    body.extend_from_slice(input);
    if let Ok(part) = decode(&body) {
        check(&part, input);
    }
});
