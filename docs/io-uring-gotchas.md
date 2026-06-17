# Why io_uring can fail

io_uring ring creation can fail with `ENOMEM` even when the system has plenty of free
memory. This doc explains the accounting mechanism and why the syscall fallback exists.

## The per-user locked-memory budget

io_uring charges ring memory (SQ/CQ rings, SQE array) to a **per-user** kernel counter
(`user->locked_vm` in `struct user_struct`), checked against `RLIMIT_MEMLOCK`. This is
not per-process — every process under the same uid shares one pool.

Other subsystems charge the same counter:
- **Other io_uring rings** (Electron apps like VS Code and Slack each open several).
- **perf_event ring buffers** (`perf record` mmaps per-CPU buffers here).

The counter is invisible from userspace. `VmLck` and `VmPin` in `/proc/pid/status` report
different, per-process counters (`mm->locked_vm` and `mm->pinned_vm`). Neither reflects
`user->locked_vm`. The only signal is `ENOMEM` from `io_uring_setup`.

## Why the default limit is usually too low

`RLIMIT_MEMLOCK` defaults to 8 MiB (2048 pages) on most distributions. A 4096-entry ring
costs ~122 pages. That's modest — but the budget is shared:

- A desktop with VS Code + Slack can have 26 io_uring rings open (~182 pages at idle).
- `perf record` charges ~129 pages per online CPU for its event ring buffers. On a 32-CPU
  machine that's 4128 pages — 2× the entire limit — before any other user of the counter.
- perf with `perf_event_paranoid = -1` bypasses `RLIMIT_MEMLOCK` enforcement for itself
  but still charges the shared counter, so io_uring and other strict consumers see the
  budget as exhausted.

The budget is first-come-first-served with no reservation or priority. Any process under
the uid can silently consume the headroom another process needs.

## Why `mlock()` still works

`mlock()` uses a separate per-**process** counter (`mm->locked_vm`). It is completely
independent of `user->locked_vm`. This is why a process can `mlock` megabytes while
`io_uring_setup` for a tiny ring fails in the same process.

## Why the syscall fallback exists

The locked-memory budget is a deployment concern the user must configure, not something a
process manager can fix at runtime. The tool must work out of the box on a default system
where Electron apps may have already consumed most of the budget. The syscall backend
(persistent fds, `lseek(0)` + `read`) is the reliable floor — correct everywhere, just
without io_uring's batching and registered-buffer page-table-walk savings.

## Raising the limit

The fix is always raising `RLIMIT_MEMLOCK`. The hard limit must be raised system-wide
(requires root, re-login):

```
# /etc/security/limits.conf
*  soft  memlock  131072
*  hard  memlock  131072
```

Or per-service: `LimitMEMLOCK=128M` in a systemd unit.

128 MiB is comfortable for a 64 GiB machine. The needed minimum is roughly
`perf_event_mlock_kb × num_cpus + io_uring_rings × pages_per_ring + headroom`.

## Kernel source references (v6.14)

- `io_uring/rsrc.c` `__io_account_mem`: the `ENOMEM` check against `user->locked_vm`.
- `kernel/events/core.c` `perf_mmap` (line ~6740): perf's `user_lock_limit` calculation
  and the `perf_is_paranoid()` gate on `RLIMIT_MEMLOCK` enforcement.
- `include/linux/perf_event.h` `perf_is_paranoid`: returns false when `paranoid = -1`,
  disabling the limit check for perf while leaving the charge on the shared counter.
