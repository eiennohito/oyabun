# Goals

## Priority order (the tie-breaker)

When goals conflict, higher wins:

1. **Minimal CPU** — the theoretical floor, not "good enough." Idle is near-zero (blocked,
   not polling); active work tracks change, not population, wherever avoidable. Every removed
   copy / scan / syscall / atomic counts.
2. **Minimal RAM**, scaled to load. A largish *floor* is accepted: THP commits whole 2 MiB
   pages, so we will not go below ~6–8 MiB regardless. Growth above the floor must track real
   load, not waste.
3. **UI responsiveness** — redraw/input latency **< 10 ms at p90, < 100 ms at p99.99**.
   Generous on purpose: loose enough to *serialize* gather and render (drop all concurrency)
   and spend that simplicity on goals 1–2. We currently run well inside it.
4. **Monitoring richness, not fanciness** — invest in *what* is observed (more signals,
   deeper tree/cgroup/thread awareness) over UI flourish.

This order is *why* the app serializes to one thread: the latency budget is loose enough that
losing gather/render parallelism costs nothing measurable, while the serial model deletes
whole classes of CPU/RAM overhead (copies, double-buffering, the cross-thread lease,
generational GC) and makes data races non-representable. See `docs/ARCHITECTURE.md`.

## Core constraints

- **Lowest possible footprint, on purpose**: a *full-featured* process manager **and** the
  smallest CPU/memory cost we can engineer — a hard goal, not a nicety. The governing belief:
  **nothing is free.** Every convenience has a price — a per-cycle copy, an allocation, a
  syscall, a bounds check on the hot path — and we would rather build the careful primitive
  once than pay that price every cycle forever. This is deliberately anti-pragmatic: when the
  pragmatic choice is "just copy it, it's cheap," the default here is to ask whether a
  primitive removes the cost entirely. The enabling strategy is **safe-ish primitives** —
  confine the unavoidable `unsafe` (mmap-backed arenas, the lock-free snapshot exchange,
  io_uring buffers) behind small, tested types so we can keep experimenting with
  extreme-low-overhead techniques without the unsafe sprawling across the codebase.
- **Lightning fast**: minimal render latency.
- **Near-zero CPU at idle**: htop is considered heavy. Event-driven where possible, poll only when visible and only at display refresh rate.
- **Robust process tree**: parent-child relationships, thread grouping, cgroup awareness.
- **CEF/Chromium-based app awareness**: Chrome, Electron apps (Slack, Spotify, VS Code, etc.) — collapse and label their process forests intelligently. Collapse rules are persistable (e.g. "always collapse Spotify").
- **Dual privilege scope**: user-scoped and root-scoped actions (e.g. `kill` via sudo) from the same UI.
- **Follow mode**: auto-scroll to keep the selected process visible as the tree reshuffles between refreshes.
- **Scale to extreme hardware**: 256+ cores, terabytes of RAM — no panics, no overflows, no O(n²) on core/process count.

## Platform support

- **Linux**: primary target. Two backends:
  - **io_uring**: preferred where available (kernel 5.6+). Batch `/proc` reads into submission queues for minimal syscall overhead.
  - **Syscall-based**: fallback for older kernels or environments without io_uring (containers, restricted seccomp). Direct `read`/`open` against `/proc`.
- **macOS**: supported. Uses `sysctl`/`libproc` APIs instead of `/proc`.
- **Windows**: not a goal.

## Configuration

Two separate files:
- **Config** (human-readable): user preferences — keybindings, refresh rate, color scheme, column layout. Edited by hand.
- **State cache** (semi-human-readable): persisted UI state — collapse rules, column widths, sort order. Written by the app, readable but not intended for hand-editing.

Collapse rules live in the state cache. Matching: check inode first (survives renames), fall back to exe path.

## I/O monitoring

- **Disk I/O**: per-process read/write bytes from `/proc/pid/io`. Nearly free — same cost as
  `/proc/pid/stat`, fits zero-copy arena model. Visibility-gated (only read for visible
  processes when I/O column shown). Requires same-user or `CAP_SYS_PTRACE`/root for other
  users' processes. In privileged mode, read from `task->ioac` in the BPF task iterator
  (zero marginal cost).
- **Network I/O**: per-process TCP/UDP throughput via eBPF (`fentry` on `tcp_sendmsg` /
  `tcp_recvmsg` / `udp_sendmsg` / `udp_recvmsg`). No unprivileged alternative exists —
  this is what motivates the privileged mode. Accumulated in per-CPU BPF hashmaps, read
  once per cycle. Fallback (unprivileged): socket count from fd scan as a "has network
  activity" indicator.

## Privileged mode

Optional, capability-based (not full root). A single BPF object loaded at startup replaces
the `/proc` observation pipeline when capabilities are available, and adds network I/O
(which has no unprivileged path). Falls back transparently to the unprivileged backend.
See `docs/plans/privileged-mode.md` for the full design.

Required capabilities: `CAP_BPF`, `CAP_PERFMON`, `CAP_NET_ADMIN`, `CAP_SYS_PTRACE`.
Granted via `tools/caprun` (a setuid wrapper — see `scripts/setup-caps.sh`).

## Stretch goals

- **GPU monitoring**: per-process GPU utilization and VRAM usage. NVIDIA via `dlsym`-loaded NVML (`libnvidia-ml.so`). AMD via sysfs (`/sys/class/drm/card*/device/gpu_busy_percent`, per-process via `/proc/pid/fdinfo` DRM stats). No hard dependency on vendor libraries — load at runtime, degrade gracefully.

## Non-goals (for now)

- Remote machine monitoring.
