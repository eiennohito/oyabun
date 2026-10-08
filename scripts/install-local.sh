#!/bin/sh
# Build and install oya from this checkout, using cached dependencies only.
# Requires the repository's Rust toolchain to be installed already.
#
#   scripts/install-local.sh               — ~/.local/bin/oya, /proc source only
#   scripts/install-local.sh --privileged  — /usr/local/bin/oya, root-owned with BPF caps
#
# Env: OYA_INSTALL_DIR overrides the target directory.
set -eu

CAPS="cap_bpf,cap_perfmon,cap_sys_ptrace+ep"

PRIVILEGED=0
if [ "$#" -gt 1 ]; then
  echo "Usage: $0 [--privileged]" >&2
  exit 1
fi
case "${1:-}" in
  "") ;;
  --privileged) PRIVILEGED=1 ;;
  *) echo "Usage: $0 [--privileged]" >&2; exit 1 ;;
esac

if [ "$PRIVILEGED" = 1 ]; then
  INSTALL_DIR="${OYA_INSTALL_DIR:-/usr/local/bin}"
else
  INSTALL_DIR="${OYA_INSTALL_DIR:-$HOME/.local/bin}"
fi

OS=$(uname -s)
[ "$OS" = Linux ] || { echo "Error: oyabun is Linux-only (got $OS)" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "Error: $1 not found${2:+ ($2)}" >&2; exit 1; }; }
need cargo
need install
if [ "$PRIVILEGED" = 1 ]; then
  need sudo
  SETCAP=$(command -v setcap || echo /usr/sbin/setcap)
  [ -x "$SETCAP" ] || { echo "Error: setcap not found (install libcap / libcap2-bin)" >&2; exit 1; }
fi

REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
trap 'exit 1' HUP INT TERM

echo "Building oya from $REPO_ROOT (offline)..."
# Stage Cargo's install separately so the final path is the same in both modes.
# Run from the checkout so rustup selects its pinned toolchain, even when invoked
# from another directory. Reuse the checkout's build cache by default.
(
  cd "$REPO_ROOT"
  export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
  cargo install --path crates/oyabun --locked --offline --root "$TMP"
)
BIN="$TMP/bin/oya"

if [ "$PRIVILEGED" = 1 ]; then
  sudo install -D -o root -g root -m 0755 "$BIN" "$INSTALL_DIR/oya"
  sudo "$SETCAP" "$CAPS" "$INSTALL_DIR/oya"
  echo "Installed $INSTALL_DIR/oya with $CAPS"
  echo "Note: every local user can now run the system-wide privileged view."
else
  mkdir -p "$INSTALL_DIR"
  install -m 0755 "$BIN" "$INSTALL_DIR/oya"
  echo "Installed $INSTALL_DIR/oya (unprivileged; rerun with --privileged for the BPF source)"
fi

case ":$PATH:" in
  *":$INSTALL_DIR:"*)
    FOUND=$(command -v oya || true)
    if [ -n "$FOUND" ] && [ "$FOUND" != "$INSTALL_DIR/oya" ]; then
      echo ""
      echo "Warning: $FOUND shadows $INSTALL_DIR/oya on your PATH."
    fi
    ;;
  *)
    echo ""
    echo "Add $INSTALL_DIR to your PATH:"
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac
