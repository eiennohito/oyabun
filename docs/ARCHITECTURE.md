# Architecture

Status: **implemented** (Linux). macOS and several features below are still future work.

This describes how oyabun is actually built. For *why* (goals/constraints) see `GOALS.md`.
The single-thread storage *policy* on top of `thoop` (the storage foundation) is
`docs/plans/thoop.md`. Every hot, randomly-probed per-PID structure, the PID index included, is
THP-resident.

## Threading model

**One thread, no async runtime.** The loop calls `gather` then `render` in sequence, so the
two never run at once — the borrow checker *proves* it (a `&mut` to gather, a `&` to render,
non-overlapping), which makes data races a compile error rather than a runtime discipline.

Single-threaded is a deliberate choice over two threads. Exclusion between a writer
and a reader can come from disjoint *memory* (a double buffer — a per-cycle copy), disjoint
*time* (serialize), or never overwriting a live cell (copy-on-write + a lease); the loose
latency budget (`GOALS.md`: < 10 ms p90) makes serializing free, and it deletes whole classes
of overhead the other two require — the copy, the lock-free snapshot exchange, the cross-thread string
lease, the generational reclaim *lag*, and the thread-safety plumbing. The cost accepted: the
UI cannot redraw or service input *while a gather runs* (sub-ms normally; a few ms only on the
periodic full `/proc` rescan at extreme scale, far inside the budget). If that ever bites, the
fix is to chunk the gather to yield to input — **not** to re-add a thread.

The single thread owns everything: the io_uring ring (it is both
creator and sole submitter, which is what the ring's modern single-issuer flags require), the
huge-page arena, the per-PID generational stores, and the process buffer. The arena + stores
hold self-referential raw pointers (a store caches a pointer to the arena), so the arena is boxed
(pinned) and the cluster is thread-bound — it lives and dies on this one thread.

The loop blocks for input with a timeout equal to the time until the next gather is due, so
idle is near-zero CPU (asleep in the kernel, not polling) and a redraw happens only when
something changed (a dirty flag set by a gather, input, resize, or scroll). A kill nudges the
next gather to *now* so the change shows without waiting a full interval (a direct deadline
change — there is no cross-thread channel).

## Process buffer (data, not snapshots)

There is no published immutable snapshot and no double buffer. The process rows are one
arena-resident buffer, reset and refilled each gather and **read in place** by render. Rows are
index-based and POD (plain data: copyable, no heap, no destructor), so a reset is O(1) and refill is zero-alloc
after warmup. Tree links are indices into the buffer.

A row carries the process identity (pid, parent pid, uid, and the start time that with the pid
names a unique incarnation — see **PID reuse model** below), the volatile stat fields (state,
priority, nice, thread count, raw ticks, resident bytes), the derived CPU% (a short
box average; history lives in the per-PID ring), the process name and cmdline, two cheap flags (a non-ASCII bit
that selects the renderer's unicode path; a kernel-thread bit that means "never read cmdline"),
and the tree links. Aggregates over subtrees are not stored on the row; the view derives them.

System-wide stats (windowed CPU and pressure, memory, swap, stuck-task tallies) are a separate
small plain-data record computed once per cycle. PID-pool overflow — live PIDs that exceeded the
persistent-fd pool and used the transient fallback (0 in the common case; non-zero ⇒
the fd limit is the binding constraint) — is a gatherer field surfaced in the footer, so a
low fd limit is never silent.

**comm inline, cmdline by handle (the lifetime rule).** The two strings use different
mechanisms because they have different *change rates*. `comm` is re-parsed from `stat` every
cycle anyway, so a store for it would be pure overhead — it lives **inline** in the row as
fixed bytes (the kernel's 15-byte name limit; truncated beyond). `cmdline` is slow-changing and read on
a coarse interval, so re-copying it every cycle was waste — it lives in a generational
cmdline string store (`thoop`) and the row holds only a stable handle. The general rule: a volatile
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

## PID reuse model

Linux allocates PIDs near-monotonically from a per-namespace counter up to `pid_max` (4194304
on 64-bit — both the kernel's own CPU-scaled init and systemd's sysctl override land there on
any modern multi-core system; 32768 is only the kernel's floor for the scaling formula). On
wrap the kernel scans forward from 1. A PID can only be reassigned after its task is **reaped**
(the kernel's reap, not just exit) *and* the allocator wraps back to it — a minimum gap of
`pid_max` allocations. At 4194304 and the 500 ms gather interval, same-cycle reuse requires ~8 M forks/s
sustained — a fork bomb that would OOM-kill the system before it wraps.

**Death detection is ≤1 cycle.** A held `/proc` fd returns ESRCH the cycle after the task
dies; an unheld PID fails its transient open. All per-PID sidecar state (CPU ring, metadata,
per-member application-memory samples, collapse/suppression) is generation-evicted the same cycle
death is detected. Therefore any reuse arriving ≥2 cycles after death hits a **clean slate** — no
stale state exists for it to collide with.

This means per-PID sidecar state needs only **generation-based liveness** (a first-seen/last-seen
generation tag), not incarnation keying. The timing gap between death
and reuse — vastly longer than 2 cycles under any non-pathological load — is the invariant that
makes PID-keyed state safe without a discriminator.

**The start time is reserved for signal safety.** The kill path is the one path where a wrong
target is dangerous regardless of probability: it re-reads `/proc/<pid>/stat` field 22 and
compares to the selected row's start time, refusing to signal on mismatch. The `pidfd` it
opens pins the kernel `task_struct` (so even a same-microsecond reuse would ESRCH, not hit the
new process); the start-time check is the formal proof on top of the physical one. Both are
cheap; both stay.

**Same-cycle reuse (the fork-bomb edge).** If it occurs, the held-fd ESRCH still fires (the fd
is bound to the old inode), triggering a reopen that reads the new incarnation. The CPU ring's
backward-counter guard catches most tick-discontinuities; metadata self-heals within the settle
window. The worst case is one frame of wrong CPU% — bounded and self-correcting. No special
guard is needed beyond what the normal eviction + reopen path already does.

## Storage foundation (`thoop`) and oyabun's single-thread policy

`thoop` is a policy-free THP storage layer; its mechanism (the mapped-region primitive, the
arena suballocator, the self-healing cached bases, the generational lifecycle, and the
deliberately-unbuilt multithread seam) is documented in `docs/plans/thoop.md`. This section is
only oyabun's *policy* on top of it. The short version of the mechanism: hot, randomly-accessed
records live on 2 MiB huge pages so the working set is a handful of TLB entries regardless of
count; many structures share one arena region (a per-structure mapping would commit a whole
huge page each); the arena relocates chunks as they grow and heals every holder's cached base
from the outside, so a handle is a stable slot index and no reference into arena memory may span
an allocation.

oyabun holds three per-PID stores (a hot CPU-history ring, cold uid/cmdline metadata, and the
cmdline string store), the **PID index** (an open-addressing map), **and** the process row buffer
in one shared arena. Because gather and
render never overlap, oyabun picks the cheapest sound instantiation — each
choice is exactly a cross-thread-safety piece *removed*:

- **immediate free**, not deferred demotion and collection: nothing holds a reference across the
  gather/render boundary (the prior render finished before this gather began, and within a
  gather a freed slot is never referenced by a live row), so a dead PID's slots and a changed
  cmdline's old slot are reclaimed at once.
- **retired arena regions reclaimed immediately**: a regime-B repack retires the old region;
  with no reader leasing its bytes, the region is freed at the end of the same cycle (the
  generational region collector told that only the just-finished generation is live —
  no lag).
- **rows read directly, cmdline resolved directly** from the cmdline store through a shared borrow — no
  per-cycle copy, no published lease view.

The per-PID fill still copies each row out by value, mutates it, and writes it back, because a
store allocation can trigger a regime-B repack that relocates the row buffer's chunk — holding
a row reference across a store op would dangle. That is the storage layer's invariant, not a threading
concern, so it stays.

Re-adding the removed pieces (an atomic base cell, deferred free, a copy-on-write lease view,
thread-safety bounds) is the documented multithread path — `thoop`'s concurrency seam — if the latency
budget ever changes. Single thread is oyabun **policy**, not a `thoop` limit.

## `/proc` enumeration — maintained live set + refresh interval + birth probe

The kernel's directory-entry call goes directly into a reused buffer, parsing dirent records by hand — no
per-entry heap strings or paths (unlike the standard-library directory iterator). The `/proc` dir fd is opened
once and rewound per scan.

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
  rule above). A **thread-leader check** (`pidfd_open`, one syscall) rejects non-leader
  threads: `/proc/<N>` resolves any task ID via VFS lookup (PIDs and TIDs alike), but
  `getdents` returns only TGIDs — without it, worker threads whose TID falls in the
  probe window would appear as phantom processes that flicker on alternating cycles.

This enumeration interval is the shared component the scale-observation plan's per-PID
*sampling* interval builds on — the two are independent knobs.

## Stat parse

The parser works on the raw bytes of the read slot: first `(` … last `)` delimits the name
(which may contain spaces and parens); numeric fields are parsed byte-wise (no UTF-8
validation, overflow-checked). The fields taken are state, parent, the kernel-thread flag
(which marks kernel threads so cmdline is never read), user and system time, resident pages,
and start time. The name is located as a slot-relative range and copied **inline** into the row
(the read slot is reused immediately), so only the ~15 B name survives — raw stat text is
never retained past the parse. A single fixed slot size covers any realistic line (max
~418 B), so no long-line promotion tier is needed.

## Linux I/O backends — persistent-fd pool

The hot path is **one stat read per live PID per cycle, ~zero opens, ~zero closes**.
A `/proc/<pid>/stat` fd is opened once and **held across cycles**, re-read at offset 0
each refresh. This avoids an `osq_lock`/`uring_lock` storm (~43% of cycles) that
per-cycle fixed-fd **closes** drove from `io-wq` workers: closing a fixed
descriptor from `IO_URING_F_UNLOCKED` context takes `ctx->uring_lock`, and ~1024
closes/cycle made that mutex an `osq_lock` spin. **No per-cycle close = no storm.**

A held `/proc/<pid>/stat` fd is bound to the task's proc inode: re-reading at offset 0
regenerates fresh content (a single-show seq_file re-traverses when `ki_pos < read_pos`
— validated), and once the task dies the read returns `ESRCH`. Death/reuse needs no
`start_time` comparison in the hot path (see **PID reuse model**): ESRCH ⇒ death detected
this cycle ⇒ state evicted ⇒ any later reuse of this PID number starts clean.

The backend is io_uring or plain syscalls, probed at startup (an environment override forces
syscalls); on any io_uring error mid-run the gatherer permanently downgrades to
syscall and redoes the cycle. The **syscall backend** is also the test oracle
(a test asserts the two produce identical results). **Both** backends hold a persistent stat-fd pool
keyed by PID, generation-evicted (vanished PID ⇒ close its fd, like the per-PID table).

**Pool capacity** is the fd soft limit minus fixed headroom, capped at a fixed ceiling, derived
at startup (an environment override exists to test the overflow path without lowering the
limit). Containers with a tiny fd limit get a proportionally smaller pool.

### io_uring backend

Built on the single owning thread (sole submitter — see Threading) with io_uring's modern
single-submitter flags, probed richest-first with fallback to a plain ring on older
kernels. They defer completion bookkeeping to the wait call and drop cross-CPU wakeups —
safe because every cycle reaches a wait.

Direct (fixed) descriptors, registered sparse with one slot per held PID; each submission's
user data encodes the slot and the operation. Per PID state:

- **cached PID**: one fixed-file read at offset 0 (1 submission).
- **new PID**: an open into the fixed slot, linked to the read (2 submissions, no close).
- **dead PID**: the slot is cleared by a registered-files update (eviction, low volume).

- **No Close in the chain** — the design's whole point. Cmdline and statx also left the
  chain (now the per-PID table's job), so a steady-state cached PID is a *single* `ReadFixed`.
- **Landing pad (the single registered buffer)**: reads do not target the arena. They
  target one huge-page mapping whose **read-slot prefix** (read-slot count × slot size)
  is the only registered — and so only pinned — region; each in-flight read claims a slot,
  and `reap` parses it, copies `comm` inline into the row, and frees the slot. The fixed-buffer read still
  skips the per-read page-table walk. The prefix never grows, so it is registered once at
  probe and never re-registered. In-flight read depth is bounded by the slot count (a few
  extra wait rounds at high PID counts; the count is the pinned-memory ↔ wakeups dial).
- **Linked open**: if the process vanished before open the read cancels
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

The ring's entry count bounds in-flight concurrency (multiple fill/reap rounds per cycle
when the pool is large). The wait retries when interrupted.

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

### Per-PID table — CPU history + uid/cmdline

All per-PID state the gatherer maintains across cycles lives in one **per-PID table**, keyed by
a single PID index so one lookup serves every store (and gives future
volatility-based update prioritization a single place to read per-PID signals — the index
value is the deliberate extension point). One table rather than one per concern costs one
per-cycle hashmap lookup instead of several.

Two per-PID records sit in *separate* huge-page generational stores, split by access
temperature: the **hot** CPU-history ring (touched every sample) and the **cold** metadata
(uid + cmdline handle, touched on the refresh interval). Co-locating them would pull cold
cache lines into every CPU update. Under the single-thread policy *every* store is freed
eagerly on death — including the cmdline string slot, since nothing leases it across the
gather/render boundary. Incarnation/refresh bookkeeping (start time, first/last-seen
generation) lives in the index value, not the records, so the reuse/settling/eviction
decisions never chase into a store.

comm rides inside stat (re-read every cycle, free). `uid` and `cmdline` do not, and change
slowly, so the table reads them via plain syscalls — backend-agnostic (the **hybrid rule**:
low-volume ops stay synchronous):

- **Kernel threads** (flagged at parse) cost **zero** syscalls — uid is root (0), cmdline is
  permanently empty. On a typical box kthreads are the majority of PIDs.
- **Userspace** PIDs read cmdline (+ uid via the same fd's `fstat`) fresh while settling
  (the first few cycles, for exec/argv settling), then on a **staggered
  coarse tick**: each PID refreshes on a cycle offset by its own number, so a small fixed
  fraction of PIDs refresh per cycle and the burst never lands on one cycle, while worst-case
  staleness stays bounded at a few seconds.
- A fresh read replaces the cmdline's slot in the cmdline store **only when the bytes
  changed** (old slot freed, new slot interned live); an unchanged or not-refreshed PID
  keeps its slot, so the row's cmdline is just a handle copy with no per-cycle
  re-materialization. PID reuse (start-time change) and eviction free the slot at once (no lease — see
  the storage policy above).

**Deleted-binary and capability signals** are cold metadata on this same coarse interval, for
the same reason cmdline is: they change slowly and are read from `/proc`, so re-reading every
cycle would be waste. Their *lifecycle rules differ by how each transitions*:

- A deleted **exe** is **permanent** — once the kernel marks `/proc/pid/exe` `" (deleted)"` the
  original inode is gone and can never come back for that incarnation — so it is probed only
  while still unmarked, then latched for the process's life (a cheap `readlink`, re-tried on the
  cmdline interval until it fires, so a mid-run package upgrade is still caught).
- A deleted **library** is **transient** (a process can unmap the old `.so` and load a
  replacement), so it must be re-resolved periodically. Because scanning a whole `maps` file is
  the heavy check, it is bounded two ways: a coarse per-process re-check interval *and* a hard
  cap on scans started per cycle. The whole population is covered over many cycles at a cost
  independent of process count — no per-cycle work proportional to PID count. Exe deletion takes
  visual priority, so a flagged exe skips the (redundant) maps scan.

The USER column is colored by **effective capability level, not uid ownership**, because on
Linux *what a process can do* is the security-interesting axis, not who owns it: a root process
that dropped its capabilities is harmless, while a non-root process holding a dangerous
capability is not. The level (none / partial / full, from `/proc/pid/status` the effective-capability mask limited to
the kernel's valid capability range) is read only through the settling window and then cached — effective caps are
fixed at `exec` and effectively immutable for a process's observable life, so unlike exe/lib
this needs no steady-state re-read at all. The username still shows as the cell's text.

### Overflow (more live PIDs than the pool holds)

Surplus PIDs use the transient read and are counted into the gatherer's pool-overflow tally
(footer-surfaced) — no silent cap. Degrades to "today minus the storm" (transient closes are `files->file_lock`,
brief). Realistic only under low container fd limits.

## Observation sources — `/proc` vs privileged BPF

A cycle has two halves: a **source** fills the row buffer with identity + volatile stat
fields, then a **source-agnostic tail** (the per-PID table's CPU%/cmdline, the tree build,
the system stats) runs unchanged. Everything above this section is the unprivileged `/proc`
source. The privileged source is a BPF object — a task iterator plus fork/free tracepoints —
selected once at startup; if it fails to load (missing caps, no kernel BTF, too-old kernel)
the `/proc` source runs and nothing else differs. On Linux the privileged source is *the*
recommended installation; the `/proc` source is the fallback. So when a signal is cheap only
with BPF (per-thread scheduler wait, for one), the fallback may serve it bounded and partial —
on-screen processes only, under a budget — rather than holding the primary path back. This is *why* the split is a source
boundary and not a new I/O backend: the BPF task iterator replaces enumeration **and** the
stat read **and** the uid lookup in one pull, so it displaces the whole `/proc` front half,
not just the read mechanism. The privileged layer can also be compiled out entirely — it is
the one feature whose dependencies (the BPF loader, the zero-copy interpreter) are absent
from the unprivileged build.

**The iterator is one binary pull, not N text reads.** The kernel walks every task and writes
a fixed C struct per thread-group leader; userspace reads the stream into one aligned buffer and
interprets it in place as a typed slice — no per-PID kernel crossing, no text formatting
in the kernel, no byte-walk parse in userspace. The struct *is* the parsed result. The shared
layout (a C header, mirrored by a Rust struct that asserts the same size on both sides) is the
contract; same machine ⇒ same endianness ⇒ no conversion on the wire.

**Emit-on-change: the walk is unavoidable, the transfer is not.** There is no kernel signal for
"a sleeping process's rss/utime moved", so polling every task each cycle is the only way to
observe change — the walk stays O(tasks). But the program *writes* a row only when the
process's observable state changed since last cycle, so the bytes crossing to userspace and the
parse are proportional to churn, not process count; on an idle box (the target of the near-zero
idle goal) the stream is nearly empty. This addresses the measured cost: the read syscall
was dominated by copying the full set to userspace every cycle. The change test is a per-leader
kernel hash of the **hot** fields — the ones that move without the process running
(**thread-group-wide** CPU time, run state, resident pages that reclaim/swap edit while it
sleeps, and the parent on reparenting) — so an unchanged hash bails *before* reading the
**cold** fields (uid, nice, thread count, start time, name) or writing anything. Cold fields
refresh only on a change or a resync, so a sleeping process that is *only* reniced or reparented
lags until the next resync — cosmetic, and bounded by it. (A collision in the 64-bit hash,
vanishingly unlikely, would also self-heal at the next resync, so it is bounded-stale, not a
correctness hole.)

**CPU time is the thread-group total, not the leader's per-thread value.** The task's user time is
per-thread; `/proc/<pid>/stat` reports the thread-group sum. The
BPF program must match that or a multi-threaded process whose non-leader threads do the work
shows near-zero CPU% — both because the emitted value is a fraction, and because the hash
(keyed on the same value) never fires for non-leader activity. The iterator walks each leader's
thread list with a bounded loop, summing live threads and adding the dead-thread accumulator from the
signal struct. Single-threaded processes take a fast path with no walk.

**Scheduler wait rides the same walk.** The time a thread spent runnable but queued for a CPU is
summed over the live threads in the same loop, at no extra traversal. The kernel keeps no
dead-thread accumulator for it (unlike CPU time), so the sum can *drop* when a thread exits; the
consumer treats a backwards step as zero rather than as a reset. It joins the change hash, but only
as whole clock ticks: wait grows by nanoseconds every time a task is merely woken, so hashing it raw
would re-emit every process that wakes once per interval, every cycle, and the idle stream would stop
being near-empty. A process that waited a tick or more still re-emits; sub-tick wait is not lost, it
rides the cumulative value of the next emit. The tick divisor is handed to the program at load.
Raw nanoseconds cross the boundary, and the Rust side converts the *cumulative* value to the same
ticks as run time, so per-interval deltas telescope and sub-tick waits are carried, not lost.

The maintained full set lives in userspace: a cycle applies the delta to it, drains the
birth/reap events, then materializes the whole set into the row buffer so the source-agnostic
tail runs unchanged. **CPU% stays exact under deltas** precisely because CPU time is a hot field
— any thread's activity forces a re-emit, so the history ring always sees a correct per-interval
delta, and a process absent from the delta genuinely consumed nothing that interval (a true zero,
not a missed sample).

**The `/proc` fallback measures wait only where it is looked at.** `/proc` has no run-queue delay
in stat, and the per-task schedstat file covers one thread, so a multi-threaded process costs a
thread listing plus a read per thread. Doing that for every process every cycle would dwarf the
rest of the scan for numbers nobody sees. So only processes whose numbers are on screen are
read — the viewport's rows and the hidden sets of visible collapsed rows — largest CPU first,
under a per-cycle read budget. The interest list is the previous cycle's viewport, so a freshly
scrolled-in row has unknown wait for its first interval. A process not read in a cycle has
*unknown* wait for that interval, not zero.

Within the watched set, a process that provably cannot have queued is not read at all: its CPU ticks
did not move this interval and it is not runnable now, so it slept through the interval. It is given
its last cumulative wait — a measured zero — and costs no budget, which leaves the reads for the
processes that matter. The price is one bounded blind spot: a wake-run-sleep wholly between two
samples that burned under a tick can have queued a little; that wait is not lost, it lands in the
next real read's interval.

**Units convert in Rust, identity is load-bearing.** The kernel exposes CPU time and start
time in nanoseconds; the row carries clock ticks, matching the `/proc` path's semantics so the
CPU-history ring is identical regardless of source. The start-time conversion is *not*
cosmetic: it must equal `/proc` field 22 to the tick, because race-safe kill re-reads that
field as the incarnation discriminator — a mismatch would make kill refuse. The conversion
divisor is constant for the process, so it is computed once. The run-state char is mapped from
raw kernel state bits in one tested place (a faithful port of the kernel's own mapping); an
exotic unmapped combo degrades to a placeholder, never to a wrong identity.

**uid ownership is source-specific.** The `/proc` source has no uid in stat, so the per-PID
table reads it (free, riding the cmdline fd's `fstat`); the BPF source carries uid in the
iterator output. So the table must *not* overwrite a source-provided uid from the cmdline read
— a permission-denied cmdline open would otherwise clobber a good uid. `cmdline` itself stays
a `/proc` read on the coarse interval in both modes (it lives in process memory, not
`task_struct`).

**fork/free tracepoints — births in the delta, deaths on reap.** Births flow through the
delta itself (a new PID has no prior hash, so the walk emits it), leaving the fork tracepoint
one job: short-lived pairing. Removal keys off the **reap** (the kernel's task release), *not* exit —
because a zombie is still a live entry the walk keeps showing as `Z` (its hot fields freeze, so
it re-emits once on zombifying then bails: O(1), never per-cycle). The reap tracepoint is
**RCU-deferred** (it fires from an RCU callback, not synchronously at the wait), so a removal
lands within roughly a grace period of the reap — far inside the cycle interval, so effectively
next-cycle, the same latency a synchronous signal would give at this interval. **Short-lived**
processes — a reap whose fork was never reconciled by any snapshot, i.e. born and gone *between*
two walks — are what a snapshot-only tool can never see, surfaced as a footer count.

**Resync is the desync backstop, not the steady state.** A forced full snapshot (every leader
re-emitted) is the only way to evict a death whose reap event was dropped when the event ring
overflowed, and to recover any other drift. It rebuilds the maintained set from scratch — so it
is armed only by a detected overflow (a kernel drop counter moved) or a slow periodic tick,
**never every cycle**, because a full snapshot is exactly the per-cycle transfer cost
emit-on-change exists to avoid. A resync whose read fails leaves the prior set intact (rather
than blanking) and re-arms, since a plain delta could not refill a cleared set.

**Deferred — open-coded iterator (no `read()`).** The remaining transfer cost is the `read()`
syscall that triggers the seq_file walk. An open-coded task iterator in a syscall-type
program, triggered by one test-run call with the delta landing in an mmap'd ring, would
drop even that. It is *not* built because the pinned aya version cannot load a `SEC("syscall")`
program, and the kernel registers the task-iterator kfuncs only for program types whose sole
`test_run`-able trigger cannot legally call them — so the library, not the kernel, is the block.
Revisit when aya gains syscall-program support.

**Build model: the artifact is the dependency.** The BPF programs are tiny C, compiled with
clang against a CO-RE type header generated from the kernel's BTF; only the compiled object is
committed and embedded at build time, so a normal `cargo build` needs neither clang nor bpftool
— exactly the stance taken toward any generated artifact. The type header is *build-only*: it
is not committed (it is large, and the right one is the rebuilder's own kernel, generated on
demand) and not read at runtime — the committed object carries its own BTF, and the loader
applies CO-RE relocations against the running kernel's BTF, so one object works across kernel
versions without recompilation (the fields read are stable ABI). The cap wrapper
(a small setuid tool) grants exactly the needed capabilities without full root; agent sessions and
dev runs use it instead of interactive sudo. Installed binaries take the opposite trade: file
capabilities on a root-owned binary bind the grant to that one program instead of to whatever
the caller hands a wrapper. Releases link glibc dynamically, not static musl, because GPU stats
`dlopen` NVML.

**Load failure is a test failure where BPF is required.** The privileged tests skip when the
object cannot load, so an unprivileged `cargo test` stays green — but a verifier rejection looks
exactly like "no caps". Where BPF is declared required (the privileged test run, CI) the skip
becomes a failure; otherwise a broken object would ship as green.

## Tree build

The process buffer is PID-sorted ⇒ parent lookup is a binary search; child lists are built by
reverse-index prepend (yields PID-ascending siblings), with no hash map or per-node vector.
One pre-order pass assigns depth and records order; its reverse accumulates subtree sizes.
Scratch space is gatherer-owned and reused. Iterative (no recursion) — safe for
pathologically deep trees.

Aggregation is not part of the structural tree: it depends on what the view collapses, and
storing "subtree totals" on each row would bake one meaning of *collapsed* into the data. The
view instead keeps per-process own totals (CPU, threads, memory, GPU, process count) and a
collapsed row sums the whole native subtrees of exactly the roots it hides. Hidden sets are
disjoint, so the walks together touch each process at most once, and cost nothing while nothing
is collapsed. A new cycle recomputes everything (the data is new); a scroll or a freshly read
memory estimate recomputes only the processes the viewport shows and the rows it shows, because
those are the only numbers either can change — no per-node allocation, no pass over the table.
Which row covers a process (for selection to follow a hidden process to the row that now shows
it) is a dense per-process map recorded by the flatten pass, so the lookup is an ancestor walk
with O(1) probes. A row's shape — leaf, expanded, or collapsed with the hidden set it stands
for — is one type, so a collapsed row cannot exist without naming what it hides.

The native tree is collapse-independent and the display rows are a separate **view tree**
(rows are processes and groups) flattened from it (see
*Process grouping*). The view tree is also where sibling ordering will live once sorting
exists. The rows are rebuilt only when something that shapes
them moves — the structural/grouping version (births, deaths, PID reuse, cmdline/cgroup
rewrites, and reparenting) or the user's collapse choices — not every cycle; a steady cycle only
refreshes the numbers. Reparenting has to count as structural: it changes the tree and so which
processes are top members, with no birth or death to signal it. Selection follows the selected row's
identity across rebuilds. The display list is a plain heap vector, not an arena buffer: it is walked
*sequentially* in render (the random walk it indexes into is the arena-backed row buffer), so it
gains nothing from the huge-page TLB win — and a heap vector's growth can never relocate the arena
and dangle the row slice the rebuild reads from.

### Process grouping

Grouping is split across the gather/view boundary, and the split is forced by one requirement:
folding is persisted and user-configurable. A persisted fold cannot key on process identifiers
(they change every launch), so it keys on a group's stable identity — but that persisted
configuration *is* the grouping policy, and it is user-owned and applied at display time. So the
gather layer answers only the config-free question — which processes share an identity, and how
trustworthy that boundary is — and the view side forms, labels, and folds groups from it while the view tree aggregates them.
This is deliberately *one* mechanism: a process resolves at most one identity, so it can never
flip between competing group labels across cycles.

Membership follows shared identity, not tree ancestry. On systemd desktops a launcher scope and
its instantiated service land in sibling cgroup scopes; resolving both to the same application id
makes them one group. Containers, pods, and system services cohere the same way, each from its
own cgroup shape. A cgroup boundary is trusted (kernel-owned, stable across runs); a purely
structural one — a shared-binary process fan, a runtime worker pool — is not, and is recognized
only where no cgroup identity exists, so the trusted identity always wins with no arbitrary
tiebreak. Identity resolution runs only on change, not per-cycle: a process's grouping is a pure
function of its cgroup and argv and the surrounding tree shape, which move only on a birth, death,
PID reuse, reparenting, or argv/cgroup rewrite. The gather layer folds exactly those into a monotonic version
stamp; while it holds steady the resolver and the view-side grouping both reuse their prior
result, so a settled desktop does no grouping work — the key goal is that work track change,
not population. Reuse is exact (a stable input provably yields identical groups), so a startup
transient still self-corrects on the next change without a settling window.

### What "collapsed" means

One model covers a collapsed process and a folded application group: **a collapsed row stands
for a set of whole native subtrees, its numbers are exactly the sum over what it hides, and every
process is either its own visible row or hidden in exactly one collapsed row.** Computing depth
and totals ad hoc per case lets a group's meaning diverge from a collapsed process's, puts a
group row at a depth unrelated to what it sums, and strands a member's non-member children as
false siblings. So depth comes from one flatten pass (a row is one deeper than the row it hangs
under), and totals from one pass over what each row hides; those states cannot be expressed.

- A collapsed process hides its native subtree and stays where it is.
- A folded group hides the *entire* native subtree of each of its **top members** — members with
  no member ancestor, found by walking through foreign intermediates, otherwise a nested member
  would be counted twice. Foreign descendants of members are therefore hidden with them and
  counted in the fold ("it's VS Code's terminal"), rather than stranded beside the group.
- The group's row sits at its **anchor**: the top member containing the representative (else the
  shallowest visible top member), at that spot's native depth — no invented level. Every other
  visible top member emits no row at its native spot: its subtree is hidden in and counted by the
  group row (its badge and totals carry the information), and a parent's own numbers never
  included those subtrees, so nothing is misstated. A selection on a process that becomes hidden
  resolves to the group row that covers it.
- An *expanded* group has no row at all: its members are ordinary process rows where they
  natively are, foreign descendants included. Expansion is a property of the group identity, not
  of a synthetic parent, so the fold is reachable from any member (a dedicated key) and from the
  top member of the application's tree (collapse there means collapse the application — the
  user's mental model of closing the app's root). Lower members keep ordinary per-process collapse.
- **Ancestor wins**, as native collapse always did: anything inside an earlier hidden set counts
  there, including members of another group. A group forms a row only from top members that are
  still outside every hidden set, and none at all when all are hidden.
- The row count badge is the number of hidden processes (a collapsed process row is itself
  visible, so it hides its descendants; a group row hides all of its set).

Collapse and fold state follow stable identity (a process id for native collapse; the group's
identity for folds), never a row position, which is what makes selection and persisted folds
survive rebuilds.

### Group formation and rationale

A group is a resolved label (an application's desktop-entry name; otherwise a container, pod,
unit, or command) over aggregated CPU/memory/GPU (the root alone is often an idle launcher whose
own numbers mislead). Tiny groups and terminals are left unfolded — a two-process fold hides as
much as it saves, and each shell is its own workspace. Selection and fold state follow the stable
identity, not whichever process currently represents the row.

Auto-fold reduces clutter by default; the user's expand/fold is remembered. Only a
cross-run-stable identity (a desktop application, a systemd unit) persists its fold across
restarts; ephemeral ones (container ids, pod uids, structural tokens) stay session-only, so an
expansion of a non-persistable group is transient state that clears when the group disappears,
while a persistable one returns as the user left it.

### Collapsed-row history

A row standing for several processes cannot borrow one member's CPU history, so each collapsed
row keeps its own short series equal to the sum of everything it hides — run *and* wait, as
separate sums, so the color is the hidden set's wait share rather than an average of shares. It
is kept O(rows), not O(hidden): each sample the row's current-interval cell (already summed
with the totals) is
appended, and only when the *set of hidden roots* changes — a new row, a group gaining or losing
a top member — is the series rebuilt from the members' own. Rebuilding on every birth or death
inside the set would erase the history of, say, a build row whose compiler children come and go.
Appending on the same sampling clock the per-process series use (a refresh too soon after the
previous one has no measured window and records nothing) keeps the two aligned. The series sums
per-interval rates rather than already-averaged member values, so a group's spikes are not
averaged away before they are shown.

## Memory: proportional set size everywhere

Every row's memory is proportional set size (PSS), and the column says so: one column must not
mix measures under one header, and a collapsed process row and a folded group over the same set
must agree. What each counter means, why PSS is the useful total, how it is estimated rather
than recomputed (cached shared/private split, staggered backstop, walk budget, bootstrap), and
why the fallback is per process and marked, are in
[Application memory accounting gotchas](application-memory-gotchas.md). This section holds only
the view-side mechanics.

- **Candidates are what the viewport shows a number for** — plain visible rows and everything
  hidden inside visible collapsed rows — so off-screen processes are never walked.
- **The pass is gated**: it runs only when the generation, layout, scroll, or viewport height
  moved, so a settled screen spends nothing per frame.
- **Sampling is a view concern**, its walk budget charged against the UI's share of the frame
  budget rather than the gatherer's, because which processes need a number is a function of
  what the view shows.

## Rendering (`etch`)

A separate workspace crate, `crates/etch/` — a **retained-mode, change-tracked** terminal
renderer with no oyabun domain types. It exists because a general widget library's grapheme
segmentation (≈43% of CPU) and blind full-buffer diff (≈22%) dominated profiles. Work is
proportional to what *changed*, not to screen size.

Two-level API: **structure declared once, values bound per frame.**

- A schema of columns (right-aligned, left-aligned, or fill) owns geometry — leading
  separator + body width + alignment, with fixed-column x-offsets precomputed once. It
  drives both the header and every body row (one source of column truth).
- Per frame, the display hands out a frame exposing free-form styled lines (stat
  header/footer), a column-title header (from the schema), and a table. Each table row
  binds one value per column, or, for the fill column, a key plus a painter closure.

**The change key is the bound value itself.** A bound value is hashed (fast non-cryptographic
hash) and compared to the value that produced the cell's last output. On a
match the value's formatter is *never invoked* — zero formatting, zero output. Because the
formatted value and the change key are the same value, the check can never drift from
what's shown. The fill column (Command) takes an explicit key + a closure that runs
only on change; its key is a **content hash of the cmdline bytes + tree prefix**, not the
cmdline-store handle (a changed cmdline reuses a freed slot index, so the handle is not a stable
identity of the *content* — hashing the bytes is what stays correct across reuse).

Per-row identity is the PID: same PID at the same screen line ⇒ per-cell change tracking; a
different PID (scroll happened) ⇒ the whole row repaints. A style change (selection
move, state-color change) also forces the row. Unoccupied rows below the table are
blanked. A terminal-size change (or the first frame) clears the screen and repaints
everything. All output is batched into one buffer and flushed once per frame commit.

ASCII is the fast path: 1 byte = 1 column, no grapheme work. The cell writer tracks
display width explicitly — a known-width path for tree connectors (`●├─│▾`), an ASCII path
for the common cmdline, and a measured unicode path (via `unicode-width`) only when the row's
non-ASCII flag is set (the gatherer flags this for free during the byte
walks that already scan name/cmdline). Block-element glyphs (sparklines) are East Asian
ambiguous-width: a terminal configured to render ambiguous characters wide will misalign them. Integration tests (`vt100`) assert both the
rendered screen and that an unchanged frame emits **zero** bytes.

The cell writer is also the **terminal trust boundary**: `comm`/`cmdline` are attacker-controllable
(any local user's `argv`, or `prctl(PR_SET_NAME)`), so every text path through it neutralizes
control characters (C0/C1) to a placeholder before they can reach the terminal's *control*
channel as raw escapes — uniformly on both the ASCII and the wide-char path, since the
non-ASCII flag routes a string with even one high byte through the latter. A root operator
watching an unprivileged user's processes must not have escape sequences injected into their
terminal.

**Color is 24-bit and it carries meaning.** etch emits only truecolor — no ANSI-16 named
colors, no 256-color fallback — so a cell's color can be a *continuous function of its value*
rather than a bucket. Magnitude columns (CPU%, PSS, nice) and the CPU sparkline glyphs map through semantic gradients
whose *shape* encodes what the magnitude means to the reader: near-zero recedes into the
background, a normal level is calm, an alarming level is hot. Memory is log-scaled, so one
perceptual step is roughly an order of magnitude (a linear ramp would make megabytes and
gigabytes indistinguishable); nice diverges from a neutral zero. CPU% has one deliberate
*discrete* break: exactly-zero is a hard step to a dim idle tone and any nonzero jumps to a
visible floor, because "used nothing" and "used a sliver" are categorically different, not
adjacent magnitudes that should blend. Categorical columns (state, user, tree glyphs) use
named colors — there is no ordering to interpolate. This is *why* the
per-cell change key hashes the value **and** its style: a gradient-band crossing must repaint even
when the formatted text is byte-identical. The Command cell is multi-colored (dim path prefix,
bright basename, muted tree connectors, an alarm tint when the binary or a linked library was
deleted), so it records color *runs* the fill painter emits with per-run escapes. The CPU
sparkline needs the same per-glyph color in a fixed-width column, so fixed columns can also
take a run-recording cell (aligned and capped at the column width) rather than one style per
column. Row-level
color is selection only (a background); process state does not tint the whole row — that
flickers, so magnitude and kind live in the per-cell colors instead.

Not yet done (deliberately deferred, measured first): terminal **scroll regions**
(`CSI S`/`T`) so a ±1 scroll shifts the terminal's own buffer instead of repainting the
visible window. The change-tracked renderer already removes both hot spots above; scroll
regions are a pure optimization for the held-arrow case.

## CPU%

Per-process CPU is a **per-core rate over real elapsed time** (`top`/`htop` "Irix
mode"): one interval's rate = `Δ(utime+stime) × 10000 / (Δwall · CLK_TCK)` basis
points, so one fully-used core = 10000 bp (100%) and a multi-threaded process can
exceed 100%. The window is a monotonic clock (a jittering gather interval
self-corrects); it does **not** read `/proc/stat`. Dividing instead by the
machine-wide tick sum would yield `1/num_cpus` of the truth — a 32× undercount on a
32-core box.

Correct per-core values magnify the inherent ±1-jiffy quantization (±2% over a 500 ms
window at 100 Hz), so each PID keeps a **bounded ring of recent sample intervals** with
exact `u64` running sums. The number and the history are deliberately separate jobs:
- **The CPU% number is a short box average** (about 2 s of wall-clock, derived from the
  refresh interval), sized only to cancel the ±1-jiffy noise. A longer window would blur
  shape into a slow ramp (a step load reads as a long climb); a box rather than an
  exponential tail means a step settles completely in exactly one window, with no residue.
  It is a second running sum over the ring's newest samples, adjusted O(1) per push.
- **History is the sparkline's job.** The ring's depth is set by the sparkline length, not by
  the average. There is no peak: a held maximum shows magnitude but not shape (one spike vs.
  periodic), the series shows both.
- **Every CPU graph has one grammar: height is CPU used, color is how much that work was
  delayed waiting for a CPU.** The header graph colors by system pressure; a row colors by its
  own wait share — time runnable-but-queued over queued-plus-running, summed over all its
  threads. High load without waiting (every thread got a core) is a healthy busy process;
  the same load with waiting means the process is being starved or the machine is
  oversubscribed — the actionable difference, which a CPU-magnitude color cannot show. A
  collapsed row sums wait and run over everything it hides and divides once, so a busy member
  outweighs an idle one rather than the shares being averaged. Wait that was not measured for
  an interval is a neutral color, never the calm end of the ramp: "nobody looked" must not
  read as "no waiting". Members with unmeasured wait add to neither side of the share.
- **Height is a log ladder, the same for every row.** Process CPU spans four orders of
  magnitude; on a linear one-core scale nearly every row is a flat lowest block and anything
  past a core is pinned at the top. A shared log ladder gives a 1% and a 50% process
  different shapes and lets multi-core use keep climbing, while rows stay comparable. The
  ladder tops out one eighth below the full block: table rows are stacked, and full blocks in
  adjacent rows touch and fuse into one shape, so the empty top eighth keeps every row's
  sparkline separate. The header graph keeps its own stepped linear scale, which fits its job (utilization of the
  machine), through the same glyph vocabulary.
- **No display hysteresis on the number yet.** Repainting only when a change exceeds the noise
  bound was deliberately deferred until flicker is actually observed. Sort-order stability is
  likewise not the CPU window's job: it belongs to the future sort feature (e.g. rank
  hysteresis), and a longer window is rejected as a fix because it blurs shape for everyone.
  The view tree is where sibling ordering will live.

**The ring is indexed by sampling cycle, and idle processes cost nothing.** A PID that
reported no change in a cycle used zero ticks, so a ring whose whole history is zero and
that sees another idle interval is not written at all — thousands of idle processes do no
ring work. When such a PID next runs, the cycles it skipped are zero-filled first (bounded
by the ring depth), restoring alignment with the cycle clock. A ring that still holds
nonzero history *is* pushed zeros each cycle, so its sparkline scrolls and drains before it
goes quiet. Readers ask for the series *as of a cycle* and treat cycles after the ring's last
write as zero, so a skipped ring never shows stale bars. The cycle clock counts only real
measurements: a too-soon refresh folds its time into the next delta, so filling it with a
zero as well would double-count.

**Wait is stored beside run, in the same ticks, so the share is exact.** A ring interval
carries the run delta and the wait delta; wait may be unknown for the interval, which is its
own state. The *idle* test therefore covers both: a ring is quiescent only when run and wait
are zero across it — an interval that did not run but waited is not idle, and keeps scrolling.
Unknown wait never fabricates a baseline: a gap in measurement forgets the cumulative baseline,
so the next reading only re-baselines (a delta spanning unmeasured intervals would be pinned on
one). A cumulative wait that goes backwards is a zero delta, because thread exit removes
wait from a live-thread sum; unlike a backwards tick counter it is not a reset.

All arithmetic is exact integer math (no float, no EWMA accumulation error).
Robustness: refreshes closer than a minimum sample interval (~100 ms — e.g. the forced refresh
after a kill) carry the windowed values forward instead of dividing by a near-zero
window; a PID whose tick counter goes backwards (reuse/wrap) resets its history;
PIDs absent from a cycle are evicted via a generation tag. The rings live on huge pages in
a generational store (the **hot** half of the per-PID table, kept apart from cold metadata) and
are keyed through the shared PID index (a THP-resident open-addressing map). Its multiplicative
integer hash is fast and *not* hash-flood-resistant — which PID keys do not need, and the
standard library's default hasher would be slow for.
The ring is updated through a copy-free in-place borrow that never spans another store's
allocation (which could relocate it).

**Display state is derived from the ring, not point-sampled.** The kernel state byte answers
"was this task on-CPU at the microsecond we read `/proc`?" — which flickers `S`↔`R` for a
low-but-active process. The S column instead shows a *derived* state: `R` when the CPU ring
recorded any ticks across a short recent window, otherwise the raw byte. This makes `R` stable
(a 1% process reads `R` continuously) and makes `D`/`Z`/`T` trustworthy (they appear only when
the process genuinely isn't executing). The raw byte stays in the row untouched — kill-safety
and the task-state tallies read *it*, never the derived one, because they must reflect the true
kernel state, not the "is it active" question the display answers.

**System-wide CPU is a short window plus a graph, not one long average.** One interval is too
coarse — a burst fills a sample and vanishes — but a long average smears a step into a ramp and
hides what is happening now. The headline number differences a small ring of cumulative
snapshots at its two endpoints over about two seconds: long enough to cancel quantization,
short enough to settle fast, and exact integer math with no per-sample scan. It is expressed in
cores, the same unit as process rows, so it reads the same on 4 or 256 CPUs. History is the
graph's job: one cell per refresh interval, as wide as the terminal leaves room for.

**Every header number must drive a decision.** Machine-wide *busy* excludes idle, IO-wait (the
CPU was free) and steal (the hypervisor ran someone else — time denied, reported separately and
only when present). The question "is busy hurting?" is answered by pressure, the share of time
some task was stalled on the resource, taken from the kernel's own stall accounting as a
duration over wall time (so each sample carries a monotonic timestamp; replay supplies the
recorded clock). Pressure replaces iowait, load average and the old used-memory figure, each of which
mixes causes or counts reclaimable cache. Where the kernel has no such accounting, CPU falls
back to run-queue length and memory/IO pressure are omitted rather than faked — absence is a
distinct state, not a zero. Conditional values (steal, swap-in, tmpfs/shm memory, stuck and
zombie tasks) show only when relevant, but each keeps a reserved slot that is left blank, so a
healthy machine's header is quiet without anything shifting when a value appears.

**Header widgets move only when the terminal is resized.** A bar or graph that changes position
or length with the current numbers reads as data changing when nothing did: a size crossing
from megabytes to gigabytes must not push the swap bar sideways, and a percentage gaining a
digit must not rescale the CPU history. So geometry is decided once per terminal width and
reused every frame; every number is printed right-aligned in a field as wide as its largest
possible value, and memory sizes are bounded to a fixed width by switching unit at a thousand.
The layout may read only structural facts — core count, whether the kernel offers stall
accounting per resource, whether swap exists, whether the machine accounts steal, the memory
and swap totals — frozen when the layout is built; if one changes mid-run (swap enabled) it
takes effect at the next resize, because moving widgets to show it would be the very jitter the
rule prevents. When a width cannot hold every slot, lower-priority ones are dropped by a fixed
order that depends on width and frozen structure alone; the bars are guaranteed a share of the
width before optional slots are admitted, so narrow terminals still get a memory bar, and the
swap bar goes before the memory bar.

**The graph's scale is stateful on purpose.** Height is utilization, color is CPU pressure (red
means tasks are being delayed — the actionable state). The scale moves in discrete steps
(doubling, ending at the machine's core count, so the peak always lands in the upper half),
rises at once, and falls only when everything visible fits under half the scale. That
hysteresis depends on what the scale was a moment ago, so it lives with the history it
summarises; rescaling to the exact visible peak was rejected because the whole shape jumps
whenever the peak scrolls out. The history ring grows to the widest terminal seen and never
shrinks, so narrowing and re-widening a window does not lose the past.

**The memory line brought bars back, with the CPU graph's grammar: length = usage, color =
waiting.** A text-only line made proportion a calculation; a bar answers "how full, and of
what" at a glance. The bar divides RAM into parts that each mean a different action: memory
held by processes and kernel that cannot be taken back; shmem (tmpfs, shared memory), held by
no process and not reclaimable without swap; cache, which is available and costs only a refill;
and free. Available memory already excludes shmem, so shmem is carved out of the unavailable
part rather than added to it, and every part is clamped by what remains: whatever the kernel
reports, the parts sum to the total and the bar cannot overrun. The in-use segment is colored
by memory pressure (neutral when the kernel has none, rather than an unbacked alarm) so a
nearly-full machine that is not stalling looks calm and a half-full one that is thrashing does
not. Swap gets its own bar, only where swap exists. Bars share what the fixed slots leave of the
width, memory the larger part, and are dropped whole before they would be squeezed too narrow
or overflow the terminal. Like the CPU line, the memory line is written once against sinks, and
its repaint key is the cells as drawn plus the printed numbers, so byte-level jitter that moves
no cell and no digit repaints nothing.

## GPU telemetry (NVML)

Optional NVIDIA telemetry is a `dlsym`-loaded NVML with no build dependency (see the module doc);
any probe failure just leaves oyabun CPU-only. It is the third of a rough gather : GPU : UI
frame-time split, so its per-cycle work runs only on change the same way the two others do —
otherwise it dominated the profile, because the naive shape re-ran every NVML call every cycle
regardless of whether the GPU did anything.

Two costs are separated by how fast the thing they read moves. **Client discovery + VRAM** (the
process-list enumeration that names GPU clients and their framebuffer footprint, plus device VRAM
totals) moves slowly, so it runs on a **coarse interval** and the result is *held* between
refreshes — a client whose GPU is idle still owns its VRAM, so holding it is correct, not stale.
The **per-process utilization poll** busy-polls the GPU's own processor for fresh perfmon samples
and is by far the expensive call, so it is constrained two ways. First, it runs only when the
GPU is actually doing work. Second, even then it runs on its own **coarse interval**, its result
*held* between polls rather than re-polled every cycle — because the driver produces a new
per-process sample only every fraction of a second and retains several seconds of them, so a
poll spaced well inside that retention drains the whole backlog losslessly. The key goal is
*which* processes are (ab)using the GPU, not a precise instantaneous per-unit figure, so holding a
reading for a second or two is imperceptible while cutting the busy-poll rate several-fold. The
held reading is forced to a true zero the moment the device reads idle, so a process that stops is
never shown lingering. The poll reads NVML's general per-process query, not its
virtualization-oriented one: the two cost the same, but the general call is specified for
concurrent processes while the older one assumes a single tenant — so the older call is kept only
as a fallback where the newer symbol is absent.

The condition is **device utilization, not client CPU activity** — a deliberate departure from
the first instinct. A desktop GPU client (a browser, a compositor) is almost always CPU-busy
even when it renders nothing, so a CPU condition would essentially never skip the poll; and a
long compute kernel runs the GPU flat-out while its host thread blocks at ~0% CPU, which a CPU
check would wrongly read as idle and zero. Device utilization is one cheap counter read that is
needed for the system bar anyway and answers exactly the right question — *is the GPU doing
anything* — for both cases. An unreadable device counts as active, so the poll is never skipped
on an unknown. Because the utilization API is timestamp-based (it returns everything since the
last observed sample), skipped cycles — whether idle or merely between coarse polls — lose no
data: the next poll drains the backlog since the last one.
Utilization sums across devices for a client spanning more than one GPU; VRAM is per-device max
(compute and graphics lists double-report one context) then summed across devices. Idle steady
state therefore costs only the coarse client refresh; an active GPU pays the utilization poll only
on its own coarse interval, which is all the per-process breakdown needs.

## Dependencies

- `io-uring` 0.7 — thin, runtime-free wrapper over the raw ring (hand-rolling the
  ring setup is hundreds of lines of memory-ordering-critical unsafe).
- `etch` (workspace path crate) — the retained-mode renderer (see above). Depends on
  `crossterm` (escape generation) and `unicode-width` (opt-in wide-char measurement);
  dev-dep `vt100` for terminal-emulator-based render tests.
- `crossterm` 0.29 — terminal setup (raw mode, alt screen) + input events; also etch's
  output backend.
- `thoop` (workspace path crate) — THP-backed storage primitives (mapped regions, the arena
  suballocator, generational stores, typed buffers, string stores). Backs the process row buffer and the
  per-PID stores; depends only on `libc`. See the storage foundation section above.
- `libc` — syscalls. No `procfs` crate (it allocates and parses more than we need).

## Deviations from the original plan & known edges

- **Persistent-fd pool** replaced the per-cycle `open→read→close` chains — the
  close-storm fix. Cmdline/uid moved out of the io_uring chain into the per-PID table (plain
  syscalls, coarse interval).
- **I/O target decoupled from storage** (implemented as a separate fixed landing pad): reads
  land in the pad, then `comm` is copied inline into the row. This bounds pinned memory and let
  the two-tier long-stat slot mechanism go away (one fixed slot size with ample margin).
- **Reopen / overflow** read transiently into a free landing slot after the ring drains.
- **Bounded in-flight** (not fixed "waves") — more overlap, self-balancing.
- **The per-PID table's PID index** is a THP-resident open-addressing map (Robin Hood,
  backward-shift deletion so per-cycle bulk eviction never accrues tombstones),
  sharing the arena and its TLB win rather than living on the heap (and not a flat
  PID-indexed array, which would be ~32 MB). The **fd pools** inside each backend are still
  heap `HashMap`s with the same per-PID-per-cycle probe pattern; moving them onto the arena is
  a natural follow-up, blocked only on the backends being able to reach the arena (they are
  built without it today). The THP map is the reusable primitive for that step.
- **Selection-follows-PID** across refreshes is implemented; full follow-mode
  auto-scroll is not.
- **Kill is race-safe**: the kill path pins the target with a `pidfd` and re-checks
  the start time (see **PID reuse model**) — the pidfd makes it physically safe, the
  start time makes it formally correct. (`sudo` escalation for protected processes is
  still future.)

## Not yet implemented (see GOALS.md)

macOS (`sysctl`/`libproc`) backend; thread-group (TGID) handling; CEF/Chromium-aware
collapse + persisted collapse rules; config + state-cache files; `sudo` escalation
for protected processes; disk-I/O and other columns (exe path); gatherer pause on `SIGTSTP`/background.

**Privileged mode** is partially implemented: the emit-on-change task iterator (delta process
snapshot) and the fork/free tracepoints (births in the delta, reap-based removal, short-lived
capture) work and fall back to the `/proc` source when the object can't load. Still to do — the
open-coded/no-`read()` iterator (deferred on the BPF library, see above) and the network-I/O
probes that
*motivated* the privileged layer (per-PID TCP/UDP throughput via `fentry`, no unprivileged
equivalent) and filesystem-I/O from the iterator's accounting fields. See
`docs/plans/privileged-mode.md` for the remaining design.
