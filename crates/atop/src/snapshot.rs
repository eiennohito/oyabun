//! The double-buffered, index-based process snapshot.
//!
//! A `Snapshot` is built by the gatherer and published via `ArcSwap`. The UI reads
//! it read-only. All cross-references (tree links) are indices into `procs`, and
//! all strings are [`StringRef`]s into `strings` — no pointers, no lifetimes, no
//! per-process heap allocation.

use crate::arena::{HugePageBuf, StringRef};

/// Sentinel index meaning "none" for tree links and roots.
pub const NONE: u32 = u32::MAX;

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

/// Per-process record. POD — `Vec::clear` drops nothing, enabling zero-alloc reuse.
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
    /// `comm` (process name), pointing into [`Snapshot::strings`].
    pub name: StringRef,
    /// Full `/proc/<pid>/cmdline` (NUL→space), pointing into [`Snapshot::strings`].
    /// Empty for kernel threads and inaccessible processes.
    pub cmdline: StringRef,
    /// Set if `comm` or `cmdline` contains any byte ≥ 0x80 — i.e. the Command column
    /// needs unicode-aware width. False for the ~99% ASCII case (renderer fast path).
    /// Computed for free during the byte-walks that already scan both fields.
    pub non_ascii: bool,
    /// `PF_KTHREAD` set in stat `flags` — a kernel thread. Its `/proc/<pid>/cmdline`
    /// is permanently empty, so the gatherer never reads it (parsed free from stat).
    pub is_kthread: bool,

    // --- tree links (filled by `tree::build`) ---
    /// Index of parent in `procs`, or [`NONE`] for roots.
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
    /// A tombstone for a PID that vanished mid-scan; compacted out before publish.
    const TOMBSTONE: ProcessEntry = ProcessEntry {
        pid: 0,
        ppid: 0,
        uid: 0,
        state: b'?',
        priority: 0,
        nice: 0,
        num_threads: 0,
        cpu_pct: 0,
        cpu_peak: 0,
        mem_bytes: 0,
        ticks: 0,
        start_time: 0,
        name: StringRef::EMPTY,
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

    /// A slot is a tombstone (read failed / PID vanished) iff `pid == 0`. Linux never
    /// exposes PID 0 (the scheduler) as a numeric `/proc` entry, so 0 is unambiguous.
    pub fn is_tombstone(&self) -> bool {
        self.pid == 0
    }
}

pub struct Snapshot {
    pub procs: Vec<ProcessEntry>,
    pub strings: HugePageBuf,
    /// Head of the root sibling chain (via `next_sibling`), or [`NONE`].
    pub first_root: u32,
    /// Monotonic version; UI rebuilds its display list when this changes.
    pub generation: u64,
    /// Stable `io_uring` registered-buffer index for `strings` (one per physical
    /// buffer in the double-buffer pool). Ignored by the syscall backend.
    pub buf_index: u16,
    /// Live PIDs this cycle that exceeded the persistent-fd pool and used the transient
    /// fallback read (0 in the common case). Non-zero ⇒ `RLIMIT_NOFILE` is the binding
    /// constraint; surfaced so the cap is never silent.
    pub pool_overflow: u32,
    /// System-wide stats collected this cycle.
    pub sys: SystemStats,
}

impl Snapshot {
    pub fn new(min_buf: usize, buf_index: u16) -> Self {
        Self {
            procs: Vec::new(),
            strings: HugePageBuf::new(min_buf),
            first_root: NONE,
            generation: 0,
            buf_index,
            pool_overflow: 0,
            sys: SystemStats::default(),
        }
    }

    /// O(1) reset: clear records (no drops — POD) and rewind the arena cursor.
    /// `generation`/`first_root` are overwritten by the gatherer before publish.
    pub fn reset(&mut self) {
        self.procs.clear();
        self.strings.reset();
        self.first_root = NONE;
        self.pool_overflow = 0;
    }

    /// Push a tombstone slot for a PID, to be filled in place by a backend and
    /// compacted out if the read failed. Returns its index.
    pub fn push_tombstone(&mut self, pid: u32) -> usize {
        let idx = self.procs.len();
        self.procs.push(ProcessEntry {
            pid,
            ..ProcessEntry::TOMBSTONE
        });
        idx
    }

    /// Drop vanished PIDs, preserving order (PIDs were enumerated sorted).
    pub fn compact(&mut self) {
        self.procs.retain(|p| !p.is_tombstone());
    }

    /// Tally process states into `sys.tasks_*`.
    pub fn count_tasks(&mut self) {
        let (mut run, mut slp, mut stp, mut zmb, mut idl) = (0u32, 0u32, 0u32, 0u32, 0u32);
        for p in &self.procs {
            match p.state {
                b'R' => run += 1,
                b'T' | b't' => stp += 1,
                b'Z' | b'X' => zmb += 1,
                b'I' | b'D' => idl += 1,
                _ => slp += 1,
            }
        }
        self.sys.tasks_running = run;
        self.sys.tasks_sleeping = slp;
        self.sys.tasks_stopped = stp;
        self.sys.tasks_zombie = zmb;
        self.sys.tasks_idle = idl;
    }
}
