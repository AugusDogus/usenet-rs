//! Writing recovery sets after the PAR2 2.0 spec, for tests: the index's packets, and one
//! recovery packet per exponent.

use std::ops::Range;

use md5::{Digest, Md5};

use super::gf;
use super::kernel::Kernel;
use super::packet::{FILE_DESC, HEADER, IFSC, MAGIC, MAIN, RECOVERY};

/// A recovery set as written.
pub(crate) struct Written {
    /// The main, file description and slice checksum packets.
    pub(crate) index: Vec<u8>,
    /// One recovery packet per exponent, in order.
    pub(crate) recovery: Vec<Vec<u8>>,
    /// Positions in the files given, in the set's order.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) order: Vec<usize>,
}

/// A packet of `kind` in `set` holding `body`, padded to a multiple of 4.
pub(crate) fn packet(set: [u8; 16], kind: &[u8; 16], body: &[u8]) -> Vec<u8> {
    let mut rest = set.to_vec();
    rest.extend_from_slice(kind);
    rest.extend_from_slice(body);
    rest.resize(rest.len().div_ceil(4) * 4, 0);
    let mut p = MAGIC.to_vec();
    p.extend_from_slice(&(rest.len() as u64 + (HEADER as u64 - 32)).to_le_bytes());
    p.extend_from_slice(&Md5::digest(&rest));
    p.extend_from_slice(&rest);
    p
}

/// Protects `files` with slices of `slice_size` bytes, a positive multiple of 4, and a
/// recovery block for each exponent.
pub(crate) fn write(files: &[(&str, &[u8])], slice_size: usize, exponents: Range<u16>) -> Written {
    let md5 = |b: &[u8]| -> [u8; 16] { Md5::digest(b).into() };
    let head = |d: &[u8]| md5(&d[..d.len().min(16 * 1024)]);
    // A file's ID hashes its first 16 KiB's MD5, its size and its name; files go in ID order.
    let mut order: Vec<(usize, [u8; 16])> = files
        .iter()
        .enumerate()
        .map(|(i, (name, data))| {
            let mut key = head(data).to_vec();
            key.extend_from_slice(&(data.len() as u64).to_le_bytes());
            key.extend_from_slice(name.as_bytes());
            (i, md5(&key))
        })
        .collect();
    order.sort_by_key(|(_, id)| *id);
    let mut main = (slice_size as u64).to_le_bytes().to_vec();
    main.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for (_, id) in &order {
        main.extend_from_slice(id);
    }
    let set = md5(&main);
    let mut index = packet(set, MAIN, &main);
    let mut slices = Vec::new();
    for &(i, id) in &order {
        let (name, data) = files[i];
        let mut desc = id.to_vec();
        desc.extend_from_slice(&md5(data));
        desc.extend_from_slice(&head(data));
        desc.extend_from_slice(&(data.len() as u64).to_le_bytes());
        desc.extend_from_slice(name.as_bytes());
        index.extend(packet(set, FILE_DESC, &desc));
        let mut ifsc = id.to_vec();
        for chunk in data.chunks(slice_size) {
            let mut padded = chunk.to_vec();
            padded.resize(slice_size, 0);
            ifsc.extend_from_slice(&md5(&padded));
            ifsc.extend_from_slice(&crc32fast::hash(&padded).to_le_bytes());
            slices.push(chunk);
        }
        index.extend(packet(set, IFSC, &ifsc));
    }
    let kernel = Kernel::best();
    let recovery = exponents
        .map(|e| {
            let mut body = vec![0u8; 4 + slice_size];
            body[..4].copy_from_slice(&u32::from(e).to_le_bytes());
            for (i, s) in slices.iter().enumerate() {
                kernel.mul_add(&mut body[4..], s, gf::coefficient(i, e).unwrap_or(0));
            }
            packet(set, RECOVERY, &body)
        })
        .collect();
    Written { index, recovery, order: order.into_iter().map(|(i, _)| i).collect() }
}
