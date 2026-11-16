//! Turns an NZB into a [`Manifest`]: reads the first article of each posted file and the
//! RAR headers, then maps every article onto the files a user would see.

use std::collections::BTreeSet;
use std::fmt;

use futures_util::future::join_all;

use crate::article::{self, ArticleError};
use crate::manifest::{self, Manifest, OutputFile, Packing, Posted, RarFormat};
use crate::nntp::Pool;
use crate::nzb::{Nzb, NzbFile};
use crate::rar::{self, Format, RarError, Volume};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ResolveError {
    Article {
        file: String,
        error: ArticleError,
    },
    /// The articles do not add up to the file their headers describe.
    Layout {
        file: String,
        reason: String,
    },
    Rar {
        file: String,
        reason: String,
    },
    Compressed {
        name: String,
    },
    Encrypted {
        name: String,
    },
    Incomplete {
        name: String,
        found: u64,
        expected: u64,
    },
    Empty,
}

impl ResolveError {
    /// True when the release cannot stream, though it might download and extract.
    pub fn is_unstreamable(&self) -> bool {
        matches!(self, Self::Compressed { .. })
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Article { file, error } if error.is_missing() => {
                write!(f, "{file} has expired or been removed: {error}")
            }
            Self::Article { file, error } => write!(f, "could not read {file}: {error}"),
            Self::Layout { file, reason } => write!(f, "{file} is posted unevenly: {reason}"),
            Self::Rar { file, reason } => write!(f, "{file}: {reason}"),
            Self::Compressed { name } => write!(f, "{name} is compressed inside its RAR archive, so it cannot stream"),
            Self::Encrypted { name } => write!(f, "{name} is in a password-protected archive"),
            Self::Incomplete { name, found, expected } => write!(
                f,
                "the NZB holds {found} of the {expected} bytes of {name}; volumes are missing or out of order"
            ),
            Self::Empty => f.write_str("the NZB holds no files newsreel can read"),
        }
    }
}

impl std::error::Error for ResolveError {}

const SKIPPED: [&str; 5] = [".par2", ".sfv", ".srr", ".srs", ".nzb"];
const PAR2: &[u8] = b"PAR2\0PKT";

struct Probed<'a> {
    file: &'a NzbFile,
    /// The name in the yEnc header, which may be obfuscated.
    name: String,
    size: u64,
    part_size: u64,
    head: Vec<u8>,
    ids: Vec<Vec<String>>,
}

fn label(file: &NzbFile, index: usize) -> String {
    if file.filename.is_empty() { format!("file {} of the NZB", index + 1) } else { file.filename.clone() }
}

impl Probed<'_> {
    fn posted(&self) -> Posted<'_> {
        Posted { size: self.size, part_size: self.part_size, segments: &self.ids }
    }

    async fn read(&self, pool: &Pool, from: u64, length: usize) -> Result<Vec<u8>, ArticleError> {
        let mut out = Vec::with_capacity(length);
        for piece in self.posted().pieces(from, length as u64, 0) {
            let part = article::fetch(pool, &piece.ids).await?;
            let skip = piece.skip as usize;
            out.extend_from_slice(part.data.get(skip..skip + piece.length as usize).unwrap_or_default());
        }
        Ok(out)
    }
}

async fn probe<'a>(pool: &Pool, index: usize, file: &'a NzbFile) -> Result<Option<Probed<'a>>, ResolveError> {
    let label = || label(file, index);
    let first = &file.segments[0];
    let part = article::fetch(pool, &first.message_ids)
        .await
        .map_err(|error| ResolveError::Article { file: label(), error })?;
    if part.data.starts_with(PAR2) {
        return Ok(None);
    }
    let part_size = part.data.len() as u64;
    let expected = if part_size == 0 { 0 } else { part.file_size.div_ceil(part_size) };
    if part.begin != 0 || expected != file.segments.len() as u64 {
        return Err(ResolveError::Layout {
            file: label(),
            reason: format!(
                "its first article holds bytes {}..{} of {}, implying {expected} articles, but the NZB lists {}",
                part.begin,
                part.end(),
                part.file_size,
                file.segments.len()
            ),
        });
    }
    let ids = file.segments.iter().map(|s| s.message_ids.clone()).collect();
    Ok(Some(Probed { file, name: part.name, size: part.file_size, part_size, head: part.data, ids }))
}

fn container_extension(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        Some(".mkv")
    } else if head.get(4..8) == Some(b"ftyp") {
        Some(".mp4")
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"AVI ") {
        Some(".avi")
    } else if head.first() == Some(&0x47) && head.get(188) == Some(&0x47) {
        Some(".ts")
    } else {
        None
    }
}

/// Long runs of letters and digits with no separators are posting obfuscation, not names.
fn obfuscated(name: &str) -> bool {
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    stem.len() >= 20 && stem.chars().all(|c| c.is_ascii_alphanumeric())
}

/// A relative path with no empty, `.`, or `..` parts.
fn safe_path(name: &str) -> String {
    name.split(['/', '\\']).filter(|p| !p.is_empty() && *p != "." && *p != "..").collect::<Vec<_>>().join("/")
}

fn direct_name(p: &Probed<'_>, title: &str) -> String {
    let given = if p.name.trim().is_empty() { p.file.filename.as_str() } else { p.name.as_str() };
    let given = safe_path(given);
    let extension = container_extension(&p.head);
    if given.is_empty() || obfuscated(&given) {
        let kept = given.rsplit_once('.').map(|(_, e)| format!(".{e}"));
        return format!("{}{}", safe_path(title), extension.map(str::to_string).or(kept).unwrap_or_default());
    }
    given
}

/// A RAR entry's path, with an obfuscated file name replaced by the release title.
fn entry_name(name: &str, title: &str) -> String {
    let path = safe_path(name);
    let (dir, file) = path.rsplit_once('/').map_or(("", path.as_str()), |(d, f)| (d, f));
    if !obfuscated(file) {
        return path;
    }
    let extension = file.rsplit_once('.').map(|(_, e)| format!(".{e}")).unwrap_or_default();
    let renamed = format!("{}{extension}", safe_path(title));
    if dir.is_empty() { renamed } else { format!("{dir}/{renamed}") }
}

fn unique(path: String, taken: &mut BTreeSet<String>) -> String {
    let mut candidate = path.clone();
    let mut n = 2;
    while !taken.insert(candidate.clone()) {
        candidate = match path.rsplit_once('.') {
            Some((stem, ext)) => format!("{stem} ({n}).{ext}"),
            None => format!("{path} ({n})"),
        };
        n += 1;
    }
    candidate
}

/// RAR5 numbers volumes; RAR4 is ordered by its naming, then by post order.
fn volume_order(volume: &Volume, file: &NzbFile) -> (u64, u64, u64) {
    if let Some(n) = volume.number {
        return (0, n, 0);
    }
    let name = file.filename.to_ascii_lowercase();
    let digits = |s: &str| s.parse::<u64>().ok();
    let by_name = if let Some((stem, _)) = name.rsplit_once(".rar") {
        stem.rsplit_once(".part").and_then(|(_, n)| digits(n)).unwrap_or(0)
    } else if let Some((_, ext)) = name.rsplit_once('.') {
        match ext.strip_prefix('r').and_then(digits) {
            Some(n) => n + 1,
            None => digits(ext).unwrap_or(u64::MAX),
        }
    } else {
        u64::MAX
    };
    let first = if volume.first == Some(true) { 0 } else { 1 };
    (first, by_name, u64::from(file.post_index.unwrap_or(u32::MAX)))
}

struct Chain {
    name: String,
    unpacked: u64,
    stored: bool,
    encrypted: bool,
    /// (volume, data offset, length)
    slices: Vec<(usize, u64, u64)>,
}

fn rar_files(
    volumes: &[(Volume, &Probed<'_>)],
    title: &str,
    taken: &mut BTreeSet<String>,
) -> Result<Vec<OutputFile>, ResolveError> {
    let mut open: Vec<Chain> = Vec::new();
    let mut done: Vec<Chain> = Vec::new();
    for (v, (volume, _)) in volumes.iter().enumerate() {
        for e in &volume.entries {
            let slice = (v, e.data_offset, e.packed_size);
            let chain = if e.split_before {
                match open.iter().position(|c| c.name == e.name) {
                    Some(i) => {
                        open[i].slices.push(slice);
                        i
                    }
                    // The set starts partway through this file: an earlier volume is missing.
                    None => continue,
                }
            } else {
                open.retain(|c| c.name != e.name);
                open.push(Chain {
                    name: e.name.clone(),
                    unpacked: e.unpacked_size,
                    stored: e.stored,
                    encrypted: e.encrypted,
                    slices: vec![slice],
                });
                open.len() - 1
            };
            if !e.split_after {
                done.push(open.remove(chain));
            }
        }
    }
    let incomplete = |c: &Chain| ResolveError::Incomplete {
        name: c.name.clone(),
        found: c.slices.iter().map(|s| s.2).sum(),
        expected: c.unpacked,
    };
    if let Some(c) = done.iter().chain(&open).find(|c| c.encrypted) {
        return Err(ResolveError::Encrypted { name: c.name.clone() });
    }
    let largest = |chains: &[Chain]| chains.iter().max_by_key(|c| c.unpacked).map(|c| c.name.clone());
    if let Some(c) = open.iter().filter(|c| c.stored).max_by_key(|c| c.unpacked) {
        return Err(incomplete(c));
    }
    let stored: Vec<&Chain> = done.iter().filter(|c| c.stored).collect();
    if stored.is_empty() {
        return match largest(&done) {
            Some(name) => Err(ResolveError::Compressed { name }),
            None => Ok(Vec::new()),
        };
    }
    let mut files = Vec::new();
    for c in stored {
        let found: u64 = c.slices.iter().map(|s| s.2).sum();
        if found != c.unpacked {
            return Err(incomplete(c));
        }
        let mut pieces = Vec::new();
        let mut at = 0;
        for &(v, offset, length) in &c.slices {
            pieces.extend(volumes[v].1.posted().pieces(offset, length, at));
            at += length;
        }
        files.push(OutputFile { path: unique(entry_name(&c.name, title), taken), size: c.unpacked, pieces });
    }
    Ok(files)
}

/// Resolves the release an NZB describes. Reads one article per posted file, plus a small
/// window wherever a RAR volume has a header past its first article.
pub async fn resolve(nzb: &Nzb, pool: &Pool) -> Result<Manifest, ResolveError> {
    let title = nzb.title().map(str::to_string).unwrap_or_else(|| label(&nzb.files[0], 0));
    let candidates = nzb
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| !SKIPPED.iter().any(|ext| f.filename.to_ascii_lowercase().ends_with(ext)));
    let probes = join_all(candidates.map(|(i, f)| probe(pool, i, f))).await;
    let mut probed = Vec::new();
    let mut failed = None;
    for p in probes {
        match p {
            Ok(Some(p)) => probed.push(p),
            Ok(None) => {}
            Err(e) => {
                failed.get_or_insert(e);
            }
        }
    }
    let (rars, direct): (Vec<&Probed<'_>>, Vec<&Probed<'_>>) =
        probed.iter().partition(|p| rar::format_of(&p.head).is_some());

    let mut taken = BTreeSet::new();
    let mut files = Vec::new();
    let mut packing = Packing::Direct;
    if !rars.is_empty() {
        // Every volume's headers are needed, so a missing one sinks the set.
        if let Some(e) = failed {
            return Err(e);
        }
        let scans = join_all(rars.iter().map(|p| async move {
            let scan = rar::scan(p.head.clone(), p.size, |at, len| p.read(pool, at, len)).await;
            (scan, *p)
        }))
        .await;
        let mut volumes = Vec::new();
        for (scan, p) in scans {
            let file = label(p.file, 0);
            match scan {
                Ok(volume) => volumes.push((volume, p)),
                Err(RarError::Read(error)) => return Err(ResolveError::Article { file, error }),
                Err(RarError::Encrypted) => return Err(ResolveError::Encrypted { name: file }),
                Err(e) => return Err(ResolveError::Rar { file, reason: e.to_string() }),
            }
        }
        volumes.sort_by_key(|(v, p)| volume_order(v, p.file));
        let format = match volumes[0].0.format {
            Format::Rar4 => RarFormat::Rar4,
            Format::Rar5 => RarFormat::Rar5,
        };
        packing = Packing::Rar { format, volumes: volumes.len() as u32 };
        files.extend(rar_files(&volumes, &title, &mut taken)?);
    }
    for p in direct {
        let path = unique(direct_name(p, &title), &mut taken);
        files.push(OutputFile { path, size: p.size, pieces: p.posted().pieces(0, p.size, 0) });
    }
    if files.is_empty() {
        return Err(failed.unwrap_or(ResolveError::Empty));
    }
    let nzb_sha1 = nzb.sha1.iter().map(|b| format!("{b:02x}")).collect();
    Ok(Manifest { format: manifest::FORMAT, nzb_sha1, title, packing, files })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolving_can_move_between_threads() {
        fn send<T: Send>(_: T) {}
        let nzb = Nzb { meta: Vec::new(), files: Vec::new(), sha1: [0; 20] };
        send(resolve(&nzb, &Pool::new(Vec::new())));
    }

    /// Every byte of every output file, read back through the manifest.
    async fn read_back(manifest: &Manifest, pool: &Pool) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for file in &manifest.files {
            let mut bytes = Vec::new();
            for piece in &file.pieces {
                let part = article::fetch(pool, &piece.ids).await.unwrap();
                assert_eq!(part.begin, piece.begin);
                bytes.extend_from_slice(&part.data[piece.skip as usize..(piece.skip + piece.length) as usize]);
            }
            out.push((file.path.clone(), bytes));
        }
        out
    }

    fn data(len: usize, seed: u32) -> Vec<u8> {
        (0..len as u32).map(|i| (i.wrapping_mul(31).wrapping_add(seed) % 251) as u8).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_direct_posting_resolves_to_its_files() {
        use crate::fake::{FakeServer, Posting};
        let (a, b) = (data(25_000, 1), data(3_000, 2));
        let posting = Posting::new("Set", &[("a.bin", &a), ("b.bin", &b), ("set.par2", b"PAR2\0PKT")], 4_000);
        let server = FakeServer::start();
        server.add(&posting);
        let pool = Pool::new(vec![server.config(4, 0)]);
        let manifest = resolve(&Nzb::parse(&posting.nzb).unwrap(), &pool).await.unwrap();
        assert_eq!((manifest.title.as_str(), manifest.packing), ("Set", Packing::Direct));
        assert_eq!(read_back(&manifest, &pool).await, [("a.bin".into(), a), ("b.bin".into(), b)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stored_rar_set_resolves_to_the_file_inside() {
        use crate::fake::{FakeServer, Posting};
        let movie = data(50_000, 3);
        let posting = Posting::stored_rar("Film.2024", "Film.2024.mkv", &movie, 12_000, 5_000);
        let server = FakeServer::start();
        server.add(&posting);
        let pool = Pool::new(vec![server.config(4, 0)]);
        let manifest = resolve(&Nzb::parse(&posting.nzb).unwrap(), &pool).await.unwrap();
        assert_eq!(manifest.packing, Packing::Rar { format: RarFormat::Rar5, volumes: 5 });
        let files = read_back(&manifest, &pool).await;
        let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, ["Sample/sample.mkv", "Film.2024.mkv"]);
        assert_eq!(files[1].1, movie);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_expired_volume_sinks_the_set() {
        use crate::fake::{FakeServer, Posting};
        let posting = Posting::stored_rar("Gone", "Gone.mkv", &data(30_000, 4), 12_000, 5_000);
        let server = FakeServer::start();
        server.add(&posting);
        server.remove(&posting.files[1].1[0]);
        let pool = Pool::new(vec![server.config(4, 0)]);
        let error = resolve(&Nzb::parse(&posting.nzb).unwrap(), &pool).await.unwrap_err();
        assert!(matches!(&error, ResolveError::Article { error, .. } if error.is_missing()), "{error}");
    }

    #[test]
    fn hex_like_names_are_obfuscated() {
        assert!(obfuscated("WKelZy6vZNnOttA6eYVWoHhVEfnbknre.mkv"));
        assert!(obfuscated("f4a4f7d7d7474090872cfbddf1c11a7d.mkv"));
        assert!(!obfuscated("Sintel.2010.1080p.mkv"));
    }

    #[test]
    fn paths_stay_inside_the_folder() {
        assert_eq!(safe_path("../a\\b//./c.mkv"), "a/b/c.mkv");
    }

    #[test]
    fn obfuscated_entries_take_the_release_title() {
        let title = "Tears.of.Steel.2012.2160p";
        assert_eq!(entry_name("f4a4f7d7d7474090872cfbddf1c11a7d.mkv", title), "Tears.of.Steel.2012.2160p.mkv");
        assert_eq!(entry_name("Set\\f4a4f7d7d7474090872cfbddf1c11a7d.mkv", title), "Set/Tears.of.Steel.2012.2160p.mkv");
        assert_eq!(entry_name("Set\\Movie.mkv", title), "Set/Movie.mkv");
    }

    #[test]
    fn colliding_names_get_numbers() {
        let mut taken = BTreeSet::new();
        assert_eq!(unique("a.mkv".into(), &mut taken), "a.mkv");
        assert_eq!(unique("a.mkv".into(), &mut taken), "a (2).mkv");
    }
}
