#!/usr/bin/env bash
# Logging wrapper for justfile recipes.
# Runs a command, captures full output to a log file,
# prints a one-line ✓/✗ plus tool-aware compact summary.
#
# Usage: run-logged.sh <name> [--cd <dir>] <cmd...>
# Requires: ATOP_LOGDIR env var (set by justfile)
set -uo pipefail

name="$1"; shift

dir="."
if [ "${1:-}" = "--cd" ]; then
    shift; dir="$1"; shift
fi

logdir="${ATOP_LOGDIR:?ATOP_LOGDIR not set}"
mkdir -p "$logdir"
log="$logdir/$name.log"

# Once per invocation: print header, prune old log dirs (keep 10)
if [ ! -f "$logdir/.header" ]; then
    echo "$logdir/"
    touch "$logdir/.header"
    ls -dt target/logs/[0-9]* 2>/dev/null | tail -n +51 | xargs rm -rf --
fi

# Detect tool for summary extraction
cmd_base=$(basename "$1")
subcmd="${2:-}"

# Run with full output to log
(cd "$dir" && nice -n 10 "$@") > "$log" 2>&1
code=$?

# Extract compact summary (tool-dependent)
summary=""
test_info=""
case "$cmd_base" in
    cargo)
        if [ "$subcmd" = "test" ]; then
            summary=$(grep -E '(^test .+ FAILED$|^test result: FAILED)' "$log" | head -5)
            test_info=$(awk '/^test result:/ {
                split($0, a, /[;,]/)
                for (i in a) {
                    if (a[i] ~ /[0-9]+ passed/) { gsub(/[^0-9]/, "", a[i]); passed += a[i] }
                    if (a[i] ~ /[0-9]+ failed/) { gsub(/[^0-9]/, "", a[i]); failed += a[i] }
                }
            } END {
                if (passed + failed > 0) {
                    printf "%d passed", passed
                    if (failed > 0) printf ", %d failed", failed
                }
            }' "$log")
        else
            summary=$(awk '/^(warning|error)(\[|: )/{
                msg=$0; getline
                if (/^ *-->/) print msg "  " $0; else print msg
            }' "$log" \
                | grep -vE '(generated [0-9]|could not compile|build failed|aborting due to)' \
                | head -5)
        fi
        ;;
esac

# Fallback: on failure with no tool-specific summary, show tail
if [ $code -ne 0 ] && [ -z "$summary" ]; then
    summary=$(tail -5 "$log")
fi

# Display: ✓/✗ + log filename + test count
if [ $code -eq 0 ]; then
    if [ -n "$test_info" ]; then
        printf "  \033[32m✓\033[0m %s.log  (%s)\n" "$name" "$test_info"
    else
        printf "  \033[32m✓\033[0m %s.log\n" "$name"
    fi
else
    if [ -n "$test_info" ]; then
        printf "  \033[31m✗\033[0m %s.log  (%s)\n" "$name" "$test_info"
    else
        printf "  \033[31m✗\033[0m %s.log\n" "$name"
    fi
fi

if [ -n "$summary" ]; then
    echo "$summary" | sed 's/^/    /'
fi

if [ $code -ne 0 ]; then
    exit $code
fi
