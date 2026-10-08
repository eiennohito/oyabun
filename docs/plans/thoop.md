# thoop — THP Generational Storage (the mechanism)

> **Scope.** A policy-free foundation for storing flat records on transparent-huge-page memory
> with a generational lifecycle. It is built for oyabun but holds **zero oyabun concepts** — no PID,
> snapshot, gather, and crucially no "single thread" written into a *contract* as a law — and is
> meant to be extractable as a standalone library. oyabun layers its own policy on top; see the
> storage section of `../ARCHITECTURE.md`. The current instantiation is single-threaded, but the
> multithread path is an additive **concurrency seam** (below), never a rewrite.

## Why huge pages

At thousands of randomly-accessed records the working set spans hundreds of 4 KiB pages — far
past the ~64-entry L1 dTLB — and thrashes it. On 2 MiB pages the same data is a handful of TLB
entries regardless of count. `MmapRegion` (anonymous, 2 MiB-aligned, `MADV_HUGEPAGE`, unmapped
on drop) is the primitive. Each *touched* region commits a whole 2 MiB of physical RAM, so the
design minimizes the number of regions.

## The arena — one region, dumb allocator

Structures do not each `mmap` a page (that wastes a 2 MiB commit per structure); they carve
chunks from **one** shared region.

- **Bump + whole-block free list, no splitting, no coalescing.** A freed chunk goes on a free
  list; a request reuses a free chunk that fits, taking it *whole* — the slack becomes that
  chunk's growth headroom, never a tracked remainder. "Don't re-split" and "over-allocate a
  little" are thus one mechanism, with no remainder bookkeeping to get wrong.
- **Compaction-on-(re)allocation is the only fragmentation defense.** When neither the free list
  nor the tail can satisfy a request, repack all live chunks into a fresh right-sized region and
  drop the old. Fragmentation is bounded — reset to zero at each compaction, which fires exactly
  when space runs out, and is rare under stable sizes + over-allocation.

## Writers — the arena is the factory

Containers (a generational slab, a string slab, a typed buffer, an open-addressing map keyed by
a flat sentinel-reserving key) are **writers** over arena chunks. A writer exists only as a
`Pin<Box<Writer>>` returned by an `arena.new_*` constructor
that, in one step, allocates the chunk, builds the writer at its final address, and registers
its base-cell location. The consequences are enforced by the type system, not convention:

- no separate wiring step to forget;
- a writer cannot be constructed against the wrong arena;
- `!Unpin` ⇒ a writer cannot be moved after construction — which would dangle the registry's
  back-reference. Rust moves are invisible, so this *must* be a type-level guarantee, not a rule.

## Base resolution — local cell, outside fix-up

Each writer caches its chunk's base in a **local tagged `Cell`** (a field of the writer). The
hot path is `self.base.get()` → mask the low bits → `base + idx·stride`, touching **no arena
memory** on read or write. Chunks are 4 KiB-aligned (≤4 KiB slack, free against the 2 MiB
floor), so the cell's low 12 bits are spare tag space — reserved; first use a "don't relocate"
flag for pinned/registered chunks.

The arena holds an **external registry** of every chunk's base-cell location (writer address +
the cell's offset within it). On a compaction it rebases every cell *from the outside* — it
knows exactly the set to patch — so no writer polls anything, and nothing reaches *into* a
writer except this. Writing a cell while its writer (or a sibling) is `&mut`-borrowed is sound
because the base is a `Cell`: the `UnsafeCell` interior is exempt from the borrow's exclusivity.

## Generational lifecycle — the library's identity

A generational slab tracks each slot's state in a one-byte tag: live, demoted-at-generation, or
free. The full lifecycle is `alloc / demote / free / gc(min_live)`:

- **immediate `free`** returns a slot at once;
- **`demote` then `gc`** defers reclamation until a generation passes — a general
  *deferred-reclamation / versioned-handle* tool (reclaim at a frame boundary, ABA-safe
  handles), **not** merely cross-thread plumbing. It is single-threaded-useful and stays core
  API even when a given consumer uses only `free`.

Records are `Flat`: fixed-size, `Copy`, no heap, embeddable handles only.

## Invariants

- **No reference into arena memory may span an `alloc`/`grow`/`free`** — any may compact and
  move bytes. Stored records are read out / modified / written back, never held by reference
  across a store op.
- **A writer is never plain-moved after construction** (`Pin` enforces; a deliberate relocation
  re-registers).
- The base `Cell` is reached only through `Cell` ops (never a `&mut` to the inner pointer), so
  the registry's outside writes stay sound.

## The concurrency seam (named, not built)

The storage layer is policy-agnostic; the single-thread instantiation is a *choice*, never asserted
as law in a contract. A multithread layer is **additive**, confined to:

- the **base cell**: `Cell` → an atomic, swapped in the one accessor;
- **reclamation**: use the deferred `demote`/`gc` path (already present) so a reader can hold a
  stale handle safely;
- a **cross-thread read view** (a base capture validated by the generational lease);
- a `Send`/`Sync` story for that published view.

None are built now (the only consumer is single-threaded). They are listed so the boundary stays
honest — a future extraction neither over-anchors on "single-thread" nor rediscovers the path.

## Validation

- **Allocator oracle:** alloc/free/reuse/compaction matches a reference model over a churny
  workload; fragmentation returns to zero after a compaction.
- **Lifecycle oracle:** alloc/demote/free/gc matches a reference model over churn.
- **Map oracle:** random insert/update/remove and bulk `retain` churn matches a `HashMap`
  reference, across the rehash relocations a tiny initial capacity forces.
- **THP backing confirmed:** non-zero `AnonHugePages` for regions where host policy permits;
  graceful fallback where it is `never`.
