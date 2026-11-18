//! PAR2 packets: finding them in a byte stream and checking them.
//!
//! Every packet starts with a 64-byte header: the magic `PAR2\0PKT`, the packet's length
//! (a multiple of 4, header included), the MD5 of everything after the hash, the recovery
//! set's ID, and the packet's type. A damaged packet is skipped by looking for the next magic.

use md5::{Digest, Md5};

pub(crate) const MAGIC: &[u8; 8] = b"PAR2\0PKT";
pub(crate) const HEADER: usize = 64;

pub(crate) const MAIN: &[u8; 16] = b"PAR 2.0\0Main\0\0\0\0";
pub(crate) const FILE_DESC: &[u8; 16] = b"PAR 2.0\0FileDesc";
pub(crate) const IFSC: &[u8; 16] = b"PAR 2.0\0IFSC\0\0\0\0";
pub(crate) const RECOVERY: &[u8; 16] = b"PAR 2.0\0RecvSlic";

/// A packet's header.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Header {
    pub(crate) length: u64,
    pub(crate) hash: [u8; 16],
    pub(crate) set: [u8; 16],
    pub(crate) kind: [u8; 16],
}

impl Header {
    /// The header at the start of `bytes`, if they start with the magic and a length a packet
    /// can have.
    pub(crate) fn read(bytes: &[u8]) -> Option<Self> {
        let header: &[u8; HEADER] = bytes.first_chunk()?;
        let (magic, rest) = header.split_first_chunk::<8>()?;
        let (length, rest) = rest.split_first_chunk::<8>()?;
        let (hash, rest) = rest.split_first_chunk::<16>()?;
        let (set, rest) = rest.split_first_chunk::<16>()?;
        let kind = rest.first_chunk::<16>()?;
        let length = u64::from_le_bytes(*length);
        if magic != MAGIC || length < HEADER as u64 || length % 4 != 0 {
            return None;
        }
        Some(Self { length, hash: *hash, set: *set, kind: *kind })
    }

    /// True when `packet`, the whole packet this header starts, hashes as the header says.
    pub(crate) fn intact(&self, packet: &[u8]) -> bool {
        packet.get(32..).is_some_and(|hashed| Md5::digest(hashed).as_slice() == self.hash)
    }
}

/// The intact packets in a buffer whose type `wanted` accepts, in order. Bytes that are not
/// part of one, and packets that are damaged, are skipped.
pub(crate) struct Packets<'a, F> {
    bytes: &'a [u8],
    at: usize,
    wanted: F,
}

impl<'a, F: Fn(&[u8; 16]) -> bool> Packets<'a, F> {
    pub(crate) fn new(bytes: &'a [u8], wanted: F) -> Self {
        Self { bytes, at: 0, wanted }
    }
}

impl<'a, F: Fn(&[u8; 16]) -> bool> Iterator for Packets<'a, F> {
    /// The header, and the packet's body after it.
    type Item = (Header, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let rest = self.bytes.get(self.at..)?;
            let start = self.at + memchr::memmem::find(rest, MAGIC)?;
            // The magic cannot overlap itself, so the next one starts past this one. A packet
            // that is not wanted is passed this way too: its length is unchecked until its hash
            // is, and a damaged length must not hide the packets after it.
            self.at = start + MAGIC.len();
            let bytes = &self.bytes[start..];
            let Some(header) = Header::read(bytes) else { continue };
            if !(self.wanted)(&header.kind) {
                continue;
            }
            let Some(packet) = usize::try_from(header.length).ok().and_then(|n| bytes.get(..n)) else { continue };
            if header.intact(packet) {
                self.at = start + packet.len();
                return Some((header, &packet[HEADER..]));
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A packet of `kind` in `set` holding `body`, padded to a multiple of 4.
    pub(crate) fn packet(set: [u8; 16], kind: &[u8; 16], body: &[u8]) -> Vec<u8> {
        let mut rest = set.to_vec();
        rest.extend_from_slice(kind);
        rest.extend_from_slice(body);
        rest.resize(rest.len().div_ceil(4) * 4, 0);
        let mut p = MAGIC.to_vec();
        p.extend_from_slice(&(rest.len() as u64 + 32).to_le_bytes());
        p.extend_from_slice(&Md5::digest(&rest));
        p.extend_from_slice(&rest);
        p
    }

    #[test]
    fn finds_intact_packets_among_garbage_and_damage() {
        let a = packet([1; 16], MAIN, b"main");
        let b = packet([1; 16], IFSC, b"ifsc");
        let mut damaged = packet([1; 16], FILE_DESC, b"desc");
        let last = damaged.len() - 1;
        damaged[last] ^= 1;
        let mut lying = packet([1; 16], FILE_DESC, b"desc");
        // A length past the end of the data.
        lying[8..16].copy_from_slice(&u64::MAX.wrapping_sub(3).to_le_bytes());
        let mut bytes = b"junk PAR2\0PKT".to_vec();
        for p in [&a, &damaged, &lying, &b] {
            bytes.extend_from_slice(p);
            bytes.extend_from_slice(b"xx");
        }
        let found: Vec<_> = Packets::new(&bytes, |_| true).map(|(h, body)| (h.kind, body.to_vec())).collect();
        assert_eq!(found, [(*MAIN, b"main".to_vec()), (*IFSC, b"ifsc".to_vec())]);
        let only: Vec<_> = Packets::new(&bytes, |k| k == IFSC).map(|(h, _)| h.kind).collect();
        assert_eq!(only, [*IFSC]);
    }
}
