//! `rar::scan` over arbitrary bytes. Under `cfg(fuzzing)` header checksums are not
//! enforced, so mutations reach the parsers behind them.
//!
//! The first three input bytes choose the length of the head handed over up front and the
//! size claimed for the volume; the rest is the volume. Reads must stay within 64 KiB and
//! only move forward, so every scan ends.
#![no_main]

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use libfuzzer_sys::fuzz_target;
use usenet::rar;

fuzz_target!(|input: &[u8]| {
    let Some((control, bytes)) = input.split_first_chunk::<3>() else { return };
    let head = bytes[..usize::from(u16::from_le_bytes([control[0], control[1]])).min(bytes.len())].to_vec();
    let len = bytes.len() as u64;
    let size = match control[2] % 4 {
        0 => len,
        1 => u64::MAX,
        2 => len + u64::from(control[2]) * 4096,
        _ => len.saturating_sub(u64::from(control[2])),
    };
    let mut last = None;
    let scan = rar::scan(head, size, |at, want| {
        assert!(want <= 64 * 1024, "a read of {want} bytes");
        assert!(last.is_none_or(|l| at > l), "a read at {at} after one at {last:?}");
        last = Some(at);
        let from = usize::try_from(at).unwrap_or(usize::MAX).min(bytes.len());
        std::future::ready(Ok::<_, ()>(bytes[from..bytes.len().min(from + want)].to_vec()))
    });
    // Every read is ready at once, so one poll finishes the scan.
    let Poll::Ready(result) = pin!(scan).poll(&mut Context::from_waker(Waker::noop())) else {
        panic!("a scan over ready reads is pending");
    };
    if let Ok(volume) = result {
        for e in &volume.entries {
            assert!(e.data_offset.checked_add(e.packed_size).is_some_and(|end| end <= size), "{e:?}");
        }
    }
});
