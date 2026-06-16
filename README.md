# atop

A fast process manager for the terminal. Linux, eventually macOS.

## Building

```
cargo build --release
```

## io_uring note

atop uses `io_uring` when available for faster `/proc` reads, falling back to
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
