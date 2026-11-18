//! `dst ^= c * src` over buffers of 16-bit little-endian words, the one operation repair
//! spends its time in, with SSSE3, AVX2 or NEON.
//!
//! A word's product splits by its four nibbles: `c * w` is the XOR of `c * (nibble << 4k)`
//! for `k` in 0..4, and each of those is a 16-entry table, looked up 16 or 32 bytes at a time
//! with a byte shuffle. The low and high bytes of each product come from separate tables, so
//! the words are split into their low and high bytes first and woven back after.

use super::gf;

/// The products of one constant with every nibble in every position of a word.
pub(crate) struct Tables {
    /// `words[k][v] = c * (v << 4k)`.
    words: [[u16; 16]; 4],
    /// The low bytes of `words`.
    lo: [[u8; 16]; 4],
    /// The high bytes of `words`.
    hi: [[u8; 16]; 4],
}

impl Tables {
    pub(crate) fn new(c: u16) -> Self {
        let mut t = Self { words: [[0; 16]; 4], lo: [[0; 16]; 4], hi: [[0; 16]; 4] };
        for k in 0..4 {
            for v in 0..16u16 {
                let p = gf::mul(c, v << (4 * k));
                t.words[k][usize::from(v)] = p;
                t.lo[k][usize::from(v)] = p as u8;
                t.hi[k][usize::from(v)] = (p >> 8) as u8;
            }
        }
        t
    }

    fn word(&self, w: u16) -> u16 {
        let n = |k: u32| usize::from((w >> (4 * k)) & 15);
        self.words[0][n(0)] ^ self.words[1][n(1)] ^ self.words[2][n(2)] ^ self.words[3][n(3)]
    }
}

/// One way to run the loop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kernel {
    Scalar,
    #[cfg(target_arch = "x86_64")]
    Ssse3,
    #[cfg(target_arch = "x86_64")]
    Avx2,
    #[cfg(target_arch = "aarch64")]
    Neon,
}

impl Kernel {
    /// The fastest kernel this CPU runs.
    pub(crate) fn best() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            if std::arch::is_x86_feature_detected!("avx2") {
                return Self::Avx2;
            }
            if std::arch::is_x86_feature_detected!("ssse3") {
                return Self::Ssse3;
            }
        }
        #[cfg(target_arch = "aarch64")]
        return Self::Neon;
        #[allow(unreachable_code)]
        Self::Scalar
    }

    /// Every kernel this CPU runs, for tests.
    #[cfg(test)]
    pub(crate) fn available() -> Vec<Self> {
        let mut out = vec![Self::Scalar];
        #[cfg(target_arch = "x86_64")]
        {
            if std::arch::is_x86_feature_detected!("ssse3") {
                out.push(Self::Ssse3);
            }
            if std::arch::is_x86_feature_detected!("avx2") {
                out.push(Self::Avx2);
            }
        }
        #[cfg(target_arch = "aarch64")]
        out.push(Self::Neon);
        out
    }

    /// `dst ^= c * src`, word by word, over `src` as if it were zero-padded to `dst`: only
    /// the first `src.len()` bytes of `dst` change, and one more when `src` ends halfway
    /// through a word. Bytes of `src` past `dst` are ignored.
    pub(crate) fn mul_add(self, dst: &mut [u8], src: &[u8], c: u16) {
        let len = src.len().min(dst.len());
        if c == 0 || len == 0 {
            return;
        }
        let t = Tables::new(c);
        let done = match self {
            Self::Scalar => 0,
            // SAFETY: these kernels are only chosen after detecting the feature.
            #[cfg(target_arch = "x86_64")]
            Self::Ssse3 => unsafe { x86::ssse3(&mut dst[..len], &src[..len], &t) },
            #[cfg(target_arch = "x86_64")]
            Self::Avx2 => unsafe { x86::avx2(&mut dst[..len], &src[..len], &t) },
            #[cfg(target_arch = "aarch64")]
            Self::Neon => neon::run(&mut dst[..len], &src[..len], &t),
        };
        let even = len & !1;
        let (words, _) = dst[done..even].as_chunks_mut::<2>();
        for (d, s) in words.iter_mut().zip(src[done..even].as_chunks::<2>().0) {
            *d = (t.word(u16::from_le_bytes(*s)) ^ u16::from_le_bytes(*d)).to_le_bytes();
        }
        if even < len {
            // The last byte is the low half of a word whose high half is padding.
            let p = t.word(u16::from(src[even]));
            dst[even] ^= p as u8;
            if let Some(d) = dst.get_mut(even + 1) {
                *d ^= (p >> 8) as u8;
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::{
        __m128i, __m256i, _mm_and_si128, _mm_loadu_si128, _mm_packus_epi16, _mm_set1_epi8, _mm_set1_epi16,
        _mm_shuffle_epi8, _mm_srli_epi16, _mm_storeu_si128, _mm_unpackhi_epi8, _mm_unpacklo_epi8, _mm_xor_si128,
        _mm256_and_si256, _mm256_broadcastsi128_si256, _mm256_loadu_si256, _mm256_packus_epi16, _mm256_set1_epi8,
        _mm256_set1_epi16, _mm256_shuffle_epi8, _mm256_srli_epi16, _mm256_storeu_si256, _mm256_unpackhi_epi8,
        _mm256_unpacklo_epi8, _mm256_xor_si256,
    };

    use super::Tables;

    /// Runs over whole 32-byte blocks of `src` and returns how many bytes it did.
    ///
    /// # Safety
    ///
    /// The CPU must support SSSE3, and `dst` must be as long as `src`.
    #[target_feature(enable = "ssse3")]
    pub(super) unsafe fn ssse3(dst: &mut [u8], src: &[u8], t: &Tables) -> usize {
        let n = src.len().min(dst.len()) / 32 * 32;
        // SAFETY: every access is 16 bytes at `i` or `i + 16` with `i + 32 <= n`, inside both
        // buffers, and the tables are 16 bytes each.
        unsafe {
            let load = |p: &[u8; 16]| _mm_loadu_si128(p.as_ptr().cast::<__m128i>());
            let lo = t.lo.each_ref().map(load);
            let hi = t.hi.each_ref().map(load);
            let bytes = _mm_set1_epi16(0x00ff);
            let nibble = _mm_set1_epi8(0x0f);
            let mut i = 0;
            while i < n {
                let a = _mm_loadu_si128(src.as_ptr().add(i).cast::<__m128i>());
                let b = _mm_loadu_si128(src.as_ptr().add(i + 16).cast::<__m128i>());
                // The low bytes of the 16 words, then their high bytes.
                let l = _mm_packus_epi16(_mm_and_si128(a, bytes), _mm_and_si128(b, bytes));
                let h = _mm_packus_epi16(_mm_srli_epi16::<8>(a), _mm_srli_epi16::<8>(b));
                let nib = [
                    _mm_and_si128(l, nibble),
                    _mm_and_si128(_mm_srli_epi16::<4>(l), nibble),
                    _mm_and_si128(h, nibble),
                    _mm_and_si128(_mm_srli_epi16::<4>(h), nibble),
                ];
                let product = |t: &[__m128i; 4]| {
                    _mm_xor_si128(
                        _mm_xor_si128(_mm_shuffle_epi8(t[0], nib[0]), _mm_shuffle_epi8(t[1], nib[1])),
                        _mm_xor_si128(_mm_shuffle_epi8(t[2], nib[2]), _mm_shuffle_epi8(t[3], nib[3])),
                    )
                };
                let (pl, ph) = (product(&lo), product(&hi));
                let da = dst.as_mut_ptr().add(i).cast::<__m128i>();
                let db = dst.as_mut_ptr().add(i + 16).cast::<__m128i>();
                _mm_storeu_si128(da, _mm_xor_si128(_mm_loadu_si128(da), _mm_unpacklo_epi8(pl, ph)));
                _mm_storeu_si128(db, _mm_xor_si128(_mm_loadu_si128(db), _mm_unpackhi_epi8(pl, ph)));
                i += 32;
            }
        }
        n
    }

    /// As [`ssse3`], 64 bytes at a time. Packing and unpacking work within each 128-bit lane,
    /// so the lanes are two independent SSSE3 blocks and need no crossing.
    ///
    /// # Safety
    ///
    /// The CPU must support AVX2, and `dst` must be as long as `src`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn avx2(dst: &mut [u8], src: &[u8], t: &Tables) -> usize {
        let n = src.len().min(dst.len()) / 64 * 64;
        // SAFETY: every access is 32 bytes at `i` or `i + 32` with `i + 64 <= n`, inside both
        // buffers, and the tables are 16 bytes each.
        unsafe {
            let load = |p: &[u8; 16]| _mm256_broadcastsi128_si256(_mm_loadu_si128(p.as_ptr().cast::<__m128i>()));
            let lo = t.lo.each_ref().map(load);
            let hi = t.hi.each_ref().map(load);
            let bytes = _mm256_set1_epi16(0x00ff);
            let nibble = _mm256_set1_epi8(0x0f);
            let mut i = 0;
            while i < n {
                let a = _mm256_loadu_si256(src.as_ptr().add(i).cast::<__m256i>());
                let b = _mm256_loadu_si256(src.as_ptr().add(i + 32).cast::<__m256i>());
                let l = _mm256_packus_epi16(_mm256_and_si256(a, bytes), _mm256_and_si256(b, bytes));
                let h = _mm256_packus_epi16(_mm256_srli_epi16::<8>(a), _mm256_srli_epi16::<8>(b));
                let nib = [
                    _mm256_and_si256(l, nibble),
                    _mm256_and_si256(_mm256_srli_epi16::<4>(l), nibble),
                    _mm256_and_si256(h, nibble),
                    _mm256_and_si256(_mm256_srli_epi16::<4>(h), nibble),
                ];
                let product = |t: &[__m256i; 4]| {
                    _mm256_xor_si256(
                        _mm256_xor_si256(_mm256_shuffle_epi8(t[0], nib[0]), _mm256_shuffle_epi8(t[1], nib[1])),
                        _mm256_xor_si256(_mm256_shuffle_epi8(t[2], nib[2]), _mm256_shuffle_epi8(t[3], nib[3])),
                    )
                };
                let (pl, ph) = (product(&lo), product(&hi));
                let da = dst.as_mut_ptr().add(i).cast::<__m256i>();
                let db = dst.as_mut_ptr().add(i + 32).cast::<__m256i>();
                _mm256_storeu_si256(da, _mm256_xor_si256(_mm256_loadu_si256(da), _mm256_unpacklo_epi8(pl, ph)));
                _mm256_storeu_si256(db, _mm256_xor_si256(_mm256_loadu_si256(db), _mm256_unpackhi_epi8(pl, ph)));
                i += 64;
            }
        }
        n
    }
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use std::arch::aarch64::{
        uint8x16_t, vandq_u8, vdupq_n_u8, veorq_u8, vld1q_u8, vld2q_u8, vqtbl1q_u8, vshrq_n_u8, vst2q_u8,
    };

    use super::Tables;

    /// Runs over whole 32-byte blocks of `src` and returns how many bytes it did. The
    /// de-interleaving loads split the words into low and high bytes directly.
    pub(super) fn run(dst: &mut [u8], src: &[u8], t: &Tables) -> usize {
        let n = src.len().min(dst.len()) / 32 * 32;
        // SAFETY: every access is 32 bytes at `i` with `i + 32 <= n`, inside both buffers, the
        // tables are 16 bytes each, and NEON is part of every aarch64 CPU.
        unsafe {
            let lo = t.lo.each_ref().map(|p| vld1q_u8(p.as_ptr()));
            let hi = t.hi.each_ref().map(|p| vld1q_u8(p.as_ptr()));
            let nibble = vdupq_n_u8(0x0f);
            let mut i = 0;
            while i < n {
                let s = vld2q_u8(src.as_ptr().add(i));
                let nib = [vandq_u8(s.0, nibble), vshrq_n_u8::<4>(s.0), vandq_u8(s.1, nibble), vshrq_n_u8::<4>(s.1)];
                let product = |t: &[uint8x16_t; 4]| {
                    veorq_u8(
                        veorq_u8(vqtbl1q_u8(t[0], nib[0]), vqtbl1q_u8(t[1], nib[1])),
                        veorq_u8(vqtbl1q_u8(t[2], nib[2]), vqtbl1q_u8(t[3], nib[3])),
                    )
                };
                let mut d = vld2q_u8(dst.as_ptr().add(i));
                d.0 = veorq_u8(d.0, product(&lo));
                d.1 = veorq_u8(d.1, product(&hi));
                vst2q_u8(dst.as_mut_ptr().add(i), d);
                i += 32;
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random(len: usize, mut state: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn reference(dst: &mut [u8], src: &[u8], c: u16) {
        let mut padded = src.to_vec();
        if padded.len() % 2 == 1 {
            padded.push(0);
        }
        for (i, w) in padded.as_chunks::<2>().0.iter().enumerate() {
            let p = gf::mul(c, u16::from_le_bytes(*w));
            for (k, byte) in p.to_le_bytes().into_iter().enumerate() {
                if let Some(d) = dst.get_mut(2 * i + k) {
                    *d ^= byte;
                }
            }
        }
    }

    #[test]
    fn every_kernel_matches_the_field_at_every_length_and_alignment() {
        let src = pseudo_random(300, 7);
        let base = pseudo_random(302, 9);
        for kernel in Kernel::available() {
            for c in [0u16, 1, 2, 0x1234, 0xffff] {
                for start in [0, 1, 3] {
                    for len in [0, 1, 2, 31, 32, 33, 63, 64, 65, 127, 200, 297] {
                        let s = &src[start..start + len];
                        let mut want = base[..len + 2].to_vec();
                        reference(&mut want[..len + 1], s, c);
                        let mut got = base[..len + 2].to_vec();
                        kernel.mul_add(&mut got[..len + 1], s, c);
                        assert_eq!(got, want, "{kernel:?} c={c:#x} start={start} len={len}");
                    }
                }
            }
        }
    }
}
