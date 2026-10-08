#!/usr/bin/env bash
# Toggle kernel profiling sysctls for perf.
# Requires root (sudo). Stores previous values to restore on disable.
#
# Usage:
#   scripts/perf-toggle.sh enable   — allow kernel symbols in perf
#   scripts/perf-toggle.sh disable  — restore original restrictive values
#   scripts/perf-toggle.sh status   — show current settings
set -euo pipefail

SAVE_FILE="/tmp/oya-perf-paranoid-saved"

# Knobs we touch:
#   perf_event_paranoid  -1 = allow everything incl. kernel tracing
#   kptr_restrict         0 = expose kernel symbol addresses in /proc/kallsyms
PARANOID_PATH="/proc/sys/kernel/perf_event_paranoid"
KPTR_PATH="/proc/sys/kernel/kptr_restrict"

read_current() {
    local paranoid kptr
    paranoid=$(cat "$PARANOID_PATH")
    kptr=$(cat "$KPTR_PATH")
    echo "$paranoid" "$kptr"
}

status() {
    local vals
    vals=$(read_current)
    local paranoid=${vals%% *}
    local kptr=${vals##* }

    echo "perf_event_paranoid = $paranoid  (need -1 for kernel profiling)"
    echo "kptr_restrict       = $kptr  (need 0 for kernel symbol addresses)"

    if [ "$paranoid" -le 0 ] && [ "$kptr" -eq 0 ]; then
        echo "→ kernel profiling is ENABLED"
    else
        echo "→ kernel profiling is RESTRICTED"
    fi

    if [ -f "$SAVE_FILE" ]; then
        echo "  (saved original values in $SAVE_FILE)"
    fi
}

enable() {
    local vals
    vals=$(read_current)
    local paranoid=${vals%% *}
    local kptr=${vals##* }

    if [ "$paranoid" -le 0 ] && [ "$kptr" -eq 0 ]; then
        echo "Already enabled (paranoid=$paranoid, kptr_restrict=$kptr)"
        return 0
    fi

    # Save current values for restore
    echo "$paranoid $kptr" > "$SAVE_FILE"

    sudo sysctl -w kernel.perf_event_paranoid=-1 kernel.kptr_restrict=0

    # perf record pins ~4 MiB of ring buffers (128 KiB × ncpus); oyabun's io_uring
    # registers ~4 MiB of arena buffers. Default memlock (8 MiB) is too tight for both.
    # 64 MiB is generous without being reckless.
    local cur_memlock need_kib=65536
    cur_memlock=$(ulimit -l)
    if [ "$cur_memlock" != "unlimited" ] && [ "$cur_memlock" -lt "$need_kib" ]; then
        local limits_file="/etc/security/limits.d/90-memlock.conf"
        echo ""
        echo "memlock is ${cur_memlock} KiB — too low for perf + io_uring together."
        echo "Raising to ${need_kib} KiB via ${limits_file} ..."
        printf '%s hard memlock %s\n%s soft memlock %s\n' \
            "$USER" "$need_kib" "$USER" "$need_kib" | sudo tee "$limits_file" > /dev/null
        echo "Done. Log out and back in (or start a new login shell) for it to take effect."
    fi
    echo ""
    echo "Kernel profiling enabled. Run: perf record -g --call-graph fp -p \$PID"
    echo "Restore with: $0 disable"
}

disable() {
    local paranoid=2
    local kptr=1

    if [ -f "$SAVE_FILE" ]; then
        local saved
        saved=$(cat "$SAVE_FILE")
        paranoid=${saved%% *}
        kptr=${saved##* }
        rm "$SAVE_FILE"
        echo "Restoring saved values: paranoid=$paranoid, kptr_restrict=$kptr"
    else
        echo "No saved values found, restoring defaults: paranoid=$paranoid, kptr_restrict=$kptr"
    fi

    sudo sysctl -w kernel.perf_event_paranoid="$paranoid" kernel.kptr_restrict="$kptr"
    echo ""
    echo "Kernel profiling disabled."
}

case "${1:-status}" in
    enable)  enable ;;
    disable) disable ;;
    status)  status ;;
    *)
        echo "Usage: $0 {enable|disable|status}" >&2
        exit 1
        ;;
esac
