//! Which of an NZB's files hold a recovery set, and which of them to fetch.

use crate::nzb::Nzb;

/// A PAR2 volume posted in an NZB.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Volume {
    /// The file's position in [`Nzb::files`].
    pub file: usize,
    /// The first recovery block's exponent, from the name.
    pub first: u32,
    /// How many recovery blocks the volume holds, from the name.
    pub blocks: u32,
}

/// The PAR2 files of one recovery set in an NZB, recognized by name: `name.par2` for the
/// index and `name.volFIRST+COUNT.par2` for the volumes, as par2cmdline, MultiPar and
/// posters name them.
///
/// A release posted under obfuscated names has no `.par2` names to find; recognize its PAR2
/// files by content instead, by [`RecoverySet::parse`](super::RecoverySet::parse) on their
/// first article.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Par2Files {
    /// The name the files share, before `.par2` or `.volFIRST+COUNT.par2`.
    pub base: String,
    /// The index's position in [`Nzb::files`], if it was posted.
    pub index: Option<usize>,
    /// The volumes, by first exponent.
    pub volumes: Vec<Volume>,
}

impl Par2Files {
    /// The PAR2 sets an NZB posts, largest first, which is usually the release's own set
    /// rather than a sample's.
    pub fn find(nzb: &Nzb) -> Vec<Par2Files> {
        let mut sets: Vec<Par2Files> = Vec::new();
        for (i, file) in nzb.files.iter().enumerate() {
            let Some((base, volume)) = parse_name(&file.filename) else { continue };
            let k = match sets.iter().position(|s| s.base.eq_ignore_ascii_case(base)) {
                Some(k) => k,
                None => {
                    sets.push(Par2Files { base: base.to_string(), index: None, volumes: Vec::new() });
                    sets.len() - 1
                }
            };
            let set = &mut sets[k];
            match volume {
                Some((first, blocks)) => set.volumes.push(Volume { file: i, first, blocks }),
                None => {
                    set.index.get_or_insert(i);
                }
            }
        }
        for set in &mut sets {
            set.volumes.sort_by_key(|v| (v.first, v.file));
        }
        sets.sort_by_key(|s| std::cmp::Reverse(s.blocks()));
        sets
    }

    /// Recovery blocks across all volumes.
    pub fn blocks(&self) -> u64 {
        self.volumes.iter().map(|v| u64::from(v.blocks)).sum()
    }

    /// Volumes holding at least `needed` blocks between them, with few blocks to spare: the
    /// smallest volume that holds them all if there is one, else the largest volumes and then
    /// the smallest that covers the rest. Their positions in [`Nzb::files`]; None when all
    /// the volumes together hold too few.
    pub fn choose(&self, needed: u32) -> Option<Vec<usize>> {
        if self.blocks() < u64::from(needed) {
            return None;
        }
        let mut left: Vec<&Volume> = self.volumes.iter().filter(|v| v.blocks > 0).collect();
        left.sort_by_key(|v| v.blocks);
        let mut chosen = Vec::new();
        let mut needed = u64::from(needed);
        while needed > 0 {
            let k = match left.iter().position(|v| u64::from(v.blocks) >= needed) {
                Some(k) => k,
                None => left.len().checked_sub(1)?,
            };
            let v = left.remove(k);
            needed = needed.saturating_sub(u64::from(v.blocks));
            chosen.push(v.file);
        }
        Some(chosen)
    }
}

/// `name.par2` as `(name, None)`, and `name.volA+B.par2` as `(name, Some((A, B)))`.
fn parse_name(filename: &str) -> Option<(&str, Option<(u32, u32)>)> {
    let cut = filename.len().checked_sub(5)?;
    if !filename.is_char_boundary(cut) || !filename[cut..].eq_ignore_ascii_case(".par2") {
        return None;
    }
    let stem = &filename[..cut];
    let volume = stem.rfind('.').and_then(|dot| {
        let tag = &stem[dot + 1..];
        let numbers = tag.get(..3).filter(|v| v.eq_ignore_ascii_case("vol")).map(|_| &tag[3..])?;
        let (first, count) = numbers.split_once(['+', '-'])?;
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !digits(first) || !digits(count) {
            return None;
        }
        Some((dot, first.parse().ok()?, count.parse().ok()?))
    });
    match volume {
        Some((dot, first, count)) => Some((&stem[..dot], Some((first, count)))),
        None => Some((stem, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_read_as_par2cmdline_writes_them() {
        assert_eq!(parse_name("movie.par2"), Some(("movie", None)));
        assert_eq!(parse_name("Movie.2024.vol07+08.PAR2"), Some(("Movie.2024", Some((7, 8)))));
        assert_eq!(parse_name("movie.vol000-001.par2"), Some(("movie", Some((0, 1)))));
        assert_eq!(parse_name("movie.volx+1.par2"), Some(("movie.volx+1", None)));
        assert_eq!(parse_name("movie.rar"), None);
        assert_eq!(parse_name("ü.par2"), Some(("ü", None)));
        assert_eq!(parse_name("par2"), None);
    }

    fn files(volumes: &[u32]) -> Par2Files {
        let volumes = volumes.iter().enumerate().map(|(i, &blocks)| Volume { file: i, first: 0, blocks }).collect();
        Par2Files { base: "x".into(), index: None, volumes }
    }

    #[test]
    fn choose_takes_few_spare_blocks() {
        let set = files(&[1, 2, 4, 8, 16]);
        assert_eq!(set.choose(1), Some(vec![0]));
        assert_eq!(set.choose(3), Some(vec![2]));
        assert_eq!(set.choose(20), Some(vec![4, 2]));
        assert_eq!(set.choose(31), Some(vec![4, 3, 2, 1, 0]));
        assert_eq!(set.choose(32), None);
        assert_eq!(set.choose(0), Some(vec![]));
    }
}
