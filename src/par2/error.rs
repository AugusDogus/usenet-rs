//! What can go wrong reading a recovery set or repairing with it, and what to do about it.

use std::fmt;
use std::io;

/// Why bytes could not be read as a PAR2 recovery set.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Par2Error {
    /// No intact PAR2 packet was found: the bytes are not PAR2, or every packet is damaged.
    NotPar2,
    /// Intact packets were found, but no main packet, which names the set's files and slice
    /// size. Every PAR2 file of a set carries one, so the bytes are a fragment or damaged.
    NoMain,
    /// The main packet, or a packet it relies on, holds something PAR2 does not allow.
    Malformed(Defect),
    /// A file the main packet lists has no intact description or slice checksums.
    Incomplete {
        /// The file's ID, from the main packet.
        file_id: [u8; 16],
        /// What is missing.
        lacking: Lacking,
    },
}

/// What makes a recovery set malformed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Defect {
    /// The slice size is 0, not a multiple of 4, or above [`MAX_SLICE`](super::MAX_SLICE).
    SliceSize {
        /// The slice size the main packet gives.
        size: u64,
    },
    /// The main packet's list of file IDs does not fit its body, or lists a file twice.
    FileList,
    /// The files hold more slices than a recovery set can: 32768.
    TooManySlices {
        /// How many the files' sizes call for.
        slices: u64,
    },
    /// A file's checksum packet lists a different number of slices than its size calls for.
    ChecksumCount {
        /// The file's ID.
        file_id: [u8; 16],
        /// Slices the file's size calls for.
        expected: u64,
        /// Checksums the packet holds.
        found: u64,
    },
}

/// The part of a file's record a recovery set lacks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lacking {
    /// The file description packet: its name and size.
    Description,
    /// The input file slice checksum packet.
    Checksums,
}

impl fmt::Display for Par2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPar2 => write!(
                f,
                "no intact PAR2 packet was found; the data is not a PAR2 file, or it is damaged, so fetch \
                 another PAR2 file of the release"
            ),
            Self::NoMain => write!(
                f,
                "the PAR2 data holds no intact main packet, so its files and slice size are unknown; fetch \
                 the release's .par2 index or another of its volumes"
            ),
            Self::Malformed(defect) => write!(f, "the PAR2 recovery set is malformed: {defect}"),
            Self::Incomplete { file_id, lacking } => {
                let what = match lacking {
                    Lacking::Description => "description",
                    Lacking::Checksums => "slice checksums",
                };
                write!(
                    f,
                    "the PAR2 data lacks the {what} of file {}; another PAR2 file of the release carries a copy",
                    hex(file_id)
                )
            }
        }
    }
}

impl fmt::Display for Defect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SliceSize { size } => {
                write!(f, "its slice size {size} is not a positive multiple of 4 of at most {} bytes", super::MAX_SLICE)
            }
            Self::FileList => write!(f, "its main packet's list of files is cut short or names a file twice"),
            Self::TooManySlices { slices } => {
                write!(f, "its files make {slices} slices, more than the 32768 a recovery set can hold")
            }
            Self::ChecksumCount { file_id, expected, found } => write!(
                f,
                "file {} needs {expected} slice checksums but its checksum packet holds {found}",
                hex(file_id)
            ),
        }
    }
}

impl std::error::Error for Par2Error {}

/// Which side of a repair's I/O failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Operation {
    /// Reading a slice that is present.
    Read,
    /// Writing a rebuilt slice.
    Write,
}

/// Why a repair could not finish.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RepairError {
    /// More slices are damaged than recovery blocks were given. Fetch more recovery blocks;
    /// when the release has no more, it cannot be repaired.
    NotEnoughRecovery {
        /// Slices damaged or missing.
        damaged: usize,
        /// Distinct recovery blocks given.
        blocks: usize,
    },
    /// The recovery blocks given are enough in number but not independent for these slices,
    /// which PAR2's construction allows in rare cases. Fetch one more recovery block.
    Dependent {
        /// Slices damaged or missing.
        damaged: usize,
    },
    /// A slice index past the end of the set.
    NoSuchSlice {
        /// The index given.
        index: u32,
        /// Slices in the set.
        slices: u32,
    },
    /// A slice was given twice, or given after being named damaged.
    Repeated {
        /// The slice's index.
        index: u32,
    },
    /// A recovery block is not one slice long.
    WrongBlock {
        /// The block's exponent.
        exponent: u16,
        /// The block's length.
        length: usize,
        /// The set's slice size.
        slice_size: u64,
    },
    /// Some slices that are not damaged have not been given yet. Repair needs every slice of
    /// the set, so it can finish only once the rest of the set is present.
    Unfinished {
        /// Slices still to give.
        waiting: u32,
        /// The first of them.
        first: u32,
    },
    /// A rebuilt slice does not match its checksum: a slice given as intact, or a recovery
    /// block, was wrong even though its checksum matched. Nothing was written.
    Unverified {
        /// The slice's index.
        index: u32,
    },
    /// Reading or writing a slice failed.
    Io {
        /// The slice's index.
        index: u32,
        /// Whether it was being read or written.
        operation: Operation,
        /// The kind of error.
        kind: io::ErrorKind,
        /// The error's message.
        message: String,
    },
}

impl RepairError {
    pub(crate) fn io(index: u32, operation: Operation, e: &io::Error) -> Self {
        Self::Io { index, operation, kind: e.kind(), message: e.to_string() }
    }
}

impl fmt::Display for RepairError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotEnoughRecovery { damaged, blocks } => write!(
                f,
                "{damaged} slices need repair but only {blocks} recovery blocks were given; fetch more PAR2 \
                 volumes, or use another release if this one has no more"
            ),
            Self::Dependent { damaged } => write!(
                f,
                "the recovery blocks given cannot rebuild these {damaged} slices together; fetch one more \
                 recovery block"
            ),
            Self::NoSuchSlice { index, slices } => {
                write!(f, "slice {index} is past the end of the recovery set, which has {slices}")
            }
            Self::Repeated { index } => write!(f, "slice {index} was given twice, or given after it was named damaged"),
            Self::WrongBlock { exponent, length, slice_size } => write!(
                f,
                "recovery block {exponent} holds {length} bytes, not the set's slice size of {slice_size}; it \
                 belongs to another recovery set"
            ),
            Self::Unfinished { waiting, first } => write!(
                f,
                "{waiting} slices of the recovery set have not been given yet, starting with slice {first}; \
                 repair needs every slice of the set, so download the rest first"
            ),
            Self::Unverified { index } => write!(
                f,
                "rebuilt slice {index} does not match its checksum, so a slice or recovery block given was \
                 wrong; nothing was written"
            ),
            Self::Io { index, operation, message, .. } => {
                let verb = match operation {
                    Operation::Read => "reading",
                    Operation::Write => "writing",
                };
                write!(f, "{verb} slice {index} failed: {message}")
            }
        }
    }
}

impl std::error::Error for RepairError {}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
