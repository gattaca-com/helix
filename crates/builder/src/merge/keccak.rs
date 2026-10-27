//! Keccak-256 of many independent inputs: eight at a time, one AVX-512 vector per state word with
//! a lane per input, when the CPU has AVX-512F; one at a time otherwise. A group of eight runs as
//! many permutations as its longest input needs, so callers put inputs of similar length together.

use ethrex_crypto::keccak::keccak_hash;

/// Bytes absorbed per permutation.
const RATE: usize = 136;
const WORDS: usize = RATE / 8;

/// Hashes `bytes[at..at + len]` for each span into `out`, in order.
pub fn many(bytes: &[u8], spans: &[(u32, u32)], out: &mut [[u8; 32]]) {
    debug_assert_eq!(spans.len(), out.len());
    let input = |&(at, len): &(u32, u32)| &bytes[at as usize..(at + len) as usize];
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx512f") {
        for (spans, out) in spans.chunks(8).zip(out.chunks_mut(8)) {
            if let [span] = spans {
                out[0] = keccak_hash(input(span));
                continue;
            }
            let inputs: [&[u8]; 8] = std::array::from_fn(|l| spans.get(l).map_or(&[][..], input));
            // SAFETY: AVX-512F was detected above.
            unsafe { x8::hash(&inputs, out) };
        }
        return;
    }
    for (span, out) in spans.iter().zip(out) {
        *out = keccak_hash(input(span));
    }
}

#[cfg(target_arch = "x86_64")]
mod x8 {
    use std::arch::x86_64::*;

    use super::{RATE, WORDS};

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

    /// Rho and pi: state word `i` = `x + 5y`, rotated, moves to `y + 5 * ((2x + 3y) % 5)`.
    /// Written out so every index is a constant and the state stays in registers.
    macro_rules! rho_pi {
        ($a:ident, $b:ident, $d:ident; $(($i:literal, $to:literal, $rot:literal)),* $(,)?) => {
            $( $b[$to] = _mm512_rol_epi64::<$rot>(_mm512_xor_si512($a[$i], $d[$i % 5])); )*
        };
    }

    /// Chi on row `y`: `a[x] = b[x] ^ (!b[x + 1] & b[x + 2])` (ternary-logic table 0xd2).
    macro_rules! chi {
        ($a:ident, $b:ident; $($y:literal),*) => {
            $(
                $a[$y] = _mm512_ternarylogic_epi64::<0xd2>($b[$y], $b[$y + 1], $b[$y + 2]);
                $a[$y + 1] = _mm512_ternarylogic_epi64::<0xd2>($b[$y + 1], $b[$y + 2], $b[$y + 3]);
                $a[$y + 2] = _mm512_ternarylogic_epi64::<0xd2>($b[$y + 2], $b[$y + 3], $b[$y + 4]);
                $a[$y + 3] = _mm512_ternarylogic_epi64::<0xd2>($b[$y + 3], $b[$y + 4], $b[$y]);
                $a[$y + 4] = _mm512_ternarylogic_epi64::<0xd2>($b[$y + 4], $b[$y], $b[$y + 1]);
            )*
        };
    }

    #[inline]
    #[target_feature(enable = "avx512f")]
    fn permute(a: &mut [__m512i; 25]) {
        for rc in RC {
            let mut c = [_mm512_setzero_si512(); 5];
            for x in 0..5 {
                let e = _mm512_xor_si512(a[x], a[x + 5]);
                let f = _mm512_xor_si512(a[x + 15], a[x + 20]);
                c[x] = _mm512_ternarylogic_epi64::<0x96>(e, a[x + 10], f);
            }
            let mut d = [_mm512_setzero_si512(); 5];
            for x in 0..5 {
                d[x] = _mm512_xor_si512(c[(x + 4) % 5], _mm512_rol_epi64::<1>(c[(x + 1) % 5]));
            }
            let mut b = [_mm512_setzero_si512(); 25];
            rho_pi!(a, b, d;
                (0, 0, 0), (1, 10, 1), (2, 20, 62), (3, 5, 28), (4, 15, 27),
                (5, 16, 36), (6, 1, 44), (7, 11, 6), (8, 21, 55), (9, 6, 20),
                (10, 7, 3), (11, 17, 10), (12, 2, 43), (13, 12, 25), (14, 22, 39),
                (15, 23, 41), (16, 8, 45), (17, 18, 15), (18, 3, 21), (19, 13, 8),
                (20, 14, 18), (21, 24, 2), (22, 9, 61), (23, 19, 56), (24, 4, 14),
            );
            chi!(a, b; 0, 5, 10, 15, 20);
            a[0] = _mm512_xor_si512(a[0], _mm512_set1_epi64(rc as i64));
        }
    }

    /// Hashes the first `out.len()` of `inputs` into `out`; the other lanes run on empty inputs.
    #[target_feature(enable = "avx512f")]
    pub fn hash(inputs: &[&[u8]; 8], out: &mut [[u8; 32]]) {
        // The padding needs at least one byte, so a multiple of `RATE` takes one more block.
        let blocks: [usize; 8] = std::array::from_fn(|l| inputs[l].len() / RATE + 1);
        let total = blocks[..out.len()].iter().copied().max().unwrap_or(0);
        let mut a = [_mm512_setzero_si512(); 25];
        let mut words = [[0u64; 8]; WORDS];
        let mut last = [0u8; RATE];
        let word = |bytes: &[u8], w: usize| {
            u64::from_le_bytes(*bytes[8 * w..].first_chunk().unwrap_or(&[0; 8]))
        };
        for b in 0..total {
            for (l, input) in inputs.iter().enumerate() {
                let at = b * RATE;
                if b >= blocks[l] {
                    for w in &mut words {
                        w[l] = 0;
                    }
                } else if b + 1 < blocks[l] {
                    for (w, words) in words.iter_mut().enumerate() {
                        words[l] = word(&input[at..], w);
                    }
                } else {
                    let rest = &input[at..];
                    last.fill(0);
                    last[..rest.len()].copy_from_slice(rest);
                    last[rest.len()] ^= 0x01;
                    last[RATE - 1] ^= 0x80;
                    for (w, words) in words.iter_mut().enumerate() {
                        words[l] = word(&last, w);
                    }
                }
            }
            for (a, words) in a.iter_mut().zip(&words) {
                // SAFETY: `words` is eight u64s, exactly one vector.
                let v = unsafe { _mm512_loadu_si512(words.as_ptr().cast()) };
                *a = _mm512_xor_si512(*a, v);
            }
            permute(&mut a);
            if !blocks[..out.len()].contains(&(b + 1)) {
                continue;
            }
            let mut digest = [[0u64; 8]; 4];
            for (d, a) in digest.iter_mut().zip(&a) {
                // SAFETY: `d` is eight u64s, exactly one vector.
                unsafe { _mm512_storeu_si512(d.as_mut_ptr().cast(), *a) };
            }
            for (l, out) in out.iter_mut().enumerate() {
                if blocks[l] == b + 1 {
                    for (w, d) in digest.iter().enumerate() {
                        out[8 * w..8 * w + 8].copy_from_slice(&d[l].to_le_bytes());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ethrex_crypto::keccak::keccak_hash;

    use super::many;

    #[test]
    fn matches_keccak() {
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i * 31 + i / 7) as u8).collect();
        // Lengths across block boundaries, grouped like and unlike, with short tails.
        let mut spans = Vec::new();
        let mut at = 0u32;
        for len in (0..700u32).chain([135, 136, 137, 272, 4000, 1, 0, 9000]) {
            spans.push((at, len));
            at += len + 3;
        }
        for n in [1, 2, 3, 8, 9, spans.len()] {
            let mut out = vec![[0u8; 32]; n];
            many(&bytes, &spans[..n], &mut out);
            for (&(at, len), got) in spans.iter().zip(&out) {
                assert_eq!(
                    *got,
                    keccak_hash(&bytes[at as usize..(at + len) as usize]),
                    "len={len}"
                );
            }
        }
    }
}
