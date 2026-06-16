//! A huge-page sub-allocator: stores carve chunks from **one** shared THP region instead of
//! each `mmap`-ing its own (with THP a touched 2 MiB mapping commits a full huge page, so a
//! mapping per store wastes memory — ~5× at a dozen stores). Growth replaces that single
//! region (regime B), so steady state is one live region plus any not-yet-reclaimed retired
//! ones. The arena hands out [`ChunkId`]s; its chunk table is the single source of truth for
//! where each chunk lives, so growth (which relocates) is transparent to holders — they keep
//! their `ChunkId`, never a raw base.
//!
//! Two growth regimes:
//! - **A (common, cheap):** a chunk outgrows its slot → bump a larger copy from the
//!   region's free tail, leaving the old bytes as a frozen *hole*. The hole stays readable
//!   by snapshots that captured its old location, and is swept by the next B.
//! - **B (rare):** the region's tail is exhausted → allocate a fresh region, **repack** all
//!   chunks into it (compacting away holes), and **retire** the old region. The old region
//!   is freed by [`gc`](Self::gc) once `min_live` passes the generation it was retired at —
//!   the same generational lease that reclaims slots, so a snapshot still reading the old
//!   region (cross-thread) keeps it alive exactly as long as needed.

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

pub struct Arena {
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

impl Arena {
    #[must_use]
    pub fn new(min_bytes: usize) -> Self {
        Self {
            region: MmapRegion::huge(min_bytes),
            chunks: Vec::new(),
            tail: 0,
            cur_gen: 0,
            retired: Vec::new(),
        }
    }

    /// Stamp the generation that subsequent retirements (B repacks) are tagged with — the
    /// generation of the snapshot currently being built. Call once at the start of a cycle.
    pub fn set_gen(&mut self, now: u64) {
        self.cur_gen = now;
    }

    /// Allocate a chunk of `len` bytes at `align`. Triggers a repack (B) first if the
    /// region's tail cannot fit it.
    ///
    /// # Panics
    /// If the live chunk count would exceed `u32::MAX` (a `ChunkId` is a `u32`).
    pub fn alloc(&mut self, len: usize, align: usize) -> ChunkId {
        if align_up(self.tail, align) + len > self.region.len() {
            self.relayout(None, len, align);
        }
        let off = align_up(self.tail, align);
        self.tail = off + len;
        let id = ChunkId(u32::try_from(self.chunks.len()).expect("chunk count fits u32"));
        self.chunks.push(ChunkMeta { off, len, align });
        id
    }

    /// Grow chunk `id` to `new_len` bytes, preserving its current contents. Returns `true`
    /// if this triggered a repack (B) — the caller's other chunks moved too, but all
    /// `ChunkId`s remain valid. `false` means a cheap in-region relocate (A).
    pub fn grow(&mut self, id: ChunkId, new_len: usize) -> bool {
        let m = self.chunks[id.0 as usize];
        debug_assert!(new_len >= m.len, "grow must not shrink");
        let off = align_up(self.tail, m.align);
        if off + new_len <= self.region.len() {
            // Regime A: relocate within the region; the old extent becomes a frozen hole.
            // SAFETY: source/dest are disjoint ranges inside the live region; `m.len` bytes
            // were initialised.
            unsafe {
                let base = self.region.as_ptr();
                std::ptr::copy_nonoverlapping(base.add(m.off), base.add(off), m.len);
            }
            self.chunks[id.0 as usize] = ChunkMeta {
                off,
                len: new_len,
                align: m.align,
            };
            self.tail = off + new_len;
            false
        } else {
            self.relayout(Some((id, new_len)), 0, 1);
            true
        }
    }

    /// Current base pointer of a chunk's bytes. Stable only until the next growth — never
    /// cache it across an `alloc`/`grow`; re-fetch (gatherer side) or capture it in a
    /// resolver at publish (reader side).
    #[must_use]
    pub fn base_of(&self, id: ChunkId) -> *mut u8 {
        // SAFETY: `off` is within the region (maintained by alloc/relayout).
        unsafe { self.region.as_ptr().add(self.chunks[id.0 as usize].off) }
    }

    #[must_use]
    pub fn len_of(&self, id: ChunkId) -> usize {
        self.chunks[id.0 as usize].len
    }

    /// Repack all chunks into a fresh region (B): compact away holes, optionally growing one
    /// chunk to `grow`'s new length and/or reserving tail room for a pending `alloc`. The
    /// old region is retired at the current generation.
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

    /// Free regions retired by B once `min_live` has **reached or passed** the generation
    /// they were retired at (`retired_at <= min_live`) — i.e. no live snapshot can still be
    /// reading them. Same lease (and same inclusive boundary) as slot reclamation.
    pub fn gc(&mut self, min_live: u64) {
        self.retired
            .retain(|(_, retired_at)| *retired_at > min_live);
    }

    /// Bytes currently committed to live + retired regions (diagnostic/metrics).
    #[must_use]
    pub fn mapped_bytes(&self) -> usize {
        self.region.len() + self.retired.iter().map(|(r, _)| r.len()).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_base_and_len() {
        let mut a = Arena::new(0);
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
    fn grow_regime_a_relocates_in_region_preserving_bytes() {
        let mut a = Arena::new(0); // 2 MiB region — room for a tail relocate
        let c = a.alloc(16, 1);
        // SAFETY: writing 5 of the 16 bytes.
        unsafe { std::ptr::copy_nonoverlapping(b"hello".as_ptr(), a.base_of(c), 5) };
        let repacked = a.grow(c, 64);
        assert!(!repacked, "should fit in the region tail (regime A)");
        // SAFETY: the bytes are preserved across the relocate.
        let bytes = unsafe { std::slice::from_raw_parts(a.base_of(c), 5) };
        assert_eq!(bytes, b"hello");
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)] // test chunk indices < 7
    fn regime_b_repacks_all_chunks_preserving_data_and_ids() {
        // Fill most of the region with chunks holding distinct bytes, leaving no tail room
        // for the grow below → forces a repack (B).
        let mut a = Arena::new(0);
        let region0 = a.region.len();
        let chunk_len = region0 / 8;
        let mut ids = Vec::new();
        for i in 0..7u8 {
            let id = a.alloc(chunk_len, 1);
            // SAFETY: chunk_len bytes.
            unsafe { std::ptr::write_bytes(a.base_of(id), i, chunk_len) };
            ids.push(id);
        }
        a.set_gen(5);
        // Grow one chunk past the tail → forces regime B (repack into a new region).
        let repacked = a.grow(ids[0], chunk_len * 2);
        assert!(repacked, "tail exhausted — expected a repack");
        // Every chunk's data survived the repack, addressed by its unchanged ChunkId.
        for (i, id) in ids.iter().enumerate() {
            // SAFETY: chunk still holds at least its original chunk_len bytes.
            let b = unsafe { *a.base_of(*id) };
            assert_eq!(b, i as u8, "chunk {i} data lost across repack");
        }
        // Old region retired at gen 5; held until min_live passes it.
        assert!(
            a.mapped_bytes() > a.region.len(),
            "old region retained pre-gc"
        );
        a.gc(4);
        assert!(
            a.mapped_bytes() > a.region.len(),
            "min_live 4 < 5 keeps the old region"
        );
        a.gc(5);
        assert_eq!(
            a.mapped_bytes(),
            a.region.len(),
            "min_live 5 frees the old region"
        );
    }
}
