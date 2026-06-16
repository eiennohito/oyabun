# Scale Observation — Two-Tier Sampling for Extreme Hardware

## Problem

The gatherer observes **every** PID every cycle: read stat, parse, feed the CPU window,
build the tree, compute aggregates. That is O(n) per cycle. At normal scale (~700 procs)
it is sub-ms and fine. At the "extreme hardware" goal (tens of thousands of tasks on
many-core boxes) reading 50k stat files every 500 ms is real, sustained work — and most
of those processes are not on screen.

The key realization: **observation ≠ display.** Collapse and scroll already bound *display*
work (`rebuild_rows` flattens only the expanded set; `render` paints only the scroll
window). They do **not** bound *observation* — and they should not naively, because a
process monitor's core job is to answer "what is using resources right now," including a
hidden/idle process that just spiked. You cannot rank or catch a spike without sampling.

But you do not need to sample **everyone every cycle** — only often enough to rank with
bounded staleness, plus keep on-screen data fresh. Amortize the global observation across
cycles instead of doing it all at once.

Unprivileged constraint (proven during planning): the cheaper enumeration/event sources —
netlink proc connector (`CAP_NET_ADMIN`), BPF task iterator (`CAP_BPF`), taskstats — are
all privileged. atop runs unprivileged by default, so `getdents64` on `/proc` stays the
enumeration path. `getdents64` is already batched (whole `/proc` in ~1 syscall); its cost
is kernel-side dirent materialization (`proc_pid_readdir`/`next_tgid`/`filldir64`), not
syscall count. The only unprivileged lever there is **frequency**, not API.

## Goal

Make O(n) observation **scale**: per-cycle work proportional to (display-relevant set +
1/N of the rest), not the full process count — while keeping ranking correct (bounded
staleness) and on-screen data always fresh. This is the extreme-hardware lever; it does
~nothing for a 700-proc box (gather is already sub-ms there). Build it **after** the THP
arena plan, which makes the all-PID path miss-free below the scale where rotation matters,
and which provides the stable storage this plan's incremental snapshot needs.

## Design

### 1. Two-tier sampling

- **Hot tier — sampled every cycle:** display-relevant PIDs = visible rows **plus**
  descendants of visible *collapsed* nodes (those feed an on-screen aggregate, so they must
  be live).
- **Cold tier — rotated 1/N per cycle:** everyone else, selected by the existing stagger
  `(gen + pid) % N == 0` (the same mechanism cmdline/uid already use). Every PID is still
  observed within N cycles → ranking works with bounded staleness; a sustained hog surfaces
  in ≤ N cycles.
- **Promote on navigation / becomes-visible:** force an immediate fresh read of newly-visible
  PIDs so what you are looking at is never stale (see §5, the hard part).

### 2. Per-PID CPU window

Today `CpuTracker` has a single global `last: Instant` and assumes every PID is sampled
every cycle. With rotation, each PID needs its **own** last-sample timestamp; its rate is
`Δticks / Δjiffies` over *its own* elapsed interval.

Correctness note (validated in planning): the moving average is `Σticks / Σjiffies` over the
window = total work ÷ total time — **invariant** to how sampling is chunked. So a cold PID
sampled every N cycles has the *same* average as one sampled every cycle; only `cpu_peak`
(the max single-bucket rate) degrades, because a spike inside an N-wide bucket is smeared
across it. Acceptable default: **averages exact, peaks under-reported for the cold tier.**

Impl: the per-PID ring depth should be **wall-clock-based** (so a cold PID's ~10 s window
holds fewer, wider samples) rather than a fixed sample count.

### 3. Incremental snapshot (the big structural change)

Rotation means the back buffer can no longer be `reset()`-and-refilled-from-scratch — that
would drop every cold-tier PID not sampled this cycle. Instead the back buffer is **seeded
from the previous snapshot** (carry forward all entries), then only hot + rotated PIDs are
updated, and births/deaths applied.

Consequences:
- A per-cycle copy of the previous `procs` into the back buffer (cheap, contiguous, and on
  THP after the arena plan).
- Carried-forward PIDs keep their `comm`/`cmdline`/`ppid`/`mem`/`ticks` from when last
  sampled. Their `StringRef`s must stay valid — but the current per-cycle string arena is
  reset each cycle, so **carry-forward requires stable string storage** that is not reset.
  This is the cmdline-intern / stable-string-arena deferred from the THP plan: it becomes a
  prerequisite here. Comm and cmdline of unsampled PIDs are referenced, not re-read.
- The tree build uses carried-forward `ppid` for cold PIDs (ppid is near-static; only changes
  on reparent — caught within N cycles, or promptly if we re-read a dead parent's former
  children).

This is the riskiest part of the plan and the reason THP-arena (which introduces the arena
partitioning and can introduce stable string storage) comes first.

### 4. Enumeration cadence decoupled from sampling

`/proc` is scanned every cycle today only to catch **births**. Deaths are nearly free: a
held-fd read returns `ESRCH` (the persistent-fd pool already relies on this). So:
- Enumerate every **K** cycles (a tunable, default small, e.g. 2–4). A new process appears
  within K×interval — fine for a monitor. Scan cost drops K×.
- Deaths of sampled PIDs are caught immediately by the pool; the rest at the next scan.
- This composes with §1: enumeration cadence (births) and per-PID sampling cadence (load)
  are independent knobs.

### 5. Viewport channel + navigation fast-path

- **Viewport channel (UI → gatherer):** a new `Ctrl::Viewport { … }` carrying the visible PID
  set (or scroll range) + the collapse set, sent on scroll/collapse/navigation. The gatherer
  stops being viewport-blind; it uses this to mark the hot set and to compute
  descendants-of-visible-collapsed (it already builds the tree, so it has the structure; the
  collapse set comes from the UI). Keep it advisory/one-way — the gatherer degrades to "treat
  all as hot" if it has no viewport info yet.
- **Navigation fast-path (the hard latency bit — do not hand-wave):** the gatherer wakes on a
  500 ms timer; a scroll keystroke cannot wait for that. Options:
  - Extend `Ctrl::Refresh` to carry a **PID subset** the gatherer services out-of-band and
    republishes, or
  - Let the UI read those few stats itself for instant feedback (UI-thread I/O — a handful of
    stat reads, ~µs each; weigh against the "UI never blocks" invariant).
  This interacts with double-buffer recycling (a partial/out-of-band update vs a full
  snapshot republish) and is the part needing the most careful design.

## Validation checkpoints (do these first)

1. **Average invariance under rotation.** A PID sampled every N cycles must report the same
   `cpu_pct` (within quantization tolerance) as one sampled every cycle, over the same wall
   window. Assert; this is the load-bearing correctness claim.
2. **Spike latency bound.** A process that goes busy while in the cold tier surfaces (rank /
   value) within ≤ N cycles. Document the peak-under-report explicitly.
3. **Incremental snapshot integrity.** After seeding-from-previous + partial update, the
   snapshot still satisfies every invariant the full rebuild did: PID-sorted, valid tree
   links, valid `StringRef`s for carried-forward PIDs (this is where stable string storage is
   exercised), correct task tallies.
4. **Navigation freshness.** A newly-visible row shows fresh data within one frame of the
   scroll, via whichever fast-path is chosen.
5. **Birth latency** under enumerate-every-K: a new process appears within K×interval; a
   death vanishes promptly (pool ESRCH) and never lingers past the next scan.

## Touch points

- `gather/mod.rs`: `CpuTracker` → per-PID `last`/wall-clock ring; the hot/cold tier decision;
  rotation selection; enumeration cadence (K); the incremental-snapshot seeding.
- `gather/mod.rs` (`Ctrl`): `Ctrl::Viewport { visible, collapsed }`; subset-carrying `Refresh`.
- `snapshot.rs`: incremental seed-from-previous; per-entry last-sampled generation (for the
  tier logic / debugging); carried-forward string validity.
- `app.rs` / `main.rs`: send viewport + collapse set on scroll/collapse/navigation; request the
  navigation fast-path refresh.
- Depends on the THP-arena plan's stable string storage for carry-forward.

## Risks / open questions

- **Incremental snapshot ↔ double-buffer recycling**: seeding the back buffer from the front
  while the front may still be UI-referenced needs care (copy under unique access; the two-
  buffer model still holds, but the "reset + refill" assumption is replaced by "copy + patch").
- **Stable string storage lifetime**: a carried-forward `StringRef` into a gatherer-owned stable
  arena must outlive any snapshot referencing it — generation-eviction of that arena must lag
  the live snapshots (≥1 generation). Cross-thread lifetime hazard; design explicitly.
- **Gatherer ↔ UI coupling**: the gatherer becomes viewport-aware. Keep the dependency one-way
  and advisory so a missing/stale viewport only costs extra sampling, never correctness.
- **Cold-tier staleness vs ranking**: bounded to N cycles; make N tunable; default conservative.
  Re-evaluate whether "find the hog" needs a guaranteed-fresh global pass at some coarse cadence
  (a periodic full sample — the "full refresh when needed").
- **Reparent detection**: a cold PID's `ppid` is stale until its next sample; a parent's death
  reparents its children immediately in the kernel but our tree lags ≤ N cycles unless we
  re-read a dead parent's former children on its death. Decide whether that targeted re-read is
  worth it.
- **Worth-it gate**: this is pure extreme-scale investment with real complexity and new coupling.
  It should not be built unless the extreme-hardware target is active — at normal scale the THP
  arena plan already makes all-PID observation cheap enough.

## How to verify it worked

- Re-profile / measure at high simulated PID count (e.g. spawn thousands of `sleep`s): per-cycle
  stat read + parse work scales with (display-relevant + n/N), not n; full enumeration cost
  drops ~K×; the UI stays responsive on scroll; `cpu_pct` averages match the full-sampling
  baseline.
- At normal scale: no regression, no behavioral change beyond peak-under-report for off-screen
  processes.

## Sequencing

**After the THP-arena plan.** That plan makes the all-PID path miss-free (so rotation is
unneeded below extreme scale) and introduces the arena partitioning + stable-string storage
this plan's incremental snapshot depends on. Build this only when targeting extreme hardware.
