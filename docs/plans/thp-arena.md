# THP Arena — Generational THP-Backed Storage

> **Status (2026-06):**
> - I/O-decoupling (originally §1a, "registered read-buffer prefix") **landed** — see below.
> - **Phase 1 landed** — the `thoop` crate: `Flat`, `Gen`, `Ref`, `GenStore`, `TypedBuf`,
>   plus `StrStore`/`StringRef`/`ByteResolver`. `MmapRegion` moved here from `atop`. Oracle
>   tests vs std models.
> - **Phase 2 landed** — cmdline migrated to a generational `Cmd` store; the per-cycle
>   re-materialization is gone; the snapshot carries a `ByteResolver<Cmd>`. **Deviation:**
>   only cmdline moved, not comm. comm is fresh-parsed in the backend each cycle (no
>   redundant copy to eliminate) and has no per-PID home until phase 3's `PidMeta`; moving
>   it now would bloat `ProcessEntry` or duplicate a backend PID-map. comm stays in the
>   per-cycle (huge-page) arena until phase 5 makes the snapshot resolver-only.
> - **Phase 2.5 landed** — `thoop::Arena` huge-page suballocator (shared by all stores, so
>   they don't each `mmap` a 2 MiB page). Containers refactored to hold a `ChunkId` and take
>   `&Arena`/`&mut Arena`; `Ref`/`StringRef` are relocation-stable indices; `ByteResolver`
>   is single-base. **Growth model (designed with the user):** *copy-now / free-deferred* —
>   regime A relocates a chunk in-region (frozen hole), regime B repacks+compacts to a new
>   region; the old backing is retired and freed by `arena.gc(min_live)` on the **same
>   lease** as slots (a relocated chunk's old bytes are just a demoted slot writ large).
> - **Phase 3 landed** — `ProcCache`'s per-PID map moved to `GenStore<PidMeta>` (huge pages)
>   + `PidIndex`. `GenStore::free` added for gatherer-internal stores (immediate reclaim, no
>   lease); the arena-threading borrow is handled by copy-in/modify/copy-out for the `Copy`
>   `PidMeta`. Confirmed: only the string stores are snapshot-leased.
> - **Remaining:** phases 4–6 (below). 4 (CpuRing) mirrors 3 — tractable. 5 (ViewEntry on
>   `TypedBuf`, fold comm, snapshot fully THP) is the intricate one — it needs a cross-thread
>   *slice view* for `TypedBuf` (like `ByteResolver` for strings) and must resolve the
>   regime-B-vs-front-buffer question (B repacks only gatherer-owned chunks; the front
>   snapshot reads its old region via the lease). 6 (ThpMap) is a hand-rolled
>   open-addressing table behind an oracle test, replacing the `FxHashMap` `PidIndex`.
>
> - **Next session** (foundation is green + solid): implement the **captured-pointer view
>   layer** (see "Next: foundational access layer" below) — raw-pointer views,
>   lease-validated, reserve-pre-pass + view-fill — then phases 4–6 on top of it.
>
> Everything below is the original design; phases 4–6 + the view layer are the live worklist.

## Done — landing pad (decouple I/O target from string store)

Reads no longer target the snapshot arena. The io_uring backend owns a small **fixed**
`MmapRegion` landing pad (`read_slots × STAT_SLOT`), registered once; each read claims a
slot, `reap` parses it and copies `comm` (~15 B) into the (unregistered) arena, then frees
the slot. Consequences:

- **Pinned memory is a small constant**, independent of PID count → a low per-process lock
  limit no longer silently downgrades io_uring as the process count grows.
- The arena shrank ~3×/PID (no retained raw stat text) and a mid-cycle grow is no longer a
  correctness hazard (arena is not a registered buffer) — reserve-up-front is now a perf
  hint only.
- `MmapRegion` (huge-page sizing, `MADV_HUGEPAGE`) is the shared primitive all new
  storage builds on.

## Problem

Two separate concerns, one shared solution:

1. **TLB pressure.** The gatherer's hot random-access structures (`procs`, PID-keyed maps)
   sit on 4 KiB heap pages. At scale (thousands of PIDs), random access to these structures
   spans hundreds of pages — far more than the ~64-entry L1 dTLB. On 2 MiB huge pages the
   same data fits in a handful of TLB entries regardless of PID count.

2. **Per-cycle string re-materialization.** ProcCache caches cmdlines in `Vec<u8>` per PID
   and copies every PID's cmdline into the snapshot's string arena every cycle (~1 MB at
   5k PIDs). This is redundant — most cmdlines haven't changed. Stable string storage
   eliminates the copy for unchanged strings.

3. **Prerequisite for incremental snapshots** (scale-observation plan). Carry-forward of
   cold-tier PIDs requires StringRefs that survive across snapshot cycles — impossible with
   the current per-cycle arena reset. The generational model provides this.

## Goal

**All hot data on huge pages, with generational lifecycle.** Every structure the gatherer
and UI touch lives on THP-backed `MmapRegion` pages. No `Vec`, `String`, `Box`, or other
heap-allocated container appears as a field in any THP-resident type. Lifecycle is
generation-tracked: data stays alive while any snapshot references it, reclaimable after.

## Core concepts

### The `Flat` marker trait

```
trait Flat: Copy {}
```

"Safe to store in THP regions." Fixed-size, bitwise-copyable, no heap containers. Handle
fields (`Ref<T>`, `StringRef<S>`) are permitted — they resolve only against THP-backed
stores by construction. Container bounds: `GenStore<T: Flat>`, `TypedBuf<T: Flat>`,
`ThpMap<K: Flat, V: Flat>`.

### `Gen` — generational lifecycle

A newtype over `u8` that encodes both slot state and generation in one byte. Two sentinel
constructors and a demoted range:

```
Gen::ALIVE   — immortal: GC skips unconditionally
Gen::FREE    — on free list, available for allocation
Gen::at(now) — demoted at generation `now`; reclaimable when min_live_gen passes it
```

`Gen` provides `is_alive()`, `is_free()`, `is_reclaimable(min_live: Gen) -> bool` (wrapping
comparison for the demoted range). All sentinel logic lives in the type — application code
never sees raw u8 values or magic constants.

At 2 Hz, the u8 range (~128 meaningful values for wrapping comparison) covers ~64 seconds —
orders of magnitude more than the live window of 2 snapshots (~1 second).

Lifecycle:

- **PID born:** slot gen = `Gen::ALIVE`.
- **PID stable:** gen stays ALIVE. GC cost: `is_alive()` → skip.
- **String changes:** old slot gen = `Gen::at(now)` (demoted), new slot gen = ALIVE.
- **PID dies:** all PID's slots demoted from ALIVE to `Gen::at(now)`.
- **GC:** scan slots; skip ALIVE and FREE; reclaim where `gen.is_reclaimable(min_live)`
  → set to FREE.

Steady state: almost all slots are ALIVE or FREE. Only recently-dead PIDs and
recently-changed strings occupy the demoted range. GC is proportional to churn, not
population.

### Snapshot-as-lease

The UI holds an `Arc<Snapshot>`. While held, every byte reachable through any reference
chain in the snapshot is guaranteed alive — the generational GC will not reclaim it. On
release (UI loads a newer snapshot, dropping the old Arc), referenced data becomes
reclaimable (in most cases: overwritten in place by the next cycle).

With double-buffering (2 live snapshots at most), `min_live_gen = current_gen - 1`.

## Abstraction stack

### Layer 0: MmapRegion (exists)

Owned anonymous mmap, 2 MiB-aligned, `MADV_HUGEPAGE`-hinted, unmapped on drop. All other
abstractions build on it.

### Layer 1: Containers (own MmapRegion pages, safe API, unsafe internals)

**`GenStore<T: Flat>`** — generational slab.

Slots are `{ gen: u8, data: MaybeUninit<T> }` on MmapRegion pages. Free list threaded
through free slots' data (zero extra space). Grows by adding pages — never relocates
existing pages (non-relocating invariant for cross-snapshot safety).

Safe API:
- `alloc(gen) → Ref<T>` — pop free list or grow a new page.
- `get(Ref<T>) → &T` — bounds-checked, asserts not FREE.
- `get_mut(Ref<T>) → &mut T` — same.
- `assign(Ref<T>, value)` — write via `ptr::write`.
- `free(Ref<T>, gen)` — demote: set gen to `now`.
- `gc(min_live_gen)` — reclaim demoted slots to free list.

**`TypedBuf<T: Flat>`** — THP-backed resettable array (snapshot buffer).

Contiguous `T` array on MmapRegion. Used for the snapshot's ViewEntry list.

Safe API:
- `push(T)` — append.
- `clear()` — rewind length to 0 (O(1), no drops).
- `as_slice() → &[T]`, `Index`, `IndexMut`.
- Grow: new larger MmapRegion + copy (rare, only at startup ramp).

**`ThpMap<K: Flat, V: Flat>`** — open-addressing hash table on THP.

FxHash, linear probing, slot array on MmapRegion. Replaces `HashMap<u32, V, FxBuildHasher>`
for PID-keyed maps.

Safe API:
- `get(&K) → Option<&V>`, `get_mut`, `insert`, `remove`, `retain`, `len`, `iter`.

### Layer 2: Typed handles (Copy, Flat, stored in THP-resident types)

**`Ref<T: Flat>`** — 4 bytes. Typed slot index into `GenStore<T>`. Compile-time type safety:
`Ref<CpuRing>` cannot be used with `GenStore<PidMeta>`.

**`StringRef<S: StringStore>`** — 8 bytes. Slot index (u32) + actual byte length (u16) +
PhantomData. Type parameter (`Comm` / `Cmd`) prevents wrong-store resolution at compile
time.

```
trait StringStore: Copy { const SLOT_SIZE: usize; }
struct Comm;  // StringStore, SLOT_SIZE = 16
struct Cmd;   // StringStore, SLOT_SIZE = 256
```

### Layer 3: Resolution (safe bridge from snapshot to generational stores)

**`StringResolver`** — constructed by the gatherer at publish time, stored in the snapshot.
Internally holds page base pointers (opaque). Safe API:

```
fn resolve<S: StringStore>(&self, sref: StringRef<S>) -> &[u8]
```

One method, type-dispatched. Application code: `snap.resolve(entry.name)` — the compiler
selects the right store from the StringRef type parameter. Wrong-store is a compile error.

## Concrete stores

| GenStore instance | Slot type | Slot size | Use |
|---|---|---|---|
| `GenStore<CpuRing>` | CPU history ring buffer | ~216 B | Per-PID, circular writes each sample |
| `GenStore<PidMeta>` | Cache state | ~48 B | Per-PID uid, StringRefs, settling, held fd |
| `GenStore<[u8; 16]>` | Comm string | 16 B | Process name from stat |
| `GenStore<[u8; 256]>` | Cmdline string | 256 B | Cleaned /proc/pid/cmdline |

CpuRing and PidMeta are **separated** (not a monolithic per-PID record) for cache
efficiency: CpuRing (~216 B) is touched every sample (hot path); PidMeta (~48 B) is
touched on birth and rare refresh ticks. Co-locating them would load ~4 cache lines of
cold data on every CPU update.

Shared **PidIndex** (`ThpMap<u32, PidSlot>`) maps PID → slot refs for all stores:

```
struct PidSlot {
    cpu: Ref<CpuRing>,
    meta: Ref<PidMeta>,
}
```

One PID lookup serves all stores. String slots are reached via PidMeta's StringRef fields
(PidMeta.name, PidMeta.cmdline).

## Application types (all Flat, all THP-resident)

```
ViewEntry {
    // Volatile (from stat parse each cycle):
    pid, ppid, uid, state, priority, nice, num_threads,
    cpu_pct, cpu_peak, mem_bytes, ticks, start_time,
    // Stable (projected from PidMeta):
    name: StringRef<Comm>,
    cmdline: StringRef<Cmd>,
    non_ascii: bool, is_kthread: bool,
    // Tree (rebuilt each publish):
    parent_idx, first_child, next_sibling, subtree_size, depth,
    subtree_cpu, subtree_mem,
}

PidMeta {
    start_time: u64, uid: u32,
    name: StringRef<Comm>, cmdline: StringRef<Cmd>,
    cmd_non_ascii: bool, is_kthread: bool,
    first_seen_gen: u8, seen_gen: u8,
    stat_fd: i32, fixed_idx: u32,
}

CpuRing {
    prev_ticks: u64,
    samples: [Sample; CPU_WINDOW],
    next: u16, len: u16,
    sum_ticks: u64, sum_jiff: u64,
    peak_bp: u32, peak_at: u16,
    seen_gen: u8,
}
```

No Vec, no String, no Box. Every inter-region reference is a typed handle.

## Snapshot structure

```
Snapshot {
    entries: TypedBuf<ViewEntry>,   // THP, double-buffered, overwritten each cycle
    resolver: StringResolver,       // read access to comm/cmd GenStores
    generation: u64,
    sys: SystemStats,
}
```

The snapshot itself lives on THP (via TypedBuf). The resolver provides safe string
resolution into the generational string stores. All data reachable from the snapshot is
THP-backed.

## Cycle flow

1. **Sample:** read stat for sampled PIDs → update `CpuRing` (ring push) + capture
   volatile fields.
2. **Refresh:** scheduled PIDs → read cmdline/uid → update PidMeta + allocate new string
   slots if changed (old slots demoted to `now`).
3. **Birth/death:** allocate/free slots in all stores via PidIndex. On death, all PID's
   slots demoted from ALIVE to `now`.
4. **Build snapshot:** iterate live PIDs, project ViewEntry from CpuRing + PidMeta +
   volatile fields → push to TypedBuf. Sort by PID. Tree build.
5. **Publish:** construct StringResolver from current store state, swap via ArcSwap.
6. **GC:** `gc(min_live_gen)` on all stores — reclaim demoted slots where generation has
   expired. Proportional to recent churn, not total population.

## Enables incremental snapshots (scale-observation)

With generational storage, carry-forward is natural:

- Cold-tier PIDs are not re-read from /proc. Their CpuRing and PidMeta persist in the
  GenStores. ViewEntry projection from persistent stores is cheap (field copies, no I/O).
- StringRefs survive across cycles — they point to ALIVE slots in the generational string
  stores, not a per-cycle arena that gets reset.
- The per-cycle cmdline re-materialization (~1 MB at 5k PIDs) is eliminated: PidMeta
  stores a StringRef, ViewEntry copies the 8-byte handle, not the string bytes.

## Next: foundational access layer — captured-pointer views (designed, not yet built)

A design we converged on during phase-3 review; **implement this before/with phases 4–6** so
the new stores get it from the start. It is an access-mechanism change, not a new store.

**Problem.** `GenStore::slot()` re-derives the base on every access via a ~3-deep dependent
load chain: `arena.region.ptr` → `arena.chunks.ptr` → `chunks[id].off` → slot address. Each
load's address depends on the previous, so the chain serialises before any slot read —
microarchitecturally bad (pipeline stall) even though it's O(1) and L1-resident. A
foundational structure shouldn't impose that. (Note: the gatherer's per-PID loop has
*independent* iterations, so OoO execution already overlaps these chains there — so this is
foundational-correctness/principle and helps the UI render path + future tight loops, not a
measured win on the 2 Hz gatherer.)

**Solution — captured-pointer views.** Access through a *view* that captures the base **as a
raw pointer**, so every access is trivial (`base + idx*stride`). Two hard constraints:

1. **Raw pointers, never `&Arena`.** A published view is read on the UI thread while the
   gatherer does `&mut Arena` on its thread; a cross-thread `&Arena` concurrent with `&mut`
   is UB. Raw pointers sidestep the aliasing model. (`ByteResolver` already does exactly
   this — it is the template to generalise.)
2. **Validity = the generational lease, not a Rust borrow.** The view's captured pointers
   reference specific *frozen* structures: copy-on-grow leaves the old bytes intact (a
   regime-A hole / a regime-B retired region) and GC defers reclaim, so the referenced data
   is immutable for the view's lifetime even as the Arena keeps mutating. This is why it
   "adheres to Rust's borrow rules in spirit": the borrowed data is effectively immutable
   while borrowed. The lease length is the borrow length.

**Trivial access, heavier borrow.** Creating the view resolves the base once (the chase,
paid once); all subsequent accesses are trivial.

**Gatherer side (the mutator).** To use captured-base views for its own same-thread access
*and* stay sound for writes-after-grow, split the cycle:
- **Reserve pre-pass** (`&mut Arena`): `store.reserve(arena, upper_bound)` for each store —
  the *only* place relocation can happen, and rare (no-op once warmed). `upper_bound` is
  computable from the live PID count (provable, not a fragile guess), so under-reserve is a
  preventable panic, never UB.
- **Fill pass** (capture bases, no `&mut Arena`): reads + allocations-within-reserved-
  capacity are trivial. Allocation within capacity needs only `&mut store` + the captured
  base — *not* `&mut Arena` — so it is compatible with a held view, and no relocation can
  occur (no `&mut Arena` in scope), so the captured bases stay valid for the whole pass.

**Implementation sketch.** A `View`/cursor capturing the base raw pointer + slot layout
(trivial `get`/`get_mut`/`alloc`/`demote`/`free`); `reserve()` confines growth; the gatherer
cycle becomes reserve → capture → fill → publish; the published snapshot's `ByteResolver`
(and phase-5 `TypedBuf` slice-view) are the same shape, lease-scoped instead of
pre-pass-scoped. The current `&Arena`-threaded `slot()` stays correct meanwhile (gatherer-
only, no cross-thread `&Arena`); the `gc` loop already hoists the base.

## Phasing

1. ~~**Flat trait + GenStore\<T\> + TypedBuf\<T\>**~~ — **DONE** (in the `thoop` crate).
   Oracle property tests vs std equivalents landed. `Gen::at`/`is_reclaimable` take a `u64`
   generation; the caller picks the GC lag (≥ live window). `GenStore::construct` does
   in-place construction (parse/byte-copy straight into a slot, no temporary).
2. ~~**String stores**~~ — **DONE (cmdline only)** via `StrStore<N, S>` (wraps
   `GenStore<[u8; N]>`) + `StringRef<S>` + `ByteResolver<S>`. `Meta.cmdline: Vec<u8>` →
   `StringRef<Cmd>`; per-cycle cmdline re-materialization eliminated; unchanged cmdlines
   keep their slot. GC lags `GC_LAG` (2) generations behind the published generation. comm
   deferred to phase 3 (see status note). `HugePageBuf` survives (comm only) until phase 5.
3. ~~**PidMeta store**~~ — **DONE.** `ProcCache.map: PidMap<Meta>` → `GenStore<PidMeta>`
   (huge pages) + `PidIndex` (`PidMap<Ref<PidMeta>>`, still `FxHashMap` until phase 6).
   `PidMeta` is `Flat`/`Copy`; the update loop copies it out, mutates locally, writes back
   (so no `meta`/`arena` borrow is held across the `cmd_store` ops). **Key realisation:**
   `PidMeta` is gatherer-internal (no snapshot references it), so it uses `GenStore::free`
   (immediate reclaim, no lease) on death/eviction — only the cmdline *string* is leased.
   (Held fd state stays backend-private per the earlier deviation; not folded in.)
4. **CpuRing store** — `GenStore<CpuRing>` replacing `PidMap<CpuHistory>`. Also
   gatherer-internal → `free`, no lease. Decide: own PID→`Ref<CpuRing>` map, or unify with
   `PidIndex` into a shared `PidSlot { cpu, meta }`.
5. **ViewEntry on THP + fold comm** — `TypedBuf<ViewEntry>` replacing `Vec<ProcessEntry>`;
   snapshot becomes fully THP-resident; `HugePageBuf` comm arena retired. The intricate one:
   - **Cross-thread slice-view.** The UI reads the row array; `TypedBuf` must publish a
     raw-pointer *slice view* (the `ByteResolver` analogue — capture base + len at publish,
     `Send`/`Sync`, lease-validated). The snapshot holds the view, not the `TypedBuf`.
   - **Fold comm** into `PidMeta` (`comm: StringRef<Comm>` + a `Comm` `StrStore<16>`), with
     change-detection like cmdline. This is where comm *finally* leaves the per-cycle arena
     (deferred from phases 2/3 precisely because there was no per-PID home + the snapshot
     still had an arena). `ViewEntry.name` becomes `StringRef<Comm>` resolved via a comm
     `ByteResolver`. comm bytes reach `PidMeta` from the backend's reap — decide the path
     (stage in the read slot vs. a small transient).
   - **Regime-B vs the front buffer.** B repacks only *gatherer-owned* chunks; the front
     snapshot (UI-held) reads its data from the **old region**, kept alive by the lease until
     released. So B must not touch UI-held memory — verify the per-snapshot row buffers and
     the persistent stores interact correctly here (this is the subtle correctness point).
6. **ThpMap** — open-addressing table on THP, replacing the `PidIndex` `FxHashMap` (and any
   remaining `PidMap<V>`). The riskiest new unsafe code; gate behind an oracle property test
   vs `std::HashMap` over a churny insert/get/remove/retain workload. Keep `FxHashMap` as a
   fallback. Do this **last**.

Phases 1–2 are the prerequisite for scale-observation. Phases 3–6 (+ the captured-pointer
view layer above) complete the TLB coverage.

## Validation

1. **GenStore oracle test:** alloc/get/free/gc lifecycle matches a reference HashMap over
   a churny PID workload (randomized births, deaths, string changes, GC sweeps).
2. **TypedBuf oracle test:** push/clear/index matches Vec.
3. **THP backing confirmed:** non-zero `AnonHugePages` in `/proc/self/smaps_rollup` for
   store regions (where host policy permits). Graceful fallback where policy is `never`.
4. **No per-cycle cmdline copy** after phase 2: the ProcCache update path allocates
   strings only on change, not every cycle.
5. **ThpMap oracle test:** (phase 6) matches std::HashMap over churny insert/get/remove/
   retain workload.
6. **get_mut uniqueness** survives the storage change — double-buffer recycling still works.

## Risks / open questions

- **Flat enforcement:** the trait is a documentation contract (like `Send`/`Sync`), not
  compiler-verified. A wrong `impl Flat` for a type with a Vec field would violate the
  THP-closed invariant. Mitigate with code review and a grep-based lint.
- **Slot alignment:** `gen: u8` before `MaybeUninit<T>` introduces padding for aligned T.
  Alternative: store gen bytes in a parallel array within the same THP page (better GC scan
  locality, avoids per-slot padding). Implementation decision, not architectural.
- **Multi-page GenStore resolution:** StringResolver needs access to all pages of a
  GenStore. With a small bounded page count (one page per ~15k comm slots or ~8k cmd
  slots), a fixed-size page table in StringResolver suffices.
- **macOS:** `MADV_HUGEPAGE` is Linux-only. The abstractions no-op the hint on macOS;
  correctness is unaffected (same types, same lifecycle, smaller pages).
- **ThpMap complexity:** an open-addressing table is the riskiest new unsafe code. Phase it
  last, gate behind oracle tests, keep `HashMap<u32, V, FxBuildHasher>` as fallback.
