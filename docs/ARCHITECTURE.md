# Architecture

Status: **implemented** (Linux). macOS and several features below are still future work.

This describes how atop is actually built. For *why* (goals/constraints) see `GOALS.md`.

## Threading model

Two OS threads, no async runtime:

- **UI thread** (main): `etch` (retained renderer) + crossterm event loop. Loads the
  current snapshot, flattens the tree to display rows, renders, handles input.
- **Gatherer thread**: enumerates `/proc`, reads stat files, parses, computes CPU%,
  builds the tree, publishes the snapshot.

Exchange is via `arc_swap::ArcSwap<Snapshot>` — lock-free; the UI never blocks the
gatherer.

Control flows UI → gatherer over an `mpsc` channel (`Ctrl::{Refresh, Quit}`). The
gatherer blocks on `recv_timeout(interval)`: a timeout triggers a refresh, `Refresh`
forces an immediate one (sent right after a kill so the change shows fast), `Quit`
(or channel hangup) stops the thread. Blocking on the channel = near-zero idle CPU.

The UI wakes on input or a short poll timeout (`UI_POLL`, 200 ms) and only redraws
when something changed (a `dirty` flag set by input, resize, scroll, or a new
snapshot generation). Pure idle = a cheap `ArcSwap` load + generation compare.

### Double-buffer recycling (critical detail)

Exactly **two** `Snapshot`s exist steady-state; they ping-pong with zero allocation
after warmup. The gatherer keeps the just-swapped-out `Arc<Snapshot>` as `recycled`:

1. `Arc::get_mut` the recycled buffer to obtain unique `&mut` (succeeds once the UI
   has advanced to a newer generation and dropped the older `Arc`).
2. Reset + refill it.
3. `arc_swap.swap(new)` → returns the previous front; stash it as the next `recycled`.

The UI **must** use `load_full()` (a real `Arc`, bumping the strong count), not the
cheap `load()` guard — otherwise `get_mut`'s uniqueness check would race the UI's
read. If `get_mut` ever fails (UI still holding it — vanishingly rare given the
500 ms interval vs ms-scale UI turnaround), the gatherer **skips that cycle** rather
than allocate a third buffer (a third buffer would have no registered io_uring
buffer index — see below).

## Snapshot model

Index-based, POD, arena-backed — no pointers, no lifetimes, no per-process heap
allocation. `Vec::clear` drops nothing, so reset is O(1).

```
Snapshot { procs: Vec<ProcessEntry>, strings: HugePageBuf, first_root: u32,
           generation: u64, buf_index: u16, sys: SystemStats }

SystemStats {                        // Copy, per-cycle system-wide stats
  cpu_user_bp, cpu_sys_bp, cpu_iowait_bp,   // CPU deltas as basis points
  mem_total, mem_used, mem_cached,           // bytes
  swap_total, swap_used,                     // bytes
  load: [u32; 3], uptime_secs, num_cores,
  tasks_running, tasks_sleeping, tasks_stopped, tasks_zombie, tasks_idle
}

ProcessEntry {                       // Copy
  pid, ppid, uid, state(u8), priority(i8), nice(i8), num_threads,
  cpu_pct(bp), cpu_peak(bp), mem_bytes, ticks,
  start_time,                        // (pid, start_time) = a unique incarnation
  name: StringRef,                   // offset+len into `strings` (comm)
  cmdline: StringRef,                // /proc/<pid>/cmdline (NUL→space), may be empty
  non_ascii: bool,                   // comm|cmdline has a byte ≥0x80 → renderer unicode path
  parent_idx, first_child, next_sibling, subtree_size, depth,  // tree links
  subtree_cpu, subtree_mem           // inclusive aggregates for collapsed display
}
StringRef { offset: u32, len: u32 }  // slice of `strings`
```

`NONE = u32::MAX` is the null index/root sentinel. PIDs vanishing mid-scan are kept
as tombstones (`pid == 0`) and `compact()`ed out before publish; because PIDs are
enumerated sorted and filled by index, `procs` stays sorted by PID (the tree build's
binary-search precondition).

## Arena (`HugePageBuf`)

`mmap(MAP_ANONYMOUS|MAP_PRIVATE)` rounded to 2 MiB, `madvise(MADV_HUGEPAGE)` to cut
TLB misses during both I/O fill and UI scan. A bump cursor; `reset()` rewinds it.
`reserve()` grows by doubling (re-mmap + copy) and reports whether the base moved so
the io_uring buffer registration can be refreshed. It is **both** the I/O target and
the string store: the gatherer reads `/proc` text straight into it and `comm` is
recorded as a `StringRef` pointing into that raw text — genuinely zero-copy. Raw stat
text persists for the snapshot's life (names point into it); ~2 KiB/PID, so ~10 MiB
at 5000 PIDs. `unsafe impl Send + Sync` is sound because mutation only happens under
`get_mut` (unique access) and shared access is read-only.

## `/proc` enumeration

`getdents64` directly into a reused buffer, parsing dirent records by hand — no
per-entry `String`/`PathBuf` (unlike `fs::read_dir`). The `/proc` dir fd is opened
once and `lseek(0)`'d per scan.

## Zero-copy stat parse

`parse_stat` works on `&[u8]` from the arena: first `(` … last `)` delimits `comm`
(handles spaces/parens); numeric fields are parsed byte-wise (no UTF-8 validation,
overflow-checked). Field indices after `) `: 0 state, 1 ppid, 11 utime, 12 stime,
21 rss(pages).

## Linux I/O backends

A `Backend` enum (`Uring | Syscall`), probed at startup; on any io_uring error
mid-run the gatherer permanently downgrades to syscall and redoes the cycle. The
**syscall backend** is also the correctness oracle in tests
(`uring_matches_syscall_backend`).

### io_uring backend

Per PID, two linked chains + an independent statx (7 SQEs, 7 CQEs), identified by
`user_data = (slot << 2) | op`:

```
OpenAt(/stat, fd=2s)    -[IO_LINK]-> ReadFixed(→ arena) -[IO_HARDLINK]-> Close(fd=2s)
OpenAt(/cmdline, fd=2s+1) -[IO_LINK]-> ReadFixed(→ arena) -[IO_HARDLINK]-> Close(fd=2s+1)
Statx(→ uid)
```

- **Direct descriptors**: `OpenAt` installs into a registered fixed-file slot
  (`register_files_sparse(n_slots * 2)`), so `ReadFixed`/`Close` can target it
  within the same linked submission without learning the fd at runtime. Two fd slots
  per PID (stat + cmdline).
- **Registered buffers**: both snapshot arenas are registered (`register_buffers`)
  at stable indices 0/1 = `Snapshot::buf_index`; `ReadFixed` skips the per-read
  page-table walk. Re-registered only when an arena grows.
- **`IO_LINK` on open**: if the process vanished before open, the chain cancels
  (read/close return `ECANCELED`) — handled, PID left as tombstone.
- **`IO_HARDLINK` on read**: guarantees `Close` runs even if the read errors after a
  successful open, so an installed direct descriptor is never leaked.
- **Two arena slots per PID**: stat text + cmdline text, each `SLOT_SIZE` (2 KiB).
  Cmdline bytes are processed in-place after the read (NUL → space) and stored
  as a `StringRef`.
- **Bounded in-flight + slot free-list** (not fixed waves): keep filling the SQ while
  slots are free, then reap completed PIDs and parse them — so the kernel reads the
  next PIDs *while the CPU parses the last ones* (continuous I/O↔parse overlap). A PID
  is parsed once all seven of its CQEs land; its slot returns to the free-list after
  both `Close` ops. uid comes from the statx result (`stx_uid`).

`N_SLOTS=512` concurrent PIDs, `RING_ENTRIES=4096` (7 SQEs/PID × 512 = 3584, fits).
`submit_and_wait` retries on `EINTR`.

## Tree build

`procs` sorted by PID ⇒ parent lookup is a binary search; child lists are built by
reverse-index prepend (yields PID-ascending siblings), no `HashMap`/per-node `Vec`.
One pre-order pass assigns `depth` and records order; its reverse accumulates
`subtree_size`. Scratch (`stack`, `order`) is gatherer-owned and reused. Iterative
(no recursion) — safe for pathologically deep trees.

A second reverse-order pass (`tree::aggregate`) computes inclusive `subtree_cpu` and
`subtree_mem` (self + all descendants) for collapsed-group display — same O(n) as the
size accumulation, using the existing `order` vector.

The structural tree is collapse-independent. The **UI** flattens it to display rows
(`app::rebuild_rows`), skipping collapsed subtrees, only when the snapshot generation
or the collapse set changes. Selection follows the selected PID across refreshes.

## Rendering (`etch`)

A separate workspace crate, `crates/etch/` — a **retained-mode, value-gated** terminal
renderer with no atop domain types. It replaced ratatui, whose `Paragraph`/
`LineTruncator` grapheme segmentation (≈43% of CPU) and blind 10k-cell `Buffer::diff`
(≈22%) dominated profiles. Work is now proportional to what *changed*, not to screen
size.

Two-level API: **structure declared once, values bound per frame.**

- A `Schema` of columns (`ColSpec::{right,left,fill}`) owns geometry — leading
  separator + body width + alignment, with fixed-column x-offsets precomputed once. It
  drives both the header and every body row (one source of column truth).
- Per frame, `Display::begin_frame` → `Frame` exposes `line()` (free-form styled spans
  for the stat header/footer), `header()` (titles from the schema), and `table()`.
  Inside `table`, each `row(id, style, …)` binds columns: `r.field(value)` and
  `r.fill(gate, closure)`.

**The gate is the bound value itself.** `field<T: Display + Hash>(v)` hashes `v`
(fast `FxHash`) and compares to the value that produced the cell's last output. On a
match the `Display` impl is *never invoked* — zero formatting, zero output. Because the
formatted value and the gated value are the same `T`, the gate can never drift from
what's shown. The `fill` column (Command) takes an explicit gate + a closure that runs
only on a miss; its gate is a **content hash of the cmdline bytes + tree prefix**, not
the arena `StringRef` (arena offsets are not stable across snapshots, so the ref would
mismatch every cycle).

Per-row identity is the PID: same PID at the same screen line ⇒ per-cell gating; a
different PID (scroll happened) ⇒ the whole row repaints. A style change (selection
move, state-color change) also forces the row. Unoccupied rows below the table are
blanked. A terminal-size change (or the first frame) clears the screen and repaints
everything. All output is batched into one buffer and flushed once per `commit()`.

ASCII is the fast path: 1 byte = 1 column, no grapheme work. The `Cell` writer tracks
display width explicitly — `glyph(s, w)` for known-width tree connectors (`●├─│▾`),
`ascii` for the common cmdline, and `unicode` (via `unicode-width`) only when
`ProcessEntry::non_ascii` is set (the gatherer flags this for free during the byte
walks that already scan comm/cmdline). Integration tests (`vt100`) assert both the
rendered screen and that an unchanged frame emits **zero** bytes.

Not yet done (deliberately deferred, measured first): terminal **scroll regions**
(`CSI S`/`T`) so a ±1 scroll shifts the terminal's own buffer instead of repainting the
visible window. The value-gated renderer already removed both ratatui hot spots; scroll
regions are a pure optimization for the held-arrow case.

## CPU%

Per-process CPU is a **per-core rate over real elapsed time** (`top`/`htop` "Irix
mode"): one interval's rate = `Δ(utime+stime) × 10000 / (Δwall · CLK_TCK)` basis
points, so one fully-used core = 10000 bp (100%) and a multi-threaded process can
exceed 100%. The window is a monotonic `Instant` (a jittering gather interval
self-corrects); it does **not** read `/proc/stat`. Dividing instead by the
`/proc/stat` machine-wide tick sum (an earlier approach) yielded `1/num_cpus` of the
truth — a 32× undercount on a 32-core box.

Because correct per-core values magnify the inherent ±1-jiffy quantization (±2% over
a 500 ms window at 100 Hz), each PID keeps a **bounded ring of the recent sample
window** (`CpuHistory`) with exact `u64` running sums. The window is a wall-clock
duration — `CPU_WINDOW_MS` (default 10 s, the controllable knob) — and the per-PID
ring depth is derived as `CPU_WINDOW_MS ÷ REFRESH_MS`:
- **`cpu_pct`** = the sum-weighted moving **average** (`Σticks / Σjiffies`) — the
  stable reading. Reaches a new sustained level over ≤ `CPU_WINDOW` intervals.
- **`cpu_peak`** = the max single-interval rate still in the window — **captures
  spikes** the average dilutes, and holds them for `CPU_WINDOW` intervals. Shown as
  a separate `PEAK` column. Tracked incrementally: a cached `peak_bp` + ring index
  updates on push and rescans only when the peak sample is evicted (~1/window).

All arithmetic is exact integer math (no float, no EWMA accumulation error).
Robustness: refreshes closer than `MIN_SAMPLE` (100 ms — e.g. the forced refresh
after a kill) carry the windowed values forward instead of dividing by a near-zero
window; a PID whose tick counter goes backwards (reuse/wrap) resets its history;
PIDs absent from a cycle are evicted via a generation tag. State is a persistent
`HashMap<pid, CpuHistory>` updated in place — zero allocation after warmup — keyed
with a small hand-rolled `FxHash`-style hasher (`FxBuildHasher`), since the default
`SipHash` is hash-flood-resistant but slow and PID keys aren't attacker-controlled.

## Dependencies

- `io-uring` 0.7 — thin, runtime-free wrapper over the raw ring (hand-rolling the
  ring setup is hundreds of lines of memory-ordering-critical unsafe).
- `arc-swap` 1 — the lock-free snapshot cell.
- `etch` (workspace path crate) — the retained-mode renderer (see above). Depends on
  `crossterm` (escape generation) and `unicode-width` (opt-in wide-char measurement);
  dev-dep `vt100` for terminal-emulator-based render tests.
- `crossterm` 0.28 — terminal setup (raw mode, alt screen) + input events; also etch's
  output backend.
- `libc` — syscalls. No `procfs` crate (it allocates and parses more than we need).

## Deviations from the original plan & known edges

- **Bounded in-flight** replaced the planned fixed io_uring "waves" — more overlap,
  self-balancing to the kernel/CPU ratio.
- **`IO_HARDLINK` close** replaced plain `IO_LINK` to close the fd-leak-on-read-error
  hole. Residual edge: if a `Close` itself ever fails the slot stays occupied for the
  rest of the cycle (slot reset each cycle bounds it); astronomically rare.
- **CpuTracker** is a persistent `HashMap`, not the plan's vague "generation side
  table" (a flat PID-indexed array would be ~32 MB).
- **Selection-follows-PID** across refreshes is implemented; full follow-mode
  auto-scroll is not.
- **Kill is race-safe**: `sys::kill_verified` pins the target with a `pidfd`, re-checks
  `(pid, start_time)` against the snapshot, then signals through the pidfd — a reused
  PID is never hit. (`sudo` escalation for protected processes is still future.)

## Not yet implemented (see GOALS.md)

macOS (`sysctl`/`libproc`) backend; thread-group (TGID) handling; CEF/Chromium-aware
collapse + persisted collapse rules; config + state-cache files; `sudo` escalation
for protected processes; disk-I/O (`/proc/pid/io`) and other columns (exe path);
per-core CPU bars (toggle); netlink/delta enumeration instead of full rescan;
gatherer pause on `SIGTSTP`/background.
