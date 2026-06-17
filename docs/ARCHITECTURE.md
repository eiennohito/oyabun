# Architecture

Status: **implemented** (Linux). macOS and several features below are still future work.

This describes how atop is actually built. For *why* (goals/constraints) see `GOALS.md`.
The single-thread storage *policy* on top of the `thoop` substrate (mechanism) is
`docs/plans/thoop.md`. With the PID index now on huge pages too, every hot, randomly-probed
per-PID structure is THP-resident.

## Threading model

**One thread, no async runtime.** The loop calls `gather` then `render` in sequence, so the
two never run at once — the borrow checker *proves* it (a `&mut` to gather, a `&` to render,
non-overlapping), which makes data races a compile error rather than a runtime discipline.

This is a deliberate collapse from an earlier two-thread design. Exclusion between a writer
and a reader can come from disjoint *memory* (a double buffer — a per-cycle copy), disjoint
*time* (serialize), or never overwriting a live cell (copy-on-write + a lease); the loose
latency budget (`GOALS.md`: < 10 ms p90) makes serializing free, and it deletes whole classes
of overhead the other two require — the copy, the `ArcSwap` exchange, the cross-thread string
lease, the generational reclaim *lag*, and the `Send`/`Sync` plumbing. The cost accepted: the
UI cannot redraw or service input *while a gather runs* (sub-ms normally; a few ms only on the
periodic full `/proc` rescan at extreme scale, far inside the budget). If that ever bites, the
fix is to chunk the gather to yield to input — **not** to re-add a thread.

The single thread owns everything that was previously split: the io_uring ring (it is both
creator and sole submitter, which is what the ring's modern single-issuer flags require), the
huge-page arena, the per-PID generational stores, and the process buffer. The arena + stores
hold self-referential raw pointers (a store caches a `*const Arena`), so the arena is boxed
(pinned) and the cluster is `!Send` — it lives and dies on this one thread.

The loop blocks for input with a timeout equal to the time until the next gather is due, so
idle is near-zero CPU (asleep in the kernel, not polling) and a redraw happens only when
something changed (a `dirty` flag set by a gather, input, resize, or scroll). A kill nudges the
next gather to *now* so the change shows without waiting a full interval (it replaces the old
UI→gatherer refresh signal — there is no channel anymore).

## Process buffer (data, not snapshots)

There is no published immutable snapshot and no double buffer. The process rows are one
arena-resident buffer, reset and refilled each gather and **read in place** by render. Rows are
index-based and POD (`Copy`, no heap, no `Drop`), so a reset is O(1) and refill is zero-alloc
after warmup. Tree links are indices into the buffer.

A row carries the process identity (`pid`+`ppid`+`uid`, and the `start_time` that with `pid`
names a unique incarnation — the basis for race-safe kill), the volatile stat fields (state,
priority, nice, thread count, raw ticks, resident bytes), the derived CPU% (a stable moving
average plus a separate peak), the process name and cmdline, two cheap flags (a non-ASCII bit
that gates the renderer's unicode path; a kernel-thread bit that means "never read cmdline"),
and the tree links + inclusive subtree aggregates used for collapsed-group display.

System-wide stats (CPU deltas, memory, swap, load, uptime, task-state tallies) are a separate
small `Copy` record computed once per cycle. PID-pool overflow — live PIDs that exceeded the
persistent-fd pool and used the transient fallback (0 in the common case; non-zero ⇒
`RLIMIT_NOFILE` is the binding constraint) — is a gatherer field surfaced in the footer, so a
low fd limit is never silent.

**comm inline, cmdline by handle (the lifetime rule).** The two strings use different
mechanisms because they have different *change rates*. `comm` is re-parsed from `stat` every
cycle anyway, so a store for it would be pure overhead — it lives **inline** in the row as
fixed bytes (≤ 15, `TASK_COMM_LEN`; truncated beyond). `cmdline` is slow-changing and read on
a coarse cadence, so re-copying it every cycle was waste — it lives in a generational `Cmd`
string store (`thoop`) and the row holds only a stable handle. The general rule: a volatile
field the gatherer rewrites each cycle is copied into the row; only slow, change-detected data
earns a referenced store slot.

A null-index sentinel marks tree roots and empty links. PIDs vanishing mid-scan are kept as
tombstones (`pid == 0`) and compacted out before the tree build; because PIDs are enumerated
sorted and filled by index, the buffer stays PID-sorted (the tree build's binary-search
precondition).

**Tombstone hygiene**: a slot is born a tombstone and only goes live when a read parses.
Every failure path (open, read, reopen, parse) must re-mark the slot dead, or it survives
compaction as a *phantom row* — a real PID with empty fields. This is load-bearing for the
birth probe (below), where most speculative reads are meant to fail.

## Storage substrate (`thoop`) and atop's single-thread policy

`thoop` is a policy-free THP storage substrate; its mechanism (the `MmapRegion` primitive, the
arena suballocator, the self-healing cached bases, the generational lifecycle, and the
deliberately-unbuilt multithread seam) is documented in `docs/plans/thoop.md`. This section is
only atop's *policy* on top of it. The short version of the mechanism: hot, randomly-accessed
records live on 2 MiB huge pages so the working set is a handful of TLB entries regardless of
count; many structures share one arena region (a per-structure mapping would commit a whole
huge page each); the arena relocates chunks as they grow and heals every holder's cached base
from the outside, so a `Ref` is a stable slot index and no reference into arena memory may span
an allocation.

atop holds three per-PID stores (a hot CPU-history ring, cold uid/cmdline metadata, and the
`Cmd` string store), the **PID index** (an open-addressing map), **and** the process row buffer
in one shared arena. Because gather and
render never overlap, atop picks the cheapest sound instantiation of the substrate — each
choice is exactly a cross-thread-safety piece *removed*:

- **immediate `free`**, not deferred `demote`/`gc`: nothing holds a reference across the
  gather/render boundary (the prior render finished before this gather began, and within a
  gather a freed slot is never referenced by a live row), so a dead PID's slots and a changed
  cmdline's old slot are reclaimed at once.
- **retired arena regions reclaimed immediately**: a regime-B repack retires the old region;
  with no reader leasing its bytes, the region is freed at the end of the same cycle (the
  substrate's generational region-GC driven with `min_live` = the just-finished generation —
  no lag).
- **rows read directly, cmdline resolved directly** from the `Cmd` store through `&self` — no
  per-cycle copy, no published lease view.

The per-PID fill still copies each row out by value, mutates it, and writes it back, because a
store allocation can trigger a regime-B repack that relocates the row buffer's chunk — holding
a `&row` across a store op would dangle. That is the substrate's invariant, not a threading
concern, so it stays.

Re-adding the removed pieces (an atomic base cell, deferred `free`, a copy-on-write lease view,
`Send`/`Sync`) is the documented multithread path — `thoop`'s concurrency seam — if the latency
budget ever changes. Single thread is atop **policy**, not a `thoop` limit.

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
slot-relative range and copied **inline** into the row (the read slot is reused
immediately), so only the ~15 B name survives — raw stat text is never retained past the
parse. A single fixed slot size covers any realistic line (max ~418 B), so the old two-tier
long-stat promotion is gone.

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

Built on the single owning thread (sole submitter — see Threading) with io_uring's modern
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
  and `reap` parses it, copies `comm` inline into the row, and frees the slot. `ReadFixed` still
  skips the per-read page-table walk. The prefix never grows, so it is registered once at
  probe and never re-registered. In-flight read depth is bounded by the slot count (a few
  extra wait rounds at high PID counts; the count is the pinned-memory ↔ wakeups dial).
- **`IO_LINK` on open**: if the process vanished before open the read cancels
  (`ECANCELED`); the slot was never installed, so it is freed without a close.
- **Reopen / overflow** are deferred until the ring fully drains, then read transiently
  into a **dedicated scratch slot** carved from the huge page's free tail (past the
  registered prefix — never a read slot, so always safe regardless of drain state; a
  non-fixed close hits `files->file_lock` briefly, never `uring_lock`), copying `comm`
  inline into the row like any other read.
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
cache lines into every CPU update. Under the single-thread policy *every* store is freed
eagerly on death — including the cmdline string slot, since nothing leases it across the
gather/render boundary. Incarnation/cadence bookkeeping (`start_time`, first/last-seen
generation) lives in the index value, not the records, so the reuse/settling/eviction
decisions never chase into a store.

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
  changed** (old slot freed, new slot interned `ALIVE`); an unchanged or not-refreshed PID
  keeps its slot, so the row's cmdline is just a handle copy — the per-cycle re-materialization
  is gone. PID reuse (`start_time` change) and eviction free the slot at once (no lease — see
  the storage policy above).

### Overflow (more live PIDs than the pool holds)

Surplus PIDs use the transient read and are counted into the gatherer's pool-overflow tally
(footer-surfaced) — no silent cap. Degrades to "today minus the storm" (transient closes are `files->file_lock`,
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

The structural tree is collapse-independent. The view flattens it to display rows
(`app::rebuild_rows`), skipping collapsed subtrees, after each gather and whenever the
collapse set changes. Selection follows the selected PID across refreshes. The flattened
display list is a plain heap `Vec`, not an arena buffer: it is walked *sequentially* in
render (the random walk it indexes into is the arena-backed row buffer), so it gains nothing
from the huge-page TLB win — and a heap `Vec`'s growth can never relocate the arena and
dangle the row slice the rebuild reads from.

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
only on a miss; its gate is a **content hash of the cmdline bytes + tree prefix**, not the
`Cmd`-store handle (a changed cmdline reuses a freed slot index, so the handle is not a stable
identity of the *content* — hashing the bytes is what stays correct across reuse).

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
are keyed through the shared PID index (a THP-resident open-addressing map). Its multiplicative
integer hash is fast and *not* hash-flood-resistant — which PID keys do not need, and the
default `SipHash` would be slow for.
The ring is updated through a copy-free in-place borrow that never spans another store's
allocation (which could relocate it).

## Dependencies

- `io-uring` 0.7 — thin, runtime-free wrapper over the raw ring (hand-rolling the
  ring setup is hundreds of lines of memory-ordering-critical unsafe).
- `etch` (workspace path crate) — the retained-mode renderer (see above). Depends on
  `crossterm` (escape generation) and `unicode-width` (opt-in wide-char measurement);
  dev-dep `vt100` for terminal-emulator-based render tests.
- `crossterm` 0.29 — terminal setup (raw mode, alt screen) + input events; also etch's
  output backend.
- `thoop` (workspace path crate) — THP-backed storage primitives (`MmapRegion`, the arena
  suballocator, `GenStore`, `TypedBuf`, `StrStore`). Backs the process row buffer and the
  per-PID stores; depends only on `libc`. See the storage substrate section above.
- `libc` — syscalls. No `procfs` crate (it allocates and parses more than we need).

(No lock-free snapshot cell anymore — the single-thread collapse retired `arc-swap`.)

## Deviations from the original plan & known edges

- **Persistent-fd pool** replaced the per-cycle `open→read→close` chains — the
  close-storm fix. Cmdline/uid moved out of the io_uring chain into the `ProcTable` (plain
  syscalls, coarse cadence).
- **I/O target decoupled from storage** (implemented as a separate fixed landing pad): reads
  land in the pad, then `comm` is copied inline into the row. This bounds pinned memory and let
  the two-tier long-stat slot mechanism go away (one fixed slot size with ample margin).
- **Reopen / overflow** read transiently into a free landing slot after the ring drains.
- **Bounded in-flight** (not fixed "waves") — more overlap, self-balancing.
- **The `ProcTable` PID index** is a THP-resident open-addressing map (`thoop::ThpMap` —
  Robin Hood, backward-shift deletion so per-cycle bulk eviction never accrues tombstones),
  sharing the arena and its TLB win rather than living on the heap (and not a flat
  PID-indexed array, which would be ~32 MB). The **fd pools** inside each backend are still
  heap `HashMap`s with the same per-PID-per-cycle probe pattern; moving them onto the arena is
  a natural follow-up, blocked only on the backends being able to reach the arena (they are
  built without it today). `ThpMap` is the reusable primitive for that step.
- **Selection-follows-PID** across refreshes is implemented; full follow-mode
  auto-scroll is not.
- **Kill is race-safe**: `sys::kill_verified` pins the target with a `pidfd`, re-checks
  `(pid, start_time)` against the selected row, then signals through the pidfd — a reused
  PID is never hit. (`sudo` escalation for protected processes is still future.)

## Not yet implemented (see GOALS.md)

macOS (`sysctl`/`libproc`) backend; thread-group (TGID) handling; CEF/Chromium-aware
collapse + persisted collapse rules; config + state-cache files; `sudo` escalation
for protected processes; disk-I/O (`/proc/pid/io`) and other columns (exe path);
per-core CPU bars (toggle); netlink/delta enumeration instead of full rescan;
gatherer pause on `SIGTSTP`/background.
