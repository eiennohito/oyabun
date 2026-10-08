# oyabun

A fast process manager for the terminal.
Near-zero idle CPU, sub-millisecond input response, and observation cost that stays below O(processes) where the kernel allows it.

Linux only today (io_uring with a syscall fallback; an optional privileged eBPF source).
macOS is a goal, Windows is not.
For *why* the design is shaped this way, read `docs/GOALS.md` then `docs/ARCHITECTURE.md` — the architecture doc is the source of truth and should be read before the code.

## Dev environment

Prerequisites:

- A recent Rust toolchain via [rustup](https://rustup.rs) (the workspace is edition 2024).
- [`just`](https://github.com/casey/just) — the task runner. `just --list` shows every recipe; use `just`, not the raw `cargo` commands, so logging and lint flags stay consistent.
- A Linux kernel exposing BTF at `/sys/kernel/btf/vmlinux` if you want the privileged eBPF source (any modern distro kernel).

Build, run, and check:

```
just build          # debug build (embeds the committed BPF object; no clang needed)
just run            # release build + run
just test           # full workspace test suite
just lint           # clippy
just fmt            # rustfmt
just precommit      # fmt + lint + check + test, run before every commit
```

A plain build needs no BPF toolchain: the committed `bpf/oya.bpf.o` is embedded at build time, exactly like a generated artifact.
Run unprivileged and oyabun uses the `/proc` source; nothing else is required.

### Rebuilding the eBPF object (only when editing `bpf/*.c`)

The committed object is the dependency.
Regenerate it only after changing `bpf/oya.bpf.c` or `bpf/oya_types.h`, then commit the new `bpf/oya.bpf.o` alongside the source.

```
just bpf            # rebuild bpf/oya.bpf.o (needs clang + bpftool + libbpf headers)
just bpf-vmlinux    # regenerate bpf/vmlinux.h from the running kernel (rare; CO-RE makes one object portable)
```

`bpf/vmlinux.h` is a build-only input generated from your own kernel's BTF; it is not committed and not read at runtime.

### Privileged mode (the eBPF source)

The privileged source needs `CAP_BPF`, `CAP_PERFMON`, `CAP_NET_ADMIN`, and `CAP_SYS_PTRACE`.
For development, `tools/caprun` is a setuid-root wrapper that grants exactly those caps without an interactive sudo each run:

```
scripts/setup-caps.sh           # build + install the wrapper (one-time; needs sudo for chown/chmod)
scripts/setup-caps.sh status    # check it is installed
scripts/setup-caps.sh remove    # uninstall

tools/caprun target/release/oya        # run privileged
tools/caprun cargo test --workspace     # run the cap-gated BPF tests
```

Without caps, oyabun falls back to the `/proc` source automatically and the BPF integration tests skip.

## io_uring note

oyabun uses `io_uring` when available for faster `/proc` reads, falling back to
plain syscalls otherwise. The fallback is automatic and correct — no action
needed.

If you want the `io_uring` path (or notice it falling back when you don't
expect it), the most common cause is a low locked-memory limit. io_uring
shares a per-user locked-memory budget with other programs — Electron apps
(VS Code, Slack), `perf record`, and other io_uring users all draw from it.
The default 8 MiB limit is often not enough.

To raise it (requires re-login):

```
# /etc/security/limits.conf
*  soft  memlock  131072
*  hard  memlock  131072
```

Or for a systemd service: `LimitMEMLOCK=128M`.

## Security risks

This is a developer setup, not a hardened deployment.
Two parts of the privileged path deliberately trade safety for dev convenience — understand them before installing on any machine you do not fully control.

**`tools/caprun` is an unrestricted capability grant.**
It is setuid-root and grants its four capabilities to *whatever binary you hand it*, with the caller's environment and `PATH` intact.
Any local user who can run it can obtain `CAP_BPF` (load arbitrary kernel BPF), `CAP_SYS_PTRACE` (read/write other same-user processes' memory), and `CAP_NET_ADMIN` — via `caprun /bin/sh`, a `PATH`-shadowed target, or `LD_PRELOAD`.
An allowlist would not help: the wrapper exists precisely so a developer can run arbitrary build outputs with caps, and installing it already requires root.
Treat it as a **single-user dev-box convenience**.
Do not install it on shared or multi-user hosts, and `scripts/setup-caps.sh remove` it when you are done.

**The committed BPF object is loaded into your kernel.**
A normal build embeds `bpf/oya.bpf.o` and the privileged source loads it into the running kernel under the caps above.
Anyone who can modify the repository can substitute a malicious object that runs in kernel context, and a binary blob shows no meaningful review diff.
A checked-in digest would be security theater — the same actor regenerates object and digest together — so there is none.
If you do not trust the committed object, rebuild it yourself from `bpf/*.c` with `just bpf`, which needs only clang and bpftool.

**Privileged mode sees the whole system.**
Running oyabun with caps reads process metadata across all users.
That is the point of the mode, but it means a privileged oyabun is a system-wide observer; run it as such.
