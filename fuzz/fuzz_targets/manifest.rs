//! `Manifest::from_bytes` over arbitrary bytes: it never panics, and whatever it accepts
//! encodes and decodes back to itself.
#![no_main]

use libfuzzer_sys::fuzz_target;
use usenet::Manifest;

fuzz_target!(|bytes: &[u8]| {
    if let Ok(manifest) = Manifest::from_bytes(bytes) {
        assert_eq!(Manifest::from_bytes(&manifest.to_bytes()), Ok(manifest));
    }
});
