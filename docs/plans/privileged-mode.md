# Privileged Mode — remaining work (network + filesystem I/O)

The privileged-mode **foundation** is implemented and documented in `docs/ARCHITECTURE.md`
("Observation sources"): the single BPF object with graceful fallback, the emit-on-change task
iterator (delta process snapshot replacing the `/proc` stat pipeline), the fork/free
tracepoints (births in the delta, reap-based removal, short-lived capture), the zero-copy
shared-layout contract, the capability probe, and the committed-`.o` build model. This file is
now only the **unbuilt** parts.

## Remaining: open-coded iterator (drop the `read()` trigger)

Emit-on-change made the seq_file stream O(churn), but the per-cycle `read()` syscall that drives
the walk remains. The end state removes it: an open-coded `bpf_iter_task` walk in a
`SEC("syscall")` program, triggered by one `bpf_prog_test_run`, with the delta landing in an
mmap'd ringbuf drained without a data syscall. That also folds the snapshot and the event
streams into one drain path and unifies births/changes with the existing ring.

**Why deferred — a library block, not a kernel one.** The kernel (task-iterator kfuncs,
open-coded iterators, `SEC("syscall")`) supports it. The pinned aya version does not parse a
`SEC("syscall")` program, and the kernel registers the task-iterator kfuncs only for program
types whose one `test_run`-able trigger may not legally call them — so there is no aya-loadable
path today. Revisit when aya gains syscall-program support (or when hand-rolling that one load
via the raw `bpf()` syscall is judged worth the loss of aya's CO-RE/map wiring).

Open question at implementation time: a forced full-snapshot resync must fit the ringbuf in one
synchronous walk (the program can't block mid-walk), so either size the ring for the worst-case
live set or chunk the resync across runs — a constraint the seq_file path does not have
(`read()` backpressures), and the reason emit-on-change shipped on seq_file first.

## Remaining: network I/O (the original motivation)

Per-process TCP/UDP throughput has **no unprivileged path** — this is what justified the BPF
layer in the first place.

- **Probes**: `fentry` on `tcp_sendmsg`/`tcp_recvmsg` and `udp_sendmsg`/`udp_recvmsg`
  (kprobe fallback on older kernels where `fentry` is unavailable). Each adds the byte count
  to a per-CPU BPF hashmap keyed by the sending task's tgid.
- **Read path**: drain the map once per cycle (batch lookup), zero-copy into per-PID counters,
  delta against the previous cycle like CPU ticks. Per-CPU values avoid contention on the hot
  path; userspace sums the per-CPU slots.
- **Plumbing**: a new column + a row field for tx/rx rate. The map and programs join the
  existing single BPF object; no new load/fallback machinery.
- **Fallback (unprivileged)**: socket count from an fd scan as a coarse "has network activity"
  indicator only — there is no per-PID byte path without BPF.

Open questions to resolve at implementation time:
- Keying by tgid in the probe needs the *sending* task's tgid; confirm it is reachable from
  the socket context in each hook (vs. needing the socket's owning task).
- Aggregation granularity: per-tgid is enough for the process view; per-socket/per-connection
  is a later refinement.

## Remaining: filesystem I/O

Dual path — both adequate, BPF is the bonus:
- **Privileged**: add the task's I/O-accounting fields (read/write bytes) to the iterator
  output — near-zero marginal cost, already walking the task. Gated on the kernel having task
  I/O accounting compiled in; absent ⇒ the field is simply zero.
- **Unprivileged**: `/proc/<pid>/io` with persistent fds and delta math like CPU ticks.
  Same-user is free; other users need `CAP_SYS_PTRACE`. Visibility-gated (only read for visible
  rows when the I/O column is shown).

These are independently valuable and independently testable; either can land first. Both add
only a column + a row field on top of the existing source split.

## Capabilities

The four caps the `caprun` wrapper already grants cover this work: `CAP_BPF` + `CAP_PERFMON`
(load/attach the network programs), `CAP_NET_ADMIN` (BPF network programs), `CAP_SYS_PTRACE`
(other users' `/proc/<pid>/io`). No new setup beyond `scripts/setup-caps.sh`.
