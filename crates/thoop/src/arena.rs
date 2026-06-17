//! A huge-page sub-allocator: stores carve chunks from **one** shared THP region instead of
//! each `mmap`-ing its own (with THP a touched 2 MiB mapping commits a full huge page, so a
//! mapping per store wastes memory — ~5× at a dozen stores). Growth replaces that single
//! region (regime B), so steady state is one live region plus any not-yet-reclaimed retired
//! ones. The arena hands out [`ChunkId`]s; its chunk table is the source of truth for where
//! each chunk lives.
//!
//! **Interior-mutable, accessed by `&self`.** The arena owns no slot data — only the region,
//! the chunk table, and retirement bookkeeping — so every operation (even allocating and
//! relocating) goes through `&self` with the mutable state behind a `RefCell`. A store
//! therefore holds a plain `*const Arena` and never needs `&mut Arena`, which sidesteps the
//! self-referential-borrow problem: many stores can share one arena and grow through it.
//!
//! **Self-healing bases (the relocation protocol).** A store caches its chunk's base pointer
//! so hot access is `base + idx·stride` with no chunk-table chase. A relocation can move
//! *other* stores' chunks too, so the arena carries an [`epoch`](Self::epoch) counter, bumped
//! whenever a relocation moves chunks the caller didn't ask to grow (a regime-B repack). A
//! store re-reads its own base from the arena the next time it sees the epoch advance — it
//! heals itself; nothing reaches into it. The epoch lives outside the `RefCell` so the
//! hot-path check is a plain `Cell` load.
//!
//! Two growth regimes:
//! - **A (common, cheap):** a chunk outgrows its slot → bump a larger copy from the region's
//!   free tail, leaving the old bytes as a frozen *hole*. Only the growing chunk moves, so
//!   the epoch is *not* bumped (the caller adopts [`grow`](Self::grow)'s return value; no
//!   other store is affected). The hole stays readable by snapshots that captured its old
//!   location, and is swept by the next B.
//! - **B (rare):** the region's tail is exhausted → allocate a fresh region, **repack** all
//!   chunks into it (compacting holes), **retire** the old region, and bump the epoch so
//!   every other store heals its base on next use. The old region is freed by
//!   [`gc`](Self::gc) once `min_live` passes the generation it was retired at — the same
//!   generational lease that reclaims slots, so a snapshot still reading the old region
//!   (cross-thread) keeps it alive exactly as long as needed.

use std::cell::{Cell, RefCell};

use crate::region::MmapRegion;

/// A stable handle to a chunk. The arena may relocate the chunk's bytes (growth), but the
/// `ChunkId` never changes — resolution goes through the arena's table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChunkId(pub(crate) u32);

#[derive(Clone, Copy)]
struct ChunkMeta {
    off: usize,
    len: usize,
    align: usize,
}

/// Fraction of headroom to add when repacking, so B does not re-trigger immediately.
const HEADROOM_NUM: usize = 1;
const HEADROOM_DEN: usize = 4; // +25%

fn align_up(n: usize, a: usize) -> usize {
    debug_assert!(n <= usize::MAX - (a - 1), "align_up overflow");
    (n + a - 1) & !(a - 1)
}

/// The arena's mutable state, guarded by a `RefCell` so the public API is `&self`.
struct Inner {
    region: MmapRegion,
    chunks: Vec<ChunkMeta>,
    /// Bump cursor within `region`.
    tail: usize,
    /// Generation stamped on regions retired by a repack; set by the owner each cycle.
    cur_gen: u64,
    /// Regions retired by B, awaiting lease expiry (`gc`). Each tagged with the generation
    /// at retirement.
    retired: Vec<(MmapRegion, u64)>,
}

pub struct Arena {
    /// Relocation counter. Bumped on every repack (regime B) that moves chunks the caller did
    /// not grow. Stores compare it on each access to self-heal a stale cached base; kept
    /// outside `inner` so the read is a plain `Cell` load, not a `RefCell` borrow.
    epoch: Cell<u64>,
    inner: RefCell<Inner>,
}

// `Arena` is interior-mutable (`Cell`/`RefCell`) and part of a self-referential cluster
// (stores hold `*const Arena`), so it is deliberately neither `Send` nor `Sync` — it lives
// and dies on one thread. Cross-thread access is via lease-based view primitives
// (`ByteResolver`), never the arena.

impl Arena {
    #[must_use]
    pub fn new(min_bytes: usize) -> Self {
        Self {
            epoch: Cell::new(0),
            inner: RefCell::new(Inner {
                region: MmapRegion::huge(min_bytes),
                chunks: Vec::new(),
                tail: 0,
                cur_gen: 0,
                retired: Vec::new(),
            }),
        }
    }

    /// Stamp the generation that subsequent retirements (B repacks) are tagged with — the
    /// generation of the snapshot currently being built. Call once at the start of a cycle.
    pub fn set_gen(&self, now: u64) {
        self.inner.borrow_mut().cur_gen = now;
    }

    /// The current relocation epoch. A store re-reads its base when this advances past the
    /// value it last saw.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch.get()
    }

    /// Allocate a chunk of `len` bytes at `align`. Triggers a repack (B) first if the
    /// region's tail cannot fit it — which relocates existing chunks, so the epoch is bumped.
    ///
    /// # Panics
    /// If the live chunk count would exceed `u32::MAX` (a `ChunkId` is a `u32`).
    pub fn alloc(&self, len: usize, align: usize) -> ChunkId {
        let mut inner = self.inner.borrow_mut();
        if align_up(inner.tail, align) + len > inner.region.len() {
            inner.relayout(None, len, align);
            self.epoch.set(self.epoch.get() + 1); // every existing chunk moved
        }
        let off = align_up(inner.tail, align);
        inner.tail = off + len;
        let id = ChunkId(u32::try_from(inner.chunks.len()).expect("chunk count fits u32"));
        inner.chunks.push(ChunkMeta { off, len, align });
        id
    }

    /// Grow chunk `id` to `new_len` bytes, preserving its contents, and **return its new
    /// base**. The growing caller adopts this return value directly. Regime A moves only this
    /// chunk (epoch unchanged); a regime-B repack relocates everything and bumps the epoch so
    /// other stores heal on next use.
    pub fn grow(&self, id: ChunkId, new_len: usize) -> *mut u8 {
        let mut inner = self.inner.borrow_mut();
        let m = inner.chunks[id.0 as usize];
        debug_assert!(new_len >= m.len, "grow must not shrink");
        let off = align_up(inner.tail, m.align);
        if off + new_len <= inner.region.len() {
            // Regime A: relocate within the region; the old extent becomes a frozen hole.
            // SAFETY: source/dest are disjoint ranges inside the live region; `m.len` bytes
            // were initialised.
            unsafe {
                let base = inner.region.as_ptr();
                std::ptr::copy_nonoverlapping(base.add(m.off), base.add(off), m.len);
            }
            inner.chunks[id.0 as usize] = ChunkMeta {
                off,
                len: new_len,
                align: m.align,
            };
            inner.tail = off + new_len;
            // SAFETY: `off` is within the region (just placed there).
            unsafe { inner.region.as_ptr().add(off) }
        } else {
            inner.relayout(Some((id, new_len)), 0, 1);
            self.epoch.set(self.epoch.get() + 1);
            // SAFETY: `id`'s offset within the freshly repacked region.
            unsafe { inner.region.as_ptr().add(inner.chunks[id.0 as usize].off) }
        }
    }

    /// Current base pointer of a chunk's bytes. A store calls this only to (re)cache its base
    /// — on wiring or when the epoch advanced — never per access.
    #[must_use]
    pub fn base_of(&self, id: ChunkId) -> *mut u8 {
        let inner = self.inner.borrow();
        // SAFETY: `off` is within the region (maintained by alloc/relayout).
        unsafe { inner.region.as_ptr().add(inner.chunks[id.0 as usize].off) }
    }

    #[must_use]
    pub fn len_of(&self, id: ChunkId) -> usize {
        self.inner.borrow().chunks[id.0 as usize].len
    }

    /// Free regions retired by B once `min_live` has **reached or passed** the generation
    /// they were retired at (`retired_at <= min_live`) — i.e. no live snapshot can still be
    /// reading them. Same lease (and same inclusive boundary) as slot reclamation.
    pub fn gc(&self, min_live: u64) {
        self.inner
            .borrow_mut()
            .retired
            .retain(|(_, retired_at)| *retired_at > min_live);
    }

    /// Bytes currently committed to live + retired regions (diagnostic/metrics).
    #[must_use]
    pub fn mapped_bytes(&self) -> usize {
        let inner = self.inner.borrow();
        inner.region.len() + inner.retired.iter().map(|(r, _)| r.len()).sum::<usize>()
    }

    #[cfg(test)]
    fn live_bytes(&self) -> usize {
        self.inner.borrow().region.len()
    }
}

impl Inner {
    /// Repack all chunks into a fresh region (B): compact away holes, optionally growing one
    /// chunk to `grow`'s new length and/or reserving tail room for a pending `alloc`. The old
    /// region is retired at the current generation.
    fn relayout(
        &mut self,
        grow: Option<(ChunkId, usize)>,
        reserve_len: usize,
        reserve_align: usize,
    ) {
        // Target byte length per chunk after compaction (one chunk may be grown).
        let targets: Vec<usize> = (0..self.chunks.len())
            .map(|i| match grow {
                Some((gid, nl)) if gid.0 as usize == i => nl,
                _ => self.chunks[i].len,
            })
            .collect();

        let mut total = 0usize;
        for (i, &len) in targets.iter().enumerate() {
            total = align_up(total, self.chunks[i].align) + len;
        }
        if reserve_len > 0 {
            total = align_up(total, reserve_align) + reserve_len;
        }
        total += total / HEADROOM_DEN * HEADROOM_NUM;

        let new_region = MmapRegion::huge(total);
        let old_ptr = self.region.as_ptr();
        let new_ptr = new_region.as_ptr();
        let mut cur = 0usize;
        for (i, &len) in targets.iter().enumerate() {
            let m = self.chunks[i];
            cur = align_up(cur, m.align);
            // SAFETY: old chunk holds `m.len` initialised bytes; new region has room for
            // `len ≥ m.len` at `cur` (sized above); regions are disjoint.
            unsafe {
                std::ptr::copy_nonoverlapping(old_ptr.add(m.off), new_ptr.add(cur), m.len);
            }
            self.chunks[i] = ChunkMeta {
                off: cur,
                len,
                align: m.align,
            };
            cur += len;
        }
        self.tail = cur;
        let old = std::mem::replace(&mut self.region, new_region);
        self.retired.push((old, self.cur_gen));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_base_and_len() {
        let a = Arena::new(0);
        let c = a.alloc(100, 8);
        assert_eq!(a.len_of(c), 100);
        // Writable, readable.
        // SAFETY: 100 bytes at base.
        unsafe {
            let p = a.base_of(c);
            p.write(0xAB);
            assert_eq!(*p, 0xAB);
        }
    }

    #[test]
    fn grow_regime_a_relocates_in_region_and_leaves_epoch() {
        let a = Arena::new(0); // 2 MiB region — room for a tail relocate
        let c = a.alloc(16, 1);
        // SAFETY: writing 5 of the 16 bytes.
        unsafe { std::ptr::copy_nonoverlapping(b"hello".as_ptr(), a.base_of(c), 5) };
        let before = a.mapped_bytes();
        let epoch_before = a.epoch();
        a.grow(c, 64);
        assert_eq!(a.mapped_bytes(), before, "regime A retires no region");
        assert_eq!(
            a.epoch(),
            epoch_before,
            "regime A moves no siblings → no epoch bump"
        );
        // SAFETY: the bytes are preserved across the relocate.
        let bytes = unsafe { std::slice::from_raw_parts(a.base_of(c), 5) };
        assert_eq!(bytes, b"hello");
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)] // test chunk indices < 7
    fn regime_b_repacks_all_chunks_and_bumps_epoch() {
        // Fill most of the region with chunks holding distinct bytes, leaving no tail room
        // for the grow below → forces a repack (B).
        let a = Arena::new(0);
        let region0 = a.live_bytes();
        let chunk_len = region0 / 8;
        let mut ids = Vec::new();
        for i in 0..7u8 {
            let id = a.alloc(chunk_len, 1);
            // SAFETY: chunk_len bytes.
            unsafe { std::ptr::write_bytes(a.base_of(id), i, chunk_len) };
            ids.push(id);
        }
        a.set_gen(5);
        let epoch_before = a.epoch();
        // Grow one chunk past the tail → forces regime B (repack into a new region).
        a.grow(ids[0], chunk_len * 2);
        assert_eq!(a.epoch(), epoch_before + 1, "a repack must bump the epoch");
        assert!(
            a.mapped_bytes() > a.live_bytes(),
            "tail exhausted — expected a repack that retires the old region"
        );
        // Every chunk's data survived the repack, addressed by its unchanged ChunkId.
        for (i, id) in ids.iter().enumerate() {
            // SAFETY: chunk still holds at least its original chunk_len bytes.
            let b = unsafe { *a.base_of(*id) };
            assert_eq!(b, i as u8, "chunk {i} data lost across repack");
        }
        a.gc(4);
        assert!(
            a.mapped_bytes() > a.live_bytes(),
            "min_live 4 < 5 keeps the old region"
        );
        a.gc(5);
        assert_eq!(
            a.mapped_bytes(),
            a.live_bytes(),
            "min_live 5 frees the old region"
        );
    }
}
