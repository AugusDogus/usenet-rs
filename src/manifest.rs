//! A resolved release: its output files, and which bytes of which article land where.
//! Store it to read the release again later without resolving it again.
//!
//! # Stability
//!
//! [`Manifest::to_bytes`] writes JSON tagged with the format version [`FORMAT`], and
//! [`Manifest::from_bytes`] reads only that version. Within one version the encoding only
//! changes in ways every reader of that version accepts, such as a new field older readers
//! ignore, so a manifest stored by one release of this crate reads back in every later
//! release that has the same [`FORMAT`]. A change older readers would misread bumps
//! [`FORMAT`], and is called out in the changelog; `from_bytes` then fails with
//! [`ManifestError::UnsupportedFormat`], and the NZB has to be resolved again.
//!
//! Treat the bytes as opaque. Serializing a [`Manifest`] with serde directly gives its
//! fields without the version tag, so use `to_bytes` and `from_bytes` to persist one.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

/// The manifest format version that [`Manifest::to_bytes`] writes and
/// [`Manifest::from_bytes`] reads.
pub const FORMAT: u32 = 1;

/// A release resolved into the files a user sees and the article pieces behind them.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// SHA-1 of the NZB, in hex. It names the download.
    pub nzb_sha1: String,
    /// The release name, from the NZB or its first file.
    pub title: String,
    /// How the files were posted.
    pub packing: Packing,
    /// The files, RAR contents first, then files posted as they are.
    pub files: Vec<OutputFile>,
}

/// How a release's files were posted.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Packing {
    /// The files were posted as they are.
    Direct,
    /// The files are stored, uncompressed, in RAR volumes.
    Rar {
        /// The archive format.
        format: RarFormat,
        /// How many volumes the set has.
        volumes: u32,
    },
}

/// The format of a RAR set.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RarFormat {
    /// RAR 2.9 to 4.x.
    Rar4,
    /// RAR 5.0 and later.
    Rar5,
}

/// One file of the release.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct OutputFile {
    /// Relative and `/`-separated, with no empty, `.` or `..` parts.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// In file order, covering the file without gaps or overlap.
    pub pieces: Vec<Piece>,
}

/// The part of one article that lands in an output file.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Piece {
    /// Copies of the same article, tried in order. Never empty.
    pub ids: Vec<String>,
    /// Where the bytes land in the output file.
    pub offset: u64,
    /// How many bytes land. Never 0.
    pub length: u64,
    /// The article's offset within its posted file, which its yEnc header must match.
    pub begin: u64,
    /// Bytes of the decoded article before the ones that land.
    pub skip: u64,
}

/// Why bytes could not be read as a [`Manifest`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ManifestError {
    /// The bytes are not JSON shaped like a manifest.
    Unreadable {
        /// Where the JSON went wrong, from 1. 0 when the input ended early.
        line: usize,
        /// The column on that line, from 1.
        column: usize,
        /// What the JSON parser found there.
        detail: String,
    },
    /// The manifest is in another format version than [`FORMAT`].
    UnsupportedFormat {
        /// The version it records.
        found: u32,
    },
    /// The manifest lists no files.
    NoFiles,
    /// A file's path is empty, absolute, or could leave the download folder.
    UnsafePath {
        /// The path.
        path: String,
    },
    /// Two files have the same path.
    DuplicatePath {
        /// The path.
        path: String,
    },
    /// A file's pieces do not cover it exactly once, in order.
    Untiled {
        /// The file's path.
        path: String,
        /// What is wrong with its pieces.
        problem: Tiling,
    },
}

/// How a file's pieces fail to cover it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tiling {
    /// A piece lists no article to read it from.
    NoCopies {
        /// Where the piece lands.
        offset: u64,
    },
    /// A piece covers no bytes.
    Empty {
        /// Where the piece lands.
        offset: u64,
    },
    /// A piece does not start where the previous one ended.
    Misplaced {
        /// Where the previous piece ended.
        expected: u64,
        /// Where this one starts.
        found: u64,
    },
    /// A piece runs past the end of the file, or past any article.
    PastEnd {
        /// Where the piece lands.
        offset: u64,
        /// Its length.
        length: u64,
    },
    /// The pieces stop short of the end of the file.
    Short {
        /// Bytes they cover.
        covered: u64,
        /// The file's size.
        size: u64,
    },
}

impl fmt::Display for Tiling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCopies { offset } => write!(f, "the piece at byte {offset} lists no article"),
            Self::Empty { offset } => write!(f, "the piece at byte {offset} is empty"),
            Self::Misplaced { expected, found } => {
                write!(f, "a piece starts at byte {found} where the previous one ended at {expected}")
            }
            Self::PastEnd { offset, length } => write!(f, "the {length}-byte piece at byte {offset} runs past the end"),
            Self::Short { covered, size } => write!(f, "the pieces cover {covered} of its {size} bytes"),
        }
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const AGAIN: &str = "resolve the NZB again to make a new one";
        match self {
            Self::Unreadable { line, column, detail } => {
                write!(f, "the bytes are not a release manifest (line {line}, column {column}: {detail}); {AGAIN}")
            }
            Self::UnsupportedFormat { found } => write!(
                f,
                "the manifest is in format {found}, but this version of the crate reads only format {FORMAT}; {AGAIN}"
            ),
            Self::NoFiles => write!(f, "the manifest lists no files, so there is nothing to read; {AGAIN}"),
            Self::UnsafePath { path } => write!(
                f,
                "the manifest names a file {path:?}, which is empty, absolute or could leave the download \
                 folder, so the manifest is refused; {AGAIN}"
            ),
            Self::DuplicatePath { path } => {
                write!(f, "the manifest lists {path:?} twice, so the manifest is refused; {AGAIN}")
            }
            Self::Untiled { path, problem } => {
                write!(f, "in the manifest, {problem} of {path}, so the file would read wrong; {AGAIN}")
            }
        }
    }
}

impl std::error::Error for ManifestError {}

/// The bytes [`Manifest::to_bytes`] writes: the version, then the manifest's fields.
#[derive(Serialize)]
struct Tagged<'a> {
    format: u32,
    #[serde(flatten)]
    manifest: &'a Manifest,
}

/// Just the version, read before the rest so another version's shape is not misreported.
#[derive(Deserialize)]
struct Tag {
    format: u32,
}

fn unreadable(e: serde_json::Error) -> ManifestError {
    ManifestError::Unreadable { line: e.line(), column: e.column(), detail: e.to_string() }
}

/// Relative and `/`-separated, with no empty, `.` or `..` parts and nothing a platform
/// reads as a root, a drive, or a separator.
fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':', '\0'])
        && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

fn tile(file: &OutputFile) -> Result<(), Tiling> {
    let mut at = 0u64;
    for piece in &file.pieces {
        let offset = piece.offset;
        if piece.ids.is_empty() {
            return Err(Tiling::NoCopies { offset });
        }
        if piece.length == 0 {
            return Err(Tiling::Empty { offset });
        }
        if offset != at {
            return Err(Tiling::Misplaced { expected: at, found: offset });
        }
        // `at` never passes `file.size`, so neither subtraction can wrap.
        if piece.length > file.size - at || piece.skip.checked_add(piece.length).is_none() {
            return Err(Tiling::PastEnd { offset, length: piece.length });
        }
        at += piece.length;
    }
    if at != file.size {
        return Err(Tiling::Short { covered: at, size: file.size });
    }
    Ok(())
}

impl Manifest {
    /// Encodes the manifest, tagged with [`FORMAT`], for [`Manifest::from_bytes`].
    pub fn to_bytes(&self) -> Vec<u8> {
        // Plain structs, enums with string tags, strings and integers always serialize.
        serde_json::to_vec(&Tagged { format: FORMAT, manifest: self }).expect("manifests serialize")
    }

    /// Parses and checks a manifest that [`Manifest::to_bytes`] wrote: its version must be
    /// [`FORMAT`], its paths must stay inside the download folder, and its pieces must
    /// tile each file exactly.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ManifestError> {
        let Tag { format } = serde_json::from_slice(bytes).map_err(unreadable)?;
        if format != FORMAT {
            return Err(ManifestError::UnsupportedFormat { found: format });
        }
        let manifest: Self = serde_json::from_slice(bytes).map_err(unreadable)?;
        if manifest.files.is_empty() {
            return Err(ManifestError::NoFiles);
        }
        let mut paths = BTreeSet::new();
        for file in &manifest.files {
            if !safe_path(&file.path) {
                return Err(ManifestError::UnsafePath { path: file.path.clone() });
            }
            if !paths.insert(file.path.as_str()) {
                return Err(ManifestError::DuplicatePath { path: file.path.clone() });
            }
            tile(file).map_err(|problem| ManifestError::Untiled { path: file.path.clone(), problem })?;
        }
        Ok(manifest)
    }
}

/// A posted file as articles of `part_size` decoded bytes, the last possibly shorter.
#[derive(Clone, Debug)]
pub struct Posted<'a> {
    /// The posted file's size.
    pub size: u64,
    /// Decoded bytes in every article but the last.
    pub part_size: u64,
    /// The message-IDs of each article, in order.
    pub segments: &'a [Vec<String>],
}

impl Posted<'_> {
    /// Pieces for bytes `[from, from + length)` of this posted file, landing at `at` in the
    /// output. Bytes past the end of the file, or past the last article listed, are left out.
    pub fn pieces(&self, from: u64, length: u64, at: u64) -> Vec<Piece> {
        let end = from.saturating_add(length).min(self.size);
        if from >= end || self.part_size == 0 {
            return Vec::new();
        }
        let last = end.div_ceil(self.part_size).min(self.segments.len() as u64);
        (from / self.part_size..last)
            .filter_map(|i| {
                let ids = self.segments.get(usize::try_from(i).ok()?)?;
                // `i * part_size` is below `end`, so it cannot overflow.
                let begin = i * self.part_size;
                let seg_end = begin.saturating_add(self.part_size).min(self.size);
                let (lo, hi) = (begin.max(from), seg_end.min(end));
                (lo < hi).then(|| Piece {
                    ids: ids.clone(),
                    offset: at.saturating_add(lo - from),
                    length: hi - lo,
                    begin,
                    skip: lo - begin,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<Vec<String>> {
        (0..n).map(|i| vec![format!("m{i}")]).collect()
    }

    #[test]
    fn a_direct_file_is_one_piece_per_article() {
        let segments = ids(4);
        let posted = Posted { size: 35, part_size: 10, segments: &segments };
        let pieces = posted.pieces(0, 35, 0);
        let summary: Vec<_> = pieces.iter().map(|p| (p.ids[0].as_str(), p.offset, p.length, p.skip)).collect();
        assert_eq!(summary, [("m0", 0, 10, 0), ("m1", 10, 10, 0), ("m2", 20, 10, 0), ("m3", 30, 5, 0)]);
    }

    #[test]
    fn stored_data_skips_headers_and_lands_after_earlier_volumes() {
        let segments = ids(4);
        // A 35-byte volume whose data starts at byte 7 and runs to the end, landing at 1000.
        let posted = Posted { size: 35, part_size: 10, segments: &segments };
        let pieces = posted.pieces(7, 28, 1000);
        let summary: Vec<_> = pieces.iter().map(|p| (p.offset, p.length, p.begin, p.skip)).collect();
        assert_eq!(summary, [(1000, 3, 0, 7), (1003, 10, 10, 0), (1013, 10, 20, 0), (1023, 5, 30, 0)]);
    }

    #[test]
    fn a_range_past_the_posted_file_is_cut_to_it() {
        let segments = ids(2);
        let posted = Posted { size: 15, part_size: 10, segments: &segments };
        let pieces = posted.pieces(5, u64::MAX, u64::MAX - 1);
        let summary: Vec<_> = pieces.iter().map(|p| (p.offset, p.length)).collect();
        assert_eq!(summary, [(u64::MAX - 1, 5), (u64::MAX, 5)]);
        let few = Posted { size: 1 << 60, part_size: 1, segments: &segments };
        assert_eq!(few.pieces(0, u64::MAX, 0).len(), 2, "never more pieces than articles");
    }

    fn piece(offset: u64, length: u64) -> Piece {
        Piece { ids: vec!["a".into()], offset, length, begin: 0, skip: 0 }
    }

    fn manifest(path: &str, pieces: Vec<Piece>, size: u64) -> Manifest {
        Manifest {
            nzb_sha1: "00".into(),
            title: "t".into(),
            packing: Packing::Direct,
            files: vec![OutputFile { path: path.into(), size, pieces }],
        }
    }

    fn untiled(problem: Tiling) -> Result<Manifest, ManifestError> {
        Err(ManifestError::Untiled { path: "a.mkv".into(), problem })
    }

    #[test]
    fn manifests_must_tile_their_files() {
        let good = manifest("a.mkv", vec![piece(0, 5), piece(5, 5)], 10);
        assert_eq!(Manifest::from_bytes(&good.to_bytes()), Ok(good));
        let parse = |pieces, size| Manifest::from_bytes(&manifest("a.mkv", pieces, size).to_bytes());
        assert_eq!(parse(vec![piece(0, 5), piece(6, 4)], 10), untiled(Tiling::Misplaced { expected: 5, found: 6 }));
        assert_eq!(parse(vec![piece(0, 5)], 10), untiled(Tiling::Short { covered: 5, size: 10 }));
        assert_eq!(parse(vec![piece(0, 5), piece(5, 0)], 5), untiled(Tiling::Empty { offset: 5 }));
        assert_eq!(parse(vec![piece(0, u64::MAX)], 10), untiled(Tiling::PastEnd { offset: 0, length: u64::MAX }));
        let mut far = piece(0, 10);
        far.skip = u64::MAX;
        assert_eq!(parse(vec![far], 10), untiled(Tiling::PastEnd { offset: 0, length: 10 }));
        let mut none = piece(0, 10);
        none.ids.clear();
        assert_eq!(parse(vec![none], 10), untiled(Tiling::NoCopies { offset: 0 }));
    }

    #[test]
    fn manifests_keep_their_files_inside_the_download_folder() {
        for path in ["", "/etc/passwd", "../a.mkv", "a/../../b", "a//b", "./a", "C:/a.mkv", "a\\b.mkv"] {
            let bytes = manifest(path, vec![piece(0, 1)], 1).to_bytes();
            assert_eq!(Manifest::from_bytes(&bytes), Err(ManifestError::UnsafePath { path: path.into() }), "{path}");
        }
        let mut two = manifest("a.mkv", vec![piece(0, 1)], 1);
        two.files.push(two.files[0].clone());
        assert_eq!(Manifest::from_bytes(&two.to_bytes()), Err(ManifestError::DuplicatePath { path: "a.mkv".into() }));
        let nested = manifest("Set/Sub/ä b.mkv", vec![piece(0, 1)], 1);
        assert_eq!(Manifest::from_bytes(&nested.to_bytes()), Ok(nested));
    }

    #[test]
    fn the_format_version_is_checked_before_the_shape() {
        let bytes = manifest("a.mkv", vec![piece(0, 1)], 1).to_bytes();
        assert!(bytes.starts_with(br#"{"format":1,"nzb_sha1":"#), "{}", String::from_utf8_lossy(&bytes));
        let future = br#"{"format":2,"files":"a different shape"}"#;
        assert_eq!(Manifest::from_bytes(future), Err(ManifestError::UnsupportedFormat { found: 2 }));
        let untagged = br#"{"nzb_sha1":"00","title":"t","packing":{"kind":"direct"},"files":[]}"#;
        assert!(matches!(Manifest::from_bytes(untagged), Err(ManifestError::Unreadable { .. })));
        assert!(matches!(Manifest::from_bytes(b"not json"), Err(ManifestError::Unreadable { line: 1, .. })));
    }

    /// A manifest as version 1 of this crate wrote it, so format 1 keeps reading.
    #[test]
    fn manifests_written_by_0_1_still_read() {
        let stored = br#"{"format":1,"nzb_sha1":"ab","title":"Film","packing":{"kind":"rar","format":"rar5","volumes":2},"files":[{"path":"Film.mkv","size":12,"pieces":[{"ids":["a@x","b@x"],"offset":0,"length":7,"begin":0,"skip":5},{"ids":["c@x"],"offset":7,"length":5,"begin":12,"skip":0}]}]}"#;
        let manifest = Manifest::from_bytes(stored).unwrap();
        assert_eq!(manifest.packing, Packing::Rar { format: RarFormat::Rar5, volumes: 2 });
        assert_eq!(manifest.files[0].pieces[0].ids, ["a@x", "b@x"]);
        assert_eq!(manifest.to_bytes(), stored);
    }

    /// Mutations of a real manifest never panic, and whatever parses keeps its promises.
    #[test]
    fn mutated_manifests_never_panic() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n.max(1) as u64) as usize
        };
        let segments: Vec<Vec<String>> = (0..40).map(|i| vec![format!("{i}@x"), format!("{i}b@x")]).collect();
        let posted = Posted { size: 3_950, part_size: 100, segments: &segments };
        let original = Manifest {
            nzb_sha1: "ab".repeat(20),
            title: "Film.2024".into(),
            packing: Packing::Rar { format: RarFormat::Rar5, volumes: 2 },
            files: vec![
                OutputFile { path: "Sample/s.mkv".into(), size: 1_000, pieces: posted.pieces(50, 1_000, 0) },
                OutputFile { path: "Film.2024.mkv".into(), size: 2_000, pieces: posted.pieces(1_100, 2_000, 0) },
            ],
        }
        .to_bytes();
        let numbers: [&[u8]; 4] = [b"18446744073709551615", b"18446744073709551616", b"-1", b"0"];
        let mut parsed = 0;
        for _ in 0..3_000 {
            let mut bytes = original.clone();
            for _ in 0..=next(3) {
                if bytes.is_empty() {
                    break;
                }
                let at = next(bytes.len());
                match next(4) {
                    0 => bytes[at] ^= 1 << next(8),
                    1 => bytes.truncate(at),
                    2 => {
                        // Replace a run of digits, if one starts here, with an extreme number.
                        let end = at + bytes[at..].iter().take_while(|b| b.is_ascii_digit()).count();
                        bytes.splice(at..end, numbers[next(numbers.len())].iter().copied());
                    }
                    _ => bytes[at] = b"{}[]\",:0"[next(8)],
                }
            }
            if let Ok(manifest) = Manifest::from_bytes(&bytes) {
                parsed += 1;
                assert_eq!(Manifest::from_bytes(&manifest.to_bytes()), Ok(manifest.clone()));
                for file in &manifest.files {
                    assert!(safe_path(&file.path));
                    assert_eq!(tile(file), Ok(()));
                }
            }
        }
        assert!(parsed > 0, "some mutations, such as digits within an ID, still parse");
    }
}
