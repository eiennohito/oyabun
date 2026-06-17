//! THP-backed generational storage primitives.
//!
//! Hot, randomly-accessed records and resettable row buffers live here on huge pages, so
//! a working set fits in a handful of TLB entries regardless of element count — TLB-miss-
//! free *by construction*, a property that holds at any scale independent of what a given
//! box lets you measure. Lifecycle is generation-tracked ([`Gen`]): data stays alive
//! while any reader leases it, reclaimable once every referencing reader is gone.
//!
//! Layering:
//! - [`MmapRegion`] (Layer 0) — owned `mmap`, 2 MiB-aligned, `MADV_HUGEPAGE`-hinted.
//! - [`Arena`] — huge-page sub-allocator: many structures share a few regions (one
//!   `mmap`-per-structure would commit a full huge page each). Hands out [`ChunkId`]s;
//!   growth relocates chunks (transparently — holders keep their `ChunkId`).
//! - [`GenStore`] / [`StrStore`] / [`TypedBuf`] / [`ThpMap`] — live in arena chunks; their
//!   growing methods take `&mut Arena`, hot reads take `&Arena`.
//!
//! Inter-region references are typed [`Ref`] handles (slot indices, stable across
//! relocation), never raw pointers — a handle resolves only against the store that issued
//! it, checked at compile time by its type parameter. Cross-thread readers resolve through
//! a [`ByteResolver`] that captures a chunk's base at publish, so it stays valid for the
//! snapshot's life even if the store later relocates.

mod arena;
mod genstore;
mod region;
mod strstore;
mod thpmap;
mod typedbuf;

pub use arena::{Arena, ChunkId};
pub use genstore::GenStore;
pub use region::{HUGE_PAGE, MmapRegion};
pub use strstore::{ByteResolver, StrStore, StringRef};
pub use thpmap::{MapKey, ThpMap};
pub use typedbuf::TypedBuf;

use std::marker::PhantomData;

/// "Safe to store in a THP region": fixed-size and bitwise-copyable, so a slot can be
/// recycled with a raw byte overwrite and no `Drop` is ever skipped.
///
/// The `Copy` bound *is* the enforcement: `Vec`/`String`/`Box` are not `Copy`, so a type
/// owning heap memory cannot be `Flat` and cannot land in a store. The only contract a
/// reviewer must still uphold is that any raw-pointer field (which `Copy` permits) points
/// into a THP store, not the heap — typed [`Ref`] handles exist precisely so that case
/// does not arise in practice.
pub trait Flat: Copy {}
impl<T: Copy> Flat for T {}

/// A typed slot index into a [`GenStore<T>`]. Four bytes, `Copy`, `Flat` — embeddable in
/// other stored records. The type parameter makes a `Ref<A>` a compile error wherever a
/// `Ref<B>` is expected, so a handle can only ever resolve against the store that issued
/// it. `PhantomData<fn() -> T>` keeps `Ref<T>` unconditionally `Send`/`Sync`/`Copy`
/// regardless of `T`.
pub struct Ref<T> {
    idx: u32,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Ref<T> {
    /// The null handle (no slot). Distinct from any real index.
    pub const NONE: Ref<T> = Ref {
        idx: u32::MAX,
        _marker: PhantomData,
    };

    pub(crate) fn new(idx: u32) -> Self {
        Self {
            idx,
            _marker: PhantomData,
        }
    }

    #[must_use]
    pub fn is_none(self) -> bool {
        self.idx == u32::MAX
    }

    pub(crate) fn index(self) -> usize {
        self.idx as usize
    }
}

// Manual impls: deriving would add a spurious `T: Clone/Copy/...` bound, but a `Ref` is
// just a u32 tag and is valid for any `T`.
impl<T> Clone for Ref<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Ref<T> {}
impl<T> PartialEq for Ref<T> {
    fn eq(&self, other: &Self) -> bool {
        self.idx == other.idx
    }
}
impl<T> Eq for Ref<T> {}
impl<T> std::fmt::Debug for Ref<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_none() {
            write!(f, "Ref(NONE)")
        } else {
            write!(f, "Ref({})", self.idx)
        }
    }
}

/// A slot's lifecycle tag, packed into one byte: two sentinels plus a wrapping range of
/// *demoted* generations.
///
/// - [`ALIVE`](Self::ALIVE) — immortal; GC skips it unconditionally. Steady state for a
///   stable record and its strings.
/// - [`FREE`](Self::FREE) — on the free list, available for allocation.
/// - [`at(now)`](Self::at) — demoted at generation `now`: still readable (a live reader
///   may lease it) but reclaimable once `min_live` advances past it.
///
/// Only recently-released slots occupy the demoted range, so GC cost is proportional to
/// churn, not population. The demoted range is 254 values wide (the two sentinels take the
/// rest); at a ~2 Hz cadence that is ~127 generations of unambiguous wrapping comparison —
/// vastly more than a live window of two readers. All sentinel and wrapping logic lives
/// here so callers never see a raw byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Gen(u8);

/// Free-list sentinel byte.
const G_FREE: u8 = 0;
/// Immortal sentinel byte.
const G_ALIVE: u8 = 1;
/// First byte value used for a demoted generation (`DEMOTED_BASE..=255`).
const DEMOTED_BASE: u8 = 2;
/// Count of distinct demoted generation values (`256 - 2` sentinels).
const DEMOTED_SPAN: u16 = 256 - DEMOTED_BASE as u16;

impl Gen {
    pub const FREE: Gen = Gen(G_FREE);
    pub const ALIVE: Gen = Gen(G_ALIVE);

    /// A slot demoted at generation `now`. Maps `now` into the wrapping demoted range.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // `now % span` < 254, fits u8
    pub fn at(now: u64) -> Gen {
        let span = u64::from(DEMOTED_SPAN);
        Gen(DEMOTED_BASE + (now % span) as u8)
    }

    #[must_use]
    pub fn is_free(self) -> bool {
        self.0 == G_FREE
    }

    #[must_use]
    pub fn is_alive(self) -> bool {
        self.0 == G_ALIVE
    }

    /// The demoted generation's position within the wrapping range, or `None` for the
    /// `ALIVE`/`FREE` sentinels.
    fn demoted_pos(self) -> Option<u16> {
        if self.0 >= DEMOTED_BASE {
            Some(u16::from(self.0 - DEMOTED_BASE))
        } else {
            None
        }
    }

    /// Whether this demoted slot may be reclaimed given `min_live` — the oldest generation
    /// any still-live reader could reference. Sentinels (`ALIVE`/`FREE`) are never
    /// reclaimable. A demoted generation `D` is reclaimable once `min_live` has reached or
    /// passed it (wrapping): every reader that could hold this slot was built before its
    /// replacement and has since been released.
    ///
    /// The caller chooses how conservative to be by how far it lags `min_live` behind the
    /// current generation (≥ the live window; more is free given the headroom).
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // `min_live % span` < 254, fits u16
    pub fn is_reclaimable(self, min_live: u64) -> bool {
        let Some(slot) = self.demoted_pos() else {
            return false;
        };
        let min = (min_live % u64::from(DEMOTED_SPAN)) as u16;
        // Forward cyclic distance slot → min over the 254-wide ring. A small distance
        // means `min` is at or ahead of `slot` (slot is old) → reclaimable; a distance in
        // the far half means `slot` is "ahead" of `min` (a generation not yet reached,
        // i.e. still in or beyond the live window) → keep.
        let dist = (min + DEMOTED_SPAN - slot) % DEMOTED_SPAN;
        dist < DEMOTED_SPAN / 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinels_are_distinct_and_not_demoted() {
        assert!(Gen::FREE.is_free());
        assert!(!Gen::FREE.is_alive());
        assert!(Gen::ALIVE.is_alive());
        assert!(!Gen::ALIVE.is_free());
        for min in [0u64, 1, 100, 1_000_000] {
            assert!(!Gen::FREE.is_reclaimable(min));
            assert!(!Gen::ALIVE.is_reclaimable(min));
        }
    }

    #[test]
    fn demoted_reclaims_once_min_live_reaches_it() {
        // Demoted at gen 10. With a 2-gen live window the caller passes min_live = gen-2.
        let g = Gen::at(10);
        assert!(!g.is_reclaimable(8), "gen 10 still live when min_live=8");
        assert!(!g.is_reclaimable(9), "gen 10 still live when min_live=9");
        assert!(
            g.is_reclaimable(10),
            "ok to reclaim the released generation"
        );
        assert!(g.is_reclaimable(11), "older still reclaims");
    }

    #[test]
    fn demoted_comparison_wraps() {
        // Across the u8 wrap: demoted at a high generation, min_live just past it.
        let span = u64::from(DEMOTED_SPAN);
        let now = span * 5 - 1; // near the top of the ring
        let g = Gen::at(now);
        assert!(!g.is_reclaimable(now - 1));
        assert!(g.is_reclaimable(now));
        assert!(g.is_reclaimable(now + 1)); // min_live wrapped past the sentinels
        assert!(g.is_reclaimable(now + 5));
    }

    #[test]
    fn ref_none_is_distinct() {
        let n: Ref<u64> = Ref::NONE;
        assert!(n.is_none());
        let r: Ref<u64> = Ref::new(0);
        assert!(!r.is_none());
        assert_ne!(r, n);
        assert_eq!(r, Ref::new(0));
    }
}
