# Persistent-fd Gather — Kill the io_uring Close-Storm

## Problem

After the renderer rewrite (etch), rendering is ~0.2% of CPU.
Profiling the new build (`sudo perf record`, ~14k samples, atop running) shows the cost
moved entirely onto `/proc` acquisition — and **~43% of all cycles is kernel lock
contention**, not useful work.

Flat self-time (of total cycles):

| self % | symbol | meaning |
|---|---|---|
| 26.77% | `osq_lock` | rwsem/mutex optimistic-spin (contended) |
| 12.18% | `native_queued_spin_lock_slowpath` | contended spinlock |
| 3.79% | `mutex_spin_on_owner` | more mutex spin |
| 2.84% | `do_task_stat` | **real** stat content generation |
| ~2% | `num_to_str` / `seq_put_decimal_ull_width` / `memset` | real stat formatting |
| 0.87% | `atop::gather::parse::parse_stat` | our parse |
| ~0.2% | `etch::*` | **rendering — gone as a cost** |

Per-thread: `iou-wrk` workers 71% (all kernel), `gatherer` (submitter) 27.6%, UI 1.0%.

### Root cause (from the callgraph)

```
iou-wrk → io_wq_submit_work → io_issue_sqe → __io_issue_sqe
   ├─ io_close 30.51% → __mutex_lock 29.32% → osq_lock 25.33% + mutex_spin 3.56%   ← THE STORM
   ├─ io_read 16.66% → seq_read → proc_tgid_stat → do_task_stat 9.46%   (real work)
   │                 └─ proc_pid_cmdline_read 4.23% → __access_remote_vm 2.40%
   └─ io_statx 1.61%
gatherer → io_uring_enter → io_submit_sqes → io_openat2 9.07% → path_openat 8.06%   (inline opens)
```

The backend re-opens `/proc/<pid>/{stat,cmdline}` **every cycle** as io_uring **direct
(fixed) descriptors**, then closes them.
When a procfs read blocks, the linked chain is punted to io-wq, so the trailing `Close`
of a **fixed** descriptor runs from `IO_URING_F_UNLOCKED` context and must take
`ctx->uring_lock` (a mutex).
With `N_SLOTS=512` → ~1024 fixed-fd closes per cycle across io-wq workers, that single
mutex becomes an `osq_lock` storm.
The design's elegance (linked `open→read→close` + direct descriptors, no runtime fd) is
exactly what creates the contention.

**The close every cycle is an artifact of opening every cycle.**
We don't need to close fds for processes that are still alive — a `/proc/<pid>/stat` fd
is re-readable across cycles.

## Goal

Make steady-state gather work proportional to process *churn* (births/deaths per cycle),
not process *count*: **one stat read per live PID per cycle, ~zero opens, ~zero closes,
~zero statx**, with the lock contention gone — and do it within the **default 1024 fd
soft limit** so no `setrlimit`, privilege, or container exception is needed.

## Design

### 1. Persistent stat-fd pool (the core change)

A gatherer-owned pool of open `/proc/<pid>/stat` fds, keyed by PID, reused across cycles
(same lifecycle pattern as `CpuTracker`'s persistent `HashMap`).

- **Capacity ≈ 960** (target 1024 default soft limit minus ~64 reserved for the `/proc`
  dir fd, the ring, std{in,out,err}, kill pidfds, headroom).
- First sighting of a PID → claim a slot, `openat` stat, install into a fixed-file slot.
- Every cycle → re-read the held fd (no open, no close). `do_task_stat` (the irreducible
  ~9%) is all that remains in the kernel read path.
- PID disappears from enumeration → close its fd, free the slot (generation-tagged
  eviction, mirroring `CpuTracker::update`'s `seen_gen` retain).
- **PID reuse**: stat carries `start_time`; if a held fd's PID reports a new `start_time`
  (or the read returns ESRCH / 0 bytes), the incarnation changed → close + reopen. The
  existing `(pid, start_time)` identity already used for safe `kill` applies here.

**Re-read mechanism** — the one semantic to validate (see Checkpoints):
- syscall backend: `lseek(fd, 0, SEEK_SET)` + `read` — the known-good idiom for
  re-reading a single-show seq_file.
- io_uring backend: `ReadFixed` at **offset 0** each cycle (no lseek op exists). Must
  confirm a proc single-show file regenerates current content on a repeated offset-0
  read; if not, fall back to the syscall re-read or a periodic reopen.

Per cached PID the io_uring chain collapses **7 SQEs → 1** (just the read).

### 2. comm is free; cmdline is coarse

- **comm** (the process name) rides *inside* stat, which we re-read every cycle. So it is
  already fresh at zero extra cost. This covers the **kernel-task** case: kworkers rename
  themselves (`kworker/u32:1` → `kworker/u32:1-events`) and we render `[comm]` for them —
  fresh every cycle for free, no separate read.
- **cmdline** (the separate `access_remote_vm` read — the costly 4.2%) is *not* immutable:
  userspace daemons rewrite their own argv (`postgres: checkpointer`, `nginx: worker
  process`, `php-fpm: pool www`). So it can't be cached forever, but it changes slowly.
  - Re-read it on a **coarse cadence** (every N cycles, ~2–5 s; a tunable like
    `CPU_WINDOW_MS`), caching the string between.
  - **Stagger** across PIDs to avoid a periodic spike: refresh PID `p` when
    `(generation + p) % N == 0`, so ~1/N of processes refresh each cycle — smooth load,
    no thundering herd.
  - cmdline stays **transient** (open→read→close on the refresh tick only); its close
    volume is ~`userspace_PIDs / N` per cycle — far too low to contend, so it can keep the
    existing chain (or use a normal fd; either is fine at this volume).
- **uid** (via statx) is also near-static (privilege drops are rare) → refresh on the same
  coarse/staggered tick.

### 3. Skip cmdline for kernel threads entirely

Kernel threads have **permanently empty** cmdline, yet today we open+read+close it every
cycle for nothing — and kworkers are often a large fraction of all PIDs.

- Parse the `flags` field of stat (currently inside the `skip(9)` run in `parse.rs`) and
  test `PF_KTHREAD` (`0x0020_0000`); if set, never attempt a cmdline read.
- Fallback if flags parsing is undesirable: cache "cmdline was empty" and don't retry
  (except maybe on the coarse tick).

### 4. Overflow (N > pool) — graceful, never silent

On boxes with more processes than the pool (GOALS "extreme hardware"):

- Hold persistent fds for as many PIDs as fit; the overflow uses **transient
  open→read→close**, but with **non-fixed fds at a low in-flight depth** so the close hits
  `files->file_lock` briefly instead of storming `uring_lock`.
- `log()` when in overflow — no silent cap (per the no-silent-descoping rule).

## Steady-state result

| regime | per-cycle I/O |
|---|---|
| ≤ ~960 procs (desktops, most servers) | 1 persistent stat read/PID; cmdline ~1/N userspace PIDs; **0 opens, 0 closes, 0 statx**; never touches `RLIMIT_NOFILE`; **no lock contention** |
| > ~960 procs | pool free; overflow transient but low-concurrency/non-fixed (degrades to "today minus the storm") |

Expected: the ~43% contention (`osq_lock` + qspinlock + `mutex_spin_on_owner`) and the
~9% inline-open path-walk both collapse in the common case, leaving the irreducible
`do_task_stat` (~9%) + parse (~1%) as the floor.

## Validation checkpoints (do these first)

1. **Offset-0 re-read** of a held `/proc/<pid>/stat` fd regenerates current values.
   - syscall `lseek(0)+read`: expected good — confirm with a quick test (read self twice
     across a busy-loop, assert ticks advanced).
   - io_uring `ReadFixed(offset=0)` on a persistent fixed slot across two cycles: **the
     real unknown** — assert the second read reflects updated `utime`. If it doesn't,
     decide: per-cycle `lseek`+`Read` (normal fd, not fixed), or periodic reopen.
2. **Direct-descriptor `RLIMIT_NOFILE` accounting** (kernel-version dependent): do
   installed fixed-file slots count against the 1024 limit? Either way target ≤960; this
   only decides whether the fixed-file table shares the budget or has slack.

## Touch points

- `gather/mod.rs`: new persistent fd pool (PID→slot map + free list), generation eviction,
  the coarse/staggered cmdline-refresh decision, overflow policy. Sits beside `CpuTracker`.
- `gather/uring.rs`: split the per-PID chain — cached PID = single `ReadFixed`; new PID =
  open(install)+read, **no close**; death = `Close` the slot; cmdline/uid = transient on
  the refresh tick. Persistent fixed-file table sized to the pool.
- `gather/syscall.rs`: same pool, `lseek(0)+read` for held fds; transient overflow/cmdline.
- `gather/parse.rs`: parse the `flags` field, expose `PF_KTHREAD` (or an `is_kthread`
  bool on `StatFields`); already carries `non_ascii` and `start_time`.
- `snapshot.rs` / `ProcessEntry`: cmdline + uid become carried-forward cached values
  (the pool/cache owns the source of truth; the snapshot still gets the current value each
  cycle from cache or fresh read).

## Risks / open questions

- **Snapshot/arena interaction**: cmdline/comm `StringRef`s point into the per-cycle arena
  (reset each cycle). Cached cmdline must be re-materialized into the current arena each
  cycle (copy the cached bytes in) so `StringRef`s stay valid — or store cached cmdline in
  a separate stable buffer the snapshot can reference. Decide during impl; affects the
  double-buffer recycling.
- **fixed-file table churn**: installing/removing fixed slots also takes `uring_lock`; with
  persistent fds this happens only on churn (fine), but verify a burst of process spawns
  (e.g. a build) doesn't reintroduce a smaller storm — bound install concurrency if so.
- **Coarse cmdline staleness**: a daemon's argv-rewrite shows up to N cycles late.
  Acceptable for a monitor; make N tunable.
- **Pool thrash under high churn** (fork bombs, build farms): if births/deaths exceed the
  pool each cycle, behaves like the overflow path — ensure that path is contention-free,
  not just the steady state.

## How to verify it worked

Re-profile (`perf record` under load) and confirm:
- `osq_lock` / `native_queued_spin_lock_slowpath` / `mutex_spin_on_owner` are no longer top
  self-time symbols (were ~43% combined).
- `io_close` is gone from the hot path in steady state.
- Remaining hot kernel work is `do_task_stat` (irreducible).
- A/B note: also measure the **syscall backend** CPU vs io_uring with persistent fds — if
  io_uring's residual machinery doesn't beat plain serial `lseek`+`read`, reconsider it as
  the default for procfs.
