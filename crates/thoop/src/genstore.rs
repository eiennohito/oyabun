//! Generational slab: long-lived [`Flat`] records in one [`Arena`] chunk, addressed by
//! [`Ref`].

use std::marker::PhantomData;
use std::mem::{MaybeUninit, size_of};

use crate::arena::{Arena, ChunkId};
use crate::strstore::ByteResolver;
use crate::{Flat, Gen, Ref};

/// One slot: its lifecycle [`Gen`] tag inline before the payload. Array-of-structs keeps a
/// slot's tag and data on the same cache line; the GC scan strides over the tags, a
/// once-per-cycle microsecond cost at realistic populations.
#[repr(C)]
struct Slot<T> {
    tag: Gen,
    data: MaybeUninit<T>,
}

/// A generational slab living in a single [`Arena`] chunk. The chunk relocates on growth
/// (the arena handles the copy; the `ChunkId` is stable), so a [`Ref`] — a slot *index* — is
/// valid across growth by construction. Free slots are chained through a free list threaded
/// into their own data bytes (zero extra space). A demoted slot keeps its data until
/// [`gc`](Self::gc) proves no reader can reach it.
///
/// All slot-touching methods take the owning `&Arena` (or `&mut Arena` when they may grow)
/// to resolve the current base; the gatherer holds the arena, so this is local threading,
/// and hot cross-thread reads go through a [`ByteResolver`] instead (captured base, no
/// arena). `T` must be at least 4 bytes (to hold the free-list link).
pub struct GenStore<T: Flat> {
    chunk: ChunkId,
    /// Slots the chunk currently holds.
    cap: usize,
    /// High-water slot count — slots `0..len` have been handed out at least once.
    len: usize,
    free_head: u32,
    free_count: usize,
    _marker: PhantomData<T>,
}

const NO_FREE: u32 = u32::MAX;

impl<T: Flat> GenStore<T> {
    /// Build a store in a fresh arena chunk sized for at least `min_slots` (floored at 1).
    ///
    /// # Panics
    /// If `T` is smaller than 4 bytes (the free list threads a `u32` through a freed slot).
    #[must_use]
    pub fn new(arena: &mut Arena, min_slots: usize) -> Self {
        assert!(
            size_of::<T>() >= size_of::<u32>(),
            "GenStore<T> requires T to be at least 4 bytes (free-list link)"
        );
        let cap = min_slots.max(1);
        let chunk = arena.alloc(cap * size_of::<Slot<T>>(), align_of::<Slot<T>>());
        Self {
            chunk,
            cap,
            len: 0,
            free_head: NO_FREE,
            free_count: 0,
            _marker: PhantomData,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn free_count(&self) -> usize {
        self.free_count
    }

    fn slot(&self, arena: &Arena, idx: usize) -> *mut Slot<T> {
        debug_assert!(idx < self.cap);
        // SAFETY: idx < cap; the chunk holds `cap` slots.
        unsafe { arena.base_of(self.chunk).cast::<Slot<T>>().add(idx) }
    }

    fn ensure_cap(&mut self, arena: &mut Arena) {
        if self.len == self.cap {
            let new_cap = self
                .cap
                .checked_mul(2)
                .expect("GenStore capacity overflow")
                .max(1);
            arena.grow(self.chunk, new_cap * size_of::<Slot<T>>());
            self.cap = new_cap;
        }
    }

    fn take_slot(&mut self, arena: &mut Arena) -> u32 {
        if self.free_head == NO_FREE {
            self.ensure_cap(arena);
            let idx = self.len;
            // A slot index of `u32::MAX` would collide with both `NO_FREE` and `Ref::NONE`.
            debug_assert!(
                idx < u32::MAX as usize,
                "slot count must stay below u32::MAX"
            );
            self.len += 1;
            return u32::try_from(idx).expect("slot count fits u32");
        }
        let idx = self.free_head;
        // SAFETY: `idx` is a valid slot; its data holds the threaded next-free link (written
        // unaligned on free).
        let next = unsafe {
            std::ptr::read_unaligned(
                (*self.slot(arena, idx as usize))
                    .data
                    .as_ptr()
                    .cast::<u32>(),
            )
        };
        self.free_head = next;
        self.free_count -= 1;
        idx
    }

    /// Allocate an **uninitialised** slot with lifecycle `tag`. Call
    /// [`assign`](Self::assign)/[`construct`](Self::construct) before [`get`](Self::get).
    pub fn alloc(&mut self, arena: &mut Arena, tag: Gen) -> Ref<T> {
        let idx = self.take_slot(arena);
        // SAFETY: freshly reserved, in-range slot.
        unsafe {
            (*self.slot(arena, idx as usize)).tag = tag;
        }
        Ref::new(idx)
    }

    /// Write a value into a slot reserved by [`alloc`](Self::alloc).
    pub fn assign(&mut self, arena: &Arena, r: Ref<T>, value: T) {
        debug_assert!(!self.gen_at(arena, r).is_free(), "assign into a freed slot");
        // SAFETY: `r` indexes a live slot; `T: Copy` so prior bytes need no drop.
        unsafe {
            (*self.slot(arena, r.index())).data.write(value);
        }
    }

    /// Allocate and initialise in one step.
    pub fn insert(&mut self, arena: &mut Arena, tag: Gen, value: T) -> Ref<T> {
        let r = self.alloc(arena, tag);
        self.assign(arena, r, value);
        r
    }

    /// Allocate a slot and initialise it **in place** through `init`, avoiding a temporary
    /// `T`. `init` must leave the slot fully initialised (the `assume_init` contract).
    pub fn construct(
        &mut self,
        arena: &mut Arena,
        tag: Gen,
        init: impl FnOnce(&mut MaybeUninit<T>),
    ) -> Ref<T> {
        let idx = self.take_slot(arena);
        // SAFETY: freshly reserved, in-range slot; we tag it then hand the caller its
        // uninitialised payload to fill.
        unsafe {
            let s = self.slot(arena, idx as usize);
            (*s).tag = tag;
            init(&mut (*s).data);
        }
        Ref::new(idx)
    }

    #[must_use]
    pub fn get(&self, arena: &Arena, r: Ref<T>) -> &T {
        debug_assert!(r.index() < self.len, "Ref out of range");
        let s = self.slot(arena, r.index());
        // SAFETY: in-range live slot; data written before any get (contract).
        unsafe {
            debug_assert!(!(*s).tag.is_free(), "get on a freed slot");
            &*(*s).data.as_ptr()
        }
    }

    pub fn get_mut(&mut self, arena: &Arena, r: Ref<T>) -> &mut T {
        debug_assert!(r.index() < self.len, "Ref out of range");
        let s = self.slot(arena, r.index());
        // SAFETY: &mut self gives unique store access; in-range live slot.
        unsafe {
            debug_assert!(!(*s).tag.is_free(), "get_mut on a freed slot");
            &mut *(*s).data.as_mut_ptr()
        }
    }

    /// Demote a slot to generation `now`: still readable (a live reader may lease it) but
    /// reclaimable once `min_live` passes `now`. For snapshot-leased data (strings).
    pub fn demote(&mut self, arena: &Arena, r: Ref<T>, now: u64) {
        // SAFETY: in-range slot; only the 1-byte tag is written.
        unsafe {
            (*self.slot(arena, r.index())).tag = Gen::at(now);
        }
    }

    /// Immediately reclaim a slot to the free list — no lease. For **gatherer-internal**
    /// stores whose slots are never referenced by a published snapshot (e.g. per-PID
    /// bookkeeping); `demote` + `gc` is for snapshot-leased data instead.
    #[allow(clippy::cast_possible_truncation)] // r.index() round-trips a u32 slot index
    pub fn free(&mut self, arena: &Arena, r: Ref<T>) {
        let s = self.slot(arena, r.index());
        // SAFETY: in-range slot; thread the free link into its (unreferenced) data bytes.
        unsafe {
            std::ptr::write_unaligned((*s).data.as_mut_ptr().cast::<u32>(), self.free_head);
            (*s).tag = Gen::FREE;
        }
        self.free_head = r.index() as u32;
        self.free_count += 1;
    }

    /// Reclaim demoted slots whose generation `min_live` has passed, linking them onto the
    /// free list. `ALIVE`/`FREE` slots are skipped.
    #[allow(clippy::cast_possible_truncation)] // idx < len ≤ u32::MAX (take_slot caps it)
    pub fn gc(&mut self, arena: &Arena, min_live: u64) {
        // Hoist the base out of the scan — it is loop-invariant (the chunk does not move
        // mid-`gc`), so the per-iteration cost is one tag read, not a base re-derivation.
        let base = arena.base_of(self.chunk).cast::<Slot<T>>();
        for idx in 0..self.len {
            // SAFETY: idx < len ≤ cap, in range; on reclaim, thread the free link into the
            // no-longer-referenced data bytes.
            unsafe {
                let s = base.add(idx);
                if (*s).tag.is_reclaimable(min_live) {
                    std::ptr::write_unaligned((*s).data.as_mut_ptr().cast::<u32>(), self.free_head);
                    (*s).tag = Gen::FREE;
                    self.free_head = idx as u32;
                    self.free_count += 1;
                }
            }
        }
    }

    /// Capture the chunk's current base + slot layout for store-free, cross-thread
    /// resolution of byte-slot [`StringRef`](crate::StringRef)s (used by
    /// [`StrStore`](crate::StrStore)). Old snapshots' captured resolvers stay valid across a
    /// later relocate (regime A hole / regime B retired region).
    #[must_use]
    pub fn byte_resolver<S>(&self, arena: &Arena) -> ByteResolver<S> {
        ByteResolver::new(
            arena.base_of(self.chunk),
            size_of::<Slot<T>>(),
            std::mem::offset_of!(Slot<T>, data),
        )
    }

    pub(crate) fn gen_at(&self, arena: &Arena, r: Ref<T>) -> Gen {
        // SAFETY: in-range slot.
        unsafe { (*self.slot(arena, r.index())).tag }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn alloc_assign_get_roundtrip() {
        let mut arena = Arena::new(0);
        let mut s: GenStore<u64> = GenStore::new(&mut arena, 0);
        let a = s.insert(&mut arena, Gen::ALIVE, 42);
        let b = s.insert(&mut arena, Gen::ALIVE, 99);
        assert_eq!(*s.get(&arena, a), 42);
        assert_eq!(*s.get(&arena, b), 99);
        *s.get_mut(&arena, a) = 7;
        assert_eq!(*s.get(&arena, a), 7);
    }

    #[test]
    fn construct_builds_in_place() {
        let mut arena = Arena::new(0);
        let mut s: GenStore<[u8; 8]> = GenStore::new(&mut arena, 0);
        let r = s.construct(&mut arena, Gen::ALIVE, |slot| {
            let p = slot.as_mut_ptr().cast::<u8>();
            // SAFETY: slot is 8 bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(b"abc".as_ptr(), p, 3);
                std::ptr::write_bytes(p.add(3), 0, 5);
            }
        });
        assert_eq!(&s.get(&arena, r)[..3], b"abc");
    }

    #[test]
    fn demoted_slot_stays_readable_until_gc() {
        let mut arena = Arena::new(0);
        let mut s: GenStore<u64> = GenStore::new(&mut arena, 0);
        let a = s.insert(&mut arena, Gen::ALIVE, 123);
        s.demote(&arena, a, 10);
        assert_eq!(*s.get(&arena, a), 123, "demoted data is still readable");
        s.gc(&arena, 9);
        assert!(!s.gen_at(&arena, a).is_free());
        s.gc(&arena, 10);
        assert!(s.gen_at(&arena, a).is_free());
        assert_eq!(s.free_count(), 1);
    }

    #[test]
    fn free_reclaims_immediately_and_reuses() {
        let mut arena = Arena::new(0);
        let mut s: GenStore<u64> = GenStore::new(&mut arena, 0);
        let a = s.insert(&mut arena, Gen::ALIVE, 1);
        let hw = s.len();
        s.free(&arena, a); // immediate, no gc
        assert!(s.gen_at(&arena, a).is_free());
        let b = s.insert(&mut arena, Gen::ALIVE, 2);
        assert_eq!(a, b, "freed slot reused without gc");
        assert_eq!(s.len(), hw);
    }

    #[test]
    fn freed_slots_are_reused() {
        let mut arena = Arena::new(0);
        let mut s: GenStore<u64> = GenStore::new(&mut arena, 0);
        let a = s.insert(&mut arena, Gen::ALIVE, 1);
        let hw = s.len();
        s.demote(&arena, a, 0);
        s.gc(&arena, 0);
        let b = s.insert(&mut arena, Gen::ALIVE, 2);
        assert_eq!(a, b, "freed slot index reused");
        assert_eq!(s.len(), hw, "high-water did not grow on reuse");
        assert_eq!(*s.get(&arena, b), 2);
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)] // loop index < 5000 fits the asserted u32
    fn survives_growth_relocation() {
        // Small min so the store grows (and the arena may relocate the chunk) many times.
        let mut arena = Arena::new(0);
        let mut s: GenStore<[u8; 64]> = GenStore::new(&mut arena, 1);
        let mut refs = Vec::new();
        for i in 0..5000u32 {
            let mut v = [0u8; 64];
            v[..4].copy_from_slice(&i.to_le_bytes());
            refs.push(s.insert(&mut arena, Gen::ALIVE, v));
        }
        for (i, r) in refs.iter().enumerate() {
            let v = s.get(&arena, *r);
            assert_eq!(u32::from_le_bytes(v[..4].try_into().unwrap()), i as u32);
        }
    }

    /// Oracle: random alloc/demote/advance/gc churn matches a reference model, across the
    /// growth relocations that the small initial capacity forces. Deterministic LCG.
    #[test]
    #[allow(clippy::cast_possible_truncation)] // LCG-derived indices, masked by `% len`
    fn oracle_lifecycle_matches_model() {
        let mut arena = Arena::new(0);
        let mut store: GenStore<u64> = GenStore::new(&mut arena, 1);
        let mut model: HashMap<u32, (u64, Option<u64>)> = HashMap::new();
        let mut rng = 0x1234_5678_9abc_def0u64;
        let mut cur_gen = 0u64;
        let lag = 2u64;
        let mut max_live = 0usize;

        for _ in 0..30_000 {
            rng = rng
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            match (rng >> 33) % 8 {
                0..=2 => {
                    let v = rng;
                    let r = store.insert(&mut arena, Gen::ALIVE, v);
                    model.insert(u32::try_from(r.index()).unwrap(), (v, None));
                }
                3 | 4 => {
                    let alive: Vec<u32> = model
                        .iter()
                        .filter(|(_, (_, d))| d.is_none())
                        .map(|(k, _)| *k)
                        .collect();
                    if !alive.is_empty() {
                        let idx = alive[(rng as usize >> 7) % alive.len()];
                        store.demote(&arena, Ref::new(idx), cur_gen);
                        model.get_mut(&idx).unwrap().1 = Some(cur_gen);
                    }
                }
                5 | 6 => cur_gen += 1,
                _ => {
                    let min_live = cur_gen.saturating_sub(lag);
                    store.gc(&arena, min_live);
                    model.retain(|_, (_, d)| match d {
                        Some(g) => !Gen::at(*g).is_reclaimable(min_live),
                        None => true,
                    });
                }
            }
            for (idx, (v, d)) in &model {
                let r: Ref<u64> = Ref::new(*idx);
                assert_eq!(*store.get(&arena, r), *v, "value mismatch at slot {idx}");
                if d.is_some() {
                    assert!(
                        !store.gen_at(&arena, r).is_free(),
                        "kept slot wrongly reclaimed"
                    );
                }
            }
            max_live = max_live.max(model.len());
        }
        assert!(
            store.len() <= max_live + 1,
            "high-water {} near peak {}",
            store.len(),
            max_live
        );
    }
}
