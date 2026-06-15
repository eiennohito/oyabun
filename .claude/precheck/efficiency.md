# Efficiency — project rules

## Resource priority (system monitor)

The app runs continuously in the background. Priorities in order:

1. **CPU at idle** — the #1 constraint. htop is considered heavy. Unnecessary wakeups, polling faster than needed, redundant procfs reads are CRITICAL when systematic.
2. **Memory** — process list can be large (thousands of entries). Unbounded growth, per-refresh allocations, retaining stale process data.
3. **Syscall count** — every `/proc` read is a syscall. Batch reads, cache where valid within a refresh cycle, avoid redundant stat/open calls.
4. **Render cost** — only redraw changed regions. Full redraws on every tick are HIGH.

## Anti-patterns

- Polling `/proc` faster than the display refresh rate.
- Allocating on every refresh cycle where a reusable buffer suffices.
- O(n²) process tree construction (sort + linear parent lookup instead of hash map).
- Parsing the same `/proc/pid/stat` file multiple times per refresh.
- Holding locks across I/O (procfs reads).
