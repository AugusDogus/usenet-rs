//! The archives in `tests/data/rar`, posted to a loopback server and resolved: every file
//! reads back through its manifest with the checksum the archive records, and archives
//! that cannot stream say why.
#![cfg(feature = "fake")]

use std::path::PathBuf;

use usenet::fake::{FakeServer, Posting};
use usenet::manifest::{Packing, RarFormat};
use usenet::{Manifest, Nzb, Pool, ResolveError, article, resolve};

/// Decoded bytes per article, small so headers past the first article are exercised.
const PART: usize = 1_500;

fn volumes(format: &str, name: &str) -> Vec<(String, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/rar").join(format).join(name);
    let mut volumes: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| {
            let path = e.unwrap().path();
            (path.file_name().unwrap().to_string_lossy().into_owned(), std::fs::read(&path).unwrap())
        })
        .collect();
    // Posted in name order, which for old-style names puts `arc.rar` last.
    volumes.sort();
    volumes
}

struct Resolved {
    result: Result<Manifest, ResolveError>,
    pool: Pool,
    /// Article bodies the server sent while resolving.
    fetched: u64,
}

async fn post_and_resolve(format: &str, name: &str) -> Resolved {
    let volumes = volumes(format, name);
    let files: Vec<(&str, &[u8])> = volumes.iter().map(|(n, v)| (n.as_str(), v.as_slice())).collect();
    let posting = Posting::new(&format!("{format}.{name}"), &files, PART);
    let server = FakeServer::start();
    server.add(&posting);
    let pool = Pool::new(vec![server.config(4, 0)]);
    let result = resolve(&Nzb::parse(&posting.nzb).unwrap(), &pool).await;
    Resolved { result, pool, fetched: server.bodies() }
}

/// Every output file's path, size, and the CRC32 of its bytes read back piece by piece.
async fn read_back(manifest: &Manifest, pool: &Pool) -> Vec<(String, u64, u32)> {
    let mut out = Vec::new();
    for file in &manifest.files {
        let mut crc = crc32fast::Hasher::new();
        let mut read = 0;
        for piece in &file.pieces {
            let part = article::fetch(pool, &piece.ids).await.unwrap();
            assert_eq!(part.begin, piece.begin);
            crc.update(&part.data[piece.skip as usize..(piece.skip + piece.length) as usize]);
            read += piece.length;
        }
        assert_eq!(read, file.size, "{}", file.path);
        out.push((file.path.clone(), file.size, crc.finalize()));
    }
    out.sort();
    out
}

fn files(expected: &[(&str, u64, u32)]) -> Vec<(String, u64, u32)> {
    let mut files: Vec<_> = expected.iter().map(|&(p, s, c)| (p.to_string(), s, c)).collect();
    files.sort();
    files
}

/// The files `gen.sh` puts in its multi-volume cases, with their CRC32s.
const VOLUMES: [(&str, u64, u32); 3] =
    [("movie.nfo", 300, 0xfa0f_1ba3), ("Movie.2024.mkv", 40_000, 0x7642_615d), ("sample.mkv", 6_000, 0x528a_69ef)];

/// Each output file's path, size and CRC32.
type Expected = &'static [(&'static str, u64, u32)];

const UNICODE: Expected = &[
    ("Sample/sample.mkv", 2_000, 0x433b_5375),
    ("Subs/Ünïcödé 日本語.srt", 500, 0xe719_6f61),
    ("Movie.2024.mkv", 8_000, 0x39a5_5d4e),
];
const LATE: Expected = &[("1_big.bin", 20_000, 0x870a_0d7b), ("2_small.mkv", 3_000, 0xf2cf_1a4b)];
const SINGLE: Expected = &[("file.bin", 5_000, 0x5b24_8d70)];

#[tokio::test(flavor = "multi_thread")]
async fn stored_archives_resolve_and_read_back_exactly() {
    let cases: [(&str, &str, RarFormat, u32, Expected); 12] = [
        ("rar4", "single", RarFormat::Rar4, 1, SINGLE),
        ("rar5", "single", RarFormat::Rar5, 1, SINGLE),
        ("rar4", "multi_dirs_unicode", RarFormat::Rar4, 1, UNICODE),
        ("rar5", "multi_dirs_unicode", RarFormat::Rar5, 1, UNICODE),
        ("rar4", "late_header", RarFormat::Rar4, 1, LATE),
        ("rar5", "late_header", RarFormat::Rar5, 1, LATE),
        ("rar4", "volumes_new", RarFormat::Rar4, 3, &VOLUMES),
        ("rar5", "volumes_new", RarFormat::Rar5, 3, &VOLUMES),
        ("rar4", "volumes_old", RarFormat::Rar4, 3, &VOLUMES),
        ("rar5", "quickopen", RarFormat::Rar5, 3, &VOLUMES),
        ("rar4", "comment_rr", RarFormat::Rar4, 4, &VOLUMES),
        ("rar5", "comment_rr", RarFormat::Rar5, 4, &VOLUMES),
    ];
    for (format, name, rar_format, volumes, expected) in cases {
        let Resolved { result, pool, .. } = post_and_resolve(format, name).await;
        let manifest = result.unwrap_or_else(|e| panic!("{format}/{name}: {e}"));
        assert_eq!(manifest.packing, Packing::Rar { format: rar_format, volumes }, "{format}/{name}");
        assert_eq!(read_back(&manifest, &pool).await, files(expected), "{format}/{name}");
        assert_eq!(Manifest::from_bytes(&manifest.to_bytes()), Ok(manifest), "{format}/{name}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn resolving_reads_headers_not_data() {
    // One article per volume, plus the 3 that hold the header after the 20 KB file and
    // the rest of the volume behind it.
    let late = post_and_resolve("rar5", "late_header").await;
    assert!(late.result.is_ok());
    assert_eq!(late.fetched, 1 + 3);
    let volumes = post_and_resolve("rar5", "volumes_new").await;
    let manifest = volumes.result.unwrap();
    let articles: usize = manifest.files.iter().map(|f| f.pieces.len()).sum();
    assert!(volumes.fetched < 10 && volumes.fetched * 4 < articles as u64, "{} of {articles}", volumes.fetched);
}

#[tokio::test(flavor = "multi_thread")]
async fn archives_that_cannot_stream_say_why() {
    for format in ["rar4", "rar5"] {
        let compressed = post_and_resolve(format, "compressed").await.result;
        assert_eq!(compressed, Err(ResolveError::Compressed { name: "a.txt".into() }), "{format}");
        assert!(compressed.unwrap_err().is_unstreamable());

        let data = post_and_resolve(format, "enc_data").await.result;
        assert_eq!(data, Err(ResolveError::Encrypted { name: "movie.nfo".into() }), "{format}");

        let headers = post_and_resolve(format, "enc_headers").await.result;
        assert_eq!(headers, Err(ResolveError::Encrypted { name: "arc.part1.rar".into() }), "{format}");
    }
}
