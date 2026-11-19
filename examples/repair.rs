//! Repairs a damaged release from its PAR2 files, against real servers.
//!
//! Downloads the files of an NZB's largest PAR2 set into a folder, zeroes 768,000 bytes (one
//! article's worth) of the first, finds the slices that hit, fetches the volumes holding
//! enough recovery blocks, repairs, and checks the file's SHA-256 against the original.
//!
//! cargo run --release --example repair -- servers.json release.nzb folder
//!
//! `servers.json` holds `{"servers": [{host, port, username, password, ...}]}`.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use usenet::nzb::NzbFile;
use usenet::par2::{Par2Files, RecoverySet, Scanner, Slice};
use usenet::{Nzb, Pool, Server, article};

#[derive(serde::Deserialize)]
struct Servers {
    servers: Vec<Server>,
}

/// One article's worth of bytes, zeroed as if no server had it.
const LOST: u64 = 768_000;

/// Fetches a posted file whole, its articles placed by their yEnc offsets.
async fn fetch(pool: &Pool, file: &NzbFile) -> Vec<u8> {
    let parts: Vec<_> = futures_util::stream::iter(&file.segments)
        .map(|s| article::fetch(pool, &s.message_ids))
        .buffered(32)
        .collect()
        .await;
    let mut out = Vec::new();
    for part in parts {
        let part = part.unwrap_or_else(|e| panic!("{}: {e}", file.filename));
        let at = part.begin as usize;
        if out.len() < at + part.data.len() {
            out.resize(at + part.data.len(), 0);
        }
        out[at..at + part.data.len()].copy_from_slice(&part.data);
    }
    out
}

fn sha256(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [config, nzb, dir] = &args[..] else { panic!("usage: repair servers.json release.nzb folder") };
    let Servers { mut servers } =
        serde_json::from_slice(&std::fs::read(config).expect("reading servers.json")).expect("a servers array");
    for s in &mut servers {
        s.connections = s.connections.min(16);
    }
    let pool = Pool::new(servers);
    let nzb = Nzb::parse(&std::fs::read(nzb).expect("reading the NZB")).expect("an NZB");
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();

    let par2 = Par2Files::find(&nzb).into_iter().next().expect("PAR2 files in the NZB");
    let set = RecoverySet::parse(&fetch(&pool, &nzb.files[par2.index]).await).expect("a recovery set");
    println!(
        "set: {} files, {} slices of {} bytes, about {} recovery blocks in {} volumes",
        set.files().len(),
        set.slice_count(),
        set.slice_size(),
        par2.blocks(set.slice_size()),
        par2.volumes.len()
    );

    // Posters often rename files, so match each posted file to the set by its size and first
    // 16 KiB, from its first article, and by name only when that fails.
    let mut posted = vec![None; set.files().len()];
    for (i, n) in nzb.files.iter().enumerate() {
        let Some(first) = n.segments.first() else { continue };
        let Ok(part) = article::fetch(&pool, &first.message_ids).await else { continue };
        let k = set.file_starting(part.file_size, &part.data).or_else(|| set.file_named(&n.filename));
        if let Some(k) = k {
            posted[k].get_or_insert(i);
        }
    }
    let started = Instant::now();
    let mut total = 0;
    for (k, f) in set.files().iter().enumerate() {
        let posted = &nzb.files[posted[k].unwrap_or_else(|| panic!("{} is not posted", f.name))];
        println!("  {} is posted as {}", f.name, posted.filename);
        let bytes = fetch(&pool, posted).await;
        assert_eq!(bytes.len() as u64, f.size, "{}", f.name);
        total += bytes.len();
        std::fs::write(dir.join(&f.name), bytes).unwrap();
    }
    println!("downloaded {} MiB in {:.1}s", total >> 20, started.elapsed().as_secs_f64());

    let read = |slice: Slice, buf: &mut [u8]| -> std::io::Result<usize> {
        let mut file = std::fs::File::open(dir.join(&set.files()[slice.file].name))?;
        file.seek(SeekFrom::Start(slice.offset))?;
        let mut n = 0;
        while n < buf.len() {
            match file.read(&mut buf[n..])? {
                0 => break,
                k => n += k,
            }
        }
        Ok(n)
    };
    let started = Instant::now();
    let damaged = set.verify(read).unwrap();
    println!("verified in {:.1}s: {} slices damaged", started.elapsed().as_secs_f64(), damaged.len());

    let target = &set.files()[0];
    let path = dir.join(&target.name);
    let original = sha256(&path);
    let at = (target.size / 3).min(target.size.saturating_sub(LOST));
    {
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(at)).unwrap();
        file.write_all(&vec![0; LOST.min(target.size) as usize]).unwrap();
    }
    let damaged: Vec<u32> = set.slices_in(0, at..at + LOST).collect();
    println!("zeroed bytes {at}..{} of {}: slices {damaged:?}, sha256 now {}", at + LOST, target.name, sha256(&path));

    let started = Instant::now();
    let mut scanner = Scanner::new(&set);
    let mut fetched = Vec::new();
    for volume in par2.choose(damaged.len() as u32, set.slice_size()) {
        if scanner.blocks().len() >= damaged.len() {
            break;
        }
        scanner.push(&fetch(&pool, &nzb.files[volume]).await);
        fetched.push(nzb.files[volume].filename.clone());
    }
    println!(
        "fetched {fetched:?} in {:.1}s: {} recovery blocks, {} damaged packets",
        started.elapsed().as_secs_f64(),
        scanner.blocks().len(),
        scanner.damaged()
    );

    let started = Instant::now();
    let write = |slice: Slice, data: &[u8]| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).open(dir.join(&set.files()[slice.file].name))?;
        file.seek(SeekFrom::Start(slice.offset))?;
        file.write_all(data)
    };
    let written = set.repair(&damaged, scanner.into_blocks(), read, write).expect("a repair");
    let repaired = sha256(&path);
    println!(
        "repaired {} slices in {:.1}s; sha256 {} (original {original}): {}",
        written.len(),
        started.elapsed().as_secs_f64(),
        repaired,
        if repaired == original { "match" } else { "MISMATCH" }
    );
    assert_eq!(repaired, original);
}
