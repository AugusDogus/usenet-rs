//! Turns an NZB into a [`Manifest`]: reads the first article of each posted file and the
//! RAR headers, then maps every article onto the files a user would see.

use std::collections::BTreeSet;
use std::fmt;
use std::ops::Range;

use futures_util::future::join_all;

use crate::article::{self, ArticleError};
use crate::manifest::{Manifest, OutputFile, Packing, Posted, RarFormat};
use crate::nntp::Pool;
use crate::nzb::{Nzb, NzbFile};
use crate::rar::{self, Format, RarError, ScanError, Volume};

/// Why an NZB could not be resolved into a [`Manifest`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ResolveError {
    /// An article could not be fetched or decoded.
    Article {
        /// The posted file the article belongs to.
        file: String,
        /// Why it could not be read.
        error: ArticleError,
    },
    /// The articles do not add up to the file their headers describe.
    Layout {
        /// The posted file.
        file: String,
        /// How its articles disagree.
        error: LayoutError,
    },
    /// A RAR volume's headers are damaged or not readable. Encrypted headers are reported
    /// as [`ResolveError::Encrypted`] instead.
    Rar {
        /// The posted volume.
        file: String,
        /// What is wrong with its headers.
        error: RarError,
    },
    /// The release's files are compressed inside its RAR archive, so it cannot stream.
    Compressed {
        /// The largest compressed file.
        name: String,
    },
    /// The release is in a password-protected RAR archive.
    Encrypted {
        /// The encrypted file, or the volume whose headers are encrypted.
        name: String,
    },
    /// The volumes hold only part of a stored file.
    Incomplete {
        /// The file inside the archive.
        name: String,
        /// Bytes of it the volumes hold.
        found: u64,
        /// Its size.
        expected: u64,
    },
    /// The NZB lists nothing but repair and index files.
    Empty,
}

/// How a posted file's articles disagree with the file their yEnc headers describe.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LayoutError {
    /// The first article the NZB lists does not hold the start of the file.
    FirstNotAtStart {
        /// The offset the article holds instead.
        begin: u64,
    },
    /// The first article's size implies another number of articles than the NZB lists.
    ArticleCount {
        /// The posted file's size, from its yEnc header.
        file_size: u64,
        /// Bytes in the first article.
        part_size: u64,
        /// Articles the file needs at that size.
        implied: u64,
        /// Articles the NZB lists.
        listed: u64,
    },
    /// An article holds other bytes than its place in the NZB implies.
    WrongBytes {
        /// The bytes of the posted file its place implies.
        wanted: Range<u64>,
        /// The bytes its yEnc header says it holds.
        held: Range<u64>,
    },
}

impl ResolveError {
    /// True when the release cannot stream, though it might download and extract.
    pub fn is_unstreamable(&self) -> bool {
        matches!(self, Self::Compressed { .. })
    }
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FirstNotAtStart { begin } => write!(f, "its first article starts at byte {begin}, not 0"),
            Self::ArticleCount { file_size, part_size, implied, listed } => write!(
                f,
                "its first article holds {part_size} of its {file_size} bytes, implying {implied} articles, \
                 but the NZB lists {listed}"
            ),
            Self::WrongBytes { wanted, held } => write!(
                f,
                "the article listed for bytes {}..{} holds bytes {}..{}",
                wanted.start, wanted.end, held.start, held.end
            ),
        }
    }
}

impl std::error::Error for LayoutError {}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Article { file, error } if error.is_missing() => write!(
                f,
                "{file} has expired or been removed ({error}), so the release cannot be read whole; \
                 try another posting of it or a server with longer retention"
            ),
            Self::Article { file, error } => {
                write!(f, "could not read {file} ({error}); check the servers and the connection, then try again")
            }
            Self::Layout { file, error } => write!(
                f,
                "{file} is posted inconsistently: {error}, so its bytes cannot be located; \
                 the posting is damaged, so try another"
            ),
            Self::Rar { file, error } => write!(f, "{file}: {error}"),
            Self::Compressed { name } => write!(
                f,
                "{name} is compressed inside its RAR archive, so the release can only be downloaded and \
                 extracted, not streamed"
            ),
            Self::Encrypted { name } => write!(
                f,
                "{name} is in a password-protected RAR archive, so it cannot be read without the password, \
                 which this crate does not take; try another release"
            ),
            Self::Incomplete { name, found, expected } => write!(
                f,
                "the NZB holds {found} of the {expected} bytes of {name}, so volumes are missing or \
                 misnamed and the file cannot be read whole; try another posting"
            ),
            Self::Empty => f.write_str(
                "the NZB holds no files this crate can read, only repair and index files, so there is nothing to stream",
            ),
        }
    }
}

impl std::error::Error for ResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Article { error, .. } => Some(error),
            Self::Layout { error, .. } => Some(error),
            Self::Rar { error, .. } => Some(error),
            _ => None,
        }
    }
}

const SKIPPED: [&str; 5] = [".par2", ".sfv", ".srr", ".srs", ".nzb"];
const PAR2: &[u8] = b"PAR2\0PKT";

struct Probed<'a> {
    file: &'a NzbFile,
    /// Its place in the NZB.
    index: usize,
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

    async fn read(&self, pool: &Pool, from: u64, length: usize) -> Result<Vec<u8>, ResolveError> {
        let file = || label(self.file, self.index);
        // `length` is at most a header window, so this is bounded.
        let mut out = Vec::with_capacity(length);
        for piece in self.posted().pieces(from, length as u64, 0) {
            let part = article::fetch(pool, &piece.ids)
                .await
                .map_err(|error| ResolveError::Article { file: file(), error })?;
            let end = piece.skip.saturating_add(piece.length);
            let range = usize::try_from(piece.skip).ok().zip(usize::try_from(end).ok());
            let bytes = range.and_then(|(skip, end)| part.data.get(skip..end)).filter(|_| part.begin == piece.begin);
            let Some(bytes) = bytes else {
                let held = part.begin..part.begin.saturating_add(part.data.len() as u64);
                let wanted = piece.begin..piece.begin.saturating_add(end);
                return Err(ResolveError::Layout { file: file(), error: LayoutError::WrongBytes { wanted, held } });
            };
            out.extend_from_slice(bytes);
        }
        Ok(out)
    }
}

async fn probe<'a>(pool: &Pool, index: usize, file: &'a NzbFile) -> Result<Option<Probed<'a>>, ResolveError> {
    let label = || label(file, index);
    let Some(first) = file.segments.first() else { return Ok(None) };
    let part = article::fetch(pool, &first.message_ids)
        .await
        .map_err(|error| ResolveError::Article { file: label(), error })?;
    if part.data.starts_with(PAR2) {
        return Ok(None);
    }
    let layout = |error| Err(ResolveError::Layout { file: label(), error });
    if part.begin != 0 {
        return layout(LayoutError::FirstNotAtStart { begin: part.begin });
    }
    let part_size = part.data.len() as u64;
    let implied = if part_size == 0 { 0 } else { part.file_size.div_ceil(part_size) };
    let listed = file.segments.len() as u64;
    if implied != listed {
        return layout(LayoutError::ArticleCount { file_size: part.file_size, part_size, implied, listed });
    }
    let ids = file.segments.iter().map(|s| s.message_ids.clone()).collect();
    Ok(Some(Probed { file, index, name: part.name, size: part.file_size, part_size, head: part.data, ids }))
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

/// A relative path with no empty, `.`, or `..` parts, and no `:` or NUL, which some
/// platforms read as a drive, a stream, or the end of the name.
fn safe_path(name: &str) -> String {
    let name = name.replace([':', '\0'], "_");
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
    let path = if path.is_empty() { "unnamed".to_string() } else { path };
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
            Some(n) => n.saturating_add(1),
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
    let found = |c: &Chain| c.slices.iter().fold(0u64, |sum, s| sum.saturating_add(s.2));
    let incomplete =
        |c: &Chain| ResolveError::Incomplete { name: c.name.clone(), found: found(c), expected: c.unpacked };
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
        if found(c) != c.unpacked {
            return Err(incomplete(c));
        }
        let mut pieces = Vec::new();
        let mut at = 0u64;
        for &(v, offset, length) in &c.slices {
            pieces.extend(volumes[v].1.posted().pieces(offset, length, at));
            at = at.saturating_add(length);
        }
        files.push(OutputFile { path: unique(entry_name(&c.name, title), taken), size: c.unpacked, pieces });
    }
    Ok(files)
}

/// Resolves the release an NZB describes. Reads one article per posted file, plus a small
/// window wherever a RAR volume has a header past its first article; file data is never
/// fetched. PAR2, SFV, SRR, SRS and nested NZB files are left out.
///
/// Files posted as they are map directly. Files stored in RAR volumes map onto the data
/// inside the volumes, in volume order; compressed or encrypted archives are reported as
/// errors, since they cannot stream.
///
/// ```no_run
/// use usenet::{Nzb, Pool, ResolveError, Server, resolve};
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let pool = Pool::new(vec![Server {
///     host: "news.example.com".into(),
///     port: 563,
///     tls: true,
///     username: "user".into(),
///     password: "secret".into(),
///     connections: 8,
///     pipeline: Server::DEFAULT_PIPELINE,
///     priority: 0,
/// }]);
/// let nzb = Nzb::parse(&std::fs::read("release.nzb")?)?;
/// match resolve(&nzb, &pool).await {
///     Ok(manifest) => {
///         for file in &manifest.files {
///             println!("{} ({} bytes)", file.path, file.size);
///         }
///         std::fs::write("release.manifest", manifest.to_bytes())?;
///     }
///     Err(e) if e.is_unstreamable() => println!("download and extract it instead: {e}"),
///     Err(ResolveError::Article { error, .. }) if error.is_missing() => println!("expired: {error}"),
///     Err(e) => return Err(e.into()),
/// }
/// # Ok(())
/// # }
/// ```
pub async fn resolve(nzb: &Nzb, pool: &Pool) -> Result<Manifest, ResolveError> {
    let Some(first) = nzb.files.first() else { return Err(ResolveError::Empty) };
    let title = nzb.title().map(str::to_string).unwrap_or_else(|| label(first, 0));
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
            let file = label(p.file, p.index);
            match scan {
                Ok(volume) => volumes.push((volume, p)),
                Err(ScanError::Read(error)) => return Err(error),
                Err(ScanError::Rar(RarError::Encrypted)) => return Err(ResolveError::Encrypted { name: file }),
                Err(ScanError::Rar(error)) => return Err(ResolveError::Rar { file, error }),
            }
        }
        volumes.sort_by_key(|(v, p)| volume_order(v, p.file));
        if let Some((first, _)) = volumes.first() {
            let format = match first.format {
                Format::Rar4 => RarFormat::Rar4,
                Format::Rar5 => RarFormat::Rar5,
            };
            packing = Packing::Rar { format, volumes: u32::try_from(volumes.len()).unwrap_or(u32::MAX) };
        }
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
    Ok(Manifest { nzb_sha1, title, packing, files })
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_header_read_from_the_wrong_article_is_a_layout_error() {
        use crate::fake::{FakeServer, Posting};
        // 500-byte articles put the movie's header, after the 1 KB sample, in the third.
        let posting = Posting::stored_rar("Mixed", "Mixed.mkv", &data(30_000, 5), 12_000, 500);
        let server = FakeServer::start();
        server.add(&posting);
        let ids = &posting.files[0].1;
        let nzb =
            String::from_utf8(posting.nzb.clone()).unwrap().replace(&format!(">{}<", ids[2]), &format!(">{}<", ids[1]));
        let pool = Pool::new(vec![server.config(4, 0)]);
        let error = resolve(&Nzb::parse(nzb.as_bytes()).unwrap(), &pool).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Layout { error: LayoutError::WrongBytes { held, .. }, .. } if *held == (500..1000)),
            "{error}"
        );
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
        assert_eq!(safe_path("C:\\Film: Part 2.mkv"), "C_/Film_ Part 2.mkv");
        assert_eq!(unique(safe_path(".."), &mut BTreeSet::new()), "unnamed");
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
