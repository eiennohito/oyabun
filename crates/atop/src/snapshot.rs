//! The double-buffered, index-based process snapshot.
//!
//! A `Snapshot` is built by the gatherer and published via `ArcSwap`. The UI reads
//! it read-only. Tree links are indices into `procs`. `comm` (the process name) lives in
//! the per-cycle `strings` arena; `cmdline` is a [`StringRef<Cmd>`] into the gatherer's
//! generational `Cmd` store, resolved read-only through the snapshot's [`ByteResolver`] —
//! so an unchanged cmdline is *not* re-copied each cycle (its slot persists across cycles).

use thoop::{ByteResolver, StringRef};

use crate::arena::{self, HugePageBuf};

/// Sentinel index meaning "none" for tree links and roots.
pub const NONE: u32 = u32::MAX;

/// Tag type for the cmdline string store: makes a [`StringRef<Cmd>`] resolvable only
/// against the `Cmd` store / its [`ByteResolver`], never another store.
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
    pub name: arena::StringRef,
    /// Full `/proc/<pid>/cmdline` (NUL→space) as a handle into the generational `Cmd`
    /// store, resolved via [`Snapshot::cmd`]. Empty for kernel threads and inaccessible
    /// processes. The handle is stable across cycles while the cmdline is unchanged.
    pub cmdline: StringRef<Cmd>,
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
    pub(crate) const TOMBSTONE: ProcessEntry = ProcessEntry {
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
        name: arena::StringRef::EMPTY,
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
    /// Per-cycle `comm` arena (reset each cycle). `ProcessEntry::name` indexes it.
    pub strings: HugePageBuf,
    /// Read-only view of the gatherer's `Cmd` store at publish time — resolves every
    /// `ProcessEntry::cmdline`. Overwritten each publish; not reset (the store is shared
    /// and persistent, unlike `strings`).
    pub cmd: ByteResolver<Cmd>,
    /// Head of the root sibling chain (via `next_sibling`), or [`NONE`].
    pub first_root: u32,
    /// Monotonic version; UI rebuilds its display list when this changes.
    pub generation: u64,
    /// Live PIDs this cycle that exceeded the persistent-fd pool and used the transient
    /// fallback read (0 in the common case). Non-zero ⇒ `RLIMIT_NOFILE` is the binding
    /// constraint; surfaced so the cap is never silent.
    pub pool_overflow: u32,
    /// System-wide stats collected this cycle.
    pub sys: SystemStats,
}

impl Snapshot {
    pub fn new(min_buf: usize) -> Self {
        Self {
            procs: Vec::new(),
            strings: HugePageBuf::new(min_buf),
            cmd: ByteResolver::EMPTY,
            first_root: NONE,
            generation: 0,
            pool_overflow: 0,
            sys: SystemStats::default(),
        }
    }

    /// O(1) reset: clear records (no drops — POD) and rewind the comm arena cursor. The
    /// `cmd` resolver is *not* reset — it is replaced at publish from the live `Cmd` store;
    /// `generation`/`first_root` are overwritten by the gatherer before publish.
    pub fn reset(&mut self) {
        self.procs.clear();
        self.strings.reset();
        self.first_root = NONE;
        self.pool_overflow = 0;
    }

    /// The `comm` (process name) bytes for an entry.
    #[must_use]
    pub fn name(&self, e: &ProcessEntry) -> &[u8] {
        self.strings.get(e.name)
    }

    /// The cmdline bytes for an entry (empty for kthreads / inaccessible processes),
    /// resolved through the published `Cmd`-store view.
    #[must_use]
    pub fn cmdline(&self, e: &ProcessEntry) -> &[u8] {
        self.cmd.resolve(e.cmdline)
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

    /// Re-tombstone a slot whose backend read failed (`ESRCH`, parse failure, open
    /// failure, speculative-probe miss): zero its `pid` so [`is_tombstone`] holds again
    /// and [`compact`] drops it. Without this a failed read leaves a phantom row — a
    /// real pid with `state '?'` and empty everything. **Load-bearing for the birth
    /// probe**, which makes failed speculative reads the common path; also fixes the
    /// pre-existing die-mid-scan phantom row. Backends own the failure paths, so they
    /// call this; `compact` stays the single tombstone gate.
    ///
    /// [`is_tombstone`]: ProcessEntry::is_tombstone
    /// [`compact`]: Self::compact
    pub fn tombstone(&mut self, idx: usize) {
        self.procs[idx].pid = 0;
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
