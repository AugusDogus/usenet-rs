//! Recovery sets written by a small encoder here, after the PAR2 2.0 spec. `tests/par2.rs`
//! checks the same code against sets par2cmdline wrote.

use std::io;

use super::packet::{FILE_DESC, IFSC, MAIN};
use super::write::packet;
use super::*;

fn pseudo_random(len: usize, mut state: u64) -> Vec<u8> {
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// A recovery set as par2cmdline would write it: the index, and a volume of recovery blocks.
struct Written {
    index: Vec<u8>,
    volume: Vec<u8>,
    /// The files, in the set's order.
    files: Vec<(String, Vec<u8>)>,
    set: RecoverySet,
}

fn write(files: &[(&str, Vec<u8>)], slice_size: usize, exponents: std::ops::Range<u16>) -> Written {
    let given: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (*n, d.as_slice())).collect();
    let written = super::write::write(&given, slice_size, exponents);
    // Volumes repeat the index's packets, as par2cmdline writes them.
    let mut volume = written.recovery.concat();
    volume.extend_from_slice(&written.index);
    let set = RecoverySet::parse(&written.index).unwrap();
    let files = written.order.iter().map(|&i| (files[i].0.to_string(), files[i].1.clone())).collect();
    Written { index: written.index, volume, files, set }
}

fn sample() -> Written {
    // Odd sizes, an empty file, and a file shorter than a slice.
    let files = [
        ("a.rar", pseudo_random(1001, 1)),
        ("b.rar", pseudo_random(640, 2)),
        ("empty", Vec::new()),
        ("c.nfo", pseudo_random(17, 3)),
    ];
    write(&files, 64, 0..4)
}

/// Reads slices out of `files`, as a caller would from disk.
fn reader(files: &[(String, Vec<u8>)]) -> impl FnMut(Slice, &mut [u8]) -> io::Result<usize> + '_ {
    |slice, buf| {
        let data = &files[slice.file].1;
        let from = (slice.offset as usize).min(data.len());
        let n = buf.len().min(data.len() - from);
        buf[..n].copy_from_slice(&data[from..from + n]);
        Ok(n)
    }
}

fn blocks(w: &Written) -> Vec<RecoveryBlock> {
    let mut scanner = Scanner::new(&w.set);
    scanner.push(&w.volume);
    scanner.into_blocks()
}

#[test]
fn parses_files_slices_and_checksums() {
    let w = sample();
    let set = &w.set;
    assert_eq!(set.slice_size(), 64);
    assert_eq!(set.files().len(), 4);
    // 16 + 10 + 0 + 1 slices.
    assert_eq!(set.slice_count(), 27);
    for (k, f) in set.files().iter().enumerate() {
        assert_eq!(f.name, w.files[k].0);
        assert_eq!(f.size, w.files[k].1.len() as u64);
        assert_eq!(set.file_named(&f.name.to_uppercase()), Some(k));
        assert_eq!(set.file_starting(f.size, &w.files[k].1), Some(k));
    }
    let a = set.file_named("a.rar").unwrap();
    let first = set.files()[a].first_slice;
    assert_eq!(set.slices_in(a, 0..1), first..first + 1);
    assert_eq!(set.slices_in(a, 63..65), first..first + 2);
    assert_eq!(set.slices_in(a, 990..u64::MAX), first + 15..first + 16);
    assert_eq!(set.slices_in(a, 2000..3000), 0..0);
    assert_eq!(set.slices_in(99, 0..1), 0..0);
    let last = set.slice(first + 15).unwrap();
    assert_eq!((last.file, last.offset, last.length), (a, 960, 41));
    assert_eq!(set.slice(27), None);
    for index in 0..set.slice_count() {
        let s = set.slice(index).unwrap();
        let data = &w.files[s.file].1[s.offset as usize..][..s.length as usize];
        assert!(set.check(index, data), "slice {index}");
        assert!(!set.check(index, &data[..data.len() - 1]), "slice {index} cut short");
    }
    assert_eq!(set.verify(reader(&w.files)).unwrap(), Vec::<u32>::new());
}

#[test]
fn any_damage_up_to_the_blocks_is_rebuilt() {
    let w = sample();
    let blocks = blocks(&w);
    assert_eq!(blocks.len(), 4);
    let picks: [&[u32]; 6] = [&[], &[0], &[26], &[3, 15], &[0, 9, 16, 26], &[16, 17, 18, 19]];
    for damaged in picks {
        let mut files = w.files.clone();
        for &i in damaged {
            let s = w.set.slice(i).unwrap();
            files[s.file].1[s.offset as usize..][..s.length as usize].fill(0);
        }
        assert_eq!(w.set.verify(reader(&files)).unwrap(), damaged);
        let mut written = Vec::new();
        let slices = w
            .set
            .repair(damaged, blocks.clone(), reader(&files), |s, data| {
                written.push((s, data.to_vec()));
                Ok(())
            })
            .unwrap();
        assert_eq!(slices.iter().map(|s| s.index).collect::<Vec<_>>(), damaged);
        for (s, data) in written {
            files[s.file].1[s.offset as usize..][..data.len()].copy_from_slice(&data);
        }
        assert_eq!(files, w.files, "{damaged:?}");
    }
}

#[test]
fn a_slice_found_damaged_while_feeding_uses_a_spare_block() {
    let w = sample();
    let mut files = w.files.clone();
    let s = w.set.slice(5).unwrap();
    files[s.file].1[s.offset as usize] ^= 1;
    // Slice 2 is named damaged up front; slice 5 is found damaged when read.
    let mut rebuilt = Vec::new();
    w.set
        .repair(&[2], blocks(&w)[..2].to_vec(), reader(&files), |s, data| {
            rebuilt.push((s.index, data.to_vec()));
            Ok(())
        })
        .unwrap();
    assert_eq!(rebuilt.iter().map(|r| r.0).collect::<Vec<_>>(), [2, 5]);

    // With no spare, it is an error, and the repair is left as it was.
    let mut repair = Repair::new(&w.set, &[2], blocks(&w)[..1].to_vec()).unwrap();
    let bad = &files[s.file].1[s.offset as usize..][..s.length as usize];
    assert_eq!(repair.feed(5, bad), Err(RepairError::NotEnoughRecovery { damaged: 2, blocks: 1 }));
    assert!(repair.is_waiting(5));
    assert_eq!(repair.damaged(), [2]);
}

#[test]
fn repair_says_what_it_still_needs() {
    let w = sample();
    assert_eq!(
        Repair::new(&w.set, &[1, 2, 3, 4, 5], blocks(&w)).unwrap_err(),
        RepairError::NotEnoughRecovery { damaged: 5, blocks: 4 }
    );
    assert_eq!(Repair::new(&w.set, &[27], blocks(&w)).unwrap_err(), RepairError::NoSuchSlice { index: 27, slices: 27 });
    assert_eq!(Repair::new(&w.set, &[1, 1], blocks(&w)).unwrap_err(), RepairError::Repeated { index: 1 });
    let short = vec![RecoveryBlock { exponent: 0, data: vec![0; 60] }];
    assert!(matches!(Repair::new(&w.set, &[], short), Err(RepairError::WrongBlock { length: 60, .. })));

    let mut repair = Repair::new(&w.set, &[3], blocks(&w)).unwrap();
    assert_eq!(repair.waiting_count(), 26);
    assert_eq!(repair.finish().unwrap_err(), RepairError::Unfinished { waiting: 26, first: 0 });
    let s = w.set.slice(0).unwrap();
    let data = &w.files[s.file].1[..s.length as usize];
    assert_eq!(repair.feed(0, data), Ok(Feed::Accepted));
    assert_eq!(repair.feed(0, data), Err(RepairError::Repeated { index: 0 }));
    assert_eq!(repair.feed(3, data), Err(RepairError::Repeated { index: 3 }));
    assert_eq!(repair.feed(99, data), Err(RepairError::NoSuchSlice { index: 99, slices: 27 }));
    repair.lose(1).unwrap();
    assert_eq!(repair.waiting().next(), Some(2));
    assert_eq!(repair.damaged(), [3, 1]);
    assert!(RepairError::Unfinished { waiting: 26, first: 0 }.to_string().contains("download the rest"));
}

#[test]
fn the_scanner_finds_blocks_however_the_volume_is_cut() {
    let w = sample();
    let other = write(&[("x", pseudo_random(100, 9))], 64, 0..2);
    let mut damaged = w.volume.clone();
    // Damage the second recovery packet's data.
    let second = 64 + 4 + 64 + 64 + 10;
    damaged[second] ^= 1;
    let mut stream = other.volume.clone();
    stream.extend_from_slice(b"some bytes between volumes PAR2\0PK");
    stream.extend_from_slice(&damaged);
    for size in [1, 7, 63, 64, 65, 1000, stream.len()] {
        let mut scanner = Scanner::new(&w.set);
        for chunk in stream.chunks(size) {
            scanner.push(chunk);
        }
        // Pushing the volume again finds nothing new.
        scanner.push(&w.volume);
        let exponents: Vec<u16> = scanner.blocks().iter().map(|b| b.exponent).collect();
        assert_eq!(exponents, [0, 2, 3, 1], "chunks of {size}");
        assert_eq!(scanner.damaged(), 1, "chunks of {size}");
        assert!(scanner.pending.len() < 8, "chunks of {size}");
    }
}

#[test]
fn a_set_parses_from_any_of_its_files_and_despite_damage() {
    let w = sample();
    assert_eq!(RecoverySet::parse(&w.volume).unwrap(), w.set);
    // A damaged copy of the main packet is passed over for an intact one.
    let mut twice = w.index.clone();
    twice[70] ^= 1;
    twice.extend_from_slice(&w.index);
    assert_eq!(RecoverySet::parse(&twice).unwrap(), w.set);
    assert_eq!(RecoverySet::parse(b"not par2").unwrap_err(), Par2Error::NotPar2);
    // Only the recovery packets: no main packet.
    let blocks_only = &w.volume[..w.volume.len() - w.index.len()];
    assert_eq!(RecoverySet::parse(blocks_only).unwrap_err(), Par2Error::NotPar2);
    let desc_only = &w.index[w.index.len() - 100..];
    assert!(matches!(RecoverySet::parse(desc_only), Err(Par2Error::NotPar2 | Par2Error::NoMain)));
    // The main packet alone lacks the files' descriptions.
    let main_len = u64::from_le_bytes(w.index[8..16].try_into().unwrap()) as usize;
    assert!(matches!(
        RecoverySet::parse(&w.index[..main_len]),
        Err(Par2Error::Incomplete { lacking: Lacking::Description, .. })
    ));
}

#[test]
fn malformed_main_packets_are_errors() {
    let set = [7; 16];
    let main = |size: u64, count: u32, ids: &[[u8; 16]]| {
        let mut body = size.to_le_bytes().to_vec();
        body.extend_from_slice(&count.to_le_bytes());
        for id in ids {
            body.extend_from_slice(id);
        }
        packet(set, MAIN, &body)
    };
    let defect = |bytes: Vec<u8>| match RecoverySet::parse(&bytes) {
        Err(Par2Error::Malformed(d)) => d,
        other => panic!("{other:?}"),
    };
    assert_eq!(defect(main(0, 0, &[])), Defect::SliceSize { size: 0 });
    assert_eq!(defect(main(6, 0, &[])), Defect::SliceSize { size: 6 });
    assert_eq!(defect(main(MAX_SLICE + 4, 0, &[])), Defect::SliceSize { size: MAX_SLICE + 4 });
    assert_eq!(defect(main(64, 2, &[[1; 16]])), Defect::FileList);
    assert_eq!(defect(main(64, 2, &[[1; 16], [1; 16]])), Defect::FileList);
    // An empty set is a set.
    assert_eq!(RecoverySet::parse(&main(64, 0, &[])).unwrap().slice_count(), 0);

    let mut desc = [1u8; 16].to_vec();
    desc.extend_from_slice(&[0; 32]);
    desc.extend_from_slice(&(u64::MAX / 2).to_le_bytes());
    let mut bytes = main(64, 1, &[[1; 16]]);
    bytes.extend(packet(set, FILE_DESC, &desc));
    bytes.extend(packet(set, IFSC, &[1; 16]));
    assert_eq!(defect(bytes), Defect::ChecksumCount { file_id: [1; 16], expected: u64::MAX / 2 / 64 + 1, found: 0 });
    let text = Par2Error::Malformed(Defect::SliceSize { size: 6 }).to_string();
    assert!(text.contains("slice size 6"), "{text}");
}

#[test]
fn mutated_sets_parse_and_repair_without_panics() {
    let w = sample();
    let mut bytes = w.volume.clone();
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..2000 {
        bytes.copy_from_slice(&w.volume);
        for _ in 0..1 + next() % 4 {
            let at = (next() as usize) % bytes.len();
            bytes[at] = next() as u8;
        }
        if let Ok(set) = RecoverySet::parse(&bytes) {
            let mut scanner = Scanner::new(&set);
            scanner.push(&bytes);
            let damaged: Vec<u32> = (0..set.slice_count().min(2)).collect();
            if let Ok(mut repair) = Repair::new(&set, &damaged, scanner.into_blocks()) {
                for i in repair.waiting().collect::<Vec<_>>() {
                    let _ = repair.feed(i, &[0; 64]);
                }
                let _ = repair.finish();
            }
            for f in 0..set.files().len() {
                let _ = set.slices_in(f, 0..u64::MAX);
            }
        }
    }
}

#[test]
fn a_wrong_block_is_caught_before_anything_is_written() {
    let w = sample();
    let mut blocks = blocks(&w);
    blocks[0].data[0] ^= 1;
    let result = w.set.repair(&[4], blocks, reader(&w.files), |_, _| panic!("nothing is written"));
    assert_eq!(result, Err(RepairError::Unverified { index: 4 }));
}

#[test]
fn fake_par2_files_are_named_as_par2cmdline_names_them() {
    let data = pseudo_random(5000, 4);
    let files = crate::fake::par2("movie", &[("movie.mkv", &data)], 512, 6);
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["movie.par2", "movie.vol0+1.par2", "movie.vol1+2.par2", "movie.vol3+3.par2"]);
    let set = RecoverySet::parse(&files[0].1).unwrap();
    let mut scanner = Scanner::new(&set);
    for (_, v) in &files[1..] {
        assert_eq!(RecoverySet::parse(v).unwrap(), set);
        scanner.push(v);
    }
    assert_eq!(scanner.blocks().len(), 6);
}
