#!/bin/sh
# Install oya (the oyabun binary) from the latest GitHub release.
#
#   install.sh               — unprivileged: ~/.local/bin/oya, /proc source only
#   install.sh --privileged  — /usr/local/bin/oya, root-owned, with file capabilities for the
#                              BPF source (needs sudo + setcap)
#
# Env: OYA_VERSION (tag, default latest), OYA_INSTALL_DIR (overrides the target directory).
set -eu

REPO="eiennohito/oyabun"
# The privileged source's needs: load BPF (CAP_BPF), attach tracing programs (CAP_PERFMON),
# read other users' /proc/<pid>/smaps_rollup and exe links (CAP_SYS_PTRACE). CAP_NET_ADMIN
# (granted by the dev wrapper tools/caprun) joins once netlink I/O monitoring lands.
CAPS="cap_bpf,cap_perfmon,cap_sys_ptrace+ep"

PRIVILEGED=0
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
ARCH=$(uname -m)
[ "$OS" = Linux ] || { echo "Error: oyabun is Linux-only (got $OS)" >&2; exit 1; }
case "$ARCH" in
  x86_64|amd64)  ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) echo "Error: unsupported architecture: $ARCH" >&2; exit 1 ;;
esac
NAME="oya-linux-$ARCH"

need() { command -v "$1" >/dev/null 2>&1 || { echo "Error: $1 not found${2:+ ($2)}" >&2; exit 1; }; }
need curl
need sha256sum
if [ "$PRIVILEGED" = 1 ]; then
  need sudo
  # setcap usually lives in /usr/sbin, which may be off a normal user's PATH.
  SETCAP=$(command -v setcap || echo /usr/sbin/setcap)
  [ -x "$SETCAP" ] || { echo "Error: setcap not found (install libcap / libcap2-bin)" >&2; exit 1; }
fi

VERSION="${OYA_VERSION:-latest}"
if [ "$VERSION" = latest ]; then
  BASE="https://github.com/$REPO/releases/latest/download"
else
  BASE="https://github.com/$REPO/releases/download/$VERSION"
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "Downloading oya ($VERSION, $ARCH)..."
curl -fsSL "$BASE/$NAME.tar.gz" -o "$TMP/$NAME.tar.gz"
curl -fsSL "$BASE/SHA256SUMS" -o "$TMP/SHA256SUMS"
(cd "$TMP" && grep " $NAME.tar.gz\$" SHA256SUMS | sha256sum -c --quiet -) \
  || { echo "Error: checksum mismatch for $NAME.tar.gz" >&2; exit 1; }
tar xzf "$TMP/$NAME.tar.gz" -C "$TMP"
BIN="$TMP/$NAME/oya"

if [ "$PRIVILEGED" = 1 ]; then
  # Root-owned so the user cannot swap in another binary under the granted caps. The kernel
  # treats a file-capability binary as secure-exec: LD_PRELOAD and friends are ignored.
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
