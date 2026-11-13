//! A resolved release: its output files, and which bytes of which article land where.
//! The engine stores it with each download and reads it back on every resume.

use serde::{Deserialize, Serialize};

pub const FORMAT: u32 = 1;

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    /// SHA-1 of the NZB, in hex. It names the download.
    pub nzb_sha1: String,
    pub title: String,
    pub packing: Packing,
    pub files: Vec<OutputFile>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Packing {
    /// The files were posted as they are.
    Direct,
    /// The files are stored, uncompressed, in RAR volumes.
    Rar { format: RarFormat, volumes: u32 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RarFormat {
    Rar4,
    Rar5,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct OutputFile {
    /// Relative, `/`-separated.
    pub path: String,
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
    pub length: u64,
    /// The article's offset within its posted file, which its yEnc header must match.
    pub begin: u64,
    /// Bytes of the decoded article before the ones that land.
    pub skip: u64,
}

impl Manifest {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("manifests serialize")
    }

    /// Parses and checks a manifest: pieces must tile each file exactly.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let manifest: Self = serde_json::from_slice(bytes).map_err(|e| format!("not a release manifest: {e}"))?;
        if manifest.format != FORMAT {
            return Err(format!("manifest format {} is not supported; resolve the NZB again", manifest.format));
        }
        if manifest.files.is_empty() {
            return Err("the manifest lists no files".into());
        }
        for file in &manifest.files {
            let mut at = 0;
            for piece in &file.pieces {
                if piece.ids.is_empty() || piece.length == 0 || piece.offset != at {
                    return Err(format!("the pieces of {} do not tile it at byte {at}", file.path));
                }
                at += piece.length;
            }
            if at != file.size {
                return Err(format!("the pieces of {} cover {at} of its {} bytes", file.path, file.size));
            }
        }
        Ok(manifest)
    }
}

/// A posted file as articles of `part_size` decoded bytes, the last possibly shorter.
#[derive(Clone, Debug)]
pub struct Posted<'a> {
    pub size: u64,
    pub part_size: u64,
    pub segments: &'a [Vec<String>],
}

impl Posted<'_> {
    /// Pieces for bytes `[from, from + length)` of this posted file, landing at `at` in the output.
    pub fn pieces(&self, from: u64, length: u64, at: u64) -> Vec<Piece> {
        let end = from + length;
        if length == 0 || self.part_size == 0 {
            return Vec::new();
        }
        (from / self.part_size..end.div_ceil(self.part_size))
            .filter_map(|i| {
                let ids = self.segments.get(i as usize)?;
                let begin = i * self.part_size;
                let seg_end = (begin + self.part_size).min(self.size);
                let (lo, hi) = (begin.max(from), seg_end.min(end));
                (lo < hi).then(|| Piece {
                    ids: ids.clone(),
                    offset: at + (lo - from),
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
    fn manifests_must_tile_their_files() {
        let piece = |offset, length| Piece { ids: vec!["a".into()], offset, length, begin: 0, skip: 0 };
        let manifest = |pieces: Vec<Piece>, size| Manifest {
            format: FORMAT,
            nzb_sha1: "00".into(),
            title: "t".into(),
            packing: Packing::Direct,
            files: vec![OutputFile { path: "a.mkv".into(), size, pieces }],
        };
        let good = manifest(vec![piece(0, 5), piece(5, 5)], 10);
        assert_eq!(Manifest::from_bytes(&good.to_bytes()), Ok(good));
        assert!(Manifest::from_bytes(&manifest(vec![piece(0, 5), piece(6, 4)], 10).to_bytes()).is_err());
        assert!(Manifest::from_bytes(&manifest(vec![piece(0, 5)], 10).to_bytes()).is_err());
    }
}
