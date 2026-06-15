# Spike: Touchable Prototype

Goal: a running TUI you can interact with. Intentionally scrappy — no arena, no io_uring, no double buffering. Prove the core loop works. A follow-up session refactors into the real architecture.

## What it does

- Lists all processes as a tree (parent → children)
- Shows: PID, user, state, CPU%, MEM%, command name
- Arrow keys to scroll, enter to collapse/expand subtrees
- `q` to quit, `k` to send SIGTERM to selected process
- Refreshes at ~2 Hz
- Handles 5000+ processes without visible lag

## What it doesn't do (deferred to refactor)

- Arena allocator / zero-copy I/O
- Double-buffered snapshots / ArcSwap
- io_uring backend
- macOS support
- Disk I/O column
- Follow mode
- Config / state cache files
- CEF-aware collapsing
- sudo escalation

## Dependencies

```toml
[dependencies]
ratatui = "0.29"
crossterm = "0.28"
```

No other deps. Parse `/proc` by hand — the procfs crate is heavy and does its own allocations.

## Structure

Single file to start (`main.rs`), split only if it exceeds ~500 lines. Three logical sections:

### 1. Process reading

```
fn read_processes() -> Vec<Process>
```

- Scan `/proc/` for numeric dirs (PIDs)
- For each PID, read `/proc/{pid}/stat` — parse pid, comm, state, ppid, utime, stime, vsize, rss
- Skip PIDs that vanish between readdir and open (ENOENT is normal, not an error)
- Compute CPU% from delta of (utime + stime) between two snapshots × 100 / elapsed ticks

```rust
struct Process {
    pid: u32,
    ppid: u32,
    name: String,
    state: char,
    cpu_pct: u32,       // basis points (hundredths of a percent)
    mem_bytes: u64,
    uid: u32,
}
```

### 2. Tree building

```
fn build_tree(procs: &[Process]) -> Vec<TreeRow>
```

- HashMap<pid, idx> for parent lookup
- Walk roots (ppid == 0 or parent not in map), DFS to produce a flat `Vec<TreeRow>` in display order
- Each TreeRow has depth (for indentation) and collapsed flag

```rust
struct TreeRow {
    proc_idx: usize,    // index into procs vec
    depth: u16,
    children_count: u32,
    collapsed: bool,
}
```

- Collapsed nodes skip their subtree in the display list
- `children_count` shown as `[+42]` when collapsed

### 3. TUI

Single-threaded event loop:

```
loop {
    // poll crossterm events with 500ms timeout
    // on timeout or no pending events: refresh snapshot, rebuild tree, redraw
    // on key event: handle navigation/actions, redraw
}
```

Ratatui rendering:
- Header row with column names
- Scrollable tree list — each row formatted as:
  `{indent}{collapse_marker} {pid:>7} {user:<8} {state} {cpu:>5} {mem:>8} {name}`
- Highlight selected row
- Footer with process count and keybinding hints

CPU% calculation:
- Keep previous snapshot's `(utime + stime)` per PID in a `HashMap<u32, u64>`
- Delta / elapsed_ticks × 10000 = basis points
- Read `CLK_TCK` once at startup via `sysconf(_SC_CLK_TCK)`
- Elapsed ticks = delta of `/proc/stat` first line's sum (total system ticks)

## Implementation order

1. **Process reading + print to stdout** — verify parsing is correct
2. **Tree building + print indented** — verify tree structure
3. **Ratatui scaffold** — blank TUI with event loop, quit on `q`
4. **Render process tree** — static snapshot displayed as tree
5. **Refresh loop** — re-read on timer, recompute CPU deltas
6. **Navigation** — scroll, select, collapse/expand
7. **Kill action** — `k` sends SIGTERM to selected PID

Each step is verifiable: run it, see output, confirm it works before moving on.

## Parsing /proc/pid/stat

The tricky part: `comm` field is wrapped in parens and can contain spaces, parens, and newlines.
Parse strategy: find first `(` and last `)` — everything between is comm. Fields after `)` are space-separated.

```
1 (bash) S 0 1 1 0 -1 4194560 ...
     ^         ^
     first (   last )
```

Fields after comm (0-indexed from after the closing paren):
- 0: state
- 1: ppid
- 11: utime
- 12: stime
- 20: vsize (bytes)
- 21: rss (pages, multiply by page_size)

Read page_size once at startup via `sysconf(_SC_PAGESIZE)`.

## Known shortcuts (ok for spike, fix in refactor session 2)

- `String` allocations per process per refresh → arena StringRef + zero-copy I/O
- HashMap for CPU delta tracking → generation-based side table
- Full rescan every tick → netlink delta + differential reads
- Single-threaded → gatherer thread + UI thread + ArcSwap
- Synchronous /proc reads → io_uring backend (with syscall fallback)
- No huge page buffers → mmap + MADV_HUGEPAGE arenas
- No error handling beyond skip-on-ENOENT

Session 2 scope: introduce io_uring backend, arena allocator with zero-copy I/O, double-buffered snapshots, two-thread model. The spike's data model (Process, TreeRow) becomes the index-based Snapshot/ProcessEntry/StringRef design from `docs/plans/architecture.md`.
