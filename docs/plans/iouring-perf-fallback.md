# io_uring backend falls back to syscall under `perf record` (non-root)

**Status: root-caused.** The mechanism is shared per-user locked-memory accounting
between `perf_event` ring buffers and `io_uring` ring setup.

## Root cause

Both subsystems account locked pages to the same per-user counter `user->locked_vm`
(in `struct user_struct`), but they check it against **different limits**:

**io_uring** (`io_uring/rsrc.c` `__io_account_mem`, v6.14):
```c
page_limit = rlimit(RLIMIT_MEMLOCK) >> PAGE_SHIFT;       // 2048 pages (8 MiB)
cur_pages = atomic_long_read(&user->locked_vm);
new_pages = cur_pages + nr_pages;
if (new_pages > page_limit)
    return -ENOMEM;                                       // ← the failing check
```
No extra allowance. Hard limit = `RLIMIT_MEMLOCK / PAGE_SIZE`.

**perf_event** (`kernel/events/core.c` `perf_mmap`, v6.14 line 6740):
```c
user_lock_limit = sysctl_perf_event_mlock >> (PAGE_SHIFT - 10);
user_lock_limit *= num_online_cpus();                     // 129 × 32 = 4128 pages
```
Perf has a much larger budget: `perf_event_mlock_kb * num_online_cpus() / 4`. Pages up
to this limit are charged **directly to `user->locked_vm`** (line 6809):
```c
atomic_long_add(user_extra, &user->locked_vm);
```
Any excess beyond `user_lock_limit` overflows to `mm->pinned_vm` and is checked against
`RLIMIT_MEMLOCK` — but that check is **gated by `perf_is_paranoid()`** (line 6770):
```c
if ((locked > lock_limit) && perf_is_paranoid() && !capable(CAP_IPC_LOCK))
```
where (`include/linux/perf_event.h:1670`):
```c
static inline int perf_is_paranoid(void) { return sysctl_perf_event_paranoid > -1; }
```
With `perf_event_paranoid = -1`, this returns **false** — the RLIMIT_MEMLOCK enforcement
is skipped entirely. Perf charges up to 4128 pages to `user->locked_vm` with no cap.

**Result:** perf puts up to 4128 pages on `user->locked_vm`; io_uring then reads 4128 and
checks `4128 + ring_pages > 2048` → `ENOMEM`.

### Why `mlock()` is unaffected

`mlock()` checks a separate per-**process** counter `mm->locked_vm` (reported as `VmLck`
in `/proc/pid/status`). This counter is not shared between processes and is invisible to
both perf and io_uring accounting.

### Why per-process VmLck/VmPin show headroom (observation 4)

`VmLck` reports `mm->locked_vm` (mlock's counter — different subsystem). `VmPin` reports
`mm->pinned_vm` (perf's overflow-beyond-`user_lock_limit` counter — per-process, so the
child process reads its own empty mm, not perf's). Neither reflects `user->locked_vm`,
which is the counter io_uring checks.

## Environment where observed

- kernel `6.18.35-1-lts`, x86-64, 32 CPUs.
- non-root (uid 1000).
- `RLIMIT_MEMLOCK` soft = hard = 8388608 (8 MiB = 2048 pages).
- `/proc/sys/kernel/perf_event_paranoid` = -1.
- `/proc/sys/kernel/perf_event_mlock_kb` = 516 → `user_lock_limit` = 129 × 32 = 4128 pages.
- Background `user->locked_vm`: VS Code (5 processes × 4 rings) + Slack (1 × 3 rings) =
  26 io_uring rings of 256 entries, consuming ~182 pages.

## Measured results

Empirical `user->locked_vm` budget, measured by creating 1-entry io_uring rings (2 pages
each) until `ENOMEM`:

| condition | user->locked_vm (pages) | residual (pages) | io_uring? |
|---|---|---|---|
| bare | ~182 (VS Code/Slack) | 1866 | ✅ (933 rings) |
| `perf stat` | ~182 | 1866 | ✅ |
| `perf record -C 0 -m 1` (1 CPU, min) | ~378 | 1672 | ✅ (836 rings) |
| `perf record -C 0,1 -m 1` (2 CPUs, min) | ~506 | 1542 | ✅ (771 rings) |
| `perf record -C 0,1,2,3 -m 1` (4 CPUs, min) | ~764 | 1284 | ✅ (642 rings) |
| `perf record -C 0` (1 CPU, default mmap) | ≥2048 | 0 | ❌ |
| `perf record -m 1` (all 32 CPUs, min) | ≥2048 | 0 | ❌ |
| `perf record` (all 32 CPUs, default) | ≥2048 | 0 | ❌ |

Per-CPU perf charge with `-m 1`: ~129 pages (= `perf_event_mlock_kb >> (PAGE_SHIFT-10)`),
scaling linearly with CPU count, matching the `user_lock_limit` formula exactly.

## Previously open questions — now answered

- **`mlock` under perf:** succeeds (even 8 MiB), because it uses `mm->locked_vm` (separate
  counter).
- **Which accounting path:** `user->locked_vm` (per-user, in `struct user_struct`), checked
  by `__io_account_mem()` in `io_uring/rsrc.c`. Not the process's own `mm->locked_vm`.
- **CPU count / event count / mmap size:** all three matter. The dominant cost is the
  per-CPU event ring buffer (~129 pages each). Even 16 CPUs × 129 = 2064 pages > the
  2048-page limit.
- **`perf stat`:** does NOT reproduce (no ring buffer mmap → no `user->locked_vm` charge).
- **`perf record -p <pid>` (attach to running process):** untested, but the mechanism
  predicts it would still fail: perf mmaps its ring buffers (charging `user->locked_vm`)
  before the target process's next io_uring operation.

## Workarounds

1. **Raise `RLIMIT_MEMLOCK`** to cover perf's `user_lock_limit` + io_uring + background.
   Need `≥ perf_event_mlock_kb * num_cpus / 4 + headroom` pages. For 32 CPUs: ≥20 MiB.
   Requires root or `systemd` unit with `LimitMEMLOCK=`.
2. **Use `perf stat`** instead of `perf record` (no ring buffers → no charge).
3. **Limit CPUs + mmap size**: `perf record -C 0-N -m 1` with `N` small enough that
   `(N+1) × 129 + 182 < 2048` pages (≤ ~14 CPUs on this system).
4. **Accept the syscall fallback**: atop silently downgrades — correct but slower.
5. **Attach after ring creation**: `perf record -p <pid>` might work if the io_uring ring
   was already built before perf started. Untested.

## Note on the prior framing

The THP-arena §3 work (bounding registered buffers to a fixed landing pad) was claimed to
make non-root `perf` profiling of the io_uring path possible. The failure happens at **ring
creation** (`io_uring_setup`), before any buffer registration, so registered-buffer size is
not the operative factor. The bounded-pinned-buffer change stands on its own merits; the
perf-profiling claim is withdrawn. The actual fix is raising `RLIMIT_MEMLOCK`.
