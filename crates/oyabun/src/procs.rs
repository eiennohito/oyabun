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

use crate::fxhash::PidMap;
use crate::gather::Waiting;

/// Sentinel index meaning "none" for tree links and roots.
pub const NONE: u32 = u32::MAX;

/// `comm` capacity in a row: `TASK_COMM_LEN` (16) minus the kernel's NUL ⇒ ≤ 15 printable
/// bytes. A name read from `stat` is already bounded by this; we truncate defensively.
pub const COMM_CAP: usize = 15;

/// Tag type for the cmdline string store: makes a [`StringRef<Cmd>`] resolvable only against
/// the `Cmd` store, never another store.
pub struct Cmd;

/// A process's effective-capability privilege level — the security-interesting axis on Linux
/// (a root process that dropped its caps is harmless; a non-root process holding
/// `CAP_SYS_ADMIN` is not). Derived from `/proc/<pid>/status` `CapEff` masked to the kernel's
/// `cap_last_cap`. Drives the USER column's color. `#[repr(u8)]` with `None` = 0 so a zeroed
/// row is a valid, unremarkable value.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[repr(u8)]
pub enum CapLevel {
    /// No effective capabilities — an ordinary unprivileged process (the common case).
    #[default]
    None,
    /// Some, but not all, capabilities — holds specific elevated privileges.
    Partial,
    /// The complete capability set — root-equivalent, can do anything.
    Full,
}

/// How badly runnable work is being delayed for CPU. PSI measures the delay directly; the
/// run-queue length is the fallback when the kernel has no PSI, so which one a reading is
/// travels with the value.
#[derive(Clone, Copy, Hash, PartialEq, Eq, Debug)]
pub enum CpuWait {
    /// Share of the window in which at least one task waited for a CPU, basis points.
    Pressure(u32),
    /// Runnable entities right now (no PSI available).
    RunQueue(u32),
}

impl Default for CpuWait {
    fn default() -> Self {
        Self::RunQueue(0)
    }
}

/// System-wide resource snapshot, computed once per gather cycle. Hashed as the header's
/// change key, so it carries exactly what the header shows.
#[derive(Clone, Copy, Default, Hash)]
pub struct SystemStats {
    pub num_cores: u32,
    /// Busy CPU over the short window, in millicores (1000 = one core fully busy).
    pub cpu_busy_mc: u32,
    /// Kernel share (system + irq + softirq) of `cpu_busy_mc`'s window, millicores.
    pub cpu_kernel_mc: u32,
    /// CPU time the hypervisor withheld, millicores.
    pub cpu_steal_mc: u32,
    /// Whether this machine accounts steal at all (its cumulative counter is nonzero).
    /// Structural: it decides whether the header reserves a steal slot.
    pub cpu_has_steal: bool,
    pub cpu_wait: CpuWait,
    pub mem_total: u64,
    pub mem_available: u64,
    /// `MemFree`: of the available memory, the part that is not even cache.
    pub mem_free: u64,
    /// tmpfs + shared memory.
    pub mem_shmem: u64,
    /// Memory / IO pressure (`some`, basis points); `None` when the kernel has no PSI.
    pub mem_wait_bp: Option<u32>,
    pub io_wait_bp: Option<u32>,
    pub swap_total: u64,
    pub swap_used: u64,
    /// Bytes per second swapped in over the short window.
    pub swap_in_bps: u64,
    /// Tasks in uninterruptible sleep (`D`) — stuck on IO or a lock.
    pub tasks_blocked: u32,
    pub tasks_zombie: u32,
    /// NVIDIA devices fixed at startup. Zero means the GPU UI is disabled.
    pub gpu_count: u32,
    /// Sum of per-device SM utilization, in basis points (may exceed 10000).
    pub gpu_util_bp: Option<u32>,
    /// Used and total framebuffer memory, summed across devices.
    pub vram: Option<VramStats>,
}

impl SystemStats {
    /// Fold per-state tallies from [`Procs::count_tasks`] into the `tasks_*` fields.
    pub fn set_task_counts(&mut self, c: TaskCounts) {
        self.tasks_blocked = c.blocked;
        self.tasks_zombie = c.zombie;
    }
}

#[derive(Clone, Copy, Default, Hash)]
pub struct VramStats {
    pub used: u64,
    pub total: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum GpuMetric<T> {
    Value(T),
    Unknown,
}

impl<T: Default> Default for GpuMetric<T> {
    fn default() -> Self {
        Self::Value(T::default())
    }
}

impl GpuMetric<u32> {
    #[must_use]
    pub fn saturating_add(self, rhs: Self) -> Self {
        match (self, rhs) {
            (Self::Value(a), Self::Value(b)) => Self::Value(a.saturating_add(b)),
            _ => Self::Unknown,
        }
    }
}

impl GpuMetric<u64> {
    #[must_use]
    pub fn saturating_add(self, rhs: Self) -> Self {
        match (self, rhs) {
            (Self::Value(a), Self::Value(b)) => Self::Value(a.saturating_add(b)),
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Debug)]
pub struct GpuMetrics {
    pub pct: GpuMetric<u32>,
    pub mem_bytes: GpuMetric<u64>,
}

impl GpuMetrics {
    #[must_use]
    pub fn saturating_add(self, rhs: Self) -> Self {
        Self {
            pct: self.pct.saturating_add(rhs.pct),
            mem_bytes: self.mem_bytes.saturating_add(rhs.mem_bytes),
        }
    }
}

/// Sparse process GPU telemetry. Missing PID means "no GPU context"; present values may still
/// carry per-metric unknowns for process/device-specific NVML failures.
#[derive(Default)]
pub struct GpuProcessStats {
    available: bool,
    live: PidMap<GpuMetrics>,
}

impl GpuProcessStats {
    pub fn clear_available(&mut self) {
        self.available = true;
        self.live.clear();
    }

    pub fn clear_unavailable(&mut self) {
        self.available = false;
        self.live.clear();
    }

    #[must_use]
    pub fn available(&self) -> bool {
        self.available
    }

    pub fn add_live(&mut self, pid: u32, metrics: GpuMetrics) {
        self.live
            .entry(pid)
            .and_modify(|m| *m = m.saturating_add(metrics))
            .or_insert(metrics);
    }

    #[must_use]
    pub fn live(&self, pid: u32) -> Option<GpuMetrics> {
        self.live.get(&pid).copied()
    }
}

/// `comm` (process name) stored inline: up to [`COMM_CAP`] bytes plus the valid length, as one
/// value. Bundling the pair makes an out-of-range length **non-representable** — the only way
/// to write it is [`set`](Self::set), which truncates to capacity, so [`as_bytes`](Self::as_bytes)
/// can never slice past the buffer. `Flat` (`Copy`), so it embeds in the POD row.
#[derive(Clone, Copy)]
pub struct InlineComm {
    bytes: [u8; COMM_CAP],
    len: u8,
}

impl InlineComm {
    /// The empty name.
    pub const EMPTY: InlineComm = InlineComm {
        bytes: [0; COMM_CAP],
        len: 0,
    };

    /// The name bytes (always in bounds by construction).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// Copy a name in, truncating to [`COMM_CAP`].
    #[allow(clippy::cast_possible_truncation)] // n ≤ COMM_CAP (15), fits u8
    pub fn set(&mut self, comm: &[u8]) {
        let n = comm.len().min(COMM_CAP);
        self.bytes[..n].copy_from_slice(&comm[..n]);
        self.len = n as u8;
    }
}

/// Per-process record. POD (`Flat`) — lives in a [`TypedBuf`] on huge pages; `clear` drops
/// nothing, enabling zero-alloc reuse each cycle.
// A flat data row of independent per-process signals, not a configuration struct — the several
// `bool` flags each mean a distinct thing and are set from unrelated sources, so bundling them
// would only obscure them.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy)]
pub struct ProcessEntry {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    /// Process state char (`R`, `S`, `Z`, …) as a raw byte — the point-sampled kernel
    /// state. Load-bearing for kill-safety and diagnostics; the S *column* renders
    /// [`display_state`](Self::display_state) instead.
    pub state: u8,
    /// Display-time state for the S column: `R` if the process accumulated CPU ticks in
    /// the recent window (see [`CpuRing::had_ticks`](crate::gather)), else the raw
    /// [`state`](Self::state). Answers "is this process active?" rather than "was it
    /// on-CPU the instant we sampled?", so it doesn't flicker. Written by the per-PID
    /// table after the CPU update; never read for kill or task tallies.
    pub display_state: u8,
    /// Kernel scheduling priority (lower = higher priority). Normal: 20+nice.
    pub priority: i8,
    /// Nice value (−20 … 19). User-controllable scheduling hint.
    pub nice: i8,
    /// Thread count (`num_threads` from `/proc/<pid>/stat`).
    pub num_threads: u32,
    /// Short box-average CPU% in basis points (hundredths of a percent); 10000 = one
    /// full core. The window only cancels quantization noise; history is the sparkline's.
    pub cpu_pct: u32,
    /// Rate (basis points) of this cycle's sample interval alone; 0 when none was
    /// measured or the PID was quiescent. Feeds the collapsed-row series.
    pub cpu_now: u32,
    /// This cycle's scheduler wait against the run it is compared with; empty when the source
    /// did not measure wait for the process (or nothing was measured). Feeds the row's
    /// sparkline color, and the collapsed-row series.
    pub wait_now: Waiting,
    /// Resident memory in bytes.
    pub mem_bytes: u64,
    /// Raw cumulative `utime + stime` (jiffies) — input to the CPU% delta.
    pub ticks: u64,
    /// Raw cumulative time spent runnable but queued for a CPU, in the same jiffies as
    /// [`ticks`](Self::ticks), summed over the process's threads; `None` when the source did
    /// not measure it this cycle.
    pub wait_ticks: Option<u64>,
    /// Process start time (jiffies since boot). With `pid`, identifies a unique
    /// process incarnation — used to make `kill` safe against PID reuse.
    pub start_time: u64,
    /// `comm` (process name) inline. Re-parsed from `stat` every cycle (a store would be pure
    /// overhead). Private so the bytes/length invariant holds: read via [`comm`](Self::comm),
    /// write via [`set_comm`](Self::set_comm).
    comm: InlineComm,
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
    /// The on-disk binary behind `/proc/<pid>/exe` was unlinked or replaced (the kernel
    /// appends `" (deleted)"` to the symlink target). Absorbing per incarnation — once set,
    /// latched. Colors the Command basename as an alarm.
    pub exe_deleted: bool,
    /// An executable mapping in `/proc/<pid>/maps` points at a deleted/replaced file (e.g. a
    /// linked `.so` swapped out by a system update). Transient — re-resolved on a coarse
    /// interval. Colors the Command basename as a warning (unless `exe_deleted` takes priority).
    pub uses_deleted_lib: bool,
    /// Effective-capability privilege level (from `/proc/<pid>/status`, coarse interval).
    /// Colors the USER column — the interesting axis is what a process *can do*, not who owns
    /// it. Kernel threads and unprivileged processes read [`CapLevel::None`].
    pub caps: CapLevel,

    // --- tree links (filled by `tree::build`) ---
    /// Index of parent in the buffer, or [`NONE`] for roots.
    pub parent_idx: u32,
    pub first_child: u32,
    pub next_sibling: u32,
    /// Total native descendants (structural-identity heuristics read it).
    pub subtree_size: u32,
    /// Distance from a root (0 = root).
    pub depth: u16,
}

impl ProcessEntry {
    /// A tombstone for a PID that vanished mid-scan; compacted out before render.
    pub const TOMBSTONE: ProcessEntry = ProcessEntry {
        pid: 0,
        ppid: 0,
        uid: 0,
        state: b'?',
        display_state: b'?',
        priority: 0,
        nice: 0,
        num_threads: 0,
        cpu_pct: 0,
        cpu_now: 0,
        wait_now: Waiting { wait: 0, run: 0 },
        mem_bytes: 0,
        ticks: 0,
        wait_ticks: None,
        start_time: 0,
        comm: InlineComm::EMPTY,
        cmdline: StringRef::EMPTY,
        non_ascii: false,
        is_kthread: false,
        exe_deleted: false,
        uses_deleted_lib: false,
        caps: CapLevel::None,
        parent_idx: NONE,
        first_child: NONE,
        next_sibling: NONE,
        subtree_size: 0,
        depth: 0,
    };

    /// The `comm` (process name) bytes.
    #[must_use]
    pub fn comm(&self) -> &[u8] {
        self.comm.as_bytes()
    }

    /// Copy `comm` in from a parsed `stat` slice, truncating to [`COMM_CAP`].
    pub fn set_comm(&mut self, comm: &[u8]) {
        self.comm.set(comm);
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

    /// Push a fully-built row by value. The BPF source constructs each row from the task
    /// iterator's output and appends it directly (no tombstone-then-fill step, since the
    /// iterator only ever emits live processes). (`/proc`-only builds don't call this.)
    #[cfg_attr(not(feature = "bpf"), allow(dead_code))]
    pub fn push(&mut self, e: ProcessEntry) {
        self.rows.push(e);
    }

    /// Sort rows by ascending PID — the tree build's binary-search precondition. The `/proc`
    /// path enumerates PIDs sorted and fills by index, so it never needs this; the BPF task
    /// iterator's output order is not guaranteed sorted, so its source sorts once after fill.
    #[cfg_attr(not(feature = "bpf"), allow(dead_code))]
    pub fn sort_by_pid(&mut self) {
        self.rows.as_mut_slice().sort_unstable_by_key(|e| e.pid);
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
                b'D' => c.blocked += 1,
                b'Z' | b'X' => c.zombie += 1,
                _ => {}
            }
        }
        c
    }
}

/// Per-state process tallies — the result of [`Procs::count_tasks`], folded into
/// [`SystemStats`] by [`SystemStats::set_task_counts`].
#[derive(Clone, Copy, Default)]
pub struct TaskCounts {
    /// Uninterruptible sleep (`D`), split from the benign idle kernel threads (`I`).
    pub blocked: u32,
    pub zombie: u32,
}
