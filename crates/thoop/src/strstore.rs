//! Generational byte-string storage: fixed-size string slots in an [`Arena`] chunk,
//! addressed by a [`StringRef`] that survives across cycles and relocations, and resolved
//! cross-thread by a [`ByteResolver`] that captures the chunk's base at publish time.

use std::marker::PhantomData;

use crate::arena::Arena;
use crate::{Gen, GenStore, Ref};

/// A handle into a [`StrStore<N, S>`]: slot index plus the string's true byte length. The
/// `S` tag (e.g. a `Comm`/`Cmd` marker) makes resolving against the wrong store a compile
/// error. Eight bytes, `Copy`, [`Flat`](crate::Flat) — embeddable in stored records and
/// snapshot rows. The index is stable across growth (the slab relocates, the index does
/// not).
pub struct StringRef<S> {
    idx: u32,
    len: u16,
    _marker: PhantomData<fn() -> S>,
}

impl<S> StringRef<S> {
    /// The empty string — resolves to `&[]` without touching any store.
    pub const EMPTY: StringRef<S> = StringRef {
        idx: 0,
        len: 0,
        _marker: PhantomData,
    };

    #[allow(clippy::cast_possible_truncation)] // len ≤ N ≤ u16::MAX, checked in `intern`
    fn new(idx: u32, len: usize) -> Self {
        Self {
            idx,
            len: len as u16,
            _marker: PhantomData,
        }
    }

    #[must_use]
    pub fn len(self) -> usize {
        self.len as usize
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

impl<S> Clone for StringRef<S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<S> Copy for StringRef<S> {}
impl<S> PartialEq for StringRef<S> {
    fn eq(&self, other: &Self) -> bool {
        self.idx == other.idx && self.len == other.len
    }
}
impl<S> Eq for StringRef<S> {}
impl<S> std::fmt::Debug for StringRef<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StringRef({}, {})", self.idx, self.len)
    }
}

/// A read-only capture of a [`StrStore`]'s chunk base + slot layout, enough to resolve a
/// [`StringRef`] without borrowing the store or its arena. Held by a published snapshot so
/// the UI thread reads strings the gatherer owns. The captured base stays valid for the
/// snapshot's life: after a relocate, the old bytes live on (a regime-A hole, or a
/// regime-B retired region the generational lease keeps until this snapshot is dropped).
pub struct ByteResolver<S> {
    base: *const u8,
    stride: u32,
    data_off: u32,
    _marker: PhantomData<fn() -> S>,
}

// SAFETY: the captured base addresses huge-page bytes that outlive any resolver (live or
// lease-retained); resolution is read-only, no interior mutability.
unsafe impl<S> Send for ByteResolver<S> {}
unsafe impl<S> Sync for ByteResolver<S> {}

impl<S> Clone for ByteResolver<S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<S> Copy for ByteResolver<S> {}

impl<S> ByteResolver<S> {
    /// An empty resolver — resolves only the empty string. The initial value for a snapshot
    /// built before the first publish.
    pub const EMPTY: ByteResolver<S> = ByteResolver {
        base: std::ptr::null(),
        stride: 1,
        data_off: 0,
        _marker: PhantomData,
    };

    #[allow(clippy::cast_possible_truncation)] // stride/offset are small slot-layout constants
    pub(crate) fn new(base: *mut u8, stride: usize, data_off: usize) -> Self {
        Self {
            base: base.cast_const(),
            stride: stride as u32,
            data_off: data_off as u32,
            _marker: PhantomData,
        }
    }

    /// Resolve a reference to its bytes. The empty string short-circuits to `&[]`.
    #[must_use]
    pub fn resolve(&self, sr: StringRef<S>) -> &[u8] {
        let len = sr.len as usize;
        if len == 0 {
            return &[];
        }
        // The `EMPTY` resolver (null base) must only ever resolve the empty string — its
        // sole use is a snapshot built before the first publish. Guard so a stray non-empty
        // resolve is a safe `&[]` in release and a loud failure in debug, never a null deref.
        if self.base.is_null() {
            debug_assert!(
                false,
                "non-empty StringRef resolved on an empty ByteResolver"
            );
            return &[];
        }
        let off = sr.idx as usize * self.stride as usize + self.data_off as usize;
        // SAFETY: `off + len` is within slot `sr.idx`'s data (len ≤ slot size); the base
        // outlives this resolver.
        unsafe { std::slice::from_raw_parts(self.base.add(off), len) }
    }
}

/// A generational store of fixed-`N`-byte string slots, tagged `S`, over a
/// [`GenStore<[u8; N]>`]. [`intern`](Self::intern) copies bytes straight into a slot (no
/// stack temporary); an unchanged string keeps its slot across cycles (no per-cycle
/// re-copy). Like `GenStore` it caches its base and phones the arena to grow, so it must be
/// [`wire`](Self::wire)d after reaching its final address. Neither `Send` nor `Sync`.
pub struct StrStore<const N: usize, S> {
    inner: GenStore<[u8; N]>,
    _marker: PhantomData<fn() -> S>,
}

impl<const N: usize, S> StrStore<N, S> {
    #[must_use]
    pub fn new(arena: &Arena, min_slots: usize) -> Self {
        Self {
            inner: GenStore::new(arena, min_slots),
            _marker: PhantomData,
        }
    }

    /// Wire to the arena after final placement (see [`GenStore::wire`]). Call once, before
    /// any string is interned or read.
    pub fn wire(&mut self, arena: &Arena) {
        self.inner.wire(arena);
    }

    /// Store `bytes` in a fresh slot tagged `tag`. Empty input gets no slot
    /// ([`StringRef::EMPTY`]).
    ///
    /// # Panics
    /// If `bytes` is longer than the slot size `N`.
    pub fn intern(&mut self, tag: Gen, bytes: &[u8]) -> StringRef<S> {
        if bytes.is_empty() {
            return StringRef::EMPTY;
        }
        assert!(bytes.len() <= N, "string exceeds {N}-byte slot");
        let r = self.inner.construct(tag, |slot| {
            // SAFETY: slot holds N bytes, `bytes.len() ≤ N`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    slot.as_mut_ptr().cast::<u8>(),
                    bytes.len(),
                );
            }
        });
        StringRef::new(slot_index(r), bytes.len())
    }

    /// The bytes a reference points to (an empty ref → `&[]`).
    #[must_use]
    pub fn get(&self, sr: StringRef<S>) -> &[u8] {
        if sr.is_empty() {
            return &[];
        }
        &self.inner.get(Ref::new(sr.idx))[..sr.len()]
    }

    /// Demote a reference's slot to generation `now`. No-op for the empty ref.
    pub fn demote(&mut self, sr: StringRef<S>, now: u64) {
        if !sr.is_empty() {
            self.inner.demote(Ref::new(sr.idx), now);
        }
    }

    /// Immediately reclaim a reference's slot to the free list — no lease. No-op for the
    /// empty ref (which owns no slot). For consumers with no cross-thread reader holding a
    /// stale handle: a changed/dead string's slot is reusable at once (the [`demote`] +
    /// [`gc`] path is for snapshot-leased data instead).
    ///
    /// [`demote`]: Self::demote
    /// [`gc`]: Self::gc
    pub fn free(&mut self, sr: StringRef<S>) {
        if !sr.is_empty() {
            self.inner.free(Ref::new(sr.idx));
        }
    }

    /// Reclaim demoted slots whose generation `min_live` has passed.
    pub fn gc(&mut self, min_live: u64) {
        self.inner.gc(min_live);
    }

    /// A base capture for store-free, cross-thread resolution.
    #[must_use]
    pub fn resolver(&self) -> ByteResolver<S> {
        self.inner.byte_resolver()
    }

    /// Slots ever handed out (high-water) — grows with distinct live strings, not cycles.
    #[must_use]
    pub fn slot_count(&self) -> usize {
        self.inner.len()
    }

    /// Slots currently free (reclaimed, awaiting reuse).
    #[must_use]
    pub fn free_count(&self) -> usize {
        self.inner.free_count()
    }
}

/// The bare slot index of a typed [`Ref`] — `StringRef` keeps only the index (plus its own
/// byte length), since the slot's element type is fixed by the owning [`StrStore`].
fn slot_index<const N: usize>(r: Ref<[u8; N]>) -> u32 {
    u32::try_from(r.index()).expect("slot index fits u32")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tag;

    /// Wire a store to `arena`. The store points at `arena` (which must stay put); the store
    /// itself may move.
    fn wired<const N: usize, S>(arena: &Arena, min_slots: usize) -> StrStore<N, S> {
        let mut s = StrStore::new(arena, min_slots);
        s.wire(arena);
        s
    }

    #[test]
    fn intern_get_roundtrip_and_empty() {
        let arena = Arena::new(0);
        let mut s: StrStore<16, Tag> = wired(&arena, 0);
        let a = s.intern(Gen::ALIVE, b"bash");
        let b = s.intern(Gen::ALIVE, b"");
        assert_eq!(s.get(a), b"bash");
        assert!(b.is_empty());
        assert_eq!(s.get(b), b"");
    }

    #[test]
    fn resolver_reads_same_bytes_across_growth() {
        // Tiny min forces growth/relocation; the resolver captured *after* must still read
        // every ref correctly.
        let arena = Arena::new(0);
        let mut s: StrStore<256, Tag> = wired(&arena, 1);
        let refs: Vec<_> = (0..2000u32)
            .map(|i| s.intern(Gen::ALIVE, format!("/usr/bin/proc-{i}").as_bytes()))
            .collect();
        let resolver = s.resolver();
        for (i, r) in refs.iter().enumerate() {
            let expect = format!("/usr/bin/proc-{i}");
            assert_eq!(resolver.resolve(*r), expect.as_bytes());
            assert_eq!(s.get(*r), expect.as_bytes());
        }
        assert_eq!(resolver.resolve(StringRef::EMPTY), b"");
    }

    #[test]
    fn free_reclaims_immediately_and_ignores_empty() {
        let arena = Arena::new(0);
        let mut s: StrStore<32, Tag> = wired(&arena, 4);
        let a = s.intern(Gen::ALIVE, b"first");
        s.free(a);
        assert_eq!(
            s.free_count(),
            1,
            "freed slot returns to the free list at once"
        );
        let b = s.intern(Gen::ALIVE, b"second");
        assert_eq!(s.get(b), b"second");
        assert_eq!(s.free_count(), 0, "freed slot reused without gc");
        s.free(StringRef::EMPTY); // owns no slot → no-op, must not panic / free slot 0
        assert_eq!(s.free_count(), 0);
    }

    #[test]
    fn lease_old_resolver_survives_until_gc() {
        let arena = Arena::new(0);
        let mut s: StrStore<32, Tag> = wired(&arena, 8);
        let old = s.intern(Gen::ALIVE, b"old-cmdline");
        let _new = s.intern(Gen::ALIVE, b"new-cmdline");
        s.demote(old, 5);
        assert_eq!(s.get(old), b"old-cmdline");
        s.gc(4); // below 5 → kept
        assert_eq!(s.get(old), b"old-cmdline");
        s.gc(5); // reached → reclaimable
        assert!(s.free_count() >= 1);
    }
}
