# THP Arena — Put Hot Random-Access Structures on Huge Pages

## Problem

After the persistent-fd work killed the io_uring close-storm, a re-profile shows the
gatherer's remaining cost is the irreducible floor (`do_task_stat`, `parse_stat`) plus
`/proc` enumeration. Absolute CPU is now tiny (sub-ms per 500 ms cycle at ~700 procs).

The next structural cost is **TLB pressure on random access**, and it is invisible to an
idle back-to-back microbenchmark — which is exactly why it was missed. A tight loop keeps
`procs`, the PID maps, and their page-table entries hot in the dTLB the whole time. That
state never occurs in production: the real cadence is one cycle per 500 ms, and in the gap
— *especially under load, when a monitor is most used* — other processes evict our TLB
entries and cache lines. Each real cycle then starts cold and the random walks page-walk.

We currently put THP on the **wrong** structure:

| structure | access pattern | backing today |
|---|---|---|
| `strings` arena (`HugePageBuf`) | sequential fill, sequential-ish UI read | **THP** ✓ |
| `procs: Vec<ProcessEntry>` (tree build, aggregate, UI `proc_idx` deref) | **random** | 4 KiB heap ✗ |
| `CpuTracker.hist`, `ProcCache.map` (PID-keyed) | **random** (hash probes) | 4 KiB heap ✗ |

Sequential access is what the prefetcher and a linear TLB walk tolerate. **Random access
is what needs THP most**, and it is the part we left on 4 KiB pages.

Scale math (2 MiB huge page, `ProcessEntry` ≈ 100 B → ~20k entries/page):

| procs | `procs` size | 4 KiB pages (TLB entries, cold) | huge pages |
|---|---|---|---|
| 700 | ~70 KiB | ~17 | 1 |
| 5 000 | ~500 KiB | ~125 (≫ ~64-entry L1 dTLB) | 1 |
| 50 000 | ~5 MiB | ~1280 | 3 |

So on 4 KiB pages a cold random walk page-walks on nearly every access at scale; on huge
pages the entire working set is covered by a handful of TLB entries regardless of scale.

## Goal

**Bound the worst-case TLB footprint structurally**, not optimize a measured idle case.
Every hot, randomly-accessed structure lives on huge pages, so a fully-cold dTLB (post
eviction, under load, at any process count) still covers the working set in a few entries.
Validation is **by construction** (assert the mapping is THP-backed), not by a flaky cycle
count — the worst case resists idle measurement (see Validation).

This is universal (helps every machine, not just extreme hardware) and has no cross-thread
coupling. It is the prerequisite for the separate scale-observation plan, whose incremental
snapshot needs stable, huge-page-backed storage anyway.

## Design

### 1. `ProcessEntry` storage → THP (the core win)

Replace `Snapshot.procs: Vec<ProcessEntry>` with a huge-page-backed typed array. The tree
build (binary-search parent lookup, reverse-index prepend), `tree::aggregate`, and the UI's
`proc_idx` dereference all index randomly into this array — that is the access pattern that
page-walks cold.

Two shapes to choose from in impl:
- A typed `HugePageVec<T>` (generalize `HugePageBuf` to back `[T]` with a length + bump
  cursor; `reset`/`push`/`retain` reimplemented), or
- Carve the `ProcessEntry` region out of the snapshot's existing `HugePageBuf` (one mapping,
  partitioned into a procs region + a strings region).

Constraints to preserve:
- **`Arc::get_mut` uniqueness** for the double-buffer recycling must still hold (the gatherer
  mutates only under unique access; the UI reads read-only). A `HugePageBuf`-owned region
  inside `Snapshot` keeps this — `get_mut(&mut Snapshot)` reborrows its fields.
- **Reserve-up-front invariant**: pre-size the procs region from the PID count before filling
  (same rule the strings arena already follows), so no mid-cycle remap relocates it.
- `reset` is O(1) (rewind cursor; POD, no drops), `compact` removes tombstones in place.

### 2. PID maps → THP-backed open-addressing tables

`CpuTracker.hist` and `ProcCache.map` are `HashMap<u32, V, FxBuildHasher>` on the global
allocator — 4 KiB pages, and hash probes are random access. `std::HashMap` cannot be
`madvise(MADV_HUGEPAGE)`'d (we do not own its allocation).

Replace with a small custom open-addressing table (FxHash, linear or Robin-Hood probing)
backed by a huge-page region. It must support the existing generation-eviction pattern
(`retain(|_, v| v.seen_gen == cur_gen)`), grow by power-of-two with rehash, and tombstone on
delete. This is the meatier piece — it can land *after* §1 (procs is the bigger TLB win and
is self-contained). Keep the `PidMap<V>` type alias so call sites are unchanged.

### 3. Registered read-buffer prefix (plan §1a, folds in here)

Today both snapshot arenas are registered with io_uring in full (`register_buffers` over the
whole `HugePageBuf`), so **pinned memory grows with PID count**. And the arena retains the
full ~`STAT_SLOT` (512 B) of raw stat text per PID for the snapshot's life, though only
`comm` (~15 B) survives parsing.

Once the arena is partitioned (§1), register only a small fixed **read-buffer prefix** —
sized to the in-flight depth (bounded by the io_uring SQ / pool concurrency), **not** the PID
count — as the `ReadFixed` target. Pinned memory becomes bounded and constant. After parse,
`comm` is copied into the unregistered string region (this copy already effectively happens;
formalize the split). cmdline stays per-snapshot and self-contained (the per-cycle copy is
retained here — eliminating it requires stable cross-snapshot string storage, which is
deferred to the scale-observation plan where carry-forward actually needs it).

## Validation checkpoints (do these first)

1. **THP actually backs the mappings.** `madvise(MADV_HUGEPAGE)` is best-effort; confirm the
   procs and map regions show non-zero `AnonHugePages` in `/proc/self/smaps_rollup` (or per
   mapping in `smaps`) on this kernel's THP policy. If THP is `madvise`-gated and disabled,
   document the degradation (we fall back to 4 KiB — correctness unaffected). Assert madvise
   was attempted; the test is structural, not a perf number.
2. **`get_mut` uniqueness** survives the storage change — the double-buffer recycling test
   (`gatherer_publishes_valid_snapshot`, the recycling path) must still pass; add an explicit
   check that the gatherer obtains unique access to the back buffer's procs region.
3. **Open-addressing map parity** with `std::HashMap`: a property/oracle test that the custom
   table agrees with `HashMap` on insert/get/retain/grow across a churny PID workload (mirror
   the `uring_matches_syscall_backend` oracle style).
4. *(Optional, confirmation only — not the design driver.)* Worst-case TLB under synthetic
   load: a `stress-ng`/co-tenant TLB-thrasher running alongside, at high simulated PID count,
   measuring `dTLB-load-misses` before/after on the gather + a UI-draw microbench. Expect a
   large drop; but the design stands on construction, so a missing/ambiguous number does not
   block.

## Touch points

- `arena.rs`: generalize `HugePageBuf` (or add `HugePageVec<T>`) to back typed arrays;
  partition into procs + read-prefix + string regions.
- `snapshot.rs`: `procs` storage type; `reset`/`push_tombstone`/`compact` reimplemented on the
  huge-page array; preserve POD/O(1) reset and the PID-sorted invariant.
- `gather/mod.rs`: `CpuTracker.hist` / `ProcCache.map` → the THP open-addressing table behind
  the `PidMap` alias; `Snapshot` construction sizes the regions.
- `gather/uring.rs`: `register_buffers` targets the read-prefix region; `ReadFixed` writes
  there; `update_buffer` on grow unchanged in spirit.
- `tree.rs`, `app.rs`, `ui.rs`: ideally untouched — keep `procs` accessible as `&[ProcessEntry]`
  / index so callers don't change.

## Risks / open questions

- **Open-addressing table correctness** (tombstones, resize, generation retain) is new unsafe-
  adjacent code — gate behind the oracle test in checkpoint 3; phase it after procs if risky.
- **Region growth mid-cycle**: the procs region must obey reserve-up-front like strings; a grow
  must not relocate while an io_uring read or the UI references it (the existing invariant).
- **THP policy varies by host** (`never`/`madvise`/`always`); correctness must not depend on
  THP actually being granted — it is a performance hint only.
- **macOS**: `MADV_HUGEPAGE` is Linux-only; the macOS backend is future work regardless. Keep
  the arena abstraction so a macOS path can no-op the hint.
- **`ProcessEntry` size growth**: adding fields shrinks entries-per-huge-page; still ≫ any real
  PID count per page. Not a concern, but keep the struct POD and tight.

## How to verify it worked

- The three structural checkpoints pass (THP-backed, uniqueness, map parity).
- Full suite green; no behavioral change (this is a memory-layout refactor, not a feature).
- Optional: the synthetic-load TLB measurement shows `dTLB-load-misses` on the gather + draw
  paths collapse at high PID count.

## Sequencing

This plan **first**. It is universal, low-coupling, and makes all-PID observation miss-free —
which means below extreme scale the scale-observation plan's rotation is unnecessary. The
stable-string / incremental-snapshot work that rotation needs builds on the arena partitioning
introduced here.
