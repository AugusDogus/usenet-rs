//! PAR2 over arbitrary bytes: parsing a recovery set, scanning for its recovery blocks in
//! pieces, and a repair that names damaged slices, feeds the rest, and finishes.
//!
//! The first input byte chooses the size of the pieces the scanner is fed and how many
//! slices to name damaged; the rest is the PAR2 data. Under `cfg(fuzzing)` packet hashes are
//! not enforced, so mutations reach the parsers behind them.
#![no_main]

use libfuzzer_sys::fuzz_target;
use usenet::par2::{RecoverySet, Repair, Scanner};

fuzz_target!(|input: &[u8]| {
    let Some((&control, bytes)) = input.split_first() else { return };
    let Ok(set) = RecoverySet::parse(bytes) else { return };
    let count = set.slice_count();
    let mut scanner = Scanner::new(&set);
    for piece in bytes.chunks(usize::from(control % 16) * 37 + 1) {
        scanner.push(piece);
    }
    for (k, f) in set.files().iter().enumerate() {
        let slices = set.slices_in(k, 0..f.size);
        assert_eq!(slices.len() as u64, u64::from(f.slices), "{f:?}");
        for index in slices {
            let slice = set.slice(index).expect("a slice of a file is in the set");
            assert!(slice.file == k && slice.offset + u64::from(slice.length) <= f.size, "{slice:?}");
        }
    }
    let damaged: Vec<u32> = (0..count.min(u32::from(control >> 4))).collect();
    let Ok(mut repair) = Repair::new(&set, &damaged, scanner.into_blocks()) else { return };
    let zeros = vec![0u8; bytes.len().min(set.slice_size() as usize)];
    for index in repair.waiting().collect::<Vec<_>>() {
        let _ = repair.feed(index, &zeros[..zeros.len().min(set.slice(index).map_or(0, |s| s.length as usize))]);
    }
    if let Ok(rebuilt) = repair.finish() {
        for r in rebuilt {
            assert!(set.check(r.slice.index, &r.data));
        }
    }
});
