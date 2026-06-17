# Architecture

Status: **implemented** (Linux). macOS and several features below are still future work.

> **Pivot in progress (2026-06).** The two-thread / `ArcSwap` / cross-thread-lease design
> described below is being collapsed to a **single thread** running one serialized
> `gather → render` loop — which retires the double buffer, the cross-thread string lease, and
> the generational reclaim lag (`GOALS.md` priority order: the latency budget makes serializing
> free, and it buys away whole classes of CPU/RAM overhead). The threading, snapshot-lease, and
> generational-storage sections here still describe the *current built code*; they are rewritten
> as the collapse lands. Target model + rationale: `docs/plans/thp-arena.md`.

This describes how atop is actually built. For *why* (goals/constraints) see `GOALS.md`.

## Threading model

Two OS threads, no async runtime:

- **UI thread** (main): `etch` (retained renderer) + crossterm event loop. Loads the
  current snapshot, flattens the tree to display rows, renders, handles input. Does no
  `/proc` I/O; at startup it waits for the gatherer's first snapshot.
- **Gatherer thread**: enumerates `/proc`, reads stat files, parses, computes CPU%,
  builds the tree, publishes the snapshot. Owns the ring and is its sole submitter, so
  the ring is built on this thread, not at construction on main — io_uring's modern
  single-submitter optimizations bind a ring to its creating thread. Its huge-page arena
  and generational stores are built here too: they hold self-referential raw pointers (a
  store caches a `*const Arena`), so they are `!Send` and must be constructed in place on
  the thread that uses them — main creates only the shared snapshot cell and the (`Send`)
  `/proc` dir fd, then hands them over. Its first gather also primes the opening snapshot.

Exchange is via `arc_swap::ArcSwap<Snapshot>` — lock-free; the UI never blocks the
gatherer.

Control flows UI → gatherer over an `mpsc` channel (`Ctrl::{Refresh, Quit}`). The
gatherer blocks on `recv_timeout(interval)`: a timeout triggers a refresh, `Refresh`
forces an immediate one (sent right after a kill so the change shows fast), `Quit`
(or channel hangup) stops the thread. Blocking on the channel = near-zero idle CPU.

The UI wakes on input or a short poll timeout (`UI_POLL`, 200 ms) and only redraws
when something changed (a `dirty` flag set by input, resize, scroll, or a new
snapshot generation). Pure idle = a cheap `ArcSwap` load + generation compare.

### Double-buffer recycling (critical detail)

Exactly **two** `Snapshot`s exist steady-state; they ping-pong with zero allocation
after warmup. The gatherer keeps the just-swapped-out `Arc<Snapshot>` as `recycled`:

1. `Arc::get_mut` the recycled buffer to obtain unique `&mut` (succeeds once the UI
   has advanced to a newer generation and dropped the older `Arc`).
2. Reset + refill it.
3. `arc_swap.swap(new)` → returns the previous front; stash it as the next `recycled`.

The UI **must** use `load_full()` (a real `Arc`, bumping the strong count), not the
cheap `load()` guard — otherwise `get_mut`'s uniqueness check would race the UI's
read. If `get_mut` ever fails (UI still holding it — vanishingly rare given the
500 ms interval vs ms-scale UI turnaround), the gatherer **skips that cycle** rather
than allocate a third buffer (the landing pad is backend-owned and shared, so a
third snapshot is no longer *impossible* — but the design keeps exactly two so the
snapshot pool stays bounded and the recycling logic stays simple).

## Snapshot model

Index-based, POD, arena-backed — no pointers, no lifetimes, no per-process heap
allocation. `Vec::clear` drops nothing, so reset is O(1).

```
Snapshot { procs: Vec<ProcessEntry>, strings: HugePageBuf, cmd: ByteResolver<Cmd>,
           first_root: u32, generation: u64, sys: SystemStats }

SystemStats {                        // Copy, per-cycle system-wide stats
  cpu_user_bp, cpu_sys_bp, cpu_iowait_bp,   // CPU deltas as basis points
  mem_total, mem_used, mem_cached,           // bytes
  swap_total, swap_used,                     // bytes
  load: [u32; 3], uptime_secs, num_cores,
  tasks_running, tasks_sleeping, tasks_stopped, tasks_zombie, tasks_idle
}

ProcessEntry {                       // Copy
  pid, ppid, uid, state(u8), priority(i8), nice(i8), num_threads,
  cpu_pct(bp), cpu_peak(bp), mem_bytes, ticks,
  start_time,                        // (pid, start_time) = a unique incarnation
  name: StringRef,                   // offset+len into `strings` (comm), per-cycle arena
  cmdline: StringRef<Cmd>,           // handle into the generational Cmd store (resolved via `cmd`)
  non_ascii: bool,                   // comm|cmdline has a byte ≥0x80 → renderer unicode path
  is_kthread: bool,                  // PF_KTHREAD — kernel thread (no cmdline ever)
  parent_idx, first_child, next_sibling, subtree_size, depth,  // tree links
  subtree_cpu, subtree_mem           // inclusive aggregates for collapsed display
}
```

`Snapshot` also carries `pool_overflow: u32` — live PIDs this cycle that exceeded the
persistent-fd pool and used the transient fallback (0 in the common case; non-zero ⇒
`RLIMIT_NOFILE` is the binding constraint). Surfaced so a low fd limit is never silent.

```
StringRef { offset: u32, len: u32 }       // slice of `strings` (comm; the `arena` crate)
StringRef<Cmd> { idx: u32, len: u16 }     // slot in the generational Cmd store (`thoop`)
```

`comm` and `cmdline` use two different string mechanisms because they have different
lifetimes. `comm` is re-parsed from `stat` every cycle (cheap, no redundancy), so it lands
in a per-cycle bump arena (`strings`, reset each cycle). `cmdline` is *cached* across cycles
(read on a coarse cadence) — re-copying the cached bytes into a per-cycle arena every cycle
was pure waste, so it lives in a **generational store** whose slots persist across cycles
(see Generational storage below).

`NONE = u32::MAX` is the null index/root sentinel. PIDs vanishing mid-scan are kept
as tombstones (`pid == 0`) and `compact()`ed out before publish; because PIDs are
enumerated sorted and filled by index, `procs` stays sorted by PID (the tree build's
binary-search precondition).

**Tombstone hygiene**: a slot is born a tombstone and only goes live when a read parses.
Every failure path (open, read, reopen, parse) must re-mark the slot dead, or it survives
compaction as a *phantom row* — a real PID with empty fields. This is load-bearing for the
birth probe (below), where most speculative reads are meant to fail.

## Memory regions (`thoop`, `HugePageBuf`)

`MmapRegion` (in the `thoop` crate) is the owned-`mmap` primitive (anonymous private
mapping, rounded up to a 2 MiB huge page, `MADV_HUGEPAGE`-hinted, unmapped on drop). It
backs the comm arena, the io_uring landing pad, and `thoop`'s generational stores. When
only part of a mapping should be *pinned*, the caller registers a sub-range — the landing
pad pins only its read-slot prefix and uses the free huge-page tail for unpinned scratch
(below), so locked memory never exceeds the prefix.

`HugePageBuf` (a huge `MmapRegion` + bump cursor; `reset()` rewinds, `reserve()` grows by
doubling) is the snapshot's **comm arena** — *not* an I/O target. After parsing, the
gatherer copies each PID's `comm` (~15 B) into it. (cmdline used to be re-materialized here
every cycle; it now lives in a generational store instead — see below.) Cross-thread
sharing is sound because mutation happens only under `get_mut` (unique access) and shared
access is read-only.

Because the arena is no longer an io_uring registered buffer, a mid-cycle grow can no
longer race an in-flight read or stale a registration. The gatherer still `reserve()`s the
cycle's need up front, but now purely to avoid a re-mmap+copy mid-fill — a performance
choice, not a correctness invariant. Reads land elsewhere (the landing pad, below); only
the small `comm` copy hits the arena.

## Generational storage (`thoop`) — the arena, the cmdline store, the lease

The gatherer's slow-changing per-PID strings should not be re-copied every cycle. `thoop`
provides a **generational store** (`GenStore<T>`, and `StrStore<N,S>` for byte slots): a
slot persists across cycles, and a one-byte `Gen` tag tracks its lifecycle — `ALIVE`
(immortal until released), demoted-at-generation (released but maybe still leased), or
`FREE`. cmdline lives in a `Cmd` `StrStore`; an unchanged cmdline keeps its slot (no
per-cycle copy — the eliminated waste), and only a *changed* cmdline allocates a new slot
and demotes the old.

**The arena.** Stores do not each `mmap` a huge page — with THP, a touched 2 MiB mapping
commits a full huge page, so a mapping per structure is ~5× waste at a dozen structures.
Instead they share a few regions via an `Arena` suballocator that hands out `ChunkId`s. The
arena is **interior-mutable** — accessed by `&self`, never `&mut` — because it owns no slot
data, only the regions and the chunk table; so allocation and relocation go through a shared
reference and many stores grow through one arena without the self-referential-borrow problem
that threading `&mut Arena` everywhere would create. A store caches its chunk's base pointer
(hot access is `base + idx·stride`, no chunk-table chase) plus a `*const Arena` to grow
through; a `Ref` is a slot *index*, stable across relocation. Growth is two-regime: **A**
(common) bumps a larger copy from the region's tail, leaving the old bytes as a frozen hole;
**B** (rare, tail exhausted) repacks all chunks into a fresh region — compacting holes, sized
to live + headroom rather than a blind 2× — and retires the old region.

**Cooperative self-heal (the relocation protocol).** A B repack moves *every* store's chunk,
so a store's cached base can be invalidated by a *sibling's* growth, not just its own. The
arena carries an epoch counter, bumped on each repack; a store compares it on access and
re-reads its own base when it advanced. The store heals *itself* — nothing reaches in to fix
it — which is the load-bearing soundness choice: an arena that wrote a sibling's base through
a back-pointer would alias the `&mut` the gatherer holds while filling that store (UB under
Rust's aliasing model). The same reason makes the per-PID fill copy a `Flat` record out,
mutate locally, and write back rather than hold a `&` into a slot across a store op: any op
may grow a store and relocate the arena, dangling an outstanding slot reference. Because the
cached base + `*const Arena` are self-referential, the arena is **pinned** (boxed) and the
write stores are `!Send`/`!Sync`; cross-thread reads use the lease view below.

**The lease.** A published snapshot carries a `ByteResolver<Cmd>` — a read-only capture of
the `Cmd` chunk's base *at publish* — so the UI thread resolves `cmdline` handles without
touching the gatherer-owned store. While a snapshot is held, every cmdline slot it
references must stay alive. This is *not* the `Arc`/`get_mut` double-buffer guarantee (that
protects the per-snapshot `procs`/comm arena, which the gatherer owns uniquely); the store
is **shared**, so safety rests on **generation arithmetic**. The unifying idea: *growth
copies now, frees later*. A demoted slot's data lingers until GC; a relocated chunk's old
bytes linger (an A hole, or a B retired region); both are reclaimed only once `min_live`
(= published generation − `GC_LAG`) passes the generation they were released/retired at, by
which point no live snapshot can still read them. `GC_LAG ≥ 2` covers the two-snapshot live
window, and the same `min_live` drives slot GC *and* arena-region GC. The `u8` generation
tag wraps with ~127 generations of headroom, so the lag is set conservatively for free.
Cross-thread reads are sound because the gatherer's concurrent mutations touch only a
slot's 1-byte tag (a distinct memory location from its data) or slots no live snapshot
references (free/new on intern, expired on GC).

## `/proc` enumeration — maintained live set + cadence + birth probe

`getdents64` directly into a reused buffer, parsing dirent records by hand — no
per-entry `String`/`PathBuf` (unlike `fs::read_dir`). The `/proc` dir fd is opened
once and `lseek(0)`'d per scan.

The live PID set is **maintained across cycles**, not re-derived every scan. A full scan's
cost is kernel-side dirent materialization, so the only unprivileged lever is frequency:

- Deaths are free — a held read fails when its task dies, dropping the PID the same cycle —
  so enumeration exists only to find *births*.
- Full re-scan only every ~1 s (a wall-clock target, not a cycle count); it resyncs anything
  the probe missed.
- Between scans, a cheap **birth probe**: since the kernel allocates PIDs near-monotonically,
  speculatively read a few numbers just above the highest live PID, bounded by the kernel's
  allocation frontier. New arrivals are caught within one cycle; in steady state the window
  is empty (zero cost). Bursts and the post-wrap low range wait for the full re-scan — a
  hit-rate, not a correctness, concern (the failed speculative reads rely on the hygiene
  rule above).

This enumeration cadence is the shared component the scale-observation plan's per-PID
*sampling* cadence builds on — the two are independent knobs.

## Stat parse

`parse_stat` works on the `&[u8]` read slot: first `(` … last `)` delimits `comm`
(handles spaces/parens); numeric fields are parsed byte-wise (no UTF-8 validation,
overflow-checked). Field indices after `) `: 0 state, 1 ppid, **6 flags** (`PF_KTHREAD`
→ `is_kthread`), 11 utime, 12 stime, 21 rss(pages), 19 starttime. `comm` is located as a
slot-relative range and copied into the string arena (the read slot is reused
immediately), so only the ~15 B name survives — raw stat text is never retained for the
snapshot's life. A single fixed slot size covers any realistic line (max ~418 B), so the
old two-tier long-stat promotion is gone.

## Linux I/O backends — persistent-fd pool

The hot path is **one stat read per live PID per cycle, ~zero opens, ~zero closes**.
A `/proc/<pid>/stat` fd is opened once and **held across cycles**, re-read at offset 0
each refresh. This killed an `osq_lock`/`uring_lock` storm (~43% of cycles) that the
old design's per-cycle fixed-fd **closes** drove from `io-wq` workers: closing a fixed
descriptor from `IO_URING_F_UNLOCKED` context takes `ctx->uring_lock`, and ~1024
closes/cycle made that mutex an `osq_lock` spin. **No per-cycle close = no storm.**

A held `/proc/<pid>/stat` fd is bound to the task's proc inode: re-reading at offset 0
regenerates fresh content (a single-show seq_file re-traverses when `ki_pos < read_pos`
— validated), and once the task dies the read returns `ESRCH`. So death/reuse needs no
`start_time` comparison in the hot path: `ESRCH` ⇒ the incarnation is gone, reopen.

A `Backend` enum (`Uring | Syscall`), probed at startup (`ATOP_FORCE_SYSCALL` forces
the latter); on any io_uring error mid-run the gatherer permanently downgrades to
syscall and redoes the cycle. The **syscall backend** is also the test oracle
(`uring_matches_syscall_backend`). **Both** backends hold a persistent stat-fd pool
keyed by PID, generation-evicted (vanished PID ⇒ close its fd, like `CpuTracker`).

**Pool capacity** = `min(RLIMIT_NOFILE.soft − 64, 4096)`, derived at startup from
`getrlimit` (`ATOP_POOL_CAP` overrides — testing the overflow path without `ulimit`).
Containers with a 256/512 fd limit get a proportionally smaller pool.

### io_uring backend

Built on the gatherer thread (sole submitter — see Threading) with io_uring's modern
single-submitter flags, probed richest-first with fallback to a plain ring on older
kernels. They defer completion bookkeeping to the wait call and drop cross-CPU wakeups —
safe because every cycle reaches a wait.

Direct (fixed) descriptors, registered sparse (`register_files_sparse(pool_cap)`),
one slot per held PID. `user_data = (fixed_idx << 1) | op`:

```
cached PID:  ReadFixed(fixed_idx, off 0)                       (1 SQE)
new PID:     OpenAt(/stat, file_index) -[IO_LINK]-> ReadFixed  (2 SQEs, no Close)
dead PID:    register_files_update(idx, -1)                    (eviction, low volume)
```

- **No Close in the chain** — the design's whole point. Cmdline and statx also left the
  chain (now `ProcCache`'s job), so a steady-state cached PID is a *single* `ReadFixed`.
- **Landing pad (the single registered buffer)**: reads do not target the arena. They
  target one huge-page mapping whose **read-slot prefix** (`read_slots × STAT_SLOT` bytes)
  is the only registered — and so only pinned — region; each in-flight read claims a slot,
  and `reap` parses it, copies `comm` into the arena, and frees the slot. `ReadFixed` still
  skips the per-read page-table walk. The prefix never grows, so it is registered once at
  probe and never re-registered. In-flight read depth is bounded by the slot count (a few
  extra wait rounds at high PID counts; the count is the pinned-memory ↔ wakeups dial).
- **`IO_LINK` on open**: if the process vanished before open the read cancels
  (`ECANCELED`); the slot was never installed, so it is freed without a close.
- **Reopen / overflow** are deferred until the ring fully drains, then read transiently
  into a **dedicated scratch slot** carved from the huge page's free tail (past the
  registered prefix — never a read slot, so always safe regardless of drain state; a
  non-fixed close hits `files->file_lock` briefly, never `uring_lock`), copying `comm`
  into the arena like any other read.
- **Wait-batching (one wakeup/cycle)**: submit the whole batch, then wait for all its
  completions at once instead of draining one at a time. This collapsed ~51 blocking waits
  per cycle — each a scheduler context switch — to ~1, the largest remaining slice of pure
  coordination overhead. Overlapping parse with later reads stays available as a tuning
  dial, but is off by default now that parse is cheap.
- **Bounded in-flight + free-list**: keep filling the SQ while fixed slots and SQ space
  remain, then reap and parse.

`RING_ENTRIES=4096` bounds in-flight concurrency (multiple fill/reap rounds per cycle
when `pool_cap` is large). `submit_and_wait` retries on `EINTR`.

**Pinned memory is bounded.** Only the landing pad is a registered (pinned) buffer, and
it is fixed-size — locked memory is a small constant, independent of PID count, so a low
per-process lock limit (8 MiB is common) no longer downgrades io_uring as the process
count grows. (A separate, **root-caused** symptom — `io_uring_setup` returning `ENOMEM` under a
non-root `perf record` — is caused by perf's per-CPU ring buffers exhausting the
per-user `user->locked_vm` counter that io_uring also checks against `RLIMIT_MEMLOCK`;
the fix is raising `RLIMIT_MEMLOCK`.)

### syscall backend

Held fds re-read with `lseek(0)+read`; new PIDs `open`ed and installed; dead PIDs
`close`d on eviction; overflow uses transient `open+read+close`. This is the kernel-5.4
floor — persistent fds eliminate the open/close churn even without io_uring (there is no
storm here regardless: plain closes hit `files->file_lock`, not `uring_lock`).

### ProcTable — per-PID state (CPU history + uid/cmdline)

All per-PID state the gatherer maintains across cycles lives in one **`ProcTable`**, keyed by
a single PID index so one lookup serves every store (and gives future
volatility-based update prioritization a single place to read per-PID signals — the index
value, `PidSlot`, is the deliberate extension point). It replaced the former separate
`CpuTracker` + `ProcCache`, collapsing two per-cycle hashmap lookups into one.

Two per-PID records sit in *separate* huge-page generational stores, split by access
temperature: the **hot** CPU-history ring (touched every sample) and the **cold** metadata
(uid + cmdline handle, touched on the refresh cadence). Co-locating them would pull cold
cache lines into every CPU update. Both are gatherer-internal — no snapshot references them —
so a dead PID's slots are freed immediately (no lease); only the cmdline *string* is
snapshot-leased. Incarnation/cadence bookkeeping (`start_time`, first/last-seen generation)
lives in the index value, not the records, so the reuse/settling/eviction decisions never
chase into a store.

comm rides inside stat (re-read every cycle, free). `uid` and `cmdline` do not, and change
slowly, so the table reads them via plain syscalls — backend-agnostic (the **hybrid rule**:
low-volume ops stay synchronous):

- **Kernel threads** (`is_kthread`) cost **zero** syscalls — uid is root (0), cmdline is
  permanently empty. On a typical box kthreads are the majority of PIDs.
- **Userspace** PIDs read cmdline (+ uid via the same fd's `fstat`) fresh while settling
  (first `CMDLINE_SETTLE_GENS=3` cycles, for exec/argv settling), then on a **staggered
  coarse tick**: PID `p` refreshes when `(gen + p) % N == 0`, so ~1/N refresh per cycle
  (`CMDLINE_REFRESH_N=16`, ~8 s worst-case staleness; `ATOP_CMDLINE_REFRESH_N` overrides).
- A fresh read replaces the cmdline's slot in the `Cmd` store **only when the bytes
  changed** (old slot demoted, new slot interned `ALIVE`); an unchanged or not-refreshed
  PID keeps its slot, so `ProcessEntry::cmdline` is just an 8-byte handle copy — the
  per-cycle re-materialization is gone. PID reuse (`start_time` change) and eviction demote
  the slot so GC can reclaim it once the lease expires (see Generational storage).

### Overflow (more live PIDs than the pool holds)

Surplus PIDs use the transient read and are counted into `Snapshot::pool_overflow` — no
silent cap. Degrades to "today minus the storm" (transient closes are `files->file_lock`,
brief). Realistic only under low container fd limits.

## Tree build

`procs` sorted by PID ⇒ parent lookup is a binary search; child lists are built by
reverse-index prepend (yields PID-ascending siblings), no `HashMap`/per-node `Vec`.
One pre-order pass assigns `depth` and records order; its reverse accumulates
`subtree_size`. Scratch (`stack`, `order`) is gatherer-owned and reused. Iterative
(no recursion) — safe for pathologically deep trees.

A second reverse-order pass (`tree::aggregate`) computes inclusive `subtree_cpu` and
`subtree_mem` (self + all descendants) for collapsed-group display — same O(n) as the
size accumulation, using the existing `order` vector.

The structural tree is collapse-independent. The **UI** flattens it to display rows
(`app::rebuild_rows`), skipping collapsed subtrees, only when the snapshot generation
or the collapse set changes. Selection follows the selected PID across refreshes.

## Rendering (`etch`)

A separate workspace crate, `crates/etch/` — a **retained-mode, value-gated** terminal
renderer with no atop domain types. It replaced ratatui, whose `Paragraph`/
`LineTruncator` grapheme segmentation (≈43% of CPU) and blind 10k-cell `Buffer::diff`
(≈22%) dominated profiles. Work is now proportional to what *changed*, not to screen
size.

Two-level API: **structure declared once, values bound per frame.**

- A `Schema` of columns (`ColSpec::{right,left,fill}`) owns geometry — leading
  separator + body width + alignment, with fixed-column x-offsets precomputed once. It
  drives both the header and every body row (one source of column truth).
- Per frame, `Display::begin_frame` → `Frame` exposes `line()` (free-form styled spans
  for the stat header/footer), `header()` (titles from the schema), and `table()`.
  Inside `table`, each `row(id, style, …)` binds columns: `r.field(value)` and
  `r.fill(gate, closure)`.

**The gate is the bound value itself.** `field<T: Display + Hash>(v)` hashes `v`
(fast `FxHash`) and compares to the value that produced the cell's last output. On a
match the `Display` impl is *never invoked* — zero formatting, zero output. Because the
formatted value and the gated value are the same `T`, the gate can never drift from
what's shown. The `fill` column (Command) takes an explicit gate + a closure that runs
only on a miss; its gate is a **content hash of the cmdline bytes + tree prefix**, not
the arena `StringRef` (arena offsets are not stable across snapshots, so the ref would
mismatch every cycle).

Per-row identity is the PID: same PID at the same screen line ⇒ per-cell gating; a
different PID (scroll happened) ⇒ the whole row repaints. A style change (selection
move, state-color change) also forces the row. Unoccupied rows below the table are
blanked. A terminal-size change (or the first frame) clears the screen and repaints
everything. All output is batched into one buffer and flushed once per `commit()`.

ASCII is the fast path: 1 byte = 1 column, no grapheme work. The `Cell` writer tracks
display width explicitly — `glyph(s, w)` for known-width tree connectors (`●├─│▾`),
`ascii` for the common cmdline, and `unicode` (via `unicode-width`) only when
`ProcessEntry::non_ascii` is set (the gatherer flags this for free during the byte
walks that already scan comm/cmdline). Integration tests (`vt100`) assert both the
rendered screen and that an unchanged frame emits **zero** bytes.

Not yet done (deliberately deferred, measured first): terminal **scroll regions**
(`CSI S`/`T`) so a ±1 scroll shifts the terminal's own buffer instead of repainting the
visible window. The value-gated renderer already removed both ratatui hot spots; scroll
regions are a pure optimization for the held-arrow case.

## CPU%

Per-process CPU is a **per-core rate over real elapsed time** (`top`/`htop` "Irix
mode"): one interval's rate = `Δ(utime+stime) × 10000 / (Δwall · CLK_TCK)` basis
points, so one fully-used core = 10000 bp (100%) and a multi-threaded process can
exceed 100%. The window is a monotonic `Instant` (a jittering gather interval
self-corrects); it does **not** read `/proc/stat`. Dividing instead by the
`/proc/stat` machine-wide tick sum (an earlier approach) yielded `1/num_cpus` of the
truth — a 32× undercount on a 32-core box.

Because correct per-core values magnify the inherent ±1-jiffy quantization (±2% over
a 500 ms window at 100 Hz), each PID keeps a **bounded ring of the recent sample
window** (`CpuRing`) with exact `u64` running sums. The window is a wall-clock
duration — `CPU_WINDOW_MS` (default 10 s, the controllable knob) — and the per-PID
ring depth is derived as `CPU_WINDOW_MS ÷ REFRESH_MS`:
- **`cpu_pct`** = the sum-weighted moving **average** (`Σticks / Σjiffies`) — the
  stable reading. Reaches a new sustained level over ≤ `CPU_WINDOW` intervals.
- **`cpu_peak`** = the max single-interval rate still in the window — **captures
  spikes** the average dilutes, and holds them for `CPU_WINDOW` intervals. Shown as
  a separate `PEAK` column. Tracked incrementally: a cached `peak_bp` + ring index
  updates on push and rescans only when the peak sample is evicted (~1/window).

All arithmetic is exact integer math (no float, no EWMA accumulation error).
Robustness: refreshes closer than `MIN_SAMPLE` (100 ms — e.g. the forced refresh
after a kill) carry the windowed values forward instead of dividing by a near-zero
window; a PID whose tick counter goes backwards (reuse/wrap) resets its history;
PIDs absent from a cycle are evicted via a generation tag. The rings live on huge pages in
a generational store (the **hot** half of `ProcTable`, kept apart from cold metadata) and
are keyed through the shared PID index — a small hand-rolled `FxHash`-style hasher, since the
default `SipHash` is hash-flood-resistant but slow and PID keys aren't attacker-controlled.
The ring is updated through a copy-free in-place borrow that never spans another store's
allocation (which could relocate it).

## Dependencies

- `io-uring` 0.7 — thin, runtime-free wrapper over the raw ring (hand-rolling the
  ring setup is hundreds of lines of memory-ordering-critical unsafe).
- `arc-swap` 1 — the lock-free snapshot cell.
- `etch` (workspace path crate) — the retained-mode renderer (see above). Depends on
  `crossterm` (escape generation) and `unicode-width` (opt-in wide-char measurement);
  dev-dep `vt100` for terminal-emulator-based render tests.
- `crossterm` 0.29 — terminal setup (raw mode, alt screen) + input events; also etch's
  output backend.
- `thoop` (workspace path crate) — THP-backed generational storage primitives
  (`MmapRegion`, `GenStore`, `TypedBuf`, `StrStore`/`ByteResolver`). Owns the gatherer's
  huge-page-resident structures; depends only on `libc`. See Generational storage above.
- `libc` — syscalls. No `procfs` crate (it allocates and parses more than we need).

## Deviations from the original plan & known edges

- **Persistent-fd pool** replaced the per-cycle `open→read→close` chains — the
  close-storm fix. Cmdline/uid moved out of the io_uring chain into `ProcCache` (plain
  syscalls, coarse cadence).
- **I/O target decoupled from the string store** (the plan's §1a, implemented as a
  separate fixed landing pad rather than a prefix carved from the arena): reads land in
  the pad, then `comm` is copied into the arena. This bounds pinned memory and let the
  two-tier long-stat slot mechanism go away (one fixed slot size with ample margin).
- **Reopen / overflow** read transiently into a free landing slot after the ring drains.
- **Bounded in-flight** (not fixed "waves") — more overlap, self-balancing.
- **CpuTracker** and the fd pools are persistent `HashMap`s, not flat PID-indexed
  arrays (which would be ~32 MB).
- **Selection-follows-PID** across refreshes is implemented; full follow-mode
  auto-scroll is not.
- **Kill is race-safe**: `sys::kill_verified` pins the target with a `pidfd`, re-checks
  `(pid, start_time)` against the snapshot, then signals through the pidfd — a reused
  PID is never hit. (`sudo` escalation for protected processes is still future.)

## Not yet implemented (see GOALS.md)

macOS (`sysctl`/`libproc`) backend; thread-group (TGID) handling; CEF/Chromium-aware
collapse + persisted collapse rules; config + state-cache files; `sudo` escalation
for protected processes; disk-I/O (`/proc/pid/io`) and other columns (exe path);
per-core CPU bars (toggle); netlink/delta enumeration instead of full rescan;
gatherer pause on `SIGTSTP`/background.
