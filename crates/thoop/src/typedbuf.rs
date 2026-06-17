//! Resettable contiguous [`Flat`] array in an [`Arena`] chunk — the snapshot row buffer.

use std::cell::Cell;
use std::marker::PhantomData;
use std::mem::size_of;

use crate::Flat;
use crate::arena::{Arena, ChunkId};

/// A growable, O(1)-resettable array of `T` in one arena chunk.
///
/// `clear` rewinds the length without dropping (`T: Flat` is `Copy`), so a double-buffered
/// snapshot refills it each cycle with zero allocation after warmup. Like the other write
/// primitives it caches its base (self-healed via the arena epoch — see [`Arena`]) and grows
/// through a `*const Arena`, so it must be [`wire`](Self::wire)d after the arena is pinned.
/// Neither `Send` nor `Sync`.
pub struct TypedBuf<T: Flat> {
    /// Owning arena (interior-mutable, `&self`). Null until [`wire`](Self::wire).
    arena: *const Arena,
    /// Cached chunk base. Re-read by [`sync`](Self::sync) when the arena's epoch advances.
    base: Cell<*mut T>,
    /// Arena epoch the cached `base` was read at.
    epoch: Cell<u64>,
    chunk: ChunkId,
    len: usize,
    cap: usize,
    _marker: PhantomData<T>,
}

impl<T: Flat> TypedBuf<T> {
    fn stride() -> usize {
        size_of::<T>().max(1)
    }

    /// Allocate a buffer in a fresh chunk sized for at least `min_capacity` elements. Not
    /// usable until [`wire`](Self::wire)d.
    #[must_use]
    pub fn new(arena: &Arena, min_capacity: usize) -> Self {
        let cap = min_capacity.max(1);
        let chunk = arena.alloc(cap * Self::stride(), align_of::<T>().max(1));
        Self {
            arena: std::ptr::null(),
            base: Cell::new(std::ptr::null_mut()),
            epoch: Cell::new(0),
            chunk,
            len: 0,
            cap,
            _marker: PhantomData,
        }
    }

    /// Bind to the arena once it is pinned (see [`GenStore::wire`](crate::GenStore::wire)).
    /// Call before any push/index.
    pub fn wire(&mut self, arena: &Arena) {
        self.arena = arena;
        self.base.set(arena.base_of(self.chunk).cast());
        self.epoch.set(arena.epoch());
    }

    fn arena(&self) -> &Arena {
        debug_assert!(!self.arena.is_null(), "TypedBuf accessed before wire()");
        // SAFETY: set by `wire` to the owning arena, which outlives this buffer.
        unsafe { &*self.arena }
    }

    /// Re-read the cached base if the arena relocated this chunk since we last looked.
    fn sync(&self) {
        let cur = self.arena().epoch();
        if self.epoch.get() != cur {
            self.base.set(self.arena().base_of(self.chunk).cast());
            self.epoch.set(cur);
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

    fn ptr(&self) -> *mut T {
        self.sync();
        self.base.get()
    }

    /// Rewind to empty. O(1), no drops (`T: Copy`).
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Ensure room for at least `min_capacity` elements, relocating if short.
    pub fn reserve(&mut self, min_capacity: usize) {
        if min_capacity > self.cap {
            self.grow(min_capacity);
        }
    }

    fn grow(&mut self, needed: usize) {
        let mut new_cap = self.cap.max(1);
        while new_cap < needed {
            new_cap *= 2;
        }
        let new_base = self.arena().grow(self.chunk, new_cap * Self::stride());
        self.base.set(new_base.cast());
        self.epoch.set(self.arena().epoch());
        self.cap = new_cap;
    }

    /// Append a value, growing if full.
    pub fn push(&mut self, value: T) {
        if self.len == self.cap {
            self.grow(self.len + 1);
        }
        // SAFETY: capacity ensured; `len` in range; base synced via `ptr`.
        unsafe {
            self.ptr().add(self.len).write(value);
        }
        self.len += 1;
    }

    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: `0..len` were initialised by push.
        unsafe { std::slice::from_raw_parts(self.ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: `0..len` initialised; &mut self gives unique access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr(), self.len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wired<T: Flat>(arena: &Arena, min_capacity: usize) -> TypedBuf<T> {
        let mut b = TypedBuf::new(arena, min_capacity);
        b.wire(arena);
        b
    }

    #[test]
    fn push_clear_reuse() {
        let arena = Arena::new(0);
        let mut b: TypedBuf<u32> = wired(&arena, 4);
        for i in 0..10u32 {
            b.push(i * 2);
        }
        assert_eq!(b.len(), 10);
        assert_eq!(b.as_slice()[3], 6);
        b.as_mut_slice()[3] = 100;
        assert_eq!(b.as_slice()[3], 100);
        b.clear();
        assert_eq!(b.len(), 0);
        b.push(5);
        assert_eq!(b.as_slice()[0], 5, "reuse after clear starts at 0");
    }

    /// Oracle: random push/clear/index churn matches a `Vec`, across growth relocations.
    #[test]
    #[allow(clippy::cast_possible_truncation)] // LCG-derived index, masked by `% len`
    fn oracle_matches_vec() {
        let arena = Arena::new(0);
        let mut buf: TypedBuf<u64> = wired(&arena, 1);
        let mut model: Vec<u64> = Vec::new();
        let mut rng = 0xdead_beef_0bad_f00du64;

        for _ in 0..50_000 {
            rng = rng
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            match (rng >> 33) % 4 {
                0 | 1 => {
                    let v = rng;
                    buf.push(v);
                    model.push(v);
                }
                2 => {
                    if !model.is_empty() {
                        let i = (rng as usize >> 7) % model.len();
                        let v = rng ^ 0xa5a5;
                        buf.as_mut_slice()[i] = v;
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
            assert_eq!(buf.as_slice(), model.as_slice());
        }
    }
}
