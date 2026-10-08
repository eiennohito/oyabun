# Project root justfile — unified build/lint/format/test commands.
# Install: pacman -S just

# Shared log directory — recursive `just` calls inherit via env
export OYA_LOGDIR := env_var_or_default("OYA_LOGDIR", "target/logs/" + `date +%Y%m%d-%H%M%S`)

# Tool detection
_has_cargo := `command -v cargo >/dev/null 2>&1 && echo true || echo false`

# Logging wrapper (also runs commands under nice -n 10)
_run := "scripts/run-logged.sh"

# List available recipes
[private]
default:
    @just --list

# --- Format ---

# Format all Rust code
fmt: _require-cargo
    @{{ _run }} fmt-rust cargo fmt --all

# --- Lint ---

# Run clippy on workspace; any warning fails (CI enforces the same)
lint: _require-cargo
    @{{ _run }} lint cargo clippy --workspace --all-targets -- -D warnings

# --- Check ---

# Compile-check workspace
check: _require-cargo _toolchain-nag
    @{{ _run }} check-rust cargo check --workspace --all-targets

# --- Test ---

# Run all tests (default features — privileged BPF layer compiled in)
test: _require-cargo
    @{{ _run }} test-rust cargo test --workspace

# Run oyabun's tests with the privileged layer compiled out (the "absent BPF" build). Only
# oyabun has the `bpf` feature, so etch/thoop are already covered by `test`.
test-nobpf: _require-cargo
    @{{ _run }} test-nobpf cargo test -p oyabun --no-default-features

# Run a filtered subset (e.g. `just testf thpmap -p thoop`); FILTER matches test names
testf FILTER *ARGS: _require-cargo
    @{{ _run }} test-filtered cargo test {{ ARGS }} -- {{ FILTER }}

# --- Build ---

# Build workspace in debug mode
build: _require-cargo
    @{{ _run }} build cargo build --workspace

# Build workspace in release mode
build-release: _require-cargo
    @{{ _run }} build-release cargo build --workspace --release

# Run the binary
run *ARGS: _require-cargo
    @cargo run --release -- {{ ARGS }}

# Build with profiling profile (frame pointers + full debug info)
build-profiling: _require-cargo
    @RUSTFLAGS="-C force-frame-pointers=yes" {{ _run }} build-profiling cargo build --workspace --profile profiling

# Run with profiling profile
run-profiling *ARGS: _require-cargo
    @RUSTFLAGS="-C force-frame-pointers=yes" cargo run --profile profiling -- {{ ARGS }}

# Run the BPF tests under caprun (verifier + runtime correctness). Skipped if caprun
# is not installed (setup: scripts/setup-caps.sh) — except in CI, where a skip would
# silently pass a broken BPF object.
test-bpf: _require-cargo
    @if [ -x tools/caprun ]; then \
        OYA_FORCE_BPF=1 {{ _run }} test-bpf tools/caprun cargo test -p oyabun --release -- bpf_; \
    elif [ -n "${CI:-}" ]; then \
        echo "test-bpf: tools/caprun not installed (CI must run scripts/setup-caps.sh)" >&2; exit 1; \
    else \
        echo "skipping test-bpf: tools/caprun not installed (run scripts/setup-caps.sh)"; \
    fi

# --- BPF (dev-only) ---

# Rebuild the committed BPF object (needs clang + libbpf headers). Run after editing
# bpf/*.c or bpf/oya_types.h, then commit the regenerated bpf/oya.bpf.o. A normal
# `cargo build` embeds the committed object and needs none of this toolchain.
bpf:
    @{{ _run }} bpf make -C bpf

# Regenerate bpf/vmlinux.h from the running kernel's BTF (needs bpftool). Only when
# retargeting a different kernel — the committed header works across kernels via CO-RE.
bpf-vmlinux:
    @{{ _run }} bpf-vmlinux make -C bpf vmlinux

# --- Pre-commit ---

# Pre-commit checklist: format, lint, check, then all three test configs in parallel
precommit: _require-cargo
    @just fmt
    @just lint
    @just check
    @just test & just test-nobpf & just test-bpf & wait
    @echo ""
    @echo "Pre-commit done. Logs: $OYA_LOGDIR/"

# --- Requirements (private) ---

# Warn (never fail) when the installed stable is newer than the rust-toolchain.toml pin:
# the pin keeps CI reproducible, but as an app we want to track stable, so bump it.
[private]
_toolchain-nag:
    @pin=$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml | cut -d. -f1,2); \
    local=$(rustup run stable rustc --version 2>/dev/null | cut -d' ' -f2 | cut -d. -f1,2); \
    if [ -n "$local" ] && [ "$local" != "$pin" ] && \
       [ "$(printf '%s\n%s\n' "$pin" "$local" | sort -V | tail -1)" = "$local" ]; then \
        echo "note: stable Rust $local is newer than the pinned $pin — bump rust-toolchain.toml and fix new clippy lints" >&2; \
    fi

[private]
[no-exit-message]
_require-cargo:
    @[ "{{ _has_cargo }}" = "true" ] || { echo "error: cargo not found — install from https://rustup.rs" >&2; exit 1; }
