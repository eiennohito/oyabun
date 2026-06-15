# Architecture Plan

## Threading model

Two threads, no async runtime:
- **UI thread**: ratatui + crossterm event loop. Reads the current snapshot, renders, handles input.
- **Gatherer thread**: reads `/proc` (Linux) or `sysctl`/`libproc` (macOS), builds snapshot, swaps it in.

Snapshot exchange via `ArcSwap<Snapshot>` — lock-free, UI never blocks gatherer.
Gatherer runs on a timer when the terminal is visible, sleeps on `SIGTSTP`/background.

## Snapshot model

Double-buffered, arena-style, index-based (no pointer lifetimes):

```rust
struct Snapshot {
    procs: Vec<ProcessEntry>,
    strings: HugePageBuf,       // all raw /proc text — I/O target + string storage
    tree: Vec<TreeNode>,        // parent/child as indices into procs
    generation: u64,
}

struct ProcessEntry {
    pid: u32,
    ppid: u32,
    name: StringRef,            // offset + len into strings buf
    cmdline: StringRef,
    exe_path: StringRef,
    exe_inode: u64,
    // ... cpu, mem, state, etc.
}

struct StringRef {
    offset: u32,
    len: u32,
}

struct TreeNode {
    proc_idx: u32,
    parent_idx: u32,            // u32::MAX = root
    first_child_idx: u32,
    next_sibling_idx: u32,
}
```

"Reset" = clear vecs (O(1), capacity retained) + reset cursor on strings buf. Zero allocations per refresh cycle after warmup.

## Zero-copy I/O

The gatherer reads `/proc` data directly into the strings buffer. No intermediate allocations.

**Syscall backend**: `read(fd, &mut strings[cursor..cursor + MAX_SLOT_SIZE])` per file. Parse in-place, record StringRefs for string fields, parse numerics into ProcessEntry.

**io_uring backend**: each SQE targets `strings_ptr + cursor` as the buffer. Submit a batch of reads for all pids, wait for CQEs, parse in-place. Use `IORING_REGISTER_BUFFERS` to skip page-table walk.

Reserve a max-size slot per read, record actual length from return value. Internal fragmentation is irrelevant — buffer resets every cycle.

## Buffer allocation

Strings buffer: minimum 4 MB, `mmap`-backed with `MAP_ANONYMOUS | MAP_PRIVATE`, `madvise(MADV_HUGEPAGE)` for transparent huge pages. Reduces TLB misses during both I/O fill and UI scan.

On macOS: plain `mmap` without THP hint (kernel does its own superpage promotion).

The `HugePageBuf` type wraps this:
- `mmap` aligned allocation (2 MB boundary for THP eligibility)
- `madvise(MADV_HUGEPAGE)` on Linux
- Exposes `&mut [u8]` slice for I/O, tracks cursor position
- `reset()` = set cursor to 0, no munmap/mmap cycle
- Grows by doubling (re-mmap) if 4 MB isn't enough — but 4 MB fits ~5000 processes comfortably

## Double-buffer swap

Two `Snapshot` instances. `ArcSwap` holds the current (UI-visible) one. Gatherer:
1. Resets the back snapshot (clear vecs, reset strings cursor)
2. Fills it from `/proc`
3. Stores it into the `ArcSwap`
4. The old snapshot becomes the new back buffer (via `Arc` refcount drop)

UI calls `arc_swap.load()` — gets an `Arc<Snapshot>` that stays alive for the duration of the render frame.

## Configuration

Two files:
- **Config** (human-readable, e.g. TOML): keybindings, refresh rate, color scheme, column layout.
- **State cache** (semi-human-readable, e.g. JSON): collapse rules, column widths, sort order. Written by the app.

Collapse rule matching: check exe inode first (survives renames), fall back to exe path.

## Linux I/O backends

- **io_uring** (preferred, kernel 5.6+): batch `/proc` reads into submission queue. Probe at startup, fall back if unavailable.
- **Syscall** (fallback): direct `open`/`read`/`close` against `/proc`. Works everywhere including containers with restricted seccomp.

## macOS backend

`sysctl` (`CTL_KERN`, `KERN_PROC`) for process list, `libproc` (`proc_pidinfo`, `proc_pidpath`) for per-process details. Synchronous, no equivalent to io_uring batching.

## Privilege escalation

User-scoped actions execute directly. Root-scoped actions (kill protected processes, renice below 0) spawn `sudo` subprocesses. The UI indicates which actions require elevation. No persistent root — escalate per action.

## Open questions

- Tree diff between snapshots for efficient UI update (only redraw changed rows)?
- Should the gatherer pre-sort by the current sort column, or let the UI sort the index?
- Thread group (TGID) handling: show threads as children of the thread group leader, or separate toggle?
