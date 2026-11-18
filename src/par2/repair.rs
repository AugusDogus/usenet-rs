//! Rebuilding slices: Reed-Solomon decoding over GF(2^16), one slice at a time.
//!
//! Recovery block `e` is `R_e = sum over slices i of c_i^e * D_i`. Moving the slices that
//! are present to the left leaves `R_e + sum over present i of c_i^e * D_i`, which is a sum
//! over the damaged slices alone; with as many independent blocks as damaged slices, that is
//! a square system to invert. So each block becomes an accumulator that every present slice
//! is added into as it arrives, and nothing else is kept.

use super::error::RepairError;
use super::gf;
use super::kernel::Kernel;
use super::{RecoveryBlock, RecoverySet, Slice};

/// Where a slice stands in a repair.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Waiting,
    Fed,
    Damaged,
}

/// What [`Repair::feed`] made of a slice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[must_use]
pub enum Feed {
    /// The slice matched its checksum and was added in.
    Accepted,
    /// The slice did not match its checksum, so it joined the damaged slices and will be
    /// rebuilt too. Its bytes were not used.
    Damaged,
}

/// One rebuilt slice.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Rebuilt {
    /// Where it goes.
    pub slice: Slice,
    /// Its bytes, `slice.length` of them, checked against the slice's checksum.
    pub data: Vec<u8>,
}

/// A repair in progress: recovery blocks, and the slices fed into them so far.
///
/// Make one with the damaged slices and the recovery blocks fetched for them, then
/// [`feed`](Self::feed) it every other slice of the set, in any order, as each becomes
/// available. Once none is [`waiting`](Self::waiting), [`finish`](Self::finish) rebuilds the
/// damaged ones. Rebuilding needs every slice of the set, so a repair can only finish once the
/// rest of the set is present; for a streamed release, that means the whole release.
///
/// It holds one slice's worth of memory per recovery block, and feeding a slice costs one pass
/// over it per block, so give it as many blocks as there are damaged slices, plus a spare or
/// two in case a slice fed later turns out damaged.
#[derive(Debug)]
pub struct Repair {
    set: RecoverySet,
    kernel: Kernel,
    exponents: Vec<u16>,
    rows: Vec<Vec<u8>>,
    states: Vec<State>,
    damaged: Vec<u32>,
    waiting: u32,
}

impl Repair {
    /// Starts rebuilding `damaged`, set-wide slice indices, from `blocks`.
    ///
    /// Fails when there are fewer distinct blocks than damaged slices, when the blocks cannot
    /// rebuild these slices together, or when a block is not one slice long.
    pub fn new(set: &RecoverySet, damaged: &[u32], mut blocks: Vec<RecoveryBlock>) -> Result<Self, RepairError> {
        let slices = set.slice_count();
        let mut states = vec![State::Waiting; slices as usize];
        for &index in damaged {
            let state = states.get_mut(index as usize).ok_or(RepairError::NoSuchSlice { index, slices })?;
            if *state == State::Damaged {
                return Err(RepairError::Repeated { index });
            }
            *state = State::Damaged;
        }
        if let Some(b) = blocks.iter().find(|b| b.data.len() as u64 != set.slice_size) {
            return Err(RepairError::WrongBlock {
                exponent: b.exponent,
                length: b.data.len(),
                slice_size: set.slice_size,
            });
        }
        blocks.sort_by_key(|b| b.exponent);
        blocks.dedup_by_key(|b| b.exponent);
        let (exponents, rows) = blocks.into_iter().map(|b| (b.exponent, b.data)).unzip();
        let repair = Self {
            set: set.clone(),
            kernel: Kernel::best(),
            exponents,
            rows,
            states,
            damaged: damaged.to_vec(),
            waiting: slices - damaged.len() as u32,
        };
        repair.solvable()?;
        Ok(repair)
    }

    /// Slices not damaged and not fed yet, in order.
    pub fn waiting(&self) -> impl Iterator<Item = u32> + '_ {
        self.states.iter().enumerate().filter(|(_, s)| **s == State::Waiting).map(|(i, _)| i as u32)
    }

    /// How many slices are still to feed.
    pub fn waiting_count(&self) -> u32 {
        self.waiting
    }

    /// True when slice `index` is still to feed.
    pub fn is_waiting(&self, index: u32) -> bool {
        self.states.get(index as usize) == Some(&State::Waiting)
    }

    /// The slices to rebuild, in the order they were named damaged.
    pub fn damaged(&self) -> &[u32] {
        &self.damaged
    }

    /// The distinct recovery blocks held: at least [`damaged`](Self::damaged)`.len()`.
    pub fn blocks(&self) -> usize {
        self.rows.len()
    }

    /// Adds slice `index`, as read from the file, into every block. A slice whose length or
    /// CRC32 does not match is named damaged instead. Only the CRC32 is checked here, since
    /// an MD5 would cost more than the rest of the repair; [`finish`](Self::finish) checks
    /// the rebuilt slices against both, which catches a wrong slice the CRC32 let through.
    ///
    /// Fails, changing nothing, when the slice is not waiting, or when it is damaged and the
    /// blocks cannot rebuild it along with the others.
    pub fn feed(&mut self, index: u32, data: &[u8]) -> Result<Feed, RepairError> {
        if !self.is_waiting(index) {
            return Err(self.not_waiting(index));
        }
        if !self.set.crc_matches(index, data) {
            self.lose(index)?;
            return Ok(Feed::Damaged);
        }
        for (row, &e) in self.rows.iter_mut().zip(&self.exponents) {
            let c = gf::coefficient(index as usize, e).unwrap_or(0);
            self.kernel.mul_add(row, data, c);
        }
        self.states[index as usize] = State::Fed;
        self.waiting -= 1;
        Ok(Feed::Accepted)
    }

    /// Names slice `index` damaged without feeding it: one that will never arrive, such as
    /// one in an article no server has.
    ///
    /// Fails, changing nothing, when the slice is not waiting, or when the blocks cannot
    /// rebuild it along with the others.
    pub fn lose(&mut self, index: u32) -> Result<(), RepairError> {
        if !self.is_waiting(index) {
            return Err(self.not_waiting(index));
        }
        self.damaged.push(index);
        if let Err(e) = self.solvable() {
            self.damaged.pop();
            return Err(e);
        }
        self.states[index as usize] = State::Damaged;
        self.waiting -= 1;
        Ok(())
    }

    /// Rebuilds the damaged slices, each checked against its checksum, in the order they
    /// were named damaged.
    ///
    /// Fails while slices are still [`waiting`](Self::waiting).
    pub fn finish(&self) -> Result<Vec<Rebuilt>, RepairError> {
        if let Some(first) = self.waiting().next() {
            return Err(RepairError::Unfinished { waiting: self.waiting, first });
        }
        let (chosen, inverse) = self.solvable()?;
        let slice_size = self.set.slice_size as usize;
        let mut out = Vec::with_capacity(self.damaged.len());
        for (j, &index) in self.damaged.iter().enumerate() {
            let slice =
                self.set.slice(index).ok_or(RepairError::NoSuchSlice { index, slices: self.set.slice_count() })?;
            let mut data = vec![0u8; slice_size];
            for (k, &row) in chosen.iter().enumerate() {
                self.kernel.mul_add(&mut data, &self.rows[row], inverse.get(j, k));
            }
            data.truncate(slice.length as usize);
            if !self.set.check(index, &data) {
                return Err(RepairError::Unverified { index });
            }
            out.push(Rebuilt { slice, data });
        }
        Ok(out)
    }

    fn not_waiting(&self, index: u32) -> RepairError {
        match self.states.get(index as usize) {
            None => RepairError::NoSuchSlice { index, slices: self.set.slice_count() },
            Some(_) => RepairError::Repeated { index },
        }
    }

    /// The blocks that rebuild the damaged slices, and the inverse of their coefficients.
    fn solvable(&self) -> Result<(Vec<usize>, gf::Matrix), RepairError> {
        let damaged = self.damaged.len();
        if self.rows.len() < damaged {
            return Err(RepairError::NotEnoughRecovery { damaged, blocks: self.rows.len() });
        }
        let candidates: Vec<Vec<u16>> = self
            .exponents
            .iter()
            .map(|&e| self.damaged.iter().map(|&i| gf::coefficient(i as usize, e).unwrap_or(0)).collect())
            .collect();
        gf::independent_rows(&candidates, damaged).ok_or(RepairError::Dependent { damaged })
    }
}
