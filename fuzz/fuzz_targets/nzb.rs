//! Parses arbitrary documents. Seed with `tests/data/nzb`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use usenet::Nzb;

fuzz_target!(|input: &[u8]| {
    let Ok(nzb) = Nzb::parse(input) else { return };
    assert!(!nzb.files.is_empty());
    for file in &nzb.files {
        assert!(!file.segments.is_empty());
        assert!(file.segments.windows(2).all(|w| w[0].number < w[1].number));
        for segment in &file.segments {
            assert!(!segment.message_ids.is_empty());
            assert!(segment.message_ids.iter().all(|id| !id.is_empty()));
        }
    }
});
