# atop — Single-Thread Collapse (storage policy)

> **Status (2026-06).** atop is collapsing to a **single thread** running one serialized
> `gather → render` loop, using the `thoop` storage substrate (mechanism: `thoop.md`) under a
> single-thread *policy*. Phases 1–4 landed (storage primitives + cmdline / per-PID metadata /
> CPU history on the arena + the io_uring landing pad). Phase 5 is the collapse; phase 6 the THP
> PID index. The cross-thread machinery the earlier design built is superseded (trail at the
> end). The mechanism is `thoop`'s; this doc is atop's policy on top of it.

## The pivot — serialize to one thread

Every copy, the double buffer, the cross-thread lease, and the generational reclaim *lag*
existed for one reason: two threads touched the data at once, so a published snapshot had to be
a private immutable value the gatherer never mutated while the UI read.

A memory fence does not remove that need. Acquire/release gives *visibility ordering* —
generation N's writes are visible to a reader that loaded N — but not *mutual exclusion*: it
says nothing about generation N+1's in-place writes racing the reader's N-view reads. Soundness
needs exclusion, and there are only three ways to get it: disjoint *memory* (the double buffer —
the copy), disjoint *time* (serialize), or never overwriting a live cell (copy-on-write +
lease). The first and third keep the concurrency machinery; the second deletes it.

Serializing is free here because the responsiveness budget is loose (`GOALS.md`: < 10 ms p90,
< 100 ms p99.99). With one thread the loop calls `gather` then `render` in sequence, so the
borrow checker *proves* they never alias (`&mut` to gather, `&` to render, non-overlapping) —
data races become a compile error, not a runtime discipline.

**Cost, accepted.** The UI cannot redraw or service input *while a gather runs* — sub-ms
normally, a few ms only on the periodic full `/proc` rescan at extreme scale, both far inside
the budget. The one thing two threads bought — painting while the gatherer is blocked in that
rescan syscall — is the only thing given up; if it ever bites, chunk the gather to yield to
input, don't re-add a thread.

## atop's policy on the substrate

`thoop` is policy-free (generational, concurrency-agnostic). atop picks the single-thread
instantiation, sound only because gather and render never overlap (exclusion by the borrow
checker). Each choice is exactly a cross-thread-safety piece *removed* because exclusion
replaced it:

- **immediate `free`**, not deferred `demote`/`gc` — nothing else holds a reference, so a
  dead/changed slot or a compacted-away region is reclaimed at once.
- **`Cell` base**, not an atomic — no other thread reads it.
- **in-place mutation** of volatile per-PID fields, and the UI **reads the stores directly** —
  no per-snapshot copy, no published lease view.

Re-adding these (atomic base, deferred free, copy-on-write/lease view) is the documented
multithread path — `thoop`'s concurrency seam — if the latency budget ever changes. Single
thread is atop **policy**, not a `thoop` limit; do not treat it as sacred.

## Data, not snapshots

The published snapshot stops being a private immutable value. Per-PID truth (CPU history, uid,
cmdline handle) lives in `thoop` stores, updated in place each gather. The row array and its
tree links live in an arena buffer the UI reads directly. UI drawables — the collapse-flattened
display list, later the sort order — allocate from the same arena (O(processes), randomly walked
during render → the same TLB win; single thread → no lease, no `Send`).

**comm inline (atop domain).** The process name is re-parsed from `stat` every cycle, so a store
for it is pure overhead — fold it into the row as fixed bytes (≤15, `TASK_COMM_LEN`; truncate
beyond). cmdline is the opposite (slow-changing, worth not re-reading) → keep its `thoop` string
store + a stable handle, read directly. The rule: volatile fields the gatherer rewrites each
cycle are copied into the row; only slow, change-detected data earns a referenced store slot.

## Cycle (single thread)

1. Enumerate births (maintained live set + birth probe; full rescan ~1 s) — unchanged.
2. `gather(&mut)`: read stat into the landing pad, parse volatile fields into the row buffer
   (comm inline), update the in-place CPU/meta stores, intern a changed cmdline (free the old
   slot at once), apply births/deaths, build the tree + aggregates + system stats.
3. `render(&)`: flatten under the collapse set into a drawable, draw the scroll window
   (value-gated). Reuses its drawables; reallocates only when the working set grows.

No publish, no swap, no GC pass.

## What retires (atop cross-thread machinery)

The lock-free snapshot cell + the UI→gatherer channel (the UI calls gather directly; a kill
calls it inline); the double-buffer recycling; the published byte-resolver lease and its
`Send`/`Sync`; the per-snapshot owned row region and the per-cycle row copy; the
regime-B-vs-front-buffer hazard. **Not** the generational lifecycle — that is `thoop`'s and
stays (atop merely uses the immediate-`free` subset). The per-cycle comm arena and atop's local
string-ref type retire as comm folds inline.

## Landing pad (done)

Reads target a small fixed `MmapRegion` landing pad (registered once for io_uring), not any
arena; parse copies the volatile fields out and frees the slot. Pinned memory is a small
constant independent of PID count; a mid-cycle arena grow is never an I/O-target hazard.

## Phasing

1–4 — done. 5 — the collapse (this doc): single-thread the loop, delete the retired machinery,
adopt the single-thread policy on `thoop`, move rows + drawables into the arena, fold comm
inline. Order the steps so rows stay PID-sorted and every tree link/handle stays valid
throughout. 6 — replace the `FxHashMap` PID index with a `thoop` open-addressing table, gated by
an oracle test; riskiest unsafe, last.

The collapse also simplifies later scale-observation: viewport/collapse state is read directly
(no channel); incremental carry-forward is in-place store updates (no lease).

## Validation (atop)

- **Borrow-check is the race proof** — `gather(&mut)` / `render(&)` non-overlap is compile-time;
  no runtime exchange remains to test for aliasing.
- **No per-cycle cmdline copy** — an unchanged cmdline keeps its slot.
- **Tree invariants** — PID-sorted rows, valid links, correct task tallies — held across the
  collapse. (`thoop`'s own oracles live in `thoop.md`.)

## Superseded — the cross-thread lease design (reasoning trail)

The earlier plan kept two threads and made the snapshot a private immutable value: rows in the
shared arena published as a `Send` view + a byte resolver, cross-thread safety resting on
generation arithmetic (copy-now / free-later — a demoted slot or a retired region lingers until
`min_live` passes it) and a cooperative epoch self-heal for cached bases. All sound, all *only*
for cross-thread reads. Serializing removes the reader/writer overlap, so the lease, the
resolver, the reclaim lag, and the epoch have nothing left to protect — they retire. What
survives is `thoop`'s substrate (now its own doc), with the generational lifecycle kept as a
general feature and the epoch self-heal replaced by the local-cell base + the arena's
outside-fix-up registry.
