//! Keccak-256 of many independent inputs at once: eight states side by side in AVX-512 lanes,
//! one vector per state word. Falls back to the scalar hash without AVX-512.

use ethrex_crypto::keccak::keccak_hash;

const RATE: usize = 136;

const RC: [u64; 24] = [
    0x0000000000000001,
    0x0000000000008082,
    0x800000000000808a,
    0x8000000080008000,
    0x000000000000808b,
    0x0000000080000001,
    0x8000000080008081,
    0x8000000000008009,
    0x000000000000008a,
    0x0000000000000088,
    0x0000000080008009,
    0x000000008000000a,
    0x000000008000808b,
    0x800000000000008b,
    0x8000000000008089,
    0x8000000000008003,
    0x8000000000008002,
    0x8000000000000080,
    0x000000000000800a,
    0x800000008000000a,
    0x8000000080008081,
    0x8000000000008080,
    0x0000000080000001,
    0x8000000080008008,
];

/// Hashes every input into `out` (same order).
pub fn hash_many(inputs: &[&[u8]], out: &mut Vec<[u8; 32]>) {
    out.clear();
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        for chunk in inputs.chunks(8) {
            // Below three inputs the eight-lane permutation costs more than scalar ones.
            if chunk.len() < 3 {
                out.extend(chunk.iter().map(|input| keccak_hash(input)));
                continue;
            }
            // SAFETY: AVX-512F was detected above.
            unsafe { out.extend_from_slice(&x8::hash8(chunk)[..chunk.len()]) };
        }
        return;
    }
    out.extend(inputs.iter().map(|input| keccak_hash(input)));
}

#[cfg(target_arch = "x86_64")]
mod x8 {
    use std::arch::x86_64::*;

    use super::{RATE, RC};

    macro_rules! rho_pi {
        ($a:ident, $last:ident; $(($j:expr, $r:expr)),*) => {
            $(
                let next = $a[$j];
                $a[$j] = _mm512_rol_epi64::<$r>($last);
                $last = next;
            )*
        };
    }

    #[target_feature(enable = "avx512f")]
    #[allow(unused_assignments)]
    fn permute(a: &mut [__m512i; 25]) {
        for rc in RC {
            let c: [__m512i; 5] = std::array::from_fn(|x| {
                _mm512_ternarylogic_epi64::<0x96>(
                    _mm512_xor_si512(a[x], a[x + 5]),
                    a[x + 10],
                    _mm512_xor_si512(a[x + 15], a[x + 20]),
                )
            });
            for x in 0..5 {
                let d = _mm512_xor_si512(c[(x + 4) % 5], _mm512_rol_epi64::<1>(c[(x + 1) % 5]));
                for y in 0..5 {
                    a[y * 5 + x] = _mm512_xor_si512(a[y * 5 + x], d);
                }
            }
            let mut last = a[1];
            rho_pi!(a, last;
                (10, 1), (7, 3), (11, 6), (17, 10), (18, 15), (3, 21), (5, 28), (16, 36),
                (8, 45), (21, 55), (24, 2), (4, 14), (15, 27), (23, 41), (19, 56), (13, 8),
                (12, 25), (2, 43), (20, 62), (14, 18), (22, 39), (9, 61), (6, 20), (1, 44));
            for y in 0..5 {
                let row = [a[y * 5], a[y * 5 + 1], a[y * 5 + 2], a[y * 5 + 3], a[y * 5 + 4]];
                for x in 0..5 {
                    a[y * 5 + x] = _mm512_ternarylogic_epi64::<0xd2>(
                        row[x],
                        row[(x + 1) % 5],
                        row[(x + 2) % 5],
                    );
                }
            }
            a[0] = _mm512_xor_si512(a[0], _mm512_set1_epi64(rc as i64));
        }
    }

    /// Keccak-256 of up to eight inputs; lanes past `inputs.len()` are garbage.
    #[target_feature(enable = "avx512f")]
    pub unsafe fn hash8(inputs: &[&[u8]]) -> [[u8; 32]; 8] {
        let blocks: [usize; 8] =
            std::array::from_fn(|i| inputs.get(i).map_or(0, |input| input.len() / RATE + 1));
        let total = blocks.iter().copied().max().unwrap_or(0);
        let mut a = [_mm512_setzero_si512(); 25];
        let mut out = [[0u8; 32]; 8];
        let mut block = [[0u8; RATE]; 8];
        for b in 0..total {
            for (lane, input) in inputs.iter().enumerate() {
                let buf = &mut block[lane];
                *buf = [0; RATE];
                if b >= blocks[lane] {
                    continue;
                }
                let start = b * RATE;
                let end = input.len().min(start + RATE);
                if start < end {
                    buf[..end - start].copy_from_slice(&input[start..end]);
                }
                if b + 1 == blocks[lane] {
                    buf[input.len() - start] ^= 0x01;
                    buf[RATE - 1] ^= 0x80;
                }
            }
            for (w, word) in a.iter_mut().take(RATE / 8).enumerate() {
                let lane = |l: usize| {
                    i64::from_le_bytes(block[l][w * 8..w * 8 + 8].try_into().unwrap_or_default())
                };
                let v = _mm512_set_epi64(
                    lane(7),
                    lane(6),
                    lane(5),
                    lane(4),
                    lane(3),
                    lane(2),
                    lane(1),
                    lane(0),
                );
                *word = _mm512_xor_si512(*word, v);
            }
            permute(&mut a);
            let mut digest = [[0i64; 8]; 4];
            for (w, slot) in digest.iter_mut().enumerate() {
                // SAFETY: `slot` is eight i64s, exactly one vector.
                unsafe { _mm512_storeu_si512(slot.as_mut_ptr().cast(), a[w]) };
            }
            for lane in 0..inputs.len() {
                if b + 1 == blocks[lane] {
                    for w in 0..4 {
                        out[lane][w * 8..w * 8 + 8].copy_from_slice(&digest[w][lane].to_le_bytes());
                    }
                }
            }
        }
        out
    }
}
