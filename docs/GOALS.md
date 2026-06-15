# Goals

## Core constraints

- **Lightning fast**: sub-millisecond response to input, minimal render latency.
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

- **Disk I/O**: per-process read/write bytes from `/proc/pid/io`. Nearly free — same cost as `/proc/pid/stat`, fits zero-copy arena model. Visibility-gated (only read for visible processes when I/O column shown). Requires same-user or `CAP_SYS_PTRACE`/root for other users' processes.
- **Network I/O**: future goal. Requires eBPF for per-process throughput (no procfs equivalent). v1 shows socket count from fd scanning as a "has network activity" indicator.

## Stretch goals

- **GPU monitoring**: per-process GPU utilization and VRAM usage. NVIDIA via `dlsym`-loaded NVML (`libnvidia-ml.so`). AMD via sysfs (`/sys/class/drm/card*/device/gpu_busy_percent`, per-process via `/proc/pid/fdinfo` DRM stats). No hard dependency on vendor libraries — load at runtime, degrade gracefully.

## Non-goals (for now)

- Remote machine monitoring.
