# Gather Coordination — Cut the io_uring Wakeup/Scheduler Overhead

## Problem

Profiling the post-storm gatherer shows ~19% of its on-CPU cycles in
`__do_sys_io_uring_enter` → `io_cqring_wait` → `__schedule` (`dequeue_task_fair`,
`pick_next_task`, `psi_task_switch`, plus `__perf_event_task_sched_out` which is partly
perf's own measurement overhead). This is **pure coordination plumbing — it produces no
`/proc` data.** It is the largest remaining slice of avoidable, unprivileged overhead above
the `do_task_stat × n` floor.

Root cause, from strace: **~20,455 `io_uring_enter` calls over ~402 cycles ≈ 51 per cycle.**
We wake the gatherer ~51 times to produce one snapshot. The backend uses
`submit_and_wait(1)`: submit the batch, block for **one** completion, drain whatever is
ready (~13), top up, block for one again. So ~`n / drained-per-wakeup` rounds, each a
sleep→wake = a full scheduler context switch.

`submit_and_wait(1)` was chosen for I/O↔parse **overlap** (parse the first arrivals while
later reads finish). But parse is now ~8% of a sub-ms cycle, while the 51 context switches
cost ~19% — the trade inverted. Each needless context switch also evicts cache/TLB, feeding
the worst-case TLB problem the THP plan addresses, *especially under load* (when a monitor
is most used).

Secondary: `/proc` enumeration runs every cycle (~15% of the gather), full O(n), when births
per cycle are tiny.

This plan is **independent** of the THP-arena (memory) and scale-observation (extreme-scale
sampling) plans — it is a coordination concern. It shares one component with
scale-observation: enumeration cadence (§3), defined here and reused there.

Out of scope: `parse_stat` itself (a future SIMD rewrite — fixed-layout decimal fields are a
strong SWAR/SIMD target — is tracked separately).

## Goal

Drive per-cycle coordination from O(n)-wakeups toward **O(1)**, and enumeration from
O(n)/cycle to **O(n)/K**, with zero added privilege. Close the ~19% pure-waste slice so the
gatherer sits ~at the unprivileged floor (`do_task_stat`-dominated). Also settle, by
measurement, whether io_uring is even the right default at normal scale now that the storm
is gone.

## Design

### 1. Wait-batching (the core change)

Replace `submit_and_wait(1)` with a wait for the **full outstanding batch** (or a large cap)
per round. In steady state — all PIDs cached → all 1-SQE reads, `n` < SQ depth (4096) — the
loop becomes: submit all `n`, `submit_and_wait(<outstanding CQEs>)`, sleep **once**, drain
all, parse. ~51 wakeups → ~1–2.

Wrinkles to handle in impl:
- **`submit_and_wait(N)` counts CQEs, not chains.** A cached read emits 1 CQE; a new-PID
  chain emits 2 (open + read). Track an outstanding-**CQE** counter (cached += 1, new += 2),
  decremented as CQEs are reaped — do not reuse the chain counter.
- **Cap N** at the CQ ring capacity and at a sane batch ceiling so a round with a huge PID
  count still makes progress in chunks rather than waiting for an impossible count.
- Keep the `EINTR` retry.
- The fill/reap loop structure stays; only the wait count and its accounting change. Overlap
  is intentionally traded away (parse-after-drain), which is correct now that parse is cheap.

### 2. Modern ring setup flags (probe + fallback)

The gatherer is the **sole** ring submitter — the exact pattern the modern single-issuer
flags optimize. Add to ring creation, each probed independently with clean fallback (mirroring
the existing granular io_uring feature-probe approach):

- `IORING_SETUP_SINGLE_ISSUER` (6.0) — assert one submitter; enables related fast paths.
- `IORING_SETUP_DEFER_TASKRUN` (6.0, requires SINGLE_ISSUER) — defer completion task-work to
  the `enter`-to-wait call instead of running it async / via IPI. Fits our batch-wait model
  exactly: we always enter to wait, so task-work runs there, coalesced.
- `IORING_SETUP_COOP_TASKRUN` (5.19) — don't IPI the target task for task-work; run on the
  next ring exit. Cheaper wakeups.

(Set via the `IoUring` builder's `setup_*` methods — confirm exact names against io-uring
0.7.x.) These shave `io_run_task_work` + IPI/reschedule overhead. With `DEFER_TASKRUN`,
completions are only processed on the wait call — benign here since the loop always waits.

### 3. Enumeration cadence (shared component with scale-observation)

Scan `/proc` every **K** cycles instead of every cycle. `getdents64` is already the batched
API (whole `/proc` in ~1 syscall); the cost is kernel-side dirent materialization
(`proc_pid_readdir`/`next_tgid`/`filldir64`), so the only unprivileged lever is frequency.

- On a **scan** cycle: full enumeration, diff the PID set (births appear here, within
  K×interval — fine for a monitor).
- On a **skip** cycle: reuse the last PID list; still read stat for all known PIDs. Deaths are
  caught immediately by the fd-pool (`ESRCH` on the held read → drop the PID that cycle), so a
  dead process never lingers past the cycle it dies in, regardless of scan cadence.
- **PID reuse inside the gap** is handled by the existing reopen-on-`ESRCH` path: the held fd
  reads `ESRCH`, we close + reopen by path and get the new incarnation immediately — *without*
  waiting for the next scan. Only a brand-new PID *number* waits ≤ K cycles.
- Knob: `K` as a const + `ATOP_ENUM_EVERY` env override (default small, e.g. 2–4).

This saves `(K-1)/K` of the enumeration cost. It is **orthogonal** to sampling: stat reads
still happen every cycle here (that is the floor / a separate concern). scale-observation's
two-tier sampling builds *on top of* this enumeration cadence — define it here, reference it
there, implement once.

### 4. io_uring-vs-syscall A/B (measurement → decision)

With the storm gone, the persistent-fd plan's deferred question is now answerable: does
io_uring's parallelism (io-wq workers reading procfs concurrently) actually beat plain serial
`lseek+read` in the syscall backend, *net of* this ~19% coordination overhead?

- Measure both backends (`ATOP_FORCE_SYSCALL` toggles): wall-clock per cycle **and** total CPU
  cycles (perf `cycles`, **not** under strace), across PID counts (spawn N `sleep`s: 100,
  1k, 5k, 20k).
- Expected shape: below some crossover N the syscall backend may have **lower total CPU**
  (zero coordination overhead, reads are fast and mostly cached), above it io_uring's
  parallel reads win. Wait-batching (§1) shifts the crossover down (less uring overhead).
- Outcome is a **decision**, biased toward simplicity: if wait-batching makes uring win at all
  realistic N, keep uring and do nothing. Only if syscall is clearly better below a common N
  do we consider pid-count-adaptive backend selection (added complexity — justify with the
  numbers, do not assume).

## Validation checkpoints (do these first)

1. **Wakeup count.** Re-run the strace `io_uring_enter` count: expect ~51/cycle → ~1–2/cycle
   after §1. Correctness guarded by the existing oracle (`uring_matches_syscall_backend`) and
   persistent-reread tests — they must stay green.
2. **Ring-flag probe + fallback.** Probe succeeds on 6.x; forcing the flags off (older kernel
   or a test toggle) falls through cleanly with no behavior change.
3. **Enumeration cadence.** A newly-spawned process appears within K×interval; a killed
   process vanishes the same cycle (pool `ESRCH`); a reused PID is picked up immediately via
   reopen; no PID leak across skip-scan cycles.
4. **A/B crossover.** Produce the PID count where uring's total CPU beats syscall's, with both
   wall and cycle numbers — the artifact that decides the default.
5. **Re-profile.** `io_uring_enter`/`__schedule` share drops from ~19% toward noise;
   `do_task_stat` becomes the clear dominant — i.e. we have reached the unprivileged floor.

## Touch points

- `gather/uring.rs`: `submit_and_wait(1)` → batched wait with outstanding-CQE accounting;
  ring `probe` gains granular setup-flag probing (single-issuer / defer-taskrun / coop-taskrun)
  with fallback.
- `gather/mod.rs`: enumeration cadence (scan every K, reuse PID list on skip cycles, deaths via
  pool); `ATOP_ENUM_EVERY` knob.
- test/measurement: extend the ignored `gather_steady_state` probe (or add one) to drive the
  A/B at varying PID counts and report wall + cycles per backend.

## Risks / open questions

- **CQE accounting bug → deadlock or premature reap.** Waiting for more CQEs than will arrive
  hangs the cycle; waiting for too few under-drains. Track outstanding CQEs exactly (cached 1,
  new 2), cap by CQ capacity, keep EINTR retry. Cover with a test that mixes new + cached PIDs.
- **`DEFER_TASKRUN` requires reaching the wait** for task-work to run — true in our loop, but
  any future code path that submits without waiting would stall completions. Note the invariant.
- **enum-every-K vs PID reuse** within the gap — covered by reopen-on-`ESRCH` (a reused PID
  appears immediately, not at the next scan); verify in checkpoint 3.
- **A/B says syscall wins below N** → tempting to add backend-selection-by-PID-count. Resist
  unless the numbers are decisive; pid-count-adaptive backend switching is real complexity
  (two persistent-fd-pool implementations, switching mid-run).
- **Shared component drift**: enumeration cadence is defined here and consumed by
  scale-observation — implement once, do not duplicate when that plan lands.

## Sequencing

Independent of THP and scale-observation; can run before, after, or alongside. Highest
pure-waste-per-effort of the unprivileged items (deletes the ~19%). The enumeration-cadence
piece (§3) is a prerequisite/shared component for scale-observation's two-tier sampling.
