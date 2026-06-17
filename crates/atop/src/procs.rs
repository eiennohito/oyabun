//! The live process buffer the gatherer fills and the renderer reads directly.
//!
//! Single-thread policy: there is no published immutable snapshot and no double buffer.
//! [`Procs`] is one arena-resident [`TypedBuf`] of [`ProcessEntry`] rows, reset and refilled
//! each cycle and read in place by render — the borrow checker proves `gather(&mut)` and
//! `render(&)` never overlap. Tree links are indices into the buffer. `comm` (the process
//! name) is re-parsed from `stat` every cycle, so it lives **inline** in the row as fixed
//! bytes; only `cmdline` (slow-changing) earns a generational `Cmd`-store slot, read directly
//! through the store (no per-snapshot lease).

use thoop::{Arena, StringRef, TypedBuf};

/// Sentinel index meaning "none" for tree links and roots.
pub const NONE: u32 = u32::MAX;

/// `comm` capacity in a row: `TASK_COMM_LEN` (16) minus the kernel's NUL ⇒ ≤ 15 printable
/// bytes. A name read from `stat` is already bounded by this; we truncate defensively.
pub const COMM_CAP: usize = 15;

/// Tag type for the cmdline string store: makes a [`StringRef<Cmd>`] resolvable only against
/// the `Cmd` store, never another store.
pub struct Cmd;

/// System-wide resource snapshot, computed once per gather cycle.
#[derive(Clone, Copy, Default, Hash)]
pub struct SystemStats {
    /// CPU user+nice as fraction of total, basis points (10000 = 100%).
    pub cpu_user_bp: u32,
    /// CPU system+irq+softirq, basis points.
    pub cpu_sys_bp: u32,
    /// CPU iowait, basis points.
    pub cpu_iowait_bp: u32,
    pub mem_total: u64,
    /// `total - available` (what's actively used by processes).
    pub mem_used: u64,
    /// Buffers + Cached (reclaimable page cache).
    pub mem_cached: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    /// Load averages × 100 (e.g. 215 = 2.15). Index 0/1/2 = 1/5/15 min.
    pub load: [u32; 3],
    pub uptime_secs: u64,
    pub num_cores: u32,
    pub tasks_running: u32,
    pub tasks_sleeping: u32,
    pub tasks_stopped: u32,
    pub tasks_zombie: u32,
    pub tasks_idle: u32,
}

impl SystemStats {
    /// Fold per-state tallies from [`Procs::count_tasks`] into the `tasks_*` fields.
    pub fn set_task_counts(&mut self, c: TaskCounts) {
        self.tasks_running = c.running;
        self.tasks_sleeping = c.sleeping;
        self.tasks_stopped = c.stopped;
        self.tasks_zombie = c.zombie;
        self.tasks_idle = c.idle;
    }
}

/// Per-process record. POD (`Flat`) — lives in a [`TypedBuf`] on huge pages; `clear` drops
/// nothing, enabling zero-alloc reuse each cycle.
#[derive(Clone, Copy)]
pub struct ProcessEntry {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    /// Process state char (`R`, `S`, `Z`, …) as a raw byte.
    pub state: u8,
    /// Kernel scheduling priority (lower = higher priority). Normal: 20+nice.
    pub priority: i8,
    /// Nice value (−20 … 19). User-controllable scheduling hint.
    pub nice: i8,
    /// Number of valid bytes in [`comm`](Self::comm_bytes).
    pub comm_len: u8,
    /// Thread count (`num_threads` from `/proc/<pid>/stat`).
    pub num_threads: u32,
    /// Moving-average CPU% in basis points (hundredths of a percent); 10000 = one
    /// full core. Stable reading over the recent sample window.
    pub cpu_pct: u32,
    /// Peak single-interval CPU% (basis points) still within the sample window —
    /// captures spikes the average damps out.
    pub cpu_peak: u32,
    /// Resident memory in bytes.
    pub mem_bytes: u64,
    /// Raw cumulative `utime + stime` (jiffies) — input to the CPU% delta.
    pub ticks: u64,
    /// Process start time (jiffies since boot). With `pid`, identifies a unique
    /// process incarnation — used to make `kill` safe against PID reuse.
    pub start_time: u64,
    /// `comm` (process name) inline. Re-parsed from `stat` every cycle (a store would be
    /// pure overhead), so the first [`comm_len`](Self::comm_len) bytes hold the name; read
    /// via [`comm`](Self::comm).
    pub comm_bytes: [u8; COMM_CAP],
    /// Full `/proc/<pid>/cmdline` (NUL→space) as a handle into the generational `Cmd` store.
    /// Empty for kernel threads and inaccessible processes. The handle is stable across
    /// cycles while the cmdline is unchanged (no per-cycle re-copy).
    pub cmdline: StringRef<Cmd>,
    /// Set if `comm` or `cmdline` contains any byte ≥ 0x80 — i.e. the Command column needs
    /// unicode-aware width. False for the ~99% ASCII case (renderer fast path).
    pub non_ascii: bool,
    /// `PF_KTHREAD` set in stat `flags` — a kernel thread. Its `/proc/<pid>/cmdline`
    /// is permanently empty, so the gatherer never reads it (parsed free from stat).
    pub is_kthread: bool,

    // --- tree links (filled by `tree::build`) ---
    /// Index of parent in the buffer, or [`NONE`] for roots.
    pub parent_idx: u32,
    pub first_child: u32,
    pub next_sibling: u32,
    /// Total descendants (shown as `[+N]` when collapsed).
    pub subtree_size: u32,
    /// Distance from a root (0 = root).
    pub depth: u16,

    // --- subtree aggregates (filled by `tree::aggregate`) ---
    /// `cpu_pct` of self + all descendants (for collapsed-group display).
    pub subtree_cpu: u32,
    /// `mem_bytes` of self + all descendants.
    pub subtree_mem: u64,
}

impl ProcessEntry {
    /// A tombstone for a PID that vanished mid-scan; compacted out before render.
    pub const TOMBSTONE: ProcessEntry = ProcessEntry {
        pid: 0,
        ppid: 0,
        uid: 0,
        state: b'?',
        priority: 0,
        nice: 0,
        comm_len: 0,
        num_threads: 0,
        cpu_pct: 0,
        cpu_peak: 0,
        mem_bytes: 0,
        ticks: 0,
        start_time: 0,
        comm_bytes: [0; COMM_CAP],
        cmdline: StringRef::EMPTY,
        non_ascii: false,
        is_kthread: false,
        parent_idx: NONE,
        first_child: NONE,
        next_sibling: NONE,
        subtree_size: 0,
        depth: 0,
        subtree_cpu: 0,
        subtree_mem: 0,
    };

    /// The `comm` (process name) bytes.
    #[must_use]
    pub fn comm(&self) -> &[u8] {
        &self.comm_bytes[..self.comm_len as usize]
    }

    /// Copy `comm` in from a parsed `stat` slice, truncating to [`COMM_CAP`].
    #[allow(clippy::cast_possible_truncation)] // n ≤ COMM_CAP (15), fits u8
    pub fn set_comm(&mut self, comm: &[u8]) {
        let n = comm.len().min(COMM_CAP);
        self.comm_bytes[..n].copy_from_slice(&comm[..n]);
        self.comm_len = n as u8;
    }

    /// A slot is a tombstone (read failed / PID vanished) iff `pid == 0`. Linux never
    /// exposes PID 0 (the scheduler) as a numeric `/proc` entry, so 0 is unambiguous.
    #[must_use]
    pub fn is_tombstone(&self) -> bool {
        self.pid == 0
    }
}

/// The cycle's process rows on a huge page. One arena [`TypedBuf`]; reset and refilled each
/// gather, read in place by render. Replaces the old double-buffered, `Arc`-published
/// `Snapshot` — single thread means no copy, no lease, no recycling.
pub struct Procs {
    rows: TypedBuf<ProcessEntry>,
}

impl Procs {
    /// Allocate the row buffer in `arena` (sized for `min_rows`). Not usable until
    /// [`wire`](Self::wire)d, after the arena is pinned.
    #[must_use]
    pub fn new(arena: &Arena, min_rows: usize) -> Self {
        Self {
            rows: TypedBuf::new(arena, min_rows),
        }
    }

    /// Bind the row buffer to the (pinned) arena. Call once before any access.
    pub fn wire(&mut self, arena: &Arena) {
        self.rows.wire(arena);
    }

    /// O(1) reset: rewind the buffer (no drops — POD).
    pub fn clear(&mut self) {
        self.rows.clear();
    }

    /// Pre-size for `n` rows so the per-cycle tombstone fill never relocates mid-fill.
    pub fn reserve(&mut self, n: usize) {
        self.rows.reserve(n);
    }

    #[must_use]
    pub fn as_slice(&self) -> &[ProcessEntry] {
        self.rows.as_slice()
    }

    pub fn as_mut_slice(&mut self) -> &mut [ProcessEntry] {
        self.rows.as_mut_slice()
    }

    /// Copy a row out by value (for the copy-out / write-back fill, which must never hold a
    /// reference into the buffer across an arena allocation that could relocate the chunk).
    #[must_use]
    pub fn row(&self, idx: usize) -> ProcessEntry {
        self.rows.as_slice()[idx]
    }

    /// Write a row back by value (the write-back half of the fill).
    pub fn set(&mut self, idx: usize, e: ProcessEntry) {
        self.rows.as_mut_slice()[idx] = e;
    }

    /// A short-lived mutable handle to row `idx`. The caller must not let it span any arena
    /// allocation (which could relocate the buffer); the backends only use it for a single
    /// in-place write per completed read, never across a store op.
    pub fn row_mut(&mut self, idx: usize) -> &mut ProcessEntry {
        &mut self.rows.as_mut_slice()[idx]
    }

    /// Push a tombstone slot for a PID, to be filled in place by a backend and compacted out
    /// if the read failed. Returns its index.
    pub fn push_tombstone(&mut self, pid: u32) -> usize {
        let idx = self.rows.len();
        self.rows.push(ProcessEntry {
            pid,
            ..ProcessEntry::TOMBSTONE
        });
        idx
    }

    /// Re-tombstone a slot whose backend read failed (`ESRCH`, parse/open failure, probe
    /// miss): zero its `pid` so [`compact`](Self::compact) drops it. Without this a failed
    /// read leaves a phantom row — a real pid with empty fields. **Load-bearing for the birth
    /// probe**, which makes failed speculative reads the common path.
    pub fn tombstone(&mut self, idx: usize) {
        self.rows.as_mut_slice()[idx].pid = 0;
    }

    /// Drop vanished PIDs, preserving order (PIDs were enumerated sorted, so the tree build's
    /// binary-search precondition holds). In-place retain into the prefix, then truncate.
    pub fn compact(&mut self) {
        let s = self.rows.as_mut_slice();
        let mut w = 0;
        for r in 0..s.len() {
            if !s[r].is_tombstone() {
                s[w] = s[r];
                w += 1;
            }
        }
        self.rows.truncate(w);
    }

    /// Tally the rows by process state. Returns a value the caller folds into [`SystemStats`]
    /// — keeping the aggregation with the data it reads, not threading `&mut SystemStats` in.
    #[must_use]
    pub fn count_tasks(&self) -> TaskCounts {
        let mut c = TaskCounts::default();
        for p in self.rows.as_slice() {
            match p.state {
                b'R' => c.running += 1,
                b'T' | b't' => c.stopped += 1,
                b'Z' | b'X' => c.zombie += 1,
                b'I' | b'D' => c.idle += 1,
                _ => c.sleeping += 1,
            }
        }
        c
    }
}

/// Per-state process tallies — the result of [`Procs::count_tasks`], folded into
/// [`SystemStats`] by [`SystemStats::set_task_counts`].
#[derive(Clone, Copy, Default)]
pub struct TaskCounts {
    pub running: u32,
    pub sleeping: u32,
    pub stopped: u32,
    pub zombie: u32,
    pub idle: u32,
}
