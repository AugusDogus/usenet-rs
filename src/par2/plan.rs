//! Which of an NZB's files hold a recovery set, and which of them to fetch.

use crate::manifest::{Manifest, Par2Posted};
use crate::nzb::Nzb;

/// Bytes a recovery packet adds to its slice: the header and the exponent.
const PACKET: u64 = 64 + 4;

/// A PAR2 volume posted in an NZB.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Volume {
    /// The file's position in [`Nzb::files`].
    pub file: usize,
    /// How many recovery blocks the volume holds, when its name says: `name.vol7+8.par2`
    /// holds 8. Posters that name volumes `name.vol-07.par2` do not say.
    pub blocks: Option<u32>,
    /// The file's posted size, the sum of its segments' sizes as the NZB gives them.
    pub bytes: u64,
}

impl Volume {
    /// The blocks the volume holds, by its name, or else estimated from its posted size:
    /// yEnc adds 1 to 3% to what it encodes, and a volume repeats the index's packets, so
    /// this is an estimate on the low side.
    pub fn estimated_blocks(&self, slice_size: u64) -> u64 {
        match self.blocks {
            Some(n) => u64::from(n),
            None => (self.bytes * 100 / 103) / (slice_size + PACKET),
        }
    }
}

/// The PAR2 files of one recovery set in an NZB, recognized by name: `name.par2` for the
/// index, and `name.vol7+8.par2` or `name.vol-07.par2` for volumes, as par2cmdline, MultiPar
/// and posters name them.
///
/// A release posted under obfuscated names has no `.par2` names to find; the resolver
/// recognizes its PAR2 files by their first article instead, which
/// [`Par2Files::find_resolved`] reads.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Par2Files {
    /// The name the files share, before `.par2` or `.vol...par2`.
    pub base: String,
    /// The file to read the set's description from, a position in [`Nzb::files`]: the
    /// index if it was posted, or else the smallest volume, since every volume carries the
    /// description too.
    pub index: usize,
    /// The volumes, in NZB order.
    pub volumes: Vec<Volume>,
}

impl Par2Files {
    /// The PAR2 sets an NZB posts, largest first, which is usually the release's own set
    /// rather than a sample's.
    pub fn find(nzb: &Nzb) -> Vec<Par2Files> {
        let mut sets: Vec<(String, Option<usize>, Vec<Volume>)> = Vec::new();
        for (i, file) in nzb.files.iter().enumerate() {
            let Some((base, volume)) = parse_name(&file.filename) else { continue };
            let k = match sets.iter().position(|s| s.0.eq_ignore_ascii_case(base)) {
                Some(k) => k,
                None => {
                    sets.push((base.to_string(), None, Vec::new()));
                    sets.len() - 1
                }
            };
            let bytes = file.segments.iter().map(|s| s.bytes).sum();
            match volume {
                Some(blocks) => sets[k].2.push(Volume { file: i, blocks, bytes }),
                None => {
                    sets[k].1.get_or_insert(i);
                }
            }
        }
        let mut found: Vec<Par2Files> = sets
            .into_iter()
            .filter_map(|(base, index, volumes)| {
                let index = index.or_else(|| volumes.iter().min_by_key(|v| v.bytes).map(|v| v.file))?;
                Some(Par2Files { base, index, volumes })
            })
            .collect();
        found.sort_by_key(|s| std::cmp::Reverse(s.volumes.iter().map(|v| v.bytes).sum::<u64>()));
        found
    }

    /// The PAR2 sets an NZB posts, by name as [`Par2Files::find`] finds them, and those the
    /// resolver recognized by content ([`Manifest::par2`]), grouped by recovery set, largest
    /// first. A set found by content has no names to say which file is the index or how many
    /// blocks a volume holds, so each of its files is a volume whose blocks are estimated from
    /// its size, and the smallest is read for the set's description.
    pub fn find_resolved(nzb: &Nzb, manifest: &Manifest) -> Vec<Par2Files> {
        let mut found = Self::find(nzb);
        let mut sets: Vec<([u8; 16], Vec<Volume>)> = Vec::new();
        for &Par2Posted { file, set } in &manifest.par2 {
            let Some(posted) = nzb.files.get(file) else { continue };
            let volume = Volume { file, blocks: None, bytes: posted.segments.iter().map(|s| s.bytes).sum() };
            match sets.iter_mut().find(|(id, _)| *id == set) {
                Some((_, volumes)) => volumes.push(volume),
                None => sets.push((set, vec![volume])),
            }
        }
        for (_, volumes) in sets {
            let Some(index) = volumes.iter().min_by_key(|v| v.bytes).map(|v| v.file) else { continue };
            found.push(Par2Files { base: nzb.files[index].filename.clone(), index, volumes });
        }
        found.sort_by_key(|s| std::cmp::Reverse(s.volumes.iter().map(|v| v.bytes).sum::<u64>()));
        found
    }

    /// Every file of the set, the index and the volumes, as positions in [`Nzb::files`].
    pub fn files(&self) -> impl Iterator<Item = usize> + '_ {
        std::iter::once(self.index).chain(self.volumes.iter().map(|v| v.file).filter(move |&f| f != self.index))
    }

    /// Recovery blocks across all volumes, by name or estimate; see
    /// [`Volume::estimated_blocks`].
    pub fn blocks(&self, slice_size: u64) -> u64 {
        self.volumes.iter().map(|v| v.estimated_blocks(slice_size)).sum()
    }

    /// The volumes to fetch for `needed` blocks, as positions in [`Nzb::files`], in the order
    /// to fetch them. The first ones should hold the blocks with few to spare: the smallest
    /// volume that holds them all, or else the largest and then the smallest that covers the
    /// rest. Fetch them in order through a [`Scanner`](super::Scanner) and stop once it holds
    /// enough. The rest follow, smallest first, in case a volume holds fewer blocks than
    /// estimated or is damaged.
    pub fn choose(&self, needed: u32, slice_size: u64) -> Vec<usize> {
        let mut left: Vec<(u64, usize)> =
            self.volumes.iter().map(|v| (v.estimated_blocks(slice_size), v.file)).collect();
        left.sort();
        let mut order = Vec::new();
        let mut needed = u64::from(needed);
        while needed > 0 && !left.is_empty() {
            let k = left.iter().position(|&(blocks, _)| blocks >= needed).unwrap_or(left.len() - 1);
            let (blocks, file) = left.remove(k);
            needed = needed.saturating_sub(blocks);
            order.push(file);
        }
        order.extend(left.into_iter().map(|(_, file)| file));
        order
    }
}

/// `name.par2` as `(name, None)`; `name.volA+B.par2` as `(name, Some(Some(B)))`; and
/// `name.vol-A.par2` or `name.volA.par2` as `(name, Some(None))`.
fn parse_name(filename: &str) -> Option<(&str, Option<Option<u32>>)> {
    let cut = filename.len().checked_sub(5)?;
    if !filename.is_char_boundary(cut) || !filename[cut..].eq_ignore_ascii_case(".par2") {
        return None;
    }
    let stem = &filename[..cut];
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let volume = stem.rfind('.').and_then(|dot| {
        let tag = &stem[dot + 1..];
        let numbers = tag.get(..3).filter(|v| v.eq_ignore_ascii_case("vol")).map(|_| &tag[3..])?;
        let count = match numbers.split_once('+') {
            Some((first, count)) if digits(first) && digits(count) => Some(count.parse().ok()?),
            Some(_) => return None,
            None if digits(numbers.strip_prefix('-').unwrap_or(numbers)) => None,
            None => return None,
        };
        Some((dot, count))
    });
    match volume {
        Some((dot, count)) => Some((&stem[..dot], Some(count))),
        None => Some((stem, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_read_as_posters_write_them() {
        assert_eq!(parse_name("movie.par2"), Some(("movie", None)));
        assert_eq!(parse_name("Movie.2024.vol07+08.PAR2"), Some(("Movie.2024", Some(Some(8)))));
        assert_eq!(parse_name("movie.vol-010.par2"), Some(("movie", Some(None))));
        assert_eq!(parse_name("movie.vol3.par2"), Some(("movie", Some(None))));
        assert_eq!(parse_name("movie.volx+1.par2"), Some(("movie.volx+1", None)));
        assert_eq!(parse_name("movie.vol-.par2"), Some(("movie.vol-", None)));
        assert_eq!(parse_name("movie.rar"), None);
        assert_eq!(parse_name("ü.par2"), Some(("ü", None)));
        assert_eq!(parse_name("par2"), None);
    }

    fn files(volumes: &[u32]) -> Par2Files {
        let volumes =
            volumes.iter().enumerate().map(|(i, &blocks)| Volume { file: i, blocks: Some(blocks), bytes: 0 }).collect();
        Par2Files { base: "x".into(), index: 0, volumes }
    }

    #[test]
    fn choose_takes_few_spare_blocks_first() {
        let set = files(&[1, 2, 4, 8, 16]);
        assert_eq!(set.choose(1, 4), [0, 1, 2, 3, 4]);
        assert_eq!(set.choose(3, 4), [2, 0, 1, 3, 4]);
        assert_eq!(set.choose(20, 4), [4, 2, 0, 1, 3]);
        assert_eq!(set.choose(31, 4), [4, 3, 2, 1, 0]);
        assert_eq!(set.choose(0, 4), [0, 1, 2, 3, 4]);
        assert_eq!(set.blocks(4), 31);
    }

    #[test]
    fn unnamed_volumes_are_estimated_from_their_size() {
        let v = Volume { file: 0, blocks: None, bytes: 1_200_000 };
        // About 1,165,000 bytes of packets of 100,068 bytes.
        assert_eq!(v.estimated_blocks(100_000), 11);
    }
}
