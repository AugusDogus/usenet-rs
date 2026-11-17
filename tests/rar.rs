//! Volumes written by RAR 6.24 (`tests/data/rar`, made by its `gen.sh`): every case of the
//! format matrix scans to the entries rar wrote, and the stored data at the offsets found
//! matches the checksums the archive records.

use std::convert::Infallible;
use std::path::PathBuf;

use usenet::rar::{self, Defect, Format, RarError, ScanError, Volume};

/// Bytes of each volume handed to `scan` up front, as if it were the first article.
const HEAD: usize = 1500;

/// A case's volumes in set order: `arc.rar` then `arc.r00`, or `arc.partN.rar` by N.
fn case(format: &str, name: &str) -> Vec<(String, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/rar").join(format).join(name);
    let mut volumes: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| {
            let path = e.unwrap().path();
            (path.file_name().unwrap().to_string_lossy().into_owned(), std::fs::read(&path).unwrap())
        })
        .collect();
    let order = |name: &str| -> u32 {
        if let Some(n) = name.strip_prefix("arc.part").and_then(|n| n.strip_suffix(".rar")) {
            n.parse().unwrap()
        } else if let Some(n) = name.strip_prefix("arc.r").and_then(|n| n.parse::<u32>().ok()) {
            n + 1
        } else {
            0
        }
    };
    volumes.sort_by_key(|(name, _)| order(name));
    assert!(!volumes.is_empty(), "{} holds no volumes", dir.display());
    volumes
}

type Reads = Vec<(u64, usize)>;

async fn scan(volume: &[u8]) -> (Result<Volume, ScanError<Infallible>>, Reads) {
    let mut reads = Vec::new();
    let head = volume[..volume.len().min(HEAD)].to_vec();
    let result = rar::scan(head, volume.len() as u64, |at, len| {
        reads.push((at, len));
        let at = at as usize;
        std::future::ready(Ok(volume[at..(at + len).min(volume.len())].to_vec()))
    })
    .await;
    (result, reads)
}

/// Every file of a stored, unencrypted case: its name and size, after checking each part
/// against its CRC and the joined data against the whole file's.
async fn stored_files(format: &str, name: &str) -> Vec<(String, u64)> {
    let mut files = Vec::new();
    let mut open: Option<(String, Vec<u8>)> = None;
    for (file, bytes) in case(format, name) {
        let volume = scan(&bytes).await.0.unwrap_or_else(|e| panic!("{format}/{name}/{file}: {e}"));
        for e in volume.entries {
            assert!(e.stored && !e.encrypted, "{format}/{name}/{file}: {e:?}");
            let data = &bytes[e.data_offset as usize..(e.data_offset + e.packed_size) as usize];
            let mut joined = match open.take() {
                Some((open_name, joined)) if e.split_before => {
                    assert_eq!(open_name, e.name, "{format}/{name}/{file}");
                    joined
                }
                None if !e.split_before => Vec::new(),
                other => panic!("{format}/{name}/{file}: {e:?} after {:?}", other.map(|o| o.0)),
            };
            joined.extend_from_slice(data);
            if e.split_after {
                assert_eq!(e.crc32, Some(crc32fast::hash(data)), "{format}/{name}/{file}: part of {}", e.name);
                open = Some((e.name, joined));
            } else {
                assert_eq!(joined.len() as u64, e.unpacked_size, "{format}/{name}: {}", e.name);
                assert_eq!(e.crc32, Some(crc32fast::hash(&joined)), "{format}/{name}: {}", e.name);
                files.push((e.name, e.unpacked_size));
            }
        }
    }
    assert!(open.is_none(), "{format}/{name}: a file never ends");
    files
}

fn sizes(files: &[(&str, u64)]) -> Vec<(String, u64)> {
    files.iter().map(|&(n, s)| (n.to_string(), s)).collect()
}

const VOLUMES: [(&str, u64); 3] = [("movie.nfo", 300), ("Movie.2024.mkv", 40_000), ("sample.mkv", 6_000)];

#[tokio::test]
async fn stored_archives_map_every_byte_of_every_file() {
    for format in ["rar4", "rar5"] {
        assert_eq!(stored_files(format, "single").await, sizes(&[("file.bin", 5_000)]));
        assert_eq!(stored_files(format, "late_header").await, sizes(&[("1_big.bin", 20_000), ("2_small.mkv", 3_000)]));
        assert_eq!(stored_files(format, "volumes_new").await, sizes(&VOLUMES), "{format}");
        assert_eq!(stored_files(format, "comment_rr").await, sizes(&VOLUMES), "{format}");
    }
    assert_eq!(stored_files("rar4", "volumes_old").await, sizes(&VOLUMES));
    assert_eq!(stored_files("rar5", "quickopen").await, sizes(&VOLUMES));
}

#[tokio::test]
async fn names_keep_their_folders_and_unicode() {
    let expected = |sep: &str| {
        let subs = format!("Subs{sep}Ünïcödé 日本語.srt");
        let sample = format!("Sample{sep}sample.mkv");
        vec![(sample, 2_000), (subs, 500), ("Movie.2024.mkv".to_string(), 8_000)]
    };
    assert_eq!(stored_files("rar4", "multi_dirs_unicode").await, expected("\\"));
    assert_eq!(stored_files("rar5", "multi_dirs_unicode").await, expected("/"));
}

#[tokio::test]
async fn volumes_say_where_they_sit_in_the_set() {
    for (format, name) in [("rar4", "volumes_new"), ("rar4", "volumes_old"), ("rar5", "volumes_new")] {
        let mut seen = Vec::new();
        for (_, bytes) in case(format, name) {
            let v = scan(&bytes).await.0.unwrap();
            seen.push((v.format, v.number, v.first));
        }
        let expected: Vec<_> = match format {
            "rar4" => (0..3).map(|i| (Format::Rar4, None, Some(i == 0))).collect(),
            _ => (0..3).map(|i| (Format::Rar5, Some(i), Some(i == 0))).collect(),
        };
        assert_eq!(seen, expected, "{format}/{name}");
    }
}

#[tokio::test]
async fn a_file_starting_mid_volume_costs_one_header_read() {
    for (format, header_at) in [("rar4", 66 + 20_000), ("rar5", 70 + 20_000)] {
        let (_, bytes) = &case(format, "late_header")[0];
        let (volume, reads) = scan(bytes).await;
        assert_eq!(volume.unwrap().entries[1].name, "2_small.mkv");
        assert_eq!(reads, [(header_at, bytes.len() - header_at as usize)], "{format}");
    }
}

#[tokio::test]
async fn compressed_files_are_reported_as_compressed() {
    for format in ["rar4", "rar5"] {
        let (_, bytes) = &case(format, "compressed")[0];
        let entries = scan(bytes).await.0.unwrap().entries;
        let summary: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.stored, e.unpacked_size)).collect();
        assert_eq!(summary, [("a.txt", false, 60_393)], "{format}");
    }
}

#[tokio::test]
async fn encrypted_files_are_reported_as_encrypted() {
    for format in ["rar4", "rar5"] {
        for (file, bytes) in case(format, "enc_data") {
            let entries = scan(&bytes).await.0.unwrap().entries;
            assert!(!entries.is_empty() && entries.iter().all(|e| e.encrypted), "{format}/{file}: {entries:?}");
        }
    }
}

#[tokio::test]
async fn encrypted_headers_need_a_password() {
    for format in ["rar4", "rar5"] {
        for (file, bytes) in case(format, "enc_headers") {
            assert_eq!(scan(&bytes).await.0, Err(ScanError::Rar(RarError::Encrypted)), "{format}/{file}");
        }
    }
}

#[tokio::test]
async fn a_damaged_header_is_reported_and_damaged_data_is_not_read() {
    for format in ["rar4", "rar5"] {
        let (_, bytes) = &case(format, "single")[0];
        let entry = scan(bytes).await.0.unwrap().entries.remove(0);
        let data_at = entry.data_offset as usize;
        let mut damaged = bytes.clone();
        damaged[data_at - 2] ^= 0x01; // the end of the file header
        let r = scan(&damaged).await.0;
        assert!(matches!(r, Err(ScanError::Rar(RarError::Corrupt { at, .. })) if at < entry.data_offset), "{r:?}");

        let mut damaged = bytes.clone();
        damaged[data_at + 100] ^= 0x01;
        assert!(scan(&damaged).await.0.is_ok(), "{format}: file data is never read, so never checked");
    }
}

#[tokio::test]
async fn a_cut_volume_is_malformed() {
    for format in ["rar4", "rar5"] {
        let (_, bytes) = &case(format, "single")[0];
        let entry = scan(bytes).await.0.unwrap().entries.remove(0);
        let end = entry.data_offset + entry.packed_size;
        let r = scan(&bytes[..end as usize - 1]).await.0;
        let defect = Defect::DataPastEnd { end, size: end - 1 };
        assert!(matches!(r, Err(ScanError::Rar(RarError::Malformed { defect: d, .. })) if d == defect), "{r:?}");
    }
}

/// The first 512 bytes of two volumes of a real posting of Big Buck Bunny, a stored RAR4
/// set of 50 MB volumes: everything `scan` needs is in them, so nothing more is read.
#[tokio::test]
async fn a_real_release_scans_from_its_first_bytes() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/rar/real");
    for (volume, first) in [(1, true), (2, false)] {
        let head = std::fs::read(dir.join(format!("big.buck.bunny.480p.h264.part0{volume}.rar.head"))).unwrap();
        let scanned = rar::scan(head, 50_000_000, |at, _| std::future::ready(Err(format!("read at {at}")))).await;
        let v = scanned.unwrap();
        assert_eq!((v.format, v.number, v.first), (Format::Rar4, None, Some(first)));
        let entries: Vec<_> = v
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.stored, e.split_before, e.split_after, e.data_offset, e.packed_size))
            .collect();
        assert_eq!(entries, [("big_buck_bunny_480p_h264.mov", true, !first, true, 80, 49_999_900)]);
        assert_eq!(v.entries[0].unpacked_size, 249_229_883);
    }
}
