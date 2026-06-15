# Security — project rules

## Attack surfaces specific to this app

- **Privilege escalation**: the app invokes `sudo` for root-scoped actions (kill, renice). Command injection in process signal paths is CRITICAL.
- **PID reuse races**: between reading a PID and acting on it, the process may have died and its PID reused. TOCTOU on PIDs is HIGH.
- **procfs parsing**: `/proc` data is untrusted input — malicious process names, symlink races, format changes across kernel versions.
- **Unbounded reads**: `/proc/*/stat`, `/proc/*/cmdline` can contain attacker-controlled data. Buffer overflows, format string issues.

## Deserialization

Parsing `/proc` files without validation before use in privilege decisions is HIGH.
