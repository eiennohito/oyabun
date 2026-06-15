//! The gatherer thread: enumerate `/proc`, read stat files (`io_uring` or syscall),
//! parse zero-copy into the back snapshot, compute CPU%, build the tree, and
//! publish via `ArcSwap` using a two-buffer recycling protocol.

mod parse;
mod syscall;
mod uring;

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

use crate::snapshot::{ProcessEntry, Snapshot, SystemStats};
use crate::sys::{self, ProcDir, RawCpuCounters, clk_tck};
use crate::tree;
use syscall::SyscallBackend;
use uring::UringBackend;

/// Fixed per-file read slot. `/proc/<pid>/stat` is well under 1 KiB; 2 KiB is safe
/// headroom even for pathological field widths on huge-core machines.
pub const SLOT_SIZE: usize = 2048;
/// Max PIDs in flight concurrently in the `io_uring` backend (also the registered
/// direct-descriptor / statx slot count).
const N_SLOTS: u32 = 512;
/// `io_uring` SQ depth. 4 SQEs per PID × `N_SLOTS` fits with headroom.
const RING_ENTRIES: u32 = 4096;
/// Initial arena size per snapshot buffer (grows by doubling if exceeded).
const INIT_BUF: usize = 4 * 1024 * 1024;

/// Display refresh / gather cadence — the gatherer produces a snapshot this often.
const REFRESH_MS: u64 = 500;
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(REFRESH_MS);

/// Control messages from the UI thread to the gatherer.
pub enum Ctrl {
    /// Gather immediately (e.g. right after a kill, so the change shows fast).
    Refresh,
    /// Stop the gatherer loop and exit the thread.
    Quit,
}

/// CPU averaging / peak window as wall-clock time — the controllable knob (start:
/// 10 s). Longer = a calmer moving average and a longer spike memory.
const CPU_WINDOW_MS: u64 = 10_000;
/// Per-process history depth, derived as `window ÷ refresh` so the window stays
/// ~`CPU_WINDOW_MS` regardless of the refresh cadence.
const CPU_WINDOW: usize = {
    let n = (CPU_WINDOW_MS / REFRESH_MS) as usize;
    if n == 0 { 1 } else { n }
};
/// Below this elapsed interval the rate is too quantized (sub-jiffy) to be
/// meaningful — carry the windowed values forward instead of sampling.
const MIN_SAMPLE: Duration = Duration::from_millis(100);

/// One measured interval: tick delta and the per-core jiffies it spanned.
#[derive(Clone, Copy, Default)]
struct Sample {
    ticks: u32,
    jiff: u32,
}

/// Bounded per-PID history: a ring of the last [`CPU_WINDOW`] intervals plus exact
/// `u64` running sums. The displayed CPU% is the sum-weighted moving **average**
/// (stable); [`peak`](Self::peak) is the max single-interval rate still in the
/// window (captures a spike for up to [`CPU_WINDOW`] intervals after it happens).
struct CpuHistory {
    /// `utime + stime` at the last sample.
    prev_ticks: u64,
    samples: [Sample; CPU_WINDOW],
    next: usize,
    len: usize,
    sum_ticks: u64,
    sum_jiff: u64,
    /// Cached peak rate (bp). Recomputed lazily only when the peak sample is evicted.
    peak_bp: u32,
    /// Ring index of the sample that produced `peak_bp` (or `usize::MAX` = dirty).
    peak_at: usize,
    /// Tracker generation when last seen, for evicting vanished PIDs.
    seen_gen: u32,
}

impl CpuHistory {
    fn new(prev_ticks: u64, seen_gen: u32) -> Self {
        Self {
            prev_ticks,
            samples: [Sample::default(); CPU_WINDOW],
            next: 0,
            len: 0,
            sum_ticks: 0,
            sum_jiff: 0,
            peak_bp: 0,
            peak_at: usize::MAX,
            seen_gen,
        }
    }

    /// Discard history (PID reuse / counter reset), re-baselining at `ticks`.
    fn reset(&mut self, ticks: u64) {
        *self = Self::new(ticks, self.seen_gen);
    }

    fn push(&mut self, ticks: u32, jiff: u32) {
        let evicting_peak = self.len == CPU_WINDOW && self.next == self.peak_at;
        if self.len == CPU_WINDOW {
            let evicted = self.samples[self.next];
            self.sum_ticks -= u64::from(evicted.ticks);
            self.sum_jiff -= u64::from(evicted.jiff);
        } else {
            self.len += 1;
        }
        let idx = self.next;
        self.samples[idx] = Sample { ticks, jiff };
        self.sum_ticks += u64::from(ticks);
        self.sum_jiff += u64::from(jiff);
        self.next = (self.next + 1) % CPU_WINDOW;

        let new_rate = rate(u64::from(ticks), u64::from(jiff));
        if new_rate >= self.peak_bp {
            self.peak_bp = new_rate;
            self.peak_at = idx;
        } else if evicting_peak {
            // The old peak was just evicted — rescan to find the new max.
            self.peak_at = usize::MAX;
        }
    }

    /// Sum-weighted moving average over the window (exact integer math).
    fn avg(&self) -> u32 {
        rate(self.sum_ticks, self.sum_jiff)
    }

    /// Max single-interval rate still in the window. O(1) in the common case;
    /// O(`CPU_WINDOW`) only when the previous peak sample is evicted (~1/window).
    fn peak(&mut self) -> u32 {
        if self.peak_at == usize::MAX {
            // Dirty: rescan.
            let (mut best, mut best_at) = (0u32, 0usize);
            for i in 0..self.len {
                let s = &self.samples[i];
                let r = rate(u64::from(s.ticks), u64::from(s.jiff));
                if r >= best {
                    best = r;
                    best_at = i;
                }
            }
            self.peak_bp = best;
            self.peak_at = best_at;
        }
        self.peak_bp
    }
}

/// Fast non-cryptographic hasher (`FxHash`-style) for internal integer keys. The
/// default `RandomState` (`SipHash`) resists hash-flooding but is slow; PIDs here
/// are not attacker-controlled, so we trade that resistance for speed on the
/// per-cycle PID→history lookups.
#[derive(Default)]
struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.add(u64::from(b));
        }
    }
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i)); // the hot path: u32 PID keys
    }
    fn finish(&self) -> u64 {
        self.hash
    }
}

#[derive(Default, Clone)]
struct FxBuildHasher;

impl BuildHasher for FxBuildHasher {
    type Hasher = FxHasher;
    fn build_hasher(&self) -> FxHasher {
        FxHasher::default()
    }
}

/// PID-keyed map using the fast hasher above.
type PidMap<V> = HashMap<u32, V, FxBuildHasher>;

/// Derives per-process CPU% as a **per-core rate over real elapsed time** —
/// `Δticks / (Δwall · CLK_TCK)`, so one fully-used core reads 100% and a
/// multi-threaded process can exceed 100% (matching `top`/`htop` "Irix mode").
/// Dividing instead by the `/proc/stat` machine-wide tick sum (an earlier approach)
/// yielded `1/num_cpus` of the truth — a 32× undercount on a 32-core box.
///
/// Each PID keeps a [`CpuHistory`]: the moving average is the stable reading, the
/// peak captures spikes. The window is a monotonic clock (a jittering gather
/// interval self-corrects), and refreshes closer than [`MIN_SAMPLE`] (e.g. the
/// forced refresh after a kill) carry forward rather than dividing by a near-zero
/// window. PIDs absent from a cycle are evicted via the generation tag.
struct CpuTracker {
    hist: PidMap<CpuHistory>,
    last: Option<Instant>,
    clk_tck: u64,
    gen_counter: u32,
}

impl CpuTracker {
    fn new(clk_tck: u64) -> Self {
        Self {
            hist: PidMap::default(),
            last: None,
            clk_tck,
            gen_counter: 0,
        }
    }

    fn update(&mut self, procs: &mut [ProcessEntry], now: Instant) {
        let window = self
            .last
            .map(|t| now.saturating_duration_since(t))
            .filter(|e| *e >= MIN_SAMPLE);

        let Some(elapsed) = window else {
            // First sample, or a refresh too soon to measure: carry the windowed
            // values forward; seed baselines only on the very first call.
            let first = self.last.is_none();
            if first {
                self.gen_counter = self.gen_counter.wrapping_add(1);
                self.last = Some(now);
            }
            for p in procs.iter_mut() {
                match self.hist.get_mut(&p.pid) {
                    Some(h) => {
                        p.cpu_pct = h.avg();
                        p.cpu_peak = h.peak();
                    }
                    None if first => {
                        self.hist
                            .insert(p.pid, CpuHistory::new(p.ticks, self.gen_counter));
                    }
                    None => {}
                }
            }
            return;
        };

        let jiff = u32::try_from(elapsed_jiffies(elapsed, self.clk_tck)).unwrap_or(u32::MAX);
        self.gen_counter = self.gen_counter.wrapping_add(1);
        let cur_gen = self.gen_counter;
        for p in procs.iter_mut() {
            let h = match self.hist.entry(p.pid) {
                Entry::Occupied(e) => {
                    let h = e.into_mut();
                    if p.ticks < h.prev_ticks {
                        h.reset(p.ticks); // PID reuse / counter wrap
                    } else {
                        let delta = u32::try_from(p.ticks - h.prev_ticks).unwrap_or(u32::MAX);
                        h.push(delta, jiff);
                        h.prev_ticks = p.ticks;
                    }
                    h
                }
                // New PID: baseline now; first real rate lands next interval.
                Entry::Vacant(e) => e.insert(CpuHistory::new(p.ticks, cur_gen)),
            };
            h.seen_gen = cur_gen;
            p.cpu_pct = h.avg();
            p.cpu_peak = h.peak();
        }
        self.hist.retain(|_, h| h.seen_gen == cur_gen); // drop vanished PIDs
        self.last = Some(now);
    }
}

/// Per-core elapsed time in jiffies, floored at 1 to keep [`rate`] division safe.
fn elapsed_jiffies(elapsed: Duration, clk_tck: u64) -> u64 {
    (u64::try_from(elapsed.as_micros())
        .unwrap_or(u64::MAX)
        .saturating_mul(clk_tck)
        / 1_000_000)
        .max(1)
}

/// CPU% in basis points: `ticks / jiffies` (10000 = one full core). 0 if no window.
fn rate(ticks: u64, jiff: u64) -> u32 {
    if jiff == 0 {
        return 0;
    }
    u32::try_from(ticks.saturating_mul(10000) / jiff).unwrap_or(u32::MAX)
}

/// Tracks cumulative `/proc/stat` CPU counters to compute deltas.
struct SysCpuAccum {
    prev: Option<RawCpuCounters>,
    num_cores: u32,
}

impl SysCpuAccum {
    fn new() -> Self {
        Self {
            prev: None,
            num_cores: sys::num_cpus(),
        }
    }

    /// Read current counters, compute basis-point rates from the delta, and fill
    /// `sys` with CPU + memory + load stats. The first call baselines only.
    fn update(&mut self, sys: &mut SystemStats) {
        let cur = sys::read_cpu_counters();
        if let Some(prev) = &self.prev {
            let d_user = cur.user.wrapping_sub(prev.user) + cur.nice.wrapping_sub(prev.nice);
            let d_sys = cur.system.wrapping_sub(prev.system)
                + cur.irq.wrapping_sub(prev.irq)
                + cur.softirq.wrapping_sub(prev.softirq);
            let d_iowait = cur.iowait.wrapping_sub(prev.iowait);
            let d_total = cur.total().wrapping_sub(prev.total()).max(1);

            sys.cpu_user_bp = u32::try_from(d_user * 10000 / d_total).unwrap_or(u32::MAX);
            sys.cpu_sys_bp = u32::try_from(d_sys * 10000 / d_total).unwrap_or(u32::MAX);
            sys.cpu_iowait_bp = u32::try_from(d_iowait * 10000 / d_total).unwrap_or(u32::MAX);
        }
        self.prev = Some(cur);
        sys.num_cores = self.num_cores;

        let mem = sys::read_meminfo();
        sys.mem_total = mem.total;
        sys.mem_used = mem.total.saturating_sub(mem.available);
        sys.mem_cached = mem.buffers.saturating_add(mem.cached);
        sys.swap_total = mem.swap_total;
        sys.swap_used = mem.swap_total.saturating_sub(mem.swap_free);

        sys.load = sys::read_loadavg();
        sys.uptime_secs = sys::read_uptime_secs();
    }
}

enum Backend {
    Uring(Box<UringBackend>),
    Syscall(SyscallBackend),
}

impl Backend {
    fn collect(
        &mut self,
        pids: &[u32],
        snap: &mut Snapshot,
        page_size: u64,
    ) -> std::io::Result<()> {
        match self {
            Backend::Uring(u) => u.collect(pids, snap, page_size),
            Backend::Syscall(s) => {
                s.collect(pids, snap, page_size);
                Ok(())
            }
        }
    }
}

pub struct Gatherer {
    arc_swap: Arc<ArcSwap<Snapshot>>,
    recycled: Option<Arc<Snapshot>>,
    backend: Backend,
    proc_dir: ProcDir,
    pids: Vec<u32>,
    dent_buf: Vec<u8>,
    cpu: CpuTracker,
    sys_cpu: SysCpuAccum,
    tree_stack: Vec<u32>,
    tree_order: Vec<u32>,
    page_size: u64,
    generation: u64,
}

impl Gatherer {
    /// Build the gatherer and the shared snapshot cell. Returns the `ArcSwap` for
    /// the UI to load from. Probes `io_uring`; falls back to the syscall backend.
    pub fn new(page_size: u64) -> std::io::Result<(Self, Arc<ArcSwap<Snapshot>>)> {
        let front = Arc::new(Snapshot::new(INIT_BUF, 0));
        let back = Arc::new(Snapshot::new(INIT_BUF, 1));

        let backend = match UringBackend::probe(N_SLOTS, RING_ENTRIES, &front, &back) {
            Some(u) => Backend::Uring(Box::new(u)),
            None => Backend::Syscall(SyscallBackend),
        };

        let arc_swap = Arc::new(ArcSwap::from(front));
        let gatherer = Self {
            arc_swap: arc_swap.clone(),
            recycled: Some(back),
            backend,
            proc_dir: ProcDir::open()?,
            pids: Vec::new(),
            dent_buf: vec![0u8; 64 * 1024],
            cpu: CpuTracker::new(clk_tck()),
            sys_cpu: SysCpuAccum::new(),
            tree_stack: Vec::new(),
            tree_order: Vec::new(),
            page_size,
            generation: 0,
        };
        Ok((gatherer, arc_swap))
    }

    /// Produce the first snapshot synchronously. Call before spawning the gatherer
    /// thread so the opening frame is populated (no startup poll on the UI side).
    pub fn prime(&mut self) {
        self.gather();
    }

    /// Gatherer thread entry point. Sleeps on the control channel between refreshes
    /// (zero idle CPU); gathers on timeout or `Refresh`; exits on `Quit`/hangup.
    /// The caller is expected to have `prime()`d the first snapshot.
    pub fn run(mut self, ctrl: &Receiver<Ctrl>, interval: Duration) {
        // Exits on Quit or a hung-up channel; gathers on Refresh or interval timeout.
        while let Ok(Ctrl::Refresh) | Err(RecvTimeoutError::Timeout) = ctrl.recv_timeout(interval) {
            self.gather();
        }
    }

    fn gather(&mut self) {
        self.proc_dir.read_pids(&mut self.dent_buf, &mut self.pids);
        self.pids.sort_unstable();
        let now = Instant::now();

        // Reclaim the recycled buffer; skip this cycle if the UI still holds it
        // (vanishingly rare — see take_back).
        let Some(mut arc) = self.take_back() else {
            return;
        };
        let snap = Arc::get_mut(&mut arc).expect("recycled buffer is unique");

        // 2 arena slots per PID: stat + cmdline.
        let needed = self.pids.len() * SLOT_SIZE * 2;
        Self::prepare(snap, &self.pids, needed, &mut self.backend);

        if self
            .backend
            .collect(&self.pids, snap, self.page_size)
            .is_err()
        {
            // io_uring failed mid-cycle: drop to syscall permanently and redo.
            self.backend = Backend::Syscall(SyscallBackend);
            Self::prepare(snap, &self.pids, needed, &mut self.backend);
            let _ = self.backend.collect(&self.pids, snap, self.page_size);
        }

        snap.compact();
        self.cpu.update(&mut snap.procs, now);
        snap.first_root = tree::build(&mut snap.procs, &mut self.tree_stack, &mut self.tree_order);
        tree::aggregate(&mut snap.procs, &self.tree_order);

        // System-wide stats (tiny reads, ~3 μs total).
        self.sys_cpu.update(&mut snap.sys);
        snap.count_tasks();

        self.generation += 1;
        snap.generation = self.generation;

        let prev = self.arc_swap.swap(arc);
        self.recycled = Some(prev);
    }

    /// Reset and pre-size the snapshot, re-registering the `io_uring` buffer if it
    /// grew (moved). Seeds tombstones for every PID in sorted order.
    ///
    /// `reserve` MUST happen before any `alloc` or `io_uring` submission that targets
    /// this buffer — a grow relocates the mapping and would invalidate registered
    /// buffer pointers and in-flight read destinations.
    fn prepare(snap: &mut Snapshot, pids: &[u32], needed: usize, backend: &mut Backend) {
        snap.reset();
        for &pid in pids {
            snap.push_tombstone(pid);
        }
        if snap.strings.reserve(needed)
            && let Backend::Uring(u) = backend
        {
            u.update_buffer(
                snap.buf_index,
                snap.strings.as_ptr(),
                snap.strings.capacity(),
            );
        }
    }

    /// Reclaim the back buffer with unique access. Spins briefly if the UI still
    /// references it; gives up (caller skips the cycle) rather than ever allocating
    /// a third, unregistered buffer.
    fn take_back(&mut self) -> Option<Arc<Snapshot>> {
        let mut arc = self.recycled.take()?;
        for _ in 0..200 {
            if Arc::get_mut(&mut arc).is_some() {
                return Some(arc);
            }
            std::thread::sleep(Duration::from_micros(50));
        }
        self.recycled = Some(arc);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_jiffies_is_per_core() {
        // 500 ms at 100 Hz = 50 jiffies on one core, regardless of core count.
        assert_eq!(elapsed_jiffies(Duration::from_millis(500), 100), 50);
        assert_eq!(elapsed_jiffies(Duration::from_secs(1), 100), 100);
        // Floored at 1 so cpu_rate never divides by zero.
        assert_eq!(elapsed_jiffies(Duration::from_micros(1), 100), 1);
    }

    #[test]
    fn fx_hasher_is_deterministic_and_distinct() {
        let bh = FxBuildHasher;
        assert_eq!(bh.hash_one(1234_u32), bh.hash_one(1234_u32));
        assert_ne!(bh.hash_one(1_u32), bh.hash_one(2_u32));
    }

    #[test]
    fn rate_is_per_core_basis_points() {
        assert_eq!(rate(50, 50), 10000); // one full core = 100%
        assert_eq!(rate(200, 50), 40000); // four busy threads = 400%
        assert_eq!(rate(25, 50), 5000); // half a core
        assert_eq!(rate(0, 50), 0); // idle
        assert_eq!(rate(10, 0), 0); // empty window
    }

    #[test]
    fn moving_average_is_exact() {
        let mut h = CpuHistory::new(0, 1);
        h.push(25, 50); // 50%
        h.push(50, 50); // 100% → (25+50)/(100) = 75%
        assert_eq!(h.avg(), 7500);
    }

    #[test]
    fn moving_average_evicts_old_samples() {
        let mut h = CpuHistory::new(0, 1);
        for _ in 0..CPU_WINDOW {
            h.push(10, 50); // fill window with 20%
        }
        assert_eq!(h.avg(), 2000);
        for _ in 0..CPU_WINDOW {
            h.push(50, 50); // overwrite the whole window with 100%
        }
        assert_eq!(h.avg(), 10000, "old samples must be fully evicted");
    }

    #[test]
    fn peak_captures_spike_the_average_damps() {
        let mut h = CpuHistory::new(0, 1);
        h.push(5, 50); // 10%
        h.push(50, 50); // 100% spike
        h.push(5, 50); // 10%
        assert_eq!(h.peak(), 10000, "peak must hold the spike");
        assert!(h.avg() < 5000, "average must stay damped, got {}", h.avg());
    }

    fn enum_pids() -> Vec<u32> {
        let dir = ProcDir::open().unwrap();
        let mut dent = vec![0u8; 64 * 1024];
        let mut pids = Vec::new();
        dir.read_pids(&mut dent, &mut pids);
        pids.sort_unstable();
        pids
    }

    fn name_of(snap: &Snapshot, p: &ProcessEntry) -> String {
        String::from_utf8_lossy(snap.strings.get(p.name)).into_owned()
    }

    /// The `io_uring` backend must agree with the syscall oracle on stable fields for
    /// processes both scans observed. (CPU/ticks/mem can drift between scans; PID
    /// reuse can churn the set — so we anchor on PID 1 and self, and require broad
    /// agreement on the overlap.)
    #[test]
    fn uring_matches_syscall_backend() {
        let page_size = crate::sys::page_size();
        let pids = enum_pids();

        let mut a = Snapshot::new(INIT_BUF, 0);
        let mut b = Snapshot::new(INIT_BUF, 1);

        let Some(mut uring) = UringBackend::probe(N_SLOTS, RING_ENTRIES, &a, &b) else {
            eprintln!("io_uring unavailable — skipping oracle comparison");
            return;
        };

        for &pid in &pids {
            a.push_tombstone(pid);
        }
        a.strings.reserve(pids.len() * SLOT_SIZE);
        SyscallBackend.collect(&pids, &mut a, page_size);
        a.compact();

        for &pid in &pids {
            b.push_tombstone(pid);
        }
        // Force growth past INIT_BUF to exercise the re-register-on-grow path even
        // on machines with few processes.
        let needed = (pids.len() * SLOT_SIZE).max(INIT_BUF + 1);
        if b.strings.reserve(needed) {
            uring.update_buffer(b.buf_index, b.strings.as_ptr(), b.strings.capacity());
        } else {
            panic!("expected buffer growth to be forced");
        }
        uring
            .collect(&pids, &mut b, page_size)
            .expect("uring collect");
        b.compact();

        assert!(b.procs.len() > 10, "uring found too few: {}", b.procs.len());

        let by_pid: HashMap<u32, &ProcessEntry> = b.procs.iter().map(|p| (p.pid, p)).collect();

        let mut common = 0;
        let mut name_matches = 0;
        for pa in &a.procs {
            if let Some(pb) = by_pid.get(&pa.pid) {
                common += 1;
                if name_of(&a, pa) == name_of(&b, pb) {
                    name_matches += 1;
                }
                if pa.pid == 1 || pa.pid == std::process::id() {
                    assert_eq!(pa.ppid, pb.ppid, "ppid mismatch for pid {}", pa.pid);
                    assert_eq!(pa.uid, pb.uid, "uid mismatch for pid {}", pa.pid);
                    assert_eq!(
                        name_of(&a, pa),
                        name_of(&b, pb),
                        "name mismatch pid {}",
                        pa.pid
                    );
                }
            }
        }
        assert!(common > 20, "too little overlap: {common}");
        // Allow a little churn, but the vast majority must match exactly.
        assert!(
            name_matches * 100 >= common * 95,
            "name agreement too low: {name_matches}/{common}"
        );
    }

    /// A process pegging a core must read as substantial per-core CPU (≫ the old
    /// `1/num_cpus` undercount). Spins a thread in *this* process and checks our own
    /// PID across a few real sample windows.
    #[test]
    fn busy_process_reads_per_core_cpu() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut x = 0u64;
            while !worker_stop.load(Ordering::Relaxed) {
                x = std::hint::black_box(x.wrapping_add(1));
            }
        });

        let (mut g, cell) = Gatherer::new(crate::sys::page_size()).expect("gatherer");
        let me = std::process::id();
        let (mut max_cpu, mut max_peak) = (0, 0);
        for _ in 0..4 {
            g.gather();
            let snap = cell.load_full();
            if let Some(p) = snap.procs.iter().find(|p| p.pid == me) {
                max_cpu = max_cpu.max(p.cpu_pct);
                max_peak = max_peak.max(p.cpu_peak);
            }
            std::thread::sleep(Duration::from_millis(300));
        }

        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();

        // One pegged core is ~10000 bp; allow window ramp + scheduling slack.
        assert!(
            max_cpu > 3000,
            "busy process should average >30% per-core, got {max_cpu} bp"
        );
        assert!(
            max_peak >= max_cpu,
            "peak ({max_peak}) must capture at least the average ({max_cpu})"
        );
    }

    /// Full gatherer pipeline (enumerate → backend → cpu → tree → publish).
    #[test]
    fn gatherer_publishes_valid_snapshot() {
        let (mut g, cell) = Gatherer::new(crate::sys::page_size()).expect("gatherer");
        g.gather();
        let snap = cell.load_full();

        assert!(snap.generation >= 1);
        assert!(snap.procs.len() > 10, "got {}", snap.procs.len());
        assert!(snap.procs.iter().any(|p| p.pid == 1), "pid 1 missing");
        assert_ne!(snap.first_root, crate::snapshot::NONE, "no roots");

        // PIDs sorted ascending (tree binary-search precondition).
        assert!(snap.procs.windows(2).all(|w| w[0].pid < w[1].pid));

        // Every non-root parent index is in range and resolves to the right PID.
        for p in &snap.procs {
            if p.parent_idx != crate::snapshot::NONE {
                let parent = &snap.procs[p.parent_idx as usize];
                assert_eq!(parent.pid, p.ppid, "parent_idx points at wrong pid");
            }
        }
    }
}
