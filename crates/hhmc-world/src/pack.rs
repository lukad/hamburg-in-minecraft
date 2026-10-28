//! Packed long arrays as used by paletted containers and heightmaps.
//!
//! Since 1.16 entries never span two longs: each long holds `64 / bits` entries, low bits
//! first, and the array has `ceil(n / (64 / bits))` longs (`SimpleBitStorage`).

/// `ceil(log2(n))`, with `ceil_log2(1) == 0`.
pub fn ceil_log2(n: usize) -> u32 {
    if n <= 1 { 0 } else { usize::BITS - (n - 1).leading_zeros() }
}

/// Bits per entry of a section's `block_states.data` for a palette of `n` entries:
/// none for a single entry, otherwise at least 4 (`PalettedContainer` strategy table).
pub fn block_bits(n: usize) -> u32 {
    match ceil_log2(n) {
        0 => 0,
        b => b.max(4),
    }
}

/// Bits per entry of a section's `biomes.data`: `ceil(log2(n))`, no minimum.
pub fn biome_bits(n: usize) -> u32 {
    ceil_log2(n)
}

pub fn packed_len(n: usize, bits: u32) -> usize {
    let per = (64 / bits) as usize;
    n.div_ceil(per)
}

pub fn pack(values: &[u32], bits: u32) -> Vec<i64> {
    assert!((1..=32).contains(&bits));
    let per = (64 / bits) as usize;
    let mut out = vec![0u64; packed_len(values.len(), bits)];
    for (i, &v) in values.iter().enumerate() {
        debug_assert!(u64::from(v) < (1u64 << bits));
        out[i / per] |= u64::from(v) << ((i % per) as u32 * bits);
    }
    out.into_iter().map(|w| w as i64).collect()
}

pub fn unpack(data: &[i64], bits: u32, n: usize) -> Option<Vec<u32>> {
    if bits == 0 || bits > 32 || data.len() != packed_len(n, bits) {
        return None;
    }
    let per = (64 / bits) as usize;
    let mask = (1u64 << bits) - 1;
    Some(
        (0..n)
            .map(|i| ((data[i / per] as u64 >> ((i % per) as u32 * bits)) & mask) as u32)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits() {
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(16), 4);
        assert_eq!(ceil_log2(17), 5);
        assert_eq!(ceil_log2(385), 9);
        assert_eq!(block_bits(1), 0);
        assert_eq!(block_bits(2), 4);
        assert_eq!(block_bits(17), 5);
        assert_eq!(block_bits(300), 9);
        assert_eq!(biome_bits(2), 1);
        assert_eq!(biome_bits(5), 3);
    }

    #[test]
    fn lengths_match_vanilla() {
        // 4096 entries at 4 bits = 256 longs; at 5 bits 12 per long = 342 longs.
        assert_eq!(packed_len(4096, 4), 256);
        assert_eq!(packed_len(4096, 5), 342);
        // Heightmap: 256 entries at 9 bits, 7 per long = 37 longs.
        assert_eq!(packed_len(256, 9), 37);
    }

    #[test]
    fn round_trip() {
        for bits in [1, 4, 5, 7, 9, 12, 16] {
            let vals: Vec<u32> =
                (0..4096u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 7) % (1 << bits)).collect();
            let packed = pack(&vals, bits);
            assert_eq!(unpack(&packed, bits, vals.len()).unwrap(), vals, "bits {bits}");
        }
    }

    #[test]
    fn no_spanning() {
        // 5 bits: 12 entries in the first long, the 13th starts the second long at bit 0.
        let mut v = vec![0u32; 13];
        v[12] = 31;
        let p = pack(&v, 5);
        assert_eq!(p[0], 0);
        assert_eq!(p[1], 31);
    }
}
