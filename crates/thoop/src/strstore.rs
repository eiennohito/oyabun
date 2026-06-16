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
/// re-copy). Methods thread the owning `&Arena`/`&mut Arena` like `GenStore`.
pub struct StrStore<const N: usize, S> {
    inner: GenStore<[u8; N]>,
    _marker: PhantomData<fn() -> S>,
}

impl<const N: usize, S> StrStore<N, S> {
    #[must_use]
    pub fn new(arena: &mut Arena, min_slots: usize) -> Self {
        Self {
            inner: GenStore::new(arena, min_slots),
            _marker: PhantomData,
        }
    }

    /// Store `bytes` in a fresh slot tagged `tag`. Empty input gets no slot
    /// ([`StringRef::EMPTY`]).
    ///
    /// # Panics
    /// If `bytes` is longer than the slot size `N`.
    pub fn intern(&mut self, arena: &mut Arena, tag: Gen, bytes: &[u8]) -> StringRef<S> {
        if bytes.is_empty() {
            return StringRef::EMPTY;
        }
        assert!(bytes.len() <= N, "string exceeds {N}-byte slot");
        let r = self.inner.construct(arena, tag, |slot| {
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
    pub fn get(&self, arena: &Arena, sr: StringRef<S>) -> &[u8] {
        if sr.is_empty() {
            return &[];
        }
        &self.inner.get(arena, Ref::new(sr.idx))[..sr.len()]
    }

    /// Demote a reference's slot to generation `now`. No-op for the empty ref.
    pub fn demote(&mut self, arena: &Arena, sr: StringRef<S>, now: u64) {
        if !sr.is_empty() {
            self.inner.demote(arena, Ref::new(sr.idx), now);
        }
    }

    /// Reclaim demoted slots whose generation `min_live` has passed.
    pub fn gc(&mut self, arena: &Arena, min_live: u64) {
        self.inner.gc(arena, min_live);
    }

    /// A base capture for store-free, cross-thread resolution.
    #[must_use]
    pub fn resolver(&self, arena: &Arena) -> ByteResolver<S> {
        self.inner.byte_resolver(arena)
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

    #[test]
    fn intern_get_roundtrip_and_empty() {
        let mut arena = Arena::new(0);
        let mut s: StrStore<16, Tag> = StrStore::new(&mut arena, 0);
        let a = s.intern(&mut arena, Gen::ALIVE, b"bash");
        let b = s.intern(&mut arena, Gen::ALIVE, b"");
        assert_eq!(s.get(&arena, a), b"bash");
        assert!(b.is_empty());
        assert_eq!(s.get(&arena, b), b"");
    }

    #[test]
    fn resolver_reads_same_bytes_across_growth() {
        // Tiny min forces growth/relocation; the resolver captured *after* must still read
        // every ref correctly.
        let mut arena = Arena::new(0);
        let mut s: StrStore<256, Tag> = StrStore::new(&mut arena, 1);
        let refs: Vec<_> = (0..2000u32)
            .map(|i| {
                s.intern(
                    &mut arena,
                    Gen::ALIVE,
                    format!("/usr/bin/proc-{i}").as_bytes(),
                )
            })
            .collect();
        let resolver = s.resolver(&arena);
        for (i, r) in refs.iter().enumerate() {
            let expect = format!("/usr/bin/proc-{i}");
            assert_eq!(resolver.resolve(*r), expect.as_bytes());
            assert_eq!(s.get(&arena, *r), expect.as_bytes());
        }
        assert_eq!(resolver.resolve(StringRef::EMPTY), b"");
    }

    #[test]
    fn lease_old_resolver_survives_until_gc() {
        let mut arena = Arena::new(0);
        let mut s: StrStore<32, Tag> = StrStore::new(&mut arena, 8);
        let old = s.intern(&mut arena, Gen::ALIVE, b"old-cmdline");
        let _new = s.intern(&mut arena, Gen::ALIVE, b"new-cmdline");
        s.demote(&arena, old, 5);
        assert_eq!(s.get(&arena, old), b"old-cmdline");
        s.gc(&arena, 4); // below 5 → kept
        assert_eq!(s.get(&arena, old), b"old-cmdline");
        s.gc(&arena, 5); // reached → reclaimable
        assert!(s.free_count() >= 1);
    }
}
