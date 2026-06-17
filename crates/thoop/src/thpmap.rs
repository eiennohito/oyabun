//! Open-addressing hash map of [`MapKey`] keys → [`Flat`] values, resident in one [`Arena`]
//! chunk — so even a randomly-probed lookup table lives on huge pages, TLB-friendly at any
//! key count (the access pattern a heap `HashMap` thrashes the dTLB on).
//!
//! **Robin Hood + backward-shift deletion.** Probe sequences are kept short and low-variance
//! by displacing richer entries on insert; deletion shifts the following run back rather than
//! leaving a tombstone. That matters because a consumer may evict in bulk every cycle
//! ([`retain`](ThpMap::retain)) — tombstones would accumulate and degrade probing, backward
//! shift never does.
//!
//! **The key is a trait, not a fixed type.** A [`MapKey`] is a flat, equality-comparable value
//! that supplies a fast hash and reserves one **empty sentinel** value (a value that never
//! occurs as a real key — e.g. 0 for a PID). The sentinel avoids a separate occupancy bitmap:
//! an empty slot is simply one whose key equals the sentinel, and the table is initialised by
//! writing the sentinel into every slot. Integer keys are provided; a composite id implements
//! the trait to map on it directly.
//!
//! The provided integer impls use **Fibonacci hashing** (multiply by 2⁶⁴/φ, the map takes the
//! high bits), which mixes every key bit into the home slot so even densely clustered keys
//! (consecutive PIDs) spread evenly. It is the fast, non-DoS-resistant choice — fine for
//! trusted integer-like ids, not for attacker-controlled input.
//!
//! Like the other writers the map caches its chunk base (self-healed via the arena epoch — see
//! [`Arena`]) and grows through a `*const Arena`, so it must be [`wire`](ThpMap::wire)d after
//! the arena is pinned. A grow is a **rehash**: the home of every key changes with the
//! capacity, so growth snapshots the live entries, enlarges the chunk, and re-inserts — the
//! table's analogue of a store regrow. Neither `Send` nor `Sync`.

use std::cell::Cell;
use std::marker::PhantomData;
use std::mem::{MaybeUninit, size_of};

use crate::Flat;
use crate::arena::{Arena, ChunkId};

/// 2⁶⁴ / φ, odd — the Fibonacci-hashing multiplier the integer [`MapKey`] impls use. The high
/// bits of `key · FIB` depend on every bit of `key`, so consecutive keys land in well-separated
/// slots.
const FIB: u64 = 0x9E37_79B9_7F4A_7C15;

/// A key usable in a [`ThpMap`]: a flat, equality-comparable value that reserves one sentinel
/// and hashes itself.
///
/// The map stores keys inline and recycles slots with a raw overwrite, so a key is [`Flat`]
/// (fixed-size, `Copy`, no `Drop`). It needs [`Eq`] to match a probe, an [`EMPTY`](Self::EMPTY)
/// value that marks a vacant slot (and so must never be a real key — the map debug-asserts
/// this), and a [`hash`](Self::hash) whose **high bits** are well mixed (the map shifts them
/// down to a home slot).
pub trait MapKey: Flat + Eq {
    /// The reserved empty-slot sentinel. Never a valid key.
    const EMPTY: Self;
    /// A well-mixed 64-bit hash; the map takes the top `log2(capacity)` bits as the home slot.
    /// Fast and not DoS-resistant — for trusted integer-like ids, not hostile input.
    #[must_use]
    fn hash(&self) -> u64;
}

impl MapKey for u32 {
    const EMPTY: Self = 0;
    fn hash(&self) -> u64 {
        u64::from(*self).wrapping_mul(FIB)
    }
}

impl MapKey for u64 {
    const EMPTY: Self = 0;
    fn hash(&self) -> u64 {
        self.wrapping_mul(FIB)
    }
}

/// One table slot: the key inline before the payload (`key == K::EMPTY` ⇒ vacant). The payload
/// of a vacant slot is never read, so it stays [`MaybeUninit`] until an insert writes it.
#[derive(Clone, Copy)]
struct Entry<K: MapKey, V: Flat> {
    key: K,
    value: MaybeUninit<V>,
}

/// A flat open-addressing map (`K` → `V`) living in a single [`Arena`] chunk. Caches the chunk
/// base for `base + idx` slot access with no chunk-table chase; the base self-heals when a
/// sibling's growth relocates this chunk (epoch compare, like [`GenStore`](crate::GenStore)).
///
/// Holds raw pointers ⇒ **neither `Send` nor `Sync`**. A key equal to [`MapKey::EMPTY`] is
/// forbidden (it marks a vacant slot). `V` is [`Flat`] (fixed-size, `Copy`, no `Drop`), so a
/// slot is recycled with a raw overwrite.
pub struct ThpMap<K: MapKey, V: Flat> {
    /// Owning arena (interior-mutable, `&self`). Null until [`wire`](Self::wire).
    arena: *const Arena,
    /// Cached chunk base. Re-read by [`sync`](Self::sync) when the arena epoch advances.
    base: Cell<*mut Entry<K, V>>,
    /// Arena epoch the cached `base` was read at.
    epoch: Cell<u64>,
    chunk: ChunkId,
    /// Slot count — a power of two.
    cap: usize,
    /// `cap - 1`, for wrapping the probe index.
    mask: usize,
    /// `64 - log2(cap)` — shift to take the top `log2(cap)` bits of the key hash.
    shift: u32,
    /// Occupied slots.
    len: usize,
    _marker: PhantomData<(K, V)>,
}

impl<K: MapKey, V: Flat> ThpMap<K, V> {
    /// Build a map in a fresh arena chunk sized for at least `min_cap` keys (rounded up to a
    /// power of two, floored at [`MIN_CAP`]). Not usable until [`wire`](Self::wire)d.
    #[must_use]
    pub fn new(arena: &Arena, min_cap: usize) -> Self {
        /// Smallest table capacity (a power of two). Keeps the home shift `< 64` and gives a
        /// tiny table some slack before the first grow.
        const MIN_CAP: usize = 8;
        let cap = min_cap.max(1).next_power_of_two().max(MIN_CAP);
        let chunk = arena.alloc(cap * size_of::<Entry<K, V>>(), align_of::<Entry<K, V>>());
        Self {
            arena: std::ptr::null(),
            base: Cell::new(std::ptr::null_mut()),
            epoch: Cell::new(0),
            chunk,
            cap,
            mask: cap - 1,
            shift: 64 - cap.trailing_zeros(),
            len: 0,
            _marker: PhantomData,
        }
    }

    /// Bind to the arena once it is at its final (pinned) address: cache the base + epoch and
    /// write the empty sentinel into every slot so the table starts vacant. The sentinel fill
    /// is load-bearing — a fresh chunk's bytes are not guaranteed to encode `K::EMPTY` (it may
    /// reuse a regime-A hole). Call once before any access.
    pub fn wire(&mut self, arena: &Arena) {
        self.arena = arena;
        self.base.set(arena.base_of(self.chunk).cast());
        self.epoch.set(arena.epoch());
        self.clear_keys(self.cap);
        self.len = 0;
    }

    fn arena(&self) -> &Arena {
        debug_assert!(!self.arena.is_null(), "ThpMap accessed before wire()");
        // SAFETY: set by `wire` to the owning arena, which outlives this map (pinned, dropped
        // after it). Only dereferenced on the owning thread.
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
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Home slot of `key`: the top `log2(cap)` bits of its hash.
    #[allow(clippy::cast_possible_truncation)] // shifted to log2(cap) bits, so < cap ≤ usize::MAX
    fn home(&self, key: K) -> usize {
        (key.hash() >> self.shift) as usize
    }

    /// Probe distance of the entry stored at `slot` (slots from its home, wrapping).
    fn dist(&self, slot: usize, key: K) -> usize {
        slot.wrapping_sub(self.home(key)) & self.mask
    }

    fn entry(&self, idx: usize) -> *mut Entry<K, V> {
        // SAFETY: idx < cap; base is current and addresses `cap` slots.
        unsafe { self.base.get().add(idx) }
    }

    /// Write the empty sentinel into the first `n` slots. Used at wire and after a grow's
    /// relocation.
    fn clear_keys(&self, n: usize) {
        for i in 0..n {
            // SAFETY: i < n ≤ cap; writing only the key field.
            unsafe {
                (*self.entry(i)).key = K::EMPTY;
            }
        }
    }

    /// Slot index holding `key`, or `None`. Robin Hood lets the search stop early: once a
    /// stored entry sits closer to its home than the distance we have already probed, `key`
    /// (which would have displaced it) cannot be further along.
    fn find(&self, key: K) -> Option<usize> {
        self.sync();
        let mut i = self.home(key);
        let mut probe = 0usize;
        loop {
            // SAFETY: in-range slot, base synced.
            let k = unsafe { (*self.entry(i)).key };
            if k == K::EMPTY {
                return None;
            }
            if k == key {
                return Some(i);
            }
            if self.dist(i, k) < probe {
                return None;
            }
            i = (i + 1) & self.mask;
            probe += 1;
        }
    }

    /// The value mapped to `key`, copied out (never a reference into the chunk — the caller may
    /// run arena allocations that relocate it before writing back).
    #[must_use]
    pub fn get(&self, key: K) -> Option<V> {
        debug_assert!(key != K::EMPTY, "the empty sentinel is not a valid key");
        self.find(key).map(|i| {
            // SAFETY: an occupied slot's value was written before any get (V: Copy → read out).
            unsafe { (*self.entry(i)).value.assume_init() }
        })
    }

    /// Like [`get`](Self::get), but also returns the resolved **slot index** so a read /
    /// modify / write-back needs only one probe: read here, then write through
    /// [`update_at`](Self::update_at) instead of a second [`update`](Self::update) probe.
    ///
    /// The slot index stays valid across intervening arena allocations on *other* stores: a
    /// regime-B repack relocates this chunk, but the logical slot position is unchanged — it is
    /// re-homed only by a map insert/remove, neither of which may run between the probe and the
    /// write-back. (A raw pointer would dangle; the index does not — `update_at` re-syncs the
    /// base.) Do **not** carry it across a `remove`/`insert`/`retain` on this map.
    #[must_use]
    pub fn get_entry(&self, key: K) -> Option<(usize, V)> {
        debug_assert!(key != K::EMPTY, "the empty sentinel is not a valid key");
        self.find(key).map(|i| {
            // SAFETY: occupied slot, value initialised before any read (V: Copy).
            (i, unsafe { (*self.entry(i)).value.assume_init() })
        })
    }

    /// Overwrite the value at a slot index from a prior [`get_entry`](Self::get_entry), with no
    /// second probe. See `get_entry` for when the index is still valid.
    pub fn update_at(&mut self, slot: usize, value: V) {
        debug_assert!(slot < self.cap, "slot out of range");
        self.sync();
        // SAFETY: in-range slot, base synced; must still be occupied (caller contract).
        unsafe {
            debug_assert!(
                (*self.entry(slot)).key != K::EMPTY,
                "update_at on a vacant slot"
            );
            (*self.entry(slot)).value.write(value);
        }
    }

    /// Insert a **new** key (the caller must have established it is absent, e.g. via a prior
    /// [`get`](Self::get)). Robin Hood placement: carry the new entry, and whenever it has
    /// probed further than a resident entry, swap — the resident is "poorer" and moves on.
    ///
    /// May grow (rehash), which relocates this chunk and, via a regime-B repack, possibly every
    /// sibling store. Hold no reference into any arena chunk across this call.
    pub fn insert(&mut self, key: K, value: V) {
        debug_assert!(key != K::EMPTY, "the empty sentinel is not a valid key");
        debug_assert!(self.find(key).is_none(), "insert of an already-present key");
        if (self.len + 1) * LOAD_DEN > self.cap * LOAD_NUM {
            self.grow();
        }
        self.place(key, value);
    }

    /// Overwrite the value of an **existing** key (the caller established presence). No
    /// structural change, so it never grows or relocates.
    ///
    /// # Panics
    /// If `key` is absent — establish presence first (e.g. via [`get`](Self::get)).
    pub fn update(&mut self, key: K, value: V) {
        debug_assert!(key != K::EMPTY, "the empty sentinel is not a valid key");
        let i = self.find(key).expect("update of an absent key");
        // SAFETY: occupied slot; V: Copy so the prior value needs no drop.
        unsafe {
            (*self.entry(i)).value.write(value);
        }
    }

    /// Robin Hood placement of one absent key into a table with room. Splitting this out of
    /// [`insert`](Self::insert) lets [`grow`](Self::grow) re-place without re-checking the load
    /// factor (the fresh table is sized) or re-asserting absence.
    fn place(&mut self, key: K, value: V) {
        self.sync();
        let mut ck = key;
        let mut cv = value;
        let mut i = self.home(ck);
        let mut probe = 0usize;
        loop {
            let slot = self.entry(i);
            // SAFETY: in-range slot, base synced.
            let k = unsafe { (*slot).key };
            if k == K::EMPTY {
                // SAFETY: empty slot — claim it.
                unsafe {
                    (*slot).key = ck;
                    (*slot).value.write(cv);
                }
                self.len += 1;
                return;
            }
            let resident = self.dist(i, k);
            if resident < probe {
                // The resident is closer to home than the carried entry — steal its slot and
                // carry the resident onward from here.
                // SAFETY: occupied slot; read the resident out (Copy) then overwrite.
                let rv = unsafe { (*slot).value.assume_init() };
                unsafe {
                    (*slot).key = ck;
                    (*slot).value.write(cv);
                }
                ck = k;
                cv = rv;
                probe = resident;
            }
            i = (i + 1) & self.mask;
            probe += 1;
        }
    }

    /// Remove `key` if present; returns whether it was. Backward-shift: pull each following
    /// displaced entry one slot toward its home until an empty slot or a home-resident entry,
    /// leaving no tombstone.
    pub fn remove(&mut self, key: K) -> bool {
        debug_assert!(key != K::EMPTY, "the empty sentinel is not a valid key");
        match self.find(key) {
            Some(i) => {
                self.remove_at(i);
                true
            }
            None => false,
        }
    }

    /// Backward-shift delete starting at the (occupied) slot `i`.
    fn remove_at(&mut self, mut i: usize) {
        self.sync();
        loop {
            let next = (i + 1) & self.mask;
            // SAFETY: in-range slots, base synced.
            let nk = unsafe { (*self.entry(next)).key };
            if nk == K::EMPTY || self.dist(next, nk) == 0 {
                break; // empty, or `next` is already home → nothing shifts into `i`.
            }
            // SAFETY: copy the whole (occupied) entry back one slot.
            unsafe {
                *self.entry(i) = *self.entry(next);
            }
            i = next;
        }
        // SAFETY: `i` is the last shifted slot (or the original) — mark it empty.
        unsafe {
            (*self.entry(i)).key = K::EMPTY;
        }
        self.len -= 1;
    }

    /// Keep only entries for which `keep(key, &value)` is true, removing the rest by
    /// backward-shift (no tombstones). `keep` may mutate sibling state (e.g. free a removed
    /// entry's slots in other stores) — it must not touch this map, and must not trigger an
    /// arena allocation that would relocate this chunk mid-scan (a `free` does not; an
    /// `alloc`/`grow` would, dangling the base captured at entry — debug-asserted below).
    ///
    /// The scan visits all `cap` slots, not just the `len` occupied ones: a tombstone-free
    /// open-addressing table has no O(len) bulk scan without an auxiliary occupancy index. At
    /// the load factor this is a small constant of empty-slot reads — the deliberate cost of
    /// never accruing tombstones (which is what would otherwise degrade the per-cycle probes).
    pub fn retain(&mut self, mut keep: impl FnMut(K, &V) -> bool) {
        self.sync();
        let start_epoch = self.arena().epoch();
        let mut i = 0;
        while i < self.cap {
            // SAFETY: in-range slot, base synced; no alloc occurs in this scan.
            let k = unsafe { (*self.entry(i)).key };
            if k != K::EMPTY {
                let drop = {
                    // SAFETY: occupied slot — value is initialised. The borrow ends before the
                    // `&mut self` `remove_at` below.
                    let v = unsafe { (*self.entry(i)).value.assume_init_ref() };
                    !keep(k, v)
                };
                // The `keep` closure must not relocate this chunk (see the contract above): if
                // it allocated through the arena, the base we are striding is now stale.
                debug_assert_eq!(
                    self.arena().epoch(),
                    start_epoch,
                    "retain's keep closure triggered an arena relocation"
                );
                if drop {
                    self.remove_at(i);
                    continue; // a shifted entry may now occupy slot `i` — re-check it.
                }
            }
            i += 1;
        }
    }

    /// Grow to the next power of two and rehash. Every key's home depends on the capacity, so a
    /// grow cannot relocate-in-place like a flat store — it snapshots the live entries, enlarges
    /// the chunk, rewrites the sentinel, and re-inserts. The transient `Vec` is acceptable: a
    /// grow is a cold path that fires O(log n) times over the whole run, never per cycle.
    fn grow(&mut self) {
        self.sync();
        let mut items: Vec<(K, V)> = Vec::with_capacity(self.len);
        for i in 0..self.cap {
            // SAFETY: in-range slot, base synced.
            let e = self.entry(i);
            let k = unsafe { (*e).key };
            if k != K::EMPTY {
                items.push((k, unsafe { (*e).value.assume_init() }));
            }
        }

        let new_cap = self.cap.checked_mul(2).expect("ThpMap capacity overflow");
        let new_base = self
            .arena()
            .grow(self.chunk, new_cap * size_of::<Entry<K, V>>());
        self.base.set(new_base.cast());
        self.epoch.set(self.arena().epoch());
        self.cap = new_cap;
        self.mask = new_cap - 1;
        self.shift = 64 - new_cap.trailing_zeros();
        self.len = 0;
        self.clear_keys(new_cap);

        for (k, v) in items {
            self.place(k, v);
        }
    }
}

/// Grow when the table would exceed this load factor (`len + 1 > cap · NUM / DEN`). Robin Hood
/// tolerates a high load with bounded probe length, so 7/8 trades a little probing for memory.
const LOAD_NUM: usize = 7;
const LOAD_DEN: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gen, GenStore};
    use std::collections::HashMap;

    /// Wire a map to `arena`. The map points at `arena` (which must stay put); the map itself
    /// may move.
    fn wired<K: MapKey, V: Flat>(arena: &Arena, min_cap: usize) -> ThpMap<K, V> {
        let mut m = ThpMap::new(arena, min_cap);
        m.wire(arena);
        m
    }

    #[test]
    fn insert_get_update_remove() {
        let arena = Arena::new(0);
        let mut m: ThpMap<u32, u64> = wired(&arena, 8);
        assert!(m.get(1).is_none());
        m.insert(1, 100);
        m.insert(2, 200);
        assert_eq!(m.get(1), Some(100));
        assert_eq!(m.get(2), Some(200));
        assert_eq!(m.len(), 2);

        m.update(1, 111);
        assert_eq!(m.get(1), Some(111));
        assert_eq!(m.len(), 2, "update must not change occupancy");

        assert!(m.remove(1));
        assert!(!m.remove(1), "double remove is a no-op");
        assert_eq!(m.get(1), None);
        assert_eq!(m.get(2), Some(200), "the sibling survives the delete-shift");
        assert_eq!(m.len(), 1);
    }

    /// The one-probe read/modify/write path: `get_entry` then `update_at` on the returned slot.
    #[test]
    fn get_entry_then_update_at() {
        let arena = Arena::new(0);
        let mut m: ThpMap<u32, u64> = wired(&arena, 8);
        for k in 1..=5u32 {
            m.insert(k, u64::from(k));
        }
        for k in 1..=5u32 {
            let (idx, v) = m.get_entry(k).expect("present");
            assert_eq!(v, u64::from(k));
            m.update_at(idx, v + 100);
        }
        for k in 1..=5u32 {
            assert_eq!(m.get(k), Some(u64::from(k) + 100));
        }
        assert!(m.get_entry(999).is_none());
    }

    /// The slot index from `get_entry` must stay valid across a *sibling* store's growth that
    /// relocates the map chunk (regime B) — the exact gatherer pattern: probe, do per-PID store
    /// work that may repack the arena, then `update_at`. Only a map insert/remove re-homes a
    /// slot; a relocation does not.
    #[test]
    fn slot_index_survives_relocation_before_update_at() {
        let arena = Arena::new(0);
        let mut m: ThpMap<u32, u64> = wired(&arena, 64);
        let mut store: GenStore<[u8; 64]> = {
            let mut s = GenStore::new(&arena, 1);
            s.wire(&arena);
            s
        };
        for k in 1..=50u32 {
            m.insert(k, 0);
        }
        let (idx, _) = m.get_entry(7).expect("present");
        // Force repacks that relocate the map chunk between probe and write-back.
        for i in 0..4000u32 {
            let mut v = [0u8; 64];
            v[..4].copy_from_slice(&i.to_le_bytes());
            store.insert(Gen::ALIVE, v);
        }
        m.update_at(idx, 777);
        assert_eq!(
            m.get(7),
            Some(777),
            "update_at hit the relocated key-7 slot"
        );
        for k in 1..=50u32 {
            if k != 7 {
                assert_eq!(m.get(k), Some(0), "neighbor {k} clobbered");
            }
        }
    }

    /// The map is generic over the key: a `u64`-keyed instance works the same.
    #[test]
    fn u64_keys() {
        let arena = Arena::new(0);
        let mut m: ThpMap<u64, u32> = wired(&arena, 8);
        m.insert(1 << 40, 7);
        m.insert(1 << 41, 9);
        assert_eq!(m.get(1 << 40), Some(7));
        assert_eq!(m.get(1 << 41), Some(9));
        assert!(m.remove(1 << 40));
        assert_eq!(m.get(1 << 40), None);
        assert_eq!(m.get(1 << 41), Some(9));
    }

    #[test]
    fn grows_and_preserves_all_entries() {
        // Tiny initial cap forces several rehash relocations.
        let arena = Arena::new(0);
        let mut m: ThpMap<u32, u64> = wired(&arena, 1);
        for k in 1..=2000u32 {
            m.insert(k, u64::from(k) * 7);
        }
        assert!(m.capacity() >= 2048, "must have grown past the tiny start");
        for k in 1..=2000u32 {
            assert_eq!(m.get(k), Some(u64::from(k) * 7), "lost key {k} across grow");
        }
    }

    /// A removed entry's slot is reusable and probing stays correct across a clustered
    /// delete/insert mix (the backward-shift must not strand a displaced entry).
    #[test]
    fn delete_shift_keeps_probes_correct() {
        let arena = Arena::new(0);
        let mut m: ThpMap<u32, u64> = wired(&arena, 16);
        // Keys chosen to collide (consecutive keys cluster under any hash within a small cap).
        for k in 1..=10u32 {
            m.insert(k, u64::from(k));
        }
        for k in [3u32, 4, 5] {
            assert!(m.remove(k));
        }
        for k in [1u32, 2, 6, 7, 8, 9, 10] {
            assert_eq!(
                m.get(k),
                Some(u64::from(k)),
                "key {k} unreachable after shifts"
            );
        }
        for k in [3u32, 4, 5] {
            assert_eq!(m.get(k), None);
        }
        // Re-insert into the freed slots.
        for k in [3u32, 4, 5] {
            m.insert(k, u64::from(k) * 100);
        }
        for k in [3u32, 4, 5] {
            assert_eq!(m.get(k), Some(u64::from(k) * 100));
        }
    }

    /// Oracle: random insert/update/remove churn matches a `HashMap` reference, across the
    /// growth relocations a tiny initial capacity forces. Deterministic LCG; keys ∈ 1..=400 so
    /// collisions and reuse are frequent.
    #[test]
    #[allow(clippy::cast_possible_truncation)] // LCG-derived keys/values, intentionally narrowed
    fn oracle_matches_hashmap() {
        let arena = Arena::new(0);
        let mut m: ThpMap<u32, u64> = wired(&arena, 1);
        let mut model: HashMap<u32, u64> = HashMap::new();
        let mut rng = 0x1234_5678_9abc_def0u64;

        for _ in 0..60_000 {
            rng = rng
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let key = ((rng >> 24) as u32 % 400) + 1; // never 0
            match (rng >> 12) % 3 {
                0 | 1 => {
                    let v = rng;
                    if model.insert(key, v).is_some() {
                        m.update(key, v);
                    } else {
                        m.insert(key, v);
                    }
                }
                _ => {
                    let existed = model.remove(&key).is_some();
                    assert_eq!(m.remove(key), existed, "remove disagreement for key {key}");
                }
            }
            assert_eq!(m.len(), model.len());
            assert_eq!(
                m.get(key),
                model.get(&key).copied(),
                "value mismatch key {key}"
            );
        }
        for (k, v) in &model {
            assert_eq!(m.get(*k), Some(*v), "final scan: key {k}");
        }
        // Everything not in the model must be absent.
        for k in 1..=400u32 {
            if !model.contains_key(&k) {
                assert_eq!(m.get(k), None, "phantom key {k}");
            }
        }
    }

    /// Oracle for bulk eviction: `retain` against a reference, across many rounds, with a tiny
    /// cap so retains interleave with grows.
    #[test]
    #[allow(clippy::cast_possible_truncation)] // LCG-derived keys/values, intentionally narrowed
    fn oracle_retain_matches_hashmap() {
        let arena = Arena::new(0);
        let mut m: ThpMap<u32, u32> = wired(&arena, 1);
        let mut model: HashMap<u32, u32> = HashMap::new();
        let mut rng = 0xdead_beef_0bad_f00du64;

        for round in 0..400u32 {
            // Insert a burst.
            for _ in 0..50 {
                rng = rng
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let key = ((rng >> 24) as u32 % 500) + 1;
                let v = rng as u32;
                if model.insert(key, v).is_some() {
                    m.update(key, v);
                } else {
                    m.insert(key, v);
                }
            }
            // Evict everything whose value is not congruent to the round — a churny predicate.
            m.retain(|_, &v| v % 4 == round % 4);
            model.retain(|_, &mut v| v % 4 == round % 4);

            assert_eq!(m.len(), model.len(), "len after retain, round {round}");
            for (k, v) in &model {
                assert_eq!(m.get(*k), Some(*v), "retained key {k} round {round}");
            }
        }
    }

    /// THP self-heal: a sibling store sharing the arena grows enough to force regime-B repacks
    /// that relocate the map's chunk; every key inserted before the growth must still resolve
    /// (the map re-read its own base on next access, nobody reached into it).
    #[test]
    fn self_heals_after_sibling_growth() {
        let arena = Arena::new(0);
        let mut m: ThpMap<u32, u64> = wired(&arena, 1);
        let mut store: GenStore<[u8; 64]> = {
            let mut s = GenStore::new(&arena, 1);
            s.wire(&arena);
            s
        };

        for k in 1..=500u32 {
            m.insert(k, u64::from(k) * 3);
        }
        // Hammer the sibling so the shared region repacks repeatedly, relocating the map chunk.
        for i in 0..4000u32 {
            let mut v = [0u8; 64];
            v[..4].copy_from_slice(&i.to_le_bytes());
            store.insert(Gen::ALIVE, v);
        }
        for k in 1..=500u32 {
            assert_eq!(
                m.get(k),
                Some(u64::from(k) * 3),
                "key {k} lost across relocation"
            );
        }
    }
}
