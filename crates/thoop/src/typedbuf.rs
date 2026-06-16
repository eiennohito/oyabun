//! Resettable contiguous [`Flat`] array in an [`Arena`] chunk — the snapshot row buffer.

use std::marker::PhantomData;
use std::mem::size_of;

use crate::Flat;
use crate::arena::Arena;

/// A growable, O(1)-resettable array of `T` in one arena chunk.
///
/// `clear` rewinds the length without dropping (`T: Flat` is `Copy`), so a double-buffered
/// snapshot refills it each cycle with zero allocation after warmup. Growth relocates the
/// chunk (arena handles the copy); callers index by position, and slice borrows are tied to
/// the `&Arena` so they cannot outlive a growth.
pub struct TypedBuf<T: Flat> {
    chunk: crate::arena::ChunkId,
    len: usize,
    cap: usize,
    _marker: PhantomData<T>,
}

impl<T: Flat> TypedBuf<T> {
    fn stride() -> usize {
        size_of::<T>().max(1)
    }

    /// Allocate a buffer in a fresh chunk sized for at least `min_capacity` elements.
    #[must_use]
    pub fn new(arena: &mut Arena, min_capacity: usize) -> Self {
        let cap = min_capacity.max(1);
        let chunk = arena.alloc(cap * Self::stride(), align_of::<T>().max(1));
        Self {
            chunk,
            len: 0,
            cap,
            _marker: PhantomData,
        }
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn ptr(&self, arena: &Arena) -> *mut T {
        arena.base_of(self.chunk).cast::<T>()
    }

    /// Rewind to empty. O(1), no drops (`T: Copy`).
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Ensure room for at least `min_capacity` elements, relocating if short.
    pub fn reserve(&mut self, arena: &mut Arena, min_capacity: usize) {
        if min_capacity > self.cap {
            self.grow(arena, min_capacity);
        }
    }

    fn grow(&mut self, arena: &mut Arena, needed: usize) {
        let mut new_cap = self.cap.max(1);
        while new_cap < needed {
            new_cap *= 2;
        }
        arena.grow(self.chunk, new_cap * Self::stride());
        self.cap = new_cap;
    }

    /// Append a value, growing if full.
    pub fn push(&mut self, arena: &mut Arena, value: T) {
        if self.len == self.cap {
            self.grow(arena, self.len + 1);
        }
        // SAFETY: capacity ensured; `len` in range.
        unsafe {
            self.ptr(arena).add(self.len).write(value);
        }
        self.len += 1;
    }

    #[must_use]
    pub fn as_slice(&self, arena: &Arena) -> &[T] {
        // SAFETY: `0..len` were initialised by push.
        unsafe { std::slice::from_raw_parts(self.ptr(arena), self.len) }
    }

    pub fn as_mut_slice(&mut self, arena: &Arena) -> &mut [T] {
        // SAFETY: `0..len` initialised; &mut self gives unique access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr(arena), self.len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_clear_reuse() {
        let mut arena = Arena::new(0);
        let mut b: TypedBuf<u32> = TypedBuf::new(&mut arena, 4);
        for i in 0..10u32 {
            b.push(&mut arena, i * 2);
        }
        assert_eq!(b.len(), 10);
        assert_eq!(b.as_slice(&arena)[3], 6);
        b.as_mut_slice(&arena)[3] = 100;
        assert_eq!(b.as_slice(&arena)[3], 100);
        b.clear();
        assert_eq!(b.len(), 0);
        b.push(&mut arena, 5);
        assert_eq!(b.as_slice(&arena)[0], 5, "reuse after clear starts at 0");
    }

    /// Oracle: random push/clear/index churn matches a `Vec`, across growth relocations.
    #[test]
    #[allow(clippy::cast_possible_truncation)] // LCG-derived index, masked by `% len`
    fn oracle_matches_vec() {
        let mut arena = Arena::new(0);
        let mut buf: TypedBuf<u64> = TypedBuf::new(&mut arena, 1);
        let mut model: Vec<u64> = Vec::new();
        let mut rng = 0xdead_beef_0bad_f00du64;

        for _ in 0..50_000 {
            rng = rng
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            match (rng >> 33) % 4 {
                0 | 1 => {
                    let v = rng;
                    buf.push(&mut arena, v);
                    model.push(v);
                }
                2 => {
                    if !model.is_empty() {
                        let i = (rng as usize >> 7) % model.len();
                        let v = rng ^ 0xa5a5;
                        buf.as_mut_slice(&arena)[i] = v;
                        model[i] = v;
                    }
                }
                _ => {
                    if rng.is_multiple_of(50) {
                        buf.clear();
                        model.clear();
                    }
                }
            }
            assert_eq!(buf.len(), model.len());
            assert_eq!(buf.as_slice(&arena), model.as_slice());
        }
    }
}
