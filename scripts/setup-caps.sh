#!/usr/bin/env bash
# Build and install the caprun setuid wrapper for running atop with
# BPF/tracing capabilities without sudo.
#
# One-time setup — requires sudo for chown+chmod only.
# After setup, `tools/caprun target/debug/atop` works without sudo.
#
# Usage:
#   scripts/setup-caps.sh           — build + install caprun
#   scripts/setup-caps.sh status    — check if caprun is installed and working
#   scripts/setup-caps.sh remove    — remove the setuid binary
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$REPO_ROOT/tools/caprun.c"
BIN="$REPO_ROOT/tools/caprun"

CAPS_HUMAN="CAP_BPF, CAP_PERFMON, CAP_NET_ADMIN, CAP_SYS_PTRACE"

check_binary() {
    if [ ! -f "$BIN" ]; then
        return 1
    fi
    # Check setuid root
    local owner perms
    owner=$(stat -c '%U' "$BIN" 2>/dev/null)
    perms=$(stat -c '%a' "$BIN" 2>/dev/null)
    if [ "$owner" = "root" ] && [[ "$perms" == 4* ]]; then
        return 0
    fi
    return 1
}

status() {
    if [ ! -f "$BIN" ]; then
        echo "caprun: not built"
        echo "  Run: $0"
        return
    fi

    local owner perms
    owner=$(stat -c '%U' "$BIN" 2>/dev/null || echo "?")
    perms=$(stat -c '%a' "$BIN" 2>/dev/null || echo "?")

    echo "caprun: $BIN"
    echo "  owner: $owner  mode: $perms"

    if check_binary; then
        echo "  → ready (setuid root)"
        echo "  Caps: $CAPS_HUMAN"
        echo ""
        echo "  Use: tools/caprun target/debug/atop"
        echo "       tools/caprun target/release/atop"
        echo "       tools/caprun target/profiling/atop"
    else
        echo "  → NOT ready (need setuid root)"
        echo "  Run: $0"
    fi
}

install() {
    echo "Building caprun..."
    cc -static -O2 -Wall -Wextra -Werror -o "$BIN" "$SRC"
    echo "  Built: $BIN"

    echo ""
    echo "Installing setuid root (needs sudo)..."
    sudo chown root "$BIN"
    sudo chmod 4755 "$BIN"

    echo ""
    if check_binary; then
        echo "Done. caprun is ready."
        echo "  Caps: $CAPS_HUMAN"
        echo ""
        echo "  Use: tools/caprun target/debug/atop"
        echo "       tools/caprun cargo test --workspace"
    else
        echo "ERROR: setuid install failed" >&2
        exit 1
    fi
}

remove() {
    if [ -f "$BIN" ]; then
        # May need sudo to remove a root-owned file
        rm -f "$BIN" 2>/dev/null || sudo rm -f "$BIN"
        echo "Removed: $BIN"
    else
        echo "caprun not found (already removed)"
    fi
}

case "${1:-install}" in
    install) install ;;
    status)  status ;;
    remove)  remove ;;
    *)
        echo "Usage: $0 {install|status|remove}" >&2
        exit 1
        ;;
esac
