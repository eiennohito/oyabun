# THP Arena — Put Hot Random-Access Structures on Huge Pages

> **Status (2026-06):** the I/O-decoupling piece (originally §1a / "registered read-buffer
> prefix") **landed** — see below. The two TLB pieces (§1 `procs`, §2 PID maps) remain and
> are what this plan now tracks. The `MmapRegion` primitive added for the landing pad is
> the foundation they build on.

## Done — landing pad (decouple I/O target from string store)

Reads no longer target the snapshot arena. The io_uring backend owns a small **fixed**
`MmapRegion` landing pad (`read_slots × STAT_SLOT`), registered once; each read claims a
slot, `reap` parses it and copies `comm` (~15 B) into the (unregistered) arena, then frees
the slot. Consequences:

- **Pinned memory is a small constant**, independent of PID count → a low per-process lock
  limit no longer silently downgrades io_uring as the process count grows. (The hoped-for
  follow-on — non-root `perf` profiling of the io_uring path — turned out to be blocked by a
  *separate* symptom at ring creation, not buffer registration; see
  `docs/plans/iouring-perf-fallback.md`. Claim withdrawn pending that investigation.)
- The arena shrank ~3×/PID (no retained raw stat text) and a mid-cycle grow is no longer a
  correctness hazard (arena is not a registered buffer) — reserve-up-front is now a perf
  hint only.
- Dropped along the way: the two-tier `long_stat` slot machinery, `Snapshot::buf_index`,
  the arena re-registration-on-grow path. `MmapRegion` (huge vs pages sizing) is the new
  shared primitive.

Deviation from the original framing: a **separate** landing pad, not a prefix carved out of
the arena — cleaner (no growth coupling, no re-registration) and bounds pinned memory the
same way.

## Remaining problem (the TLB pieces)

The gatherer's hot random-access structures are still on 4 KiB heap pages, and random access
is exactly what page-walks when the dTLB is cold (post-eviction, under load, at any process
count — invisible to a tight idle microbenchmark, which keeps everything hot):

| structure | access pattern | backing today |
|---|---|---|
| `strings` arena (`HugePageBuf`) | sequential fill, sequential-ish UI read | **THP** ✓ |
| `procs: Vec<ProcessEntry>` (tree build, aggregate, UI `proc_idx` deref) | **random** | 4 KiB heap ✗ |
| `CpuTracker.hist`, `ProcCache.map`, backend `held` (PID-keyed) | **random** (hash probes) | 4 KiB heap ✗ |

Scale math (2 MiB huge page, `ProcessEntry` ≈ 104 B → ~20k entries/page): at 5 000 procs the
`procs` array spans ~125 cold 4 KiB pages (≫ a ~64-entry L1 dTLB) but **one** huge page; the
PID-keyed maps are larger still (`CpuHistory` ≈ 216 B/entry). On huge pages the whole working
set is covered by a handful of TLB entries regardless of scale.

## Goal

**Bound the worst-case TLB footprint by construction** — every hot, randomly-accessed
structure on huge pages — not optimize a measured idle case. Validation is structural
(assert the mapping is THP-backed where policy allows; the worst case resists idle timing).

## Design

### §1 — `ProcessEntry` storage → THP

Replace `Snapshot.procs: Vec<ProcessEntry>` with a typed huge-page array (`HugePageVec<T>`
layered on `MmapRegion::huge`, mirroring how `HugePageBuf` is layered now). Keep it usable as
`&[ProcessEntry]` (Deref) so `tree.rs`/`app.rs`/`ui.rs` and the `&mut snap.procs` call sites
are untouched; only `snapshot.rs` (`push`/`clear`/`retain`/`len`) changes.

Constraints to preserve: `Arc::get_mut` uniqueness for double-buffer recycling (a
`HugePageVec` owned inside `Snapshot` keeps it — `get_mut(&mut Snapshot)` reborrows fields);
O(1) POD `reset`; in-place `compact`. `procs` grows only in `prepare` (never during
`collect`), and it is not an io_uring target, so a grow/move is always safe — no
reserve-coupling.

### §2 — PID maps → THP open-addressing tables

`CpuTracker.hist`, `ProcCache.map`, and both backends' `held` are `HashMap<u32, V,
FxBuildHasher>` on the global allocator (4 KiB; hash probes are random). `std::HashMap` can't
be `madvise`'d (we don't own its allocation). Replace with a small open-addressing table
(FxHash, linear probing, tombstones + rehash-on-load or backward-shift deletion) whose slot
array is a `HugePageVec<Slot<V>>`. Must support the generation-eviction `retain`, grow with
rehash, `remove`, `get`/`get_mut`/`get_or_insert_with`, `len`, `values`. Keep the `PidMap<V>`
alias so all four call sites flip at once.

This is the riskier piece (new unsafe-adjacent code) — **gate behind an oracle property test**
vs `std::HashMap` over a churny insert/get/remove/retain/grow PID workload (mirror
`uring_matches_syscall_backend`). Phase it after §1.

Tradeoff to weigh when doing this: each THP structure occupies ≥1 fully-faulted 2 MiB page
even when nearly empty → ~+12 MiB RSS floor (2 procs buffers + the maps). Intended by "bound
the worst case at any scale," but real at small scale. The tiny `held` maps benefit from THP
only at extreme PID counts; the alias flips them too for uniformity.

## Validation checkpoints

1. **THP actually backs the mappings.** Confirm the procs/map regions show non-zero
   `AnonHugePages` in `/proc/self/smaps_rollup` on this host's policy (here: `enabled=always`,
   so a faulted 2 MiB-aligned madvised region is huge-backed). Where policy is `never`, document
   the degradation (fall back to 4 KiB — correctness unaffected); assert madvise was attempted.
2. **`get_mut` uniqueness** survives the storage change — the recycling test must still pass.
3. **Open-addressing map parity** with `std::HashMap` (the oracle test above).
4. *(Optional, confirmation only.)* `dTLB-load-misses` on gather + UI-draw under a synthetic
   TLB-thrasher at high simulated PID count, before/after. The design stands on construction.

## Touch points

- `arena.rs`: add `HugePageVec<T>` on `MmapRegion::huge` (the primitive already exists).
- `snapshot.rs`: `procs` storage type; reimplement `reset`/`push_tombstone`/`compact`.
- `gather/mod.rs`: `CpuTracker.hist` / `ProcCache.map` / backend `held` → the THP table behind
  `PidMap`; size the regions.
- `tree.rs`, `app.rs`, `ui.rs`: untouched (keep `procs` as `&[ProcessEntry]`).

## Notes / open questions

- THP policy varies by host (`never`/`madvise`/`always`); correctness must not depend on it.
- macOS: `MADV_HUGEPAGE` is Linux-only; keep the abstraction so a macOS path no-ops the hint.
- Keep `ProcessEntry` POD and tight (entries-per-huge-page shrinks as fields are added, but
  stays ≫ any real PID count per page).
