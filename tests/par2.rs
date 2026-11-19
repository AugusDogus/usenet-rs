//! The recovery set par2cmdline wrote in `tests/data/par2`: it parses, its files verify, and
//! the damaged copy of `a.bin` repairs back to the original's SHA-256, both from files on
//! disk and from volumes posted to a loopback server and fetched article by article.

use std::collections::HashMap;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use usenet::par2::{Feed, Par2Files, RecoveryBlock, RecoverySet, Repair, Scanner, Slice};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/par2/set")
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// The SHA-256 `SHA256SUMS` records for each file.
fn sums() -> HashMap<String, String> {
    String::from_utf8(read("SHA256SUMS"))
        .unwrap()
        .lines()
        .map(|l| {
            let (sum, name) = l.split_once("  ").unwrap();
            (name.to_string(), sum.to_string())
        })
        .collect()
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

const VOLUMES: [&str; 3] = ["set.vol0+3.par2", "set.vol3+3.par2", "set.vol6+2.par2"];

/// The set's files as on disk, with `a.bin` replaced by its damaged copy.
fn damaged_files(set: &RecoverySet) -> Vec<Vec<u8>> {
    set.files().iter().map(|f| read(if f.name == "a.bin" { "a.bin.damaged" } else { &f.name })).collect()
}

fn reader(files: &[Vec<u8>]) -> impl FnMut(Slice, &mut [u8]) -> std::io::Result<usize> + '_ {
    |slice, buf| {
        let data = &files[slice.file];
        let from = (slice.offset as usize).min(data.len());
        let n = buf.len().min(data.len() - from);
        buf[..n].copy_from_slice(&data[from..from + n]);
        Ok(n)
    }
}

#[test]
fn the_index_describes_the_files() {
    let set = RecoverySet::parse(&read("set.par2")).unwrap();
    assert_eq!(set.slice_size(), 4096);
    let mut names: Vec<&str> = set.files().iter().map(|f| f.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["a.bin", "b.bin", "c.bin"]);
    assert_eq!(set.slice_count(), 25 + 1 + 18);
    for f in set.files() {
        let data = read(&f.name);
        assert_eq!(f.size, data.len() as u64);
        assert_eq!(f.md5, <[u8; 16]>::from(md5::Md5::digest(&data)), "{}", f.name);
    }
    // Every volume carries the same description.
    for v in VOLUMES {
        assert_eq!(RecoverySet::parse(&read(v)).unwrap(), set, "{v}");
    }
    let intact: Vec<Vec<u8>> = set.files().iter().map(|f| read(&f.name)).collect();
    assert_eq!(set.verify(reader(&intact)).unwrap(), Vec::<u32>::new());
}

#[test]
fn the_damaged_file_repairs_to_the_original_from_the_fewest_blocks() {
    let set = RecoverySet::parse(&read("set.par2")).unwrap();
    let files = damaged_files(&set);
    let a = set.file_named("a.bin").unwrap();
    let damaged: Vec<u32> = set.slices_in(a, 8000..8768).collect();
    assert_eq!(damaged.len(), 2);
    assert_eq!(set.verify(reader(&files)).unwrap(), damaged);

    // Two blocks are needed; the two-block volume holds exactly that.
    let mut scanner = Scanner::new(&set);
    scanner.push(&read("set.vol6+2.par2"));
    let blocks: Vec<RecoveryBlock> = scanner.into_blocks();
    assert_eq!(blocks.iter().map(|b| b.exponent).collect::<Vec<_>>(), [6, 7]);

    let mut repaired = files[a].clone();
    let written = set
        .repair(&damaged, blocks, reader(&files), |slice, data| {
            assert_eq!(slice.file, a);
            repaired[slice.offset as usize..][..data.len()].copy_from_slice(data);
            Ok(())
        })
        .unwrap();
    assert_eq!(written.iter().map(|s| s.index).collect::<Vec<_>>(), damaged);
    let sums = sums();
    assert_ne!(sha256(&files[a]), sums["a.bin"]);
    assert_eq!(sha256(&repaired), sums["a.bin"]);
}

#[test]
fn every_volume_and_any_mix_of_blocks_repairs() {
    let set = RecoverySet::parse(&read("set.par2")).unwrap();
    let mut scanner = Scanner::new(&set);
    for v in VOLUMES {
        scanner.push(&read(v));
    }
    let all = scanner.into_blocks();
    assert_eq!(all.len(), 8);
    let intact: Vec<Vec<u8>> = set.files().iter().map(|f| read(&f.name)).collect();
    // Eight damaged slices across all three files, missing outright, from all eight blocks.
    let damaged: Vec<u32> = (0..8).map(|k| k * 5 + 1).collect();
    let mut repair = Repair::new(&set, &damaged, all.clone()).unwrap();
    for index in repair.waiting().collect::<Vec<_>>() {
        let s = set.slice(index).unwrap();
        let data = &intact[s.file][s.offset as usize..][..s.length as usize];
        assert_eq!(repair.feed(index, data).unwrap(), Feed::Accepted);
    }
    for r in repair.finish().unwrap() {
        assert_eq!(r.data, intact[r.slice.file][r.slice.offset as usize..][..r.slice.length as usize]);
    }
    // One slice, from each block alone.
    for block in all {
        let e = block.exponent;
        let mut repair = Repair::new(&set, &[43], vec![block]).unwrap();
        for index in repair.waiting().collect::<Vec<_>>() {
            let s = set.slice(index).unwrap();
            let _ = repair.feed(index, &intact[s.file][s.offset as usize..][..s.length as usize]).unwrap();
        }
        let rebuilt = repair.finish().unwrap();
        let s = rebuilt[0].slice;
        assert_eq!(rebuilt[0].data, intact[s.file][s.offset as usize..][..s.length as usize], "block {e}");
    }
}

/// The set posted as a release's PAR2 files would be, then found, fetched and scanned one
/// article at a time.
#[cfg(feature = "fake")]
#[tokio::test(flavor = "multi_thread")]
async fn posted_volumes_are_found_fetched_and_repair() {
    use usenet::fake::{FakeServer, Posting};
    use usenet::{Nzb, Pool, article};

    let names = ["set.par2", VOLUMES[0], VOLUMES[1], VOLUMES[2], "a.bin", "b.bin", "c.bin"];
    let contents: Vec<Vec<u8>> = names.iter().map(|n| read(n)).collect();
    let files: Vec<(&str, &[u8])> = names.iter().zip(&contents).map(|(n, c)| (*n, c.as_slice())).collect();
    // Articles smaller than a recovery packet, so packets straddle them.
    let posting = Posting::new("set", &files, 3000);
    let server = FakeServer::start();
    server.add(&posting);
    let pool = Pool::new(vec![server.config(4, 0)]);
    let nzb = Nzb::parse(&posting.nzb).unwrap();

    let found = Par2Files::find(&nzb);
    assert_eq!(found.len(), 1);
    let par2 = &found[0];
    assert_eq!(par2.base, "set");
    assert_eq!(par2.volumes.len(), 3);
    let fetch = async |file: usize| {
        let mut bytes = Vec::new();
        for segment in &nzb.files[file].segments {
            bytes.extend(article::fetch(&pool, &segment.message_ids).await.unwrap().data);
        }
        bytes
    };
    assert_eq!(nzb.files[par2.index].filename, "set.par2");
    let set = RecoverySet::parse(&fetch(par2.index).await).unwrap();
    assert_eq!(par2.blocks(set.slice_size()), 8);
    let a = set.file_named("a.bin").unwrap();
    let damaged: Vec<u32> = set.slices_in(a, 8000..8768).collect();
    let chosen = par2.choose(damaged.len() as u32, set.slice_size());
    assert_eq!(nzb.files[chosen[0]].filename, "set.vol6+2.par2");

    let mut scanner = Scanner::new(&set);
    for segment in &nzb.files[chosen[0]].segments {
        scanner.push(&article::fetch(&pool, &segment.message_ids).await.unwrap().data);
    }
    let files = damaged_files(&set);
    let mut repaired = files[a].clone();
    set.repair(&damaged, scanner.into_blocks(), reader(&files), |slice, data| {
        repaired[slice.offset as usize..][..data.len()].copy_from_slice(data);
        Ok(())
    })
    .unwrap();
    assert_eq!(sha256(&repaired), sums()["a.bin"]);
}
