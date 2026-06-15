//! The double-buffered, index-based process snapshot.
//!
//! A `Snapshot` is built by the gatherer and published via `ArcSwap`. The UI reads
//! it read-only. All cross-references (tree links) are indices into `procs`, and
//! all strings are [`StringRef`]s into `strings` — no pointers, no lifetimes, no
//! per-process heap allocation.

use crate::arena::{HugePageBuf, StringRef};

/// Sentinel index meaning "none" for tree links and roots.
pub const NONE: u32 = u32::MAX;

/// Per-process record. POD — `Vec::clear` drops nothing, enabling zero-alloc reuse.
#[derive(Clone, Copy)]
pub struct ProcessEntry {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    /// Process state char (`R`, `S`, `Z`, …) as a raw byte.
    pub state: u8,
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

    // --- tree links (filled by `tree::build`) ---
    /// Index of parent in `procs`, or [`NONE`] for roots.
    pub parent_idx: u32,
    pub first_child: u32,
    pub next_sibling: u32,
    /// Total descendants (shown as `[+N]` when collapsed).
    pub subtree_size: u32,
    /// Distance from a root (0 = root).
    pub depth: u16,
}

impl ProcessEntry {
    /// A tombstone for a PID that vanished mid-scan; compacted out before publish.
    const TOMBSTONE: ProcessEntry = ProcessEntry {
        pid: 0,
        ppid: 0,
        uid: 0,
        state: b'?',
        cpu_pct: 0,
        cpu_peak: 0,
        mem_bytes: 0,
        ticks: 0,
        start_time: 0,
        name: StringRef::EMPTY,
        parent_idx: NONE,
        first_child: NONE,
        next_sibling: NONE,
        subtree_size: 0,
        depth: 0,
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
}

impl Snapshot {
    pub fn new(min_buf: usize, buf_index: u16) -> Self {
        Self {
            procs: Vec::new(),
            strings: HugePageBuf::new(min_buf),
            first_root: NONE,
            generation: 0,
            buf_index,
        }
    }

    /// O(1) reset: clear records (no drops — POD) and rewind the arena cursor.
    /// `generation`/`first_root` are overwritten by the gatherer before publish.
    pub fn reset(&mut self) {
        self.procs.clear();
        self.strings.reset();
        self.first_root = NONE;
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
}
