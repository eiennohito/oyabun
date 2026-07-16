//! Value hashing for change detection.
//!
//! A cell's change key is the hash of the value(s) bound to it. On the next frame we
//! hash the new value and compare: equal ⇒ the value is unchanged ⇒ skip formatting
//! and I/O entirely. Collisions are statistically negligible for u64 and self-correct
//! on the next change, so a fast non-cryptographic mix beats `SipHash` here (the keys
//! are domain values, not attacker-controlled).

use std::hash::{Hash, Hasher};

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// `FxHash`-style hasher: one rotate-xor-multiply per word.
#[derive(Default)]
pub struct ChangeHasher {
    state: u64,
}

impl ChangeHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.state = (self.state.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for ChangeHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.state
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        // Mix 8 bytes per round (FxHash-style) instead of one, with a zero-padded tail.
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            let mut last = [0u8; 8];
            last[..rem.len()].copy_from_slice(rem);
            self.add(u64::from_le_bytes(last));
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(u64::from(i));
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
}

/// Hash a value to a `u64` for change detection.
#[inline]
pub fn hash_value<T: Hash>(value: &T) -> u64 {
    let mut h = ChangeHasher::default();
    value.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_values_distinct_hashes() {
        assert_ne!(hash_value(&1_u32), hash_value(&2_u32));
        assert_ne!(hash_value(&"foo"), hash_value(&"bar"));
        assert_ne!(hash_value(&(1_u32, 2_u32)), hash_value(&(2_u32, 1_u32)));
    }

    #[test]
    fn equal_values_equal_hashes() {
        assert_eq!(hash_value(&12345_u64), hash_value(&12345_u64));
        assert_eq!(
            hash_value(&"slack --type=renderer"),
            hash_value(&"slack --type=renderer")
        );
    }
}
