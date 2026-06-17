# atop — THP-resident PID index (the last storage step)

> **Status (2026-06).** The single-thread collapse landed: atop runs one serialized
> `gather → render` loop, fills an arena-resident row buffer in place, folds `comm` inline,
> and frees per-PID/cmdline slots immediately (no lease). That design + rationale now live in
> `docs/ARCHITECTURE.md` (atop's policy) and `docs/plans/thoop.md` (the substrate mechanism).
> **This doc is the one remaining piece**: moving the PID index itself onto huge pages.

## What's left

Every per-PID *record* (CPU history, uid/cmdline metadata, cmdline strings) and the process
*row buffer* are THP-resident. The one structure still on the general heap is the **PID →
slot-handles index** — an `FxHashMap`. At thousands of PIDs, randomly probed once per PID per
cycle, it is exactly the TLB-thrashing access pattern huge pages exist to fix; leaving it on
4 KiB pages undercuts the rest of the work.

## The step

Replace the `FxHashMap` with a `thoop` open-addressing hash table resident in the shared arena
(same arena as the stores and the row buffer). Keys are PIDs (`u32`, not attacker-controlled —
the existing fast `FxHash`-style hash stays); values are the existing per-PID slot record
(the CPU/meta `Ref`s plus the incarnation/cadence bookkeeping that already lives in the index
value, *not* in the stores — so the table stays a flat `Copy` payload with no chase).

Design constraints carried over from the substrate:

- **Flat, open-addressing, no per-entry heap.** Tombstone-or-Robin-Hood deletion (eviction of
  vanished PIDs happens every cycle, so deletion must not degrade probing).
- **Lives in an arena chunk**, grows through the arena like the other writers (cached base,
  epoch self-heal). A rehash-on-grow is the table's analogue of a store regrow.
- **No reference into the table may span an arena allocation** — same invariant as the stores;
  the per-PID fill already copies records out and writes them back.

## Fold in: per-PID encapsulation + priority gathering

This rewrite reworks the per-PID value record (`PidSlot` is the deliberate extension point for
volatility signals → adaptive sampling, see `scale-observation.md`). Do the encapsulation
cleanup here too, rather than as a separate pass: tighten the per-PID POD that today exposes
raw fields whose invariant nothing guards — notably `ProcessEntry`'s inline `comm` pair
(`comm_bytes`/`comm_len`), which should be reachable only through `comm()`/`set_comm()` so an
out-of-range length is non-representable. It's a no-op for a binary crate's external surface,
but the right discipline to land while the records are already being reshaped.

## Why last, why gated

This is the riskiest unsafe in the codebase: open addressing + relocation + a per-cycle
insert/lookup/evict mix is easy to get subtly wrong, and a corrupt index mis-attributes CPU
history or cmdlines across PIDs (silent, not a crash). So it ships **behind an oracle test**:
a churny insert/lookup/evict/grow workload checked against a reference `HashMap` model (the
same shape as `thoop`'s allocator/lifecycle oracles), plus the existing gatherer invariants
(PID-sorted rows, valid tree links, correct CPU/cmdline attribution across births/deaths/reuse)
held across the swap. Land it only when the oracle is green.

## Validation

- **Index oracle** — random insert/get/remove/grow churn matches a `HashMap` reference model,
  across the rehash relocations a small initial capacity forces.
- **No attribution drift** — the gatherer's existing oracle (`uring_matches_syscall_backend`)
  and the CPU/cmdline tests pass unchanged with the new index in place.
- **THP backing** — the index chunk shares the arena, so it inherits the substrate's
  `AnonHugePages` confirmation; no separate mapping to check.
