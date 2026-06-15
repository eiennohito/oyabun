# Project root justfile — unified build/lint/format/test commands.
# Install: pacman -S just

# Shared log directory — recursive `just` calls inherit via env
export ATOP_LOGDIR := env_var_or_default("ATOP_LOGDIR", "target/logs/" + `date +%Y%m%d-%H%M%S`)

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

# Run clippy on workspace
lint: _require-cargo
    @{{ _run }} lint cargo clippy --workspace --all-targets

# --- Check ---

# Compile-check workspace
check: _require-cargo
    @{{ _run }} check-rust cargo check --workspace --all-targets

# --- Test ---

# Run all tests
test: _require-cargo
    @{{ _run }} test-rust cargo test --workspace

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

# --- Pre-commit ---

# Pre-commit checklist: format, lint, check, test
precommit: _require-cargo
    @just fmt
    @just lint
    @just check
    @just test
    @echo ""
    @echo "Pre-commit done. Logs: $ATOP_LOGDIR/"

# --- Requirements (private) ---

[private]
[no-exit-message]
_require-cargo:
    @[ "{{ _has_cargo }}" = "true" ] || { echo "error: cargo not found — install from https://rustup.rs" >&2; exit 1; }
