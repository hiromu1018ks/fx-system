/// Expand a 64-bit CLI/config seed into a full 32-byte RNG seed.
///
/// Uses SplitMix64 so every output byte depends on the input seed while
/// remaining deterministic across crates and entry points.
pub fn expand_u64_seed(seed: u64) -> [u8; 32] {
    let mut state = seed;
    let mut expanded = [0u8; 32];

    for chunk in expanded.chunks_exact_mut(8) {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        chunk.copy_from_slice(&z.to_le_bytes());
    }

    expanded
}

#[cfg(test)]
mod tests {
    use super::expand_u64_seed;

    #[test]
    fn test_expand_u64_seed_is_deterministic() {
        assert_eq!(expand_u64_seed(42), expand_u64_seed(42));
    }

    #[test]
    fn test_expand_u64_seed_uses_full_output() {
        let seed = expand_u64_seed(42);
        assert!(seed[8..].iter().any(|&byte| byte != 0));
    }

    #[test]
    fn test_expand_u64_seed_differs_for_different_inputs() {
        assert_ne!(expand_u64_seed(42), expand_u64_seed(43));
    }
}
