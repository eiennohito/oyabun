//! Fast non-cryptographic hashing for internal integer keys (PIDs). The default
//! `RandomState` (`SipHash`) resists hash-flooding but is slow; PIDs here are not
//! attacker-controlled, so we trade that resistance for speed on the per-cycle lookups.
//! Shared by the `/proc` backends' fd pools and the BPF source's live-set maps.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};

/// `FxHash`-style hasher: one multiply-rotate-xor per word.
#[derive(Default)]
pub(crate) struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rem) = bytes.as_chunks::<8>();
        for &c in chunks {
            self.add(u64::from_le_bytes(c));
        }
        if !rem.is_empty() {
            let mut last = [0u8; 8];
            last[..rem.len()].copy_from_slice(rem);
            self.add(u64::from_le_bytes(last));
        }
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

#[derive(Default, Clone)]
pub(crate) struct FxBuildHasher;

impl BuildHasher for FxBuildHasher {
    type Hasher = FxHasher;
    fn build_hasher(&self) -> FxHasher {
        FxHasher::default()
    }
}

/// PID-keyed map using the fast hasher above.
pub(crate) type PidMap<V> = HashMap<u32, V, FxBuildHasher>;
/// General map using the same deterministic fast hasher. Prefer this over the standard
/// `HashMap` default hasher for internal, non-adversarial keys.
pub(crate) type FxMap<K, V> = HashMap<K, V, FxBuildHasher>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fx_hasher_is_deterministic_and_distinct() {
        let bh = FxBuildHasher;
        assert_eq!(bh.hash_one(1234_u32), bh.hash_one(1234_u32));
        assert_ne!(bh.hash_one(1_u32), bh.hash_one(2_u32));
    }
}
