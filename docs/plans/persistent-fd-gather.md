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

## Architecture: hybrid gather

The persistent-fd optimization works with **both** backends. The architecture is a
**shared fd pool** that the uring and syscall backends both consume, not an uring-only
design. This matters because:

- The syscall backend is the fallback and must remain correct and reasonably fast.
- Some operations are better done synchronously (cmdline re-reads, uid refreshes) even
  when uring is available — the volume is too low to justify SQE overhead.
- Older kernels (5.4+) may lack io_uring features but still benefit from persistent fds
  via the syscall backend.

**Hybrid rule**: stat reads (the hot path, every PID every cycle) go through uring when
available. Cmdline re-reads and uid refreshes (coarse cadence, ~1/N PIDs per cycle) use
plain syscalls regardless — their volume is too low for batching to matter, and keeping
them synchronous avoids complicating the SQE chains.

### Kernel version support

| feature | minimum kernel | fallback |
|---|---|---|
| io_uring basic (ring + submit) | 5.6 | syscall backend (existing) |
| `IORING_REGISTER_FILES_SPARSE` | 5.19 | pre-allocate dense table (5.6+) or skip fixed files |
| `IORING_OP_READ_FIXED` | 5.6 | ok |
| fixed-file install via `OpenAt` + `file_index` | 5.15 | open via syscall, install via `IORING_REGISTER_FILES_UPDATE` (5.6+) |
| direct descriptors don't count against `RLIMIT_NOFILE` | 5.12 | cap pool to soft limit minus headroom |

**Target**: optimized path for 6.0+ (all features). Clean degradation to 5.4:

- **5.4** (no io_uring): syscall backend with persistent fds (`lseek+read`). The pool
  uses real fds capped to `RLIMIT_NOFILE - headroom`. Still eliminates open/close per
  cycle. This is the floor — everything above is additive.
- **5.6–5.14**: io_uring available but fixed-file install via openat may not work. Open
  stat fds via syscall, register into the fixed-file table via
  `IORING_REGISTER_FILES_UPDATE`, read via `ReadFixed`. Close via syscall on eviction.
- **5.15+**: full direct-descriptor path. `OpenAt` with `file_index` installs directly.
- **5.19+**: sparse file table (current code). No change needed.

The probe already returns `None` on unsupported kernels; the new code just needs to
probe each feature independently rather than all-or-nothing.

## Design

### 1. Persistent stat-fd pool (the core change)

A gatherer-owned pool of open `/proc/<pid>/stat` fds, keyed by PID, reused across cycles
(same lifecycle pattern as `CpuTracker`'s persistent `HashMap`).

- **Capacity**: derived at startup from `getrlimit(RLIMIT_NOFILE).soft - RESERVED` (where
  `RESERVED ≈ 64` covers the `/proc` dir fd, the ring, stdio, kill pidfds, headroom).
  On uring with fixed-file slots (≥5.12), fixed slots don't count against the fd limit,
  so the pool can be larger — but still capped to a sane max (~4096) to bound memory.
- First sighting of a PID → claim a slot, open stat fd (syscall or uring depending on
  kernel), install into the pool.
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

### 1a. Fixed read-buffer pool (registered prefix)

Today the arena is both the I/O target and the string store — raw stat text (~500 bytes)
stays allocated for the snapshot's lifetime even though only `comm` (~15 bytes) survives
parsing. Decouple them:

- **Read-buffer pool**: a fixed-size prefix of each arena, registered with io_uring.
  `N_SLOTS` buffers of `STAT_SLOT` bytes each, recycled as each PID is parsed. Size TBD
  (256–512 KiB); pinned memory is bounded regardless of PID count.
- **String intern zone**: the rest of the arena (unregistered, grows as needed). After
  parsing stat, copy `comm` into the intern zone; cmdline goes here too. Only display
  strings are retained per-snapshot.
- `ReadFixed` targets the registered prefix; after parse, the buffer slot is released for
  the next PID. The intern zone is never an I/O target — it just receives copies.

The arena remains one contiguous mmap (THP-friendly). Only the prefix is registered,
keeping pinned memory small and stable. Broader arena restructuring (moving `ProcessEntry`
structs and other hot data into the THP mapping) is future work.

### 2. comm is free; cmdline is coarse

- **comm** (the process name) rides *inside* stat, which we re-read every cycle. So it is
  already fresh at zero extra cost. This covers the **kernel-task** case: kworkers rename
  themselves (`kworker/u32:1` → `kworker/u32:1-events`) and we render `[comm]` for them —
  fresh every cycle for free, no separate read.
- **cmdline** (the separate `access_remote_vm` read — the costly 4.2%) is *not* immutable:
  userspace daemons rewrite their own argv (`postgres: checkpointer`, `nginx: worker
  process`, `php-fpm: pool www`). So it can't be cached forever, but it changes slowly.
  - **Age-adaptive cadence**: fresh processes (first few cycles after first sighting) get
    cmdline read every cycle — they may still be exec'ing or settling into their final
    argv. After a settling window (~3 cycles / 1.5 s), drop to coarse refresh (every N
    cycles, ~5–10 s). The per-PID `first_seen_gen` (already implicit in `CpuTracker`'s
    generation tag) determines age.
  - **Stagger** the coarse refreshes across PIDs to avoid a periodic spike: refresh PID
    `p` when `(generation + p) % N == 0`, so ~1/N of settled processes refresh each
    cycle — smooth load, no thundering herd.
  - cmdline stays **transient** (open→read→close on the refresh tick only); its close
    volume is ~`userspace_PIDs / N` per cycle — far too low to contend, so it uses plain
    syscalls (the hybrid rule).
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
   installed fixed-file slots count against the 1024 limit? Either way derive pool size
   from the runtime soft limit; this decides whether the fixed-file table shares the
   budget or has slack.
3. **Feature-probe each io_uring capability independently** on a 5.6-era kernel (or
   container with restricted seccomp) — verify the degradation path compiles, probes, and
   falls through cleanly. Specifically: sparse file table (5.19), direct-descriptor
   install via openat (5.15), and basic registered files (5.6).

## Touch points

- `gather/mod.rs`: new persistent fd pool (PID→slot map + free list), generation eviction,
  the coarse/staggered cmdline-refresh decision, overflow policy. Sits beside `CpuTracker`.
  Pool capacity derived from `getrlimit` at startup. Cmdline/uid refresh uses plain
  syscalls (the hybrid: low-volume ops stay synchronous regardless of backend).
- `gather/uring.rs`: cached PID = single `ReadFixed` (1 SQE, no chain); new PID =
  open(install)+read, **no close**; death = `Close` the slot. Feature probing becomes
  granular: sparse table, direct-descriptor openat, basic registered files — each probed
  independently with fallback.
- `gather/syscall.rs`: same pool, `lseek(0)+read` for held fds; transient overflow/cmdline.
  This is also the full-fallback backend for 5.4 (no io_uring) — persistent fds still
  eliminate open/close churn via plain syscalls.
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
- **Container fd limits**: containers often set `RLIMIT_NOFILE` to 256 or 512. The pool
  must derive its capacity from the runtime soft limit, not a compile-time constant.
  With very low limits, the pool might only hold a fraction of PIDs — the overflow path
  must be efficient enough for this to be acceptable.
- **Kernel 5.4 regression**: the syscall-backend persistent-fd path (lseek+read) is the
  floor for old kernels. It must be tested independently — don't let uring-only testing
  mask a broken syscall path. The existing `uring_matches_syscall_backend` oracle test
  pattern extends naturally.

## How to verify it worked

Re-profile (`perf record` under load) and confirm:
- `osq_lock` / `native_queued_spin_lock_slowpath` / `mutex_spin_on_owner` are no longer top
  self-time symbols (were ~43% combined).
- `io_close` is gone from the hot path in steady state.
- Remaining hot kernel work is `do_task_stat` (irreducible).
- A/B note: also measure the **syscall backend** CPU vs io_uring with persistent fds — if
  io_uring's residual machinery doesn't beat plain serial `lseek`+`read`, reconsider it as
  the default for procfs.
