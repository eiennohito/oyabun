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
names a unique incarnation — see **PID reuse model** below), the volatile stat fields (state,
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

## PID reuse model

Linux allocates PIDs near-monotonically from a per-namespace counter up to `pid_max` (4194304
on 64-bit — both the kernel's own CPU-scaled init and systemd's sysctl override land there on
any modern multi-core system; 32768 is only the kernel's floor for the scaling formula). On
wrap the kernel scans forward from 1. A PID can only be reassigned after its task is **reaped**
(`release_task`, not just exit) *and* the allocator wraps back to it — a minimum gap of
`pid_max` allocations. At 4194304 and the 500 ms gather interval, same-cycle reuse requires ~8 M forks/s
sustained — a fork bomb that would OOM-kill the system before it wraps.

**Death detection is ≤1 cycle.** A held `/proc` fd returns ESRCH the cycle after the task
dies; an unheld PID fails its transient open. All per-PID sidecar state (CPU ring, metadata,
per-member application-memory samples, collapse/suppression) is generation-evicted the same cycle
death is detected. Therefore any reuse arriving ≥2 cycles after death hits a **clean slate** — no
stale state exists for it to collide with.

This means per-PID sidecar state needs only **generation-based liveness** (the existing
`seen_gen` / `last_seen_gen` pattern), not incarnation keying. The timing gap between death
and reuse — vastly longer than 2 cycles under any non-pathological load — is the invariant that
makes PID-keyed state safe without a discriminator.

**`start_time` is reserved for signal safety.** `kill_verified` is the one path where a wrong
target is dangerous regardless of probability: it re-reads `/proc/<pid>/stat` field 22 and
compares to the selected row's `start_time`, refusing to signal on mismatch. The `pidfd` it
opens pins the kernel `task_struct` (so even a same-microsecond reuse would ESRCH, not hit the
new process); the `start_time` check is the formal proof on top of the physical one. Both are
cheap; both stay.

**Same-cycle reuse (the fork-bomb edge).** If it occurs, the held-fd ESRCH still fires (the fd
is bound to the old inode), triggering a reopen that reads the new incarnation. The CPU ring's
backward-counter guard catches most tick-discontinuities; metadata self-heals within the settle
window. The worst case is one frame of wrong CPU% — bounded and self-correcting. No special
guard is needed beyond what the normal eviction + reopen path already does.

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
  rule above). A **thread-leader gate** (`pidfd_open`, one syscall) rejects non-leader
  threads: `/proc/<N>` resolves any task ID via VFS lookup (PIDs and TIDs alike), but
  `getdents` returns only TGIDs — without the gate, worker threads whose TID falls in the
  probe window would appear as phantom processes that flicker on alternating cycles.

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
— validated), and once the task dies the read returns `ESRCH`. Death/reuse needs no
`start_time` comparison in the hot path (see **PID reuse model**): ESRCH ⇒ death detected
this cycle ⇒ state evicted ⇒ any later reuse of this PID number starts clean.

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

**Deleted-binary and capability signals** are cold metadata on this same coarse cadence, for
the same reason cmdline is: they change slowly and are read from `/proc`, so re-reading every
cycle would be waste. Their *lifecycle rules differ by how each transitions*:

- A deleted **exe** is **absorbing** — once the kernel marks `/proc/pid/exe` `" (deleted)"` the
  original inode is gone and can never come back for that incarnation — so it is probed only
  while still unmarked, then latched for the process's life (a cheap `readlink`, re-tried on the
  cmdline cadence until it fires, so a mid-run package upgrade is still caught).
- A deleted **library** is **transient** (a process can unmap the old `.so` and load a
  replacement), so it must be re-resolved periodically. Because scanning a whole `maps` file is
  the heavy check, it is bounded two ways: a coarse per-process re-check interval *and* a hard
  cap on scans started per cycle. The whole population is covered over many cycles at a cost
  independent of process count — no per-cycle work proportional to PID count. Exe deletion takes
  visual priority, so a flagged exe skips the (redundant) maps scan.

The USER column is colored by **effective capability level, not uid ownership**, because on
Linux *what a process can do* is the security-interesting axis, not who owns it: a root process
that dropped its capabilities is harmless, while a non-root process holding a dangerous
capability is not. The level (none / partial / full, from `/proc/pid/status` `CapEff` masked to
`cap_last_cap`) is read only through the settling window and then cached — effective caps are
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
the `/proc` source runs and nothing else differs. This is *why* the split is a source
boundary and not a new I/O backend: the BPF task iterator replaces enumeration **and** the
stat read **and** the uid lookup in one pull, so it displaces the whole `/proc` front half,
not just the read mechanism. The privileged layer can also be compiled out entirely — it is
the one feature whose dependencies (the BPF loader, the zero-copy interpreter) are absent
from the unprivileged build.

**The iterator is one binary pull, not N text reads.** The kernel walks every task and writes
a fixed C struct per thread-group leader; userspace reads the stream into one aligned buffer and
interprets it in place as a typed slice — no per-PID kernel crossing, no `seq_printf` formatting
in the kernel, no byte-walk parse in userspace. The struct *is* the parsed result. The shared
layout (a C header, mirrored by a Rust struct that asserts the same size on both sides) is the
contract; same machine ⇒ same endianness ⇒ no conversion on the wire.

**Emit-on-change: the walk is unavoidable, the transfer is not.** There is no kernel signal for
"a sleeping process's rss/utime moved", so polling every task each cycle is the only way to
observe change — the walk stays O(tasks). But the program *writes* a row only when the
process's observable state changed since last cycle, so the bytes crossing to userspace and the
parse are proportional to churn, not process count; on an idle box (the target of the near-zero
idle goal) the stream is nearly empty. This was the fix for the measured cost: the read syscall
was dominated by `copy_to_user` of the full set every cycle. The change test is a per-leader
kernel hash of the **hot** fields — the ones that move without the process running (CPU time,
run state, resident pages that reclaim/swap edit while it sleeps, and the parent on reparenting)
— so an unchanged hash bails *before* reading the **cold** fields (uid, nice, thread count, start
time, name) or writing anything. Cold fields refresh only on a change or a resync, so a sleeping
process that is *only* reniced or reparented lags until the next resync — cosmetic, and bounded
by it. (A collision in the 64-bit hash, vanishingly unlikely, would also self-heal at the next
resync, so it is bounded-stale, not a correctness hole.)

The maintained full set lives in userspace: a cycle applies the delta to it, drains the
birth/reap events, then materializes the whole set into the row buffer so the source-agnostic
tail runs unchanged. **CPU% stays exact under deltas** precisely because CPU time is a hot field
— any activity forces a re-emit, so the history ring always sees a correct per-interval delta,
and a process absent from the delta genuinely consumed nothing that interval (a true zero, not a
missed sample).

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
a `/proc` read on the coarse cadence in both modes (it lives in process memory, not
`task_struct`).

**fork/free tracepoints — births in the delta, deaths on reap.** Births now flow through the
delta itself (a new PID has no prior hash, so the walk emits it), leaving the fork tracepoint
one job: short-lived pairing. Removal keys off the **reap** (`release_task`), *not* exit —
because a zombie is still a live entry the walk keeps showing as `Z` (its hot fields freeze, so
it re-emits once on zombifying then bails: O(1), never per-cycle). The reap tracepoint is
**RCU-deferred** (it fires from an RCU callback, not synchronously at the wait), so a removal
lands within roughly a grace period of the reap — far inside the cycle interval, so effectively
next-cycle, the same latency a synchronous signal would give at this cadence. **Short-lived**
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
syscall that triggers the seq_file walk. An open-coded `bpf_iter_task` in a `SEC("syscall")`
program, triggered by one `bpf_prog_test_run` with the delta landing in an mmap'd ring, would
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
(`tools/caprun`) grants exactly the needed capabilities without full root; agent sessions and
dev runs use it instead of interactive sudo.

## Tree build

`procs` sorted by PID ⇒ parent lookup is a binary search; child lists are built by
reverse-index prepend (yields PID-ascending siblings), no `HashMap`/per-node `Vec`.
One pre-order pass assigns `depth` and records order; its reverse accumulates
`subtree_size`. Scratch (`stack`, `order`) is gatherer-owned and reused. Iterative
(no recursion) — safe for pathologically deep trees.

A second reverse-order pass (`tree::aggregate`) computes inclusive `subtree_cpu` and
`subtree_mem` (self + all descendants) for manually collapsed process subtrees — same O(n)
as the size accumulation, using the existing `order` vector.

The structural tree is collapse-independent. The view flattens it to display rows
(`app::rebuild_rows`), skipping collapsed subtrees, after each gather and whenever the
collapse set changes. Selection follows the selected PID across refreshes. The flattened
display list is a plain heap `Vec`, not an arena buffer: it is walked *sequentially* in
render (the random walk it indexes into is the arena-backed row buffer), so it gains nothing
from the huge-page TLB win — and a heap `Vec`'s growth can never relocate the arena and
dangle the row slice the rebuild reads from.

### Process grouping

Grouping is split across the gather/view boundary, and the split is forced by one requirement:
folding is persisted and user-configurable. A persisted fold cannot key on process identifiers
(they change every launch), so it keys on a group's stable identity — but that persisted
configuration *is* the grouping policy, and it is user-owned and applied at display time. So the
gather layer answers only the config-free question — which processes share an identity, and how
trustworthy that boundary is — and the view forms, folds, labels, and aggregates groups from it.
This is deliberately *one* mechanism: a process resolves at most one identity, so it can never
flip between competing group labels across cycles.

Membership follows shared identity, not tree ancestry. On systemd desktops a launcher scope and
its instantiated service land in sibling cgroup scopes; resolving both to the same application id
makes them one group. Containers, pods, and system services cohere the same way, each from its
own cgroup shape. A cgroup boundary is trusted (kernel-owned, stable across runs); a purely
structural one — a shared-binary process fan, a runtime worker pool — is not, and is recognized
only where no cgroup identity exists, so the trusted identity always wins with no arbitrary
tiebreak. Identity resolution is change-gated, not per-cycle: a process's grouping is a pure
function of its cgroup and argv and the surrounding tree shape, which move only on a birth, death,
PID reuse, or argv/cgroup rewrite. The gather layer folds exactly those into a monotonic version
stamp; while it holds steady the resolver and the view-side grouping both reuse their prior
result, so a settled desktop does no grouping work — the governing goal is that work track change,
not population. Reuse is exact (a stable input provably yields identical groups), so a startup
transient still self-corrects on the next change without a settling window.

A group is shown as one row: a resolved label (an application's desktop-entry name; otherwise a
container, pod, unit, or command), aggregated CPU/memory/GPU (the root alone is often an idle
launcher whose own numbers mislead), and the hidden-member count. Tiny groups and terminals are
left unfolded — a two-process fold hides as much as it saves, and each shell is its own
workspace. Expanding a group reconstructs its members as a forest while foreign processes that
happen to be children of a member stay in the native tree. Selection and fold state follow the
stable identity, not whichever process currently represents the row.

Auto-fold reduces clutter by default; the user's expand/fold is remembered. Only a
cross-run-stable identity (a desktop application, a systemd unit) persists its fold across
restarts; ephemeral ones (container ids, pod uids, structural tokens) stay session-only, so an
expansion of a non-persistable group is transient state that clears when the group disappears,
while a persistable one returns as the user left it.

Application memory is proportional set size summed across every member — resident-set addition
double-counts the shared pages a browser's helpers map, so it is only the fallback used when a
live member cannot be read. Because a per-member read walks the whole address space, it is
change-gated, not recomputed: resident-set size is a free staleness proxy, so only members whose
resident set moved (measured absolutely, against host memory) are re-read, highest-change first
under a per-cycle budget, with a slow periodic refresh backstopping the shared-page drift the
resident gate cannot see. Sampling is a view concern, done only for folded rows in the viewport.
Cgroup memory is deliberately excluded: it measures charged cache and kernel resources, not
resident footprint. See [Application memory accounting gotchas](application-memory-gotchas.md).

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

`Cell` is also the **terminal trust boundary**: `comm`/`cmdline` are attacker-controllable
(any local user's `argv`, or `prctl(PR_SET_NAME)`), so every text path through it neutralizes
control characters (C0/C1) to a placeholder before they can reach the terminal's *control*
channel as raw escapes — uniformly on both the ASCII and the wide-char path, since the
`non_ascii` flag routes a string with even one high byte through the latter. A root operator
watching an unprivileged user's processes must not have escape sequences injected into their
terminal.

**Color is 24-bit and it carries meaning.** etch emits only truecolor — no ANSI-16 named
colors, no 256-color fallback — so a cell's color can be a *continuous function of its value*
rather than a bucket. Magnitude columns (CPU%, peak, RSS, nice) map through semantic gradients
whose *shape* encodes what the magnitude means to the reader: near-zero recedes into the
background, a normal level is calm, an alarming level is hot. RSS is log-scaled, so one
perceptual step is roughly an order of magnitude (a linear ramp would make megabytes and
gigabytes indistinguishable); nice diverges from a neutral zero. CPU% has one deliberate
*discrete* break: exactly-zero is a hard step to a dim idle tone and any nonzero jumps to a
visible floor, because "used nothing" and "used a sliver" are categorically different, not
adjacent magnitudes that should blend. Categorical columns (state, user, tree glyphs) use
named colors — there is no ordering to interpolate. This is *why* the
per-cell gate hashes the value **and** its style: a gradient-band crossing must repaint even
when the formatted text is byte-identical. The Command cell is multi-colored (dim path prefix,
bright basename, muted tree connectors, an alarm tint when the binary or a linked library was
deleted), so it records color *runs* the fill painter emits with per-run escapes. Row-level
color is selection only (a background); process state no longer tints the whole row — that
flickered, and magnitude/kind now live in the per-cell colors instead.

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

**Display state is derived from the ring, not point-sampled.** The kernel state byte answers
"was this task on-CPU at the microsecond we read `/proc`?" — which flickers `S`↔`R` for a
low-but-active process. The S column instead shows a *derived* state: `R` when the CPU ring
recorded any ticks across a short recent window, otherwise the raw byte. This makes `R` stable
(a 1% process reads `R` continuously) and makes `D`/`Z`/`T` trustworthy (they appear only when
the process genuinely isn't executing). The raw byte stays in the row untouched — kill-safety
and the task-state tallies read *it*, never the derived one, because they must reflect the true
kernel state, not the "is it active" question the display answers.

**System-wide CPU% is windowed the same way.** The machine-wide rate differences a ring of
recent `/proc/stat` snapshots at its two endpoints (the same multi-second window as per-process
CPU), not a single interval. A bare `cur − prev` delta at this cadence resolves only a few tens
of jiffies, so a transient burst dominates one sample then vanishes — the bar jumps. The
user/sys/iowait split is preserved because every counter is differenced over the same endpoints.

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
- **Kill is race-safe**: `sys::kill_verified` pins the target with a `pidfd` and re-checks
  `start_time` (see **PID reuse model**) — the pidfd makes it physically safe, the
  `start_time` makes it formally correct. (`sudo` escalation for protected processes is
  still future.)

## Not yet implemented (see GOALS.md)

macOS (`sysctl`/`libproc`) backend; thread-group (TGID) handling; CEF/Chromium-aware
collapse + persisted collapse rules; config + state-cache files; `sudo` escalation
for protected processes; disk-I/O and other columns (exe path); per-core CPU bars
(toggle); gatherer pause on `SIGTSTP`/background.

**Privileged mode** is partially implemented: the emit-on-change task iterator (delta process
snapshot) and the fork/free tracepoints (births in the delta, reap-based removal, short-lived
capture) work and fall back to the `/proc` source when the object can't load. Still to do — the
open-coded/no-`read()` iterator (deferred on the BPF library, see above) and the network-I/O
probes that
*motivated* the privileged layer (per-PID TCP/UDP throughput via `fentry`, no unprivileged
equivalent) and filesystem-I/O from the iterator's accounting fields. See
`docs/plans/privileged-mode.md` for the remaining design.
