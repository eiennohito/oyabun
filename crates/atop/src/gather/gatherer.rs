//! The single-threaded data producer and its observation-source selection.
//!
//! A cycle fills the live [`Procs`] buffer from a [`Source`] (privileged BPF or unprivileged
//! `/proc`, chosen once at startup), then runs the source-agnostic tail: the per-PID table
//! (CPU% + uid/cmdline), the tree build, and the system-wide stats. No publish, no double
//! buffer, no channel — [`Gatherer::cycle`] runs to completion before the caller renders, so
//! the borrow checker proves gather and render never alias.

use thoop::Arena;

use super::config::INITIAL_ROWS;
use super::nvml::NvmlSampler;
use super::procfs::ProcSource;
#[cfg(feature = "record")]
use super::record::Recorder;
#[cfg(test)]
use super::replay::ReplaySource;
use super::source::{Source, SourceCtx};
use super::sysstat::SystemSampler;
use super::table::{ProcReader, ProcTable, RealProcReader, cmdline_refresh_n};
use crate::procs::{GpuMetrics, GpuProcessStats, NONE, ProcessEntry, Procs, SystemStats};
use crate::sys::{ProcDir, clk_tck};
use crate::tree;

/// Where the live process rows come from. Selected once at startup ([`Gatherer::with_source`]):
/// the privileged BPF source if it loads (caps present), else the unprivileged `/proc` source.
/// The rest of a cycle — the per-PID table, tree build, system stats — is source-agnostic.
enum ObservationSource {
    Proc(Box<ProcSource>),
    #[cfg(feature = "bpf")]
    Bpf(Box<super::bpf::BpfSource>),
    #[cfg(test)]
    Replay(Box<ReplaySource>),
}

impl ObservationSource {
    /// Fill the source-output rows, then run the per-PID table pass with the matching metadata
    /// reader. Replay owns both sides; live sources share the gatherer's real `/proc` reader.
    fn populate_and_update(
        &mut self,
        procs: &mut Procs,
        table: &mut ProcTable,
        live_reader: &mut RealProcReader,
        page_size: u64,
        prev_gen: u64,
        building_gen: u64,
    ) -> super::source::CycleResult {
        match self {
            ObservationSource::Proc(p) => Self::populate_with_reader(
                p.as_mut(),
                live_reader,
                procs,
                table,
                page_size,
                prev_gen,
                building_gen,
            ),
            #[cfg(feature = "bpf")]
            ObservationSource::Bpf(b) => Self::populate_with_reader(
                b.as_mut(),
                live_reader,
                procs,
                table,
                page_size,
                prev_gen,
                building_gen,
            ),
            #[cfg(test)]
            ObservationSource::Replay(r) => {
                let result = {
                    let ctx = SourceCtx {
                        index: table.index(),
                        page_size,
                        prev_gen,
                    };
                    r.populate(procs, ctx)
                };
                table.update(procs, result.now, building_gen, r.as_mut());
                result
            }
        }
    }

    fn populate_with_reader<S: Source, R: ProcReader>(
        source: &mut S,
        reader: &mut R,
        procs: &mut Procs,
        table: &mut ProcTable,
        page_size: u64,
        prev_gen: u64,
        building_gen: u64,
    ) -> super::source::CycleResult {
        let result = {
            let ctx = SourceCtx {
                index: table.index(),
                page_size,
                prev_gen,
            };
            source.populate(procs, ctx)
        };
        table.update(procs, result.now, building_gen, reader);
        result
    }

    #[cfg(test)]
    fn replay_sys(&self) -> Option<SystemStats> {
        match self {
            ObservationSource::Proc(_) => None,
            #[cfg(feature = "bpf")]
            ObservationSource::Bpf(_) => None,
            ObservationSource::Replay(r) => Some(r.sys()),
        }
    }
}

/// The single-threaded data producer: fills the live [`Procs`] buffer from a [`Source`], then
/// builds the per-PID table + tree + system stats. Owns the THP arena, the per-PID stores, and
/// the process buffer the renderer reads directly. No `ArcSwap`, no double buffer, no channel —
/// [`cycle`](Self::cycle) runs to completion before the caller renders, so the borrow checker
/// proves gather and render never alias.
pub struct Gatherer {
    /// Shared THP sub-allocator backing the per-PID stores **and** the process buffer.
    /// **Boxed so it is pinned**: every store/buffer caches a `*const Arena` (set at `wire`),
    /// so the gatherer may move freely while the arena's heap address stays fixed.
    arena: Box<Arena>,
    /// The live process rows on a huge page — reset and refilled each cycle, read in place.
    procs: Procs,
    /// The observation source (privileged BPF or unprivileged `/proc`), chosen at startup.
    source: ObservationSource,
    /// Unified per-PID table (CPU history + uid/cmdline + the shared PID index).
    table: ProcTable,
    /// Real `/proc/<pid>` metadata reader used by live sources. Replay provides its own reader.
    reader: RealProcReader,
    sys_sampler: SystemSampler,
    /// Optional runtime NVML sampler, fixed at startup.
    nvml: Option<NvmlSampler>,
    /// System-wide stats for this cycle.
    sys: SystemStats,
    /// Sparse per-process GPU telemetry for the latest cycle.
    gpu: GpuProcessStats,
    /// Head of the root sibling chain (via `next_sibling`), or [`NONE`].
    first_root: u32,
    /// Live PIDs this cycle that exceeded the pool and used the transient fallback (0 common;
    /// `/proc` source only — the BPF source has no fd pool).
    pool_overflow: u32,
    /// Processes that were born and died between this and the previous cycle, caught only by
    /// the BPF fork/exit events (the snapshot never saw them). 0 in `/proc` mode.
    short_lived: u32,
    tree_stack: Vec<u32>,
    tree_order: Vec<u32>,
    page_size: u64,
    generation: u64,
    #[cfg(feature = "record")]
    recorder: Option<Recorder>,
}

impl Gatherer {
    /// Build the gatherer on the **calling thread**, which must stay the only thread that uses
    /// it: the arena + stores are `!Send`, and the `io_uring` ring binds to its creator. The
    /// arena is boxed (pinned) so the stores/buffer can cache a `*const Arena` during `wire`.
    /// Probes the privileged BPF source first; on success the `/proc` backend is never built.
    /// `proc_dir` is opened by the caller (a fallible op kept out of here).
    #[must_use]
    pub fn new(page_size: u64, proc_dir: ProcDir) -> Self {
        Self::with_source(page_size, proc_dir, true)
    }

    /// Like [`new`](Self::new) but `allow_bpf` gates the privileged probe — `false` forces the
    /// `/proc` source (the proc-path tests assert proc-specific behaviour and must not flip to
    /// BPF when run under `caprun`).
    fn with_source(page_size: u64, proc_dir: ProcDir, allow_bpf: bool) -> Self {
        let clk_tck = clk_tck();
        let arena = Box::new(Arena::new(0));

        // Probe the privileged source first; its presence decides who owns uid. Its landing
        // buffer is allocated in the shared arena (wired below, once the arena is pinned).
        #[cfg(feature = "bpf")]
        let bpf = if allow_bpf {
            super::bpf::BpfSource::probe(&arena, page_size, clk_tck)
        } else {
            None
        };
        #[cfg(not(feature = "bpf"))]
        let bpf: Option<std::convert::Infallible> = {
            let _ = allow_bpf;
            None
        };

        let source_provides_uid = bpf.is_some();

        let mut table = ProcTable::new(&arena, clk_tck, cmdline_refresh_n(), source_provides_uid);
        table.wire(&arena); // arena is pinned (boxed) → stores may cache its address
        let mut procs = Procs::new(&arena, INITIAL_ROWS);
        procs.wire(&arena);

        let source = match bpf {
            #[cfg(feature = "bpf")]
            Some(mut b) => {
                b.wire(&arena); // bind the landing buffer now that the arena is pinned
                ObservationSource::Bpf(Box::new(b))
            }
            None => ObservationSource::Proc(Box::new(ProcSource::new(proc_dir))),
        };

        let nvml = NvmlSampler::probe();
        let sys = SystemStats {
            gpu_count: nvml.as_ref().map_or(0, NvmlSampler::device_count),
            ..SystemStats::default()
        };

        Self::from_parts(arena, procs, source, table, nvml, sys, page_size)
    }

    #[cfg(test)]
    pub(crate) fn replay(stream: super::replay::Stream) -> Self {
        Self::replay_with_refresh_n(stream, cmdline_refresh_n())
    }

    #[cfg(test)]
    pub(crate) fn replay_with_refresh_n(stream: super::replay::Stream, refresh_n: u32) -> Self {
        let clk_tck = 100;
        let arena = Box::new(Arena::new(0));
        let mut table = ProcTable::new(&arena, clk_tck, refresh_n.max(1), true);
        table.wire(&arena);
        let mut procs = Procs::new(&arena, INITIAL_ROWS);
        procs.wire(&arena);

        Self::from_parts(
            arena,
            procs,
            ObservationSource::Replay(Box::new(ReplaySource::new(stream))),
            table,
            None,
            SystemStats::default(),
            4096,
        )
    }

    fn from_parts(
        arena: Box<Arena>,
        procs: Procs,
        source: ObservationSource,
        table: ProcTable,
        nvml: Option<NvmlSampler>,
        sys: SystemStats,
        page_size: u64,
    ) -> Self {
        Self {
            arena,
            procs,
            source,
            table,
            reader: RealProcReader::new(),
            sys_sampler: SystemSampler::new(),
            nvml,
            sys,
            gpu: GpuProcessStats::default(),
            first_root: NONE,
            pool_overflow: 0,
            short_lived: 0,
            tree_stack: Vec::new(),
            tree_order: Vec::new(),
            page_size,
            generation: 0,
            #[cfg(feature = "record")]
            recorder: Recorder::from_env(),
        }
    }

    /// The live process rows (read by render).
    #[must_use]
    pub fn procs(&self) -> &Procs {
        &self.procs
    }

    /// System-wide stats for the latest cycle.
    #[must_use]
    pub fn sys(&self) -> &SystemStats {
        &self.sys
    }

    /// Head of the root sibling chain, or [`NONE`].
    #[must_use]
    pub fn first_root(&self) -> u32 {
        self.first_root
    }

    /// Live PIDs that overflowed the persistent-fd pool this cycle (0 in the common case;
    /// always 0 in privileged mode — there is no fd pool).
    #[must_use]
    pub fn pool_overflow(&self) -> u32 {
        self.pool_overflow
    }

    /// Short-lived processes (born+died between cycles) caught by the BPF fork/exit events
    /// this cycle — invisible to a snapshot-only tool. Always 0 in `/proc` mode.
    #[must_use]
    pub fn short_lived(&self) -> u32 {
        self.short_lived
    }

    /// Whether a usable NVIDIA device set was discovered at startup.
    #[must_use]
    pub fn gpu_available(&self) -> bool {
        self.nvml.is_some()
    }

    #[must_use]
    pub fn gpu_process_available(&self) -> bool {
        self.nvml
            .as_ref()
            .is_some_and(NvmlSampler::process_available)
    }

    #[must_use]
    pub fn gpu_process_sample_available(&self) -> bool {
        self.gpu.available()
    }

    #[must_use]
    pub fn gpu_for_pid(&self, pid: u32) -> Option<GpuMetrics> {
        self.gpu.live(pid)
    }

    #[must_use]
    pub fn subtree_gpu_for_pid(&self, pid: u32) -> Option<GpuMetrics> {
        self.gpu.subtree(pid)
    }

    /// Empty physical-device indices, or `None` when both context lists were not available.
    #[must_use]
    pub fn empty_gpus(&self) -> Option<&[u32]> {
        self.nvml.as_ref().and_then(NvmlSampler::empty_devices)
    }

    /// Whether the privileged BPF source is active.
    #[must_use]
    #[cfg_attr(not(feature = "bpf"), allow(clippy::unused_self))] // always false without the feature
    pub fn is_privileged(&self) -> bool {
        #[cfg(feature = "bpf")]
        {
            matches!(self.source, ObservationSource::Bpf(_))
        }
        #[cfg(not(feature = "bpf"))]
        {
            false
        }
    }

    /// A row's cmdline bytes, resolved directly from the `Cmd` store (no lease).
    #[must_use]
    pub fn cmdline(&self, e: &ProcessEntry) -> &[u8] {
        self.table.cmdline(e)
    }

    /// Run one gather cycle, filling [`procs`](Self::procs) + [`sys`](Self::sys) in place. The
    /// caller renders the same buffer afterward; the two never overlap, so this needs no
    /// publish, no swap, and no GC lease — slots are freed eagerly and retired arena regions
    /// reclaimed at once (`min_live` = the just-finished generation).
    pub fn cycle(&mut self) {
        // The generation being built. It advances every cycle, in lock-step with the per-PID
        // cadence and the arena's region-retirement tagging.
        let building_gen = self.generation + 1;
        self.arena.set_gen(building_gen);

        // Source-specific fill + the matching metadata reader, followed by the common tail.
        let result = self.source.populate_and_update(
            &mut self.procs,
            &mut self.table,
            &mut self.reader,
            self.page_size,
            self.generation,
            building_gen,
        );
        self.pool_overflow = result.pool_overflow;
        self.short_lived = result.short_lived;

        // System CPU/memory plus optional GPU telemetry. Replay streams carry their own system
        // stats; live sources read the host counters here.
        #[cfg(test)]
        let replay_sys = self.source.replay_sys();
        #[cfg(not(test))]
        let replay_sys: Option<SystemStats> = None;
        if let Some(sys) = replay_sys {
            self.sys = sys;
            self.gpu.clear_unavailable();
        } else {
            self.sys_sampler.update(&mut self.sys);
            if let Some(nvml) = &mut self.nvml {
                nvml.sample(&mut self.sys, self.procs.as_slice(), &mut self.gpu);
            } else {
                self.gpu.clear_unavailable();
            }
        }
        self.first_root = tree::build(
            self.procs.as_mut_slice(),
            &mut self.tree_stack,
            &mut self.tree_order,
        );
        tree::aggregate(self.procs.as_mut_slice(), &self.tree_order, &mut self.gpu);

        self.sys.set_task_counts(self.procs.count_tasks());

        #[cfg(feature = "record")]
        if let Some(recorder) = &mut self.recorder {
            recorder.record(result.now, &self.sys, self.procs.as_slice(), &self.table);
        }

        self.generation = building_gen;

        // Reclaim any arena region a store relocate (regime B) retired this cycle. With no
        // reader leasing the old bytes (render of the prior cycle is finished, this cycle's
        // is not started), `min_live` = the current generation reclaims immediately.
        self.arena.gc(building_gen);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::gather::config::Config;

    /// Build a gatherer on the current (test) thread (the single-threaded owner): the arena +
    /// stores are `!Send`, so they are constructed where they are used. Forces the `/proc`
    /// source (`allow_bpf = false`) — these tests assert proc-path behaviour (birth probe,
    /// same-cycle death, getdents flicker) and must not flip to BPF when run under `caprun`.
    fn new_test() -> Gatherer {
        let proc_dir = ProcDir::open().expect("open /proc");
        Gatherer::with_source(crate::sys::page_size(), proc_dir, false)
    }

    /// The `/proc` source's tuning knobs, for tests that drive the enumeration cadence.
    /// Panics if the gatherer is not in `/proc` mode (it always is here — `new_test`).
    fn proc_config(g: &mut Gatherer) -> &mut Config {
        match &mut g.source {
            ObservationSource::Proc(p) => &mut p.config,
            #[cfg(feature = "bpf")]
            ObservationSource::Bpf(_) => panic!("test gatherer must be /proc mode"),
            #[cfg(test)]
            ObservationSource::Replay(_) => panic!("test gatherer must be /proc mode"),
        }
    }

    /// A process pegging a core must read as substantial per-core CPU (≫ the old
    /// `1/num_cpus` undercount). Spins a thread in *this* process and checks our own
    /// PID across a few real sample windows.
    #[test]
    fn busy_process_reads_per_core_cpu() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut x = 0u64;
            while !worker_stop.load(Ordering::Relaxed) {
                x = std::hint::black_box(x.wrapping_add(1));
            }
        });

        let mut g = new_test();
        let me = std::process::id();
        let (mut max_cpu, mut max_peak) = (0, 0);
        for _ in 0..4 {
            g.cycle();
            if let Some(p) = g.procs().as_slice().iter().find(|p| p.pid == me) {
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

    /// Full gather pipeline (enumerate → backend → cpu → tree).
    #[test]
    fn gather_fills_valid_buffer() {
        let mut g = new_test();
        g.cycle();
        let procs = g.procs().as_slice();

        assert!(g.generation >= 1);
        assert!(procs.len() > 10, "got {}", procs.len());
        assert!(procs.iter().any(|p| p.pid == 1), "pid 1 missing");
        assert_ne!(g.first_root(), NONE, "no roots");

        // PIDs sorted ascending (tree binary-search precondition).
        assert!(procs.windows(2).all(|w| w[0].pid < w[1].pid));

        // Every non-root parent index is in range and resolves to the right PID.
        for p in procs {
            if p.parent_idx != NONE {
                let parent = &procs[p.parent_idx as usize];
                assert_eq!(parent.pid, p.ppid, "parent_idx points at wrong pid");
            }
        }

        assert_eq!(g.pool_overflow(), 0, "default pool should not overflow");

        // The table filled the slow fields: pid 1 is root-owned; this test process has a
        // non-empty cmdline; kernel threads are flagged and have no cmdline.
        let init = procs.iter().find(|p| p.pid == 1).expect("pid 1");
        assert_eq!(init.uid, 0, "pid 1 is owned by root");

        let me = procs
            .iter()
            .find(|p| p.pid == std::process::id())
            .expect("self");
        assert!(!me.is_kthread, "the test process is not a kernel thread");
        assert!(!me.cmdline.is_empty(), "self should have a cmdline");

        for p in procs {
            if p.is_kthread {
                assert!(
                    p.cmdline.is_empty(),
                    "kthread {} must have empty cmdline",
                    p.pid
                );
            }
        }
    }

    /// Steady-state throughput sanity / profiling target. Ignored by default (timing
    /// is machine-dependent); run with:
    /// `cargo test -p atop --release gather_steady_state -- --ignored --nocapture`.
    /// After warmup, cached PIDs cost one stat read each — no opens, no closes.
    #[test]
    #[ignore = "timing-dependent; run manually for profiling"]
    #[allow(clippy::cast_precision_loss)] // a print, not a measurement
    fn gather_steady_state_is_cheap() {
        const ITERS: u32 = 400;
        let mut g = new_test();
        g.cycle();
        g.cycle(); // warm up the persistent-fd pool

        let start = Instant::now();
        for _ in 0..ITERS {
            g.cycle();
        }
        let elapsed = start.elapsed();
        let per_cycle_us = elapsed.as_micros() as f64 / f64::from(ITERS);
        eprintln!(
            "pids={} kthreads={} overflow={} per_cycle={per_cycle_us:.1}µs",
            g.procs().as_slice().len(),
            g.procs().as_slice().iter().filter(|p| p.is_kthread).count(),
            g.pool_overflow(),
        );
        assert_eq!(g.pool_overflow(), 0, "default pool should not overflow");
    }

    /// cmdline survives the coarse cadence: a PID not refreshed this cycle keeps its
    /// persistent `Cmd`-store slot, still resolvable directly from the store.
    #[test]
    fn cmdline_persists_across_coarse_cycles() {
        let mut g = new_test();
        let me = std::process::id();
        g.cycle();
        let cmd1 = {
            let p = *g
                .procs()
                .as_slice()
                .iter()
                .find(|p| p.pid == me)
                .expect("self c1");
            String::from_utf8_lossy(g.cmdline(&p)).into_owned()
        };
        assert!(!cmd1.is_empty(), "self cmdline should be present");

        // A second cycle (no time for argv to change) must still resolve the cmdline from
        // the persistent slot — no re-copy.
        g.cycle();
        let cmd2 = {
            let p = *g
                .procs()
                .as_slice()
                .iter()
                .find(|p| p.pid == me)
                .expect("self c2");
            String::from_utf8_lossy(g.cmdline(&p)).into_owned()
        };
        assert_eq!(cmd1, cmd2, "cmdline must persist across cycles");
    }

    /// The win: an unchanged cmdline is **not** re-interned each cycle, so the `Cmd` store's
    /// high-water slot count stays proportional to live processes, not to cycle count. If
    /// the old per-cycle re-materialization were still happening (or unchanged strings were
    /// re-interned), the store would grow by ~one slot per userspace PID per cycle.
    #[test]
    fn cmd_store_does_not_grow_per_cycle() {
        let mut g = new_test();
        let me = std::process::id();

        // Warm up: settle cmdlines and let births/deaths reach steady state.
        for _ in 0..6 {
            g.cycle();
        }
        let slots_warm = g.table.cmd_slot_count();
        let userspace = g
            .procs()
            .as_slice()
            .iter()
            .filter(|p| !p.is_kthread)
            .count();

        for _ in 0..30 {
            g.cycle();
        }
        let slots_after = g.table.cmd_slot_count();

        // Slack covers genuine churn (new userspace PIDs over ~18 s); the disaster mode
        // would be `userspace × 30` extra slots. (Freed slots are also reused now, so this is
        // an even tighter bound than under the old demote/gc lease.)
        let growth = slots_after - slots_warm;
        assert!(
            growth <= userspace.max(64),
            "Cmd store grew by {growth} slots over 30 cycles (userspace pids ≈ {userspace}); \
             unchanged cmdlines are being re-interned"
        );

        // And the latest buffer still resolves self correctly.
        let p = *g
            .procs()
            .as_slice()
            .iter()
            .find(|p| p.pid == me)
            .expect("self");
        assert!(!g.cmdline(&p).is_empty(), "self cmdline must resolve");
    }

    /// §3a: a sequentially-allocated new process is caught by the skip-cycle birth probe
    /// within **1** cycle — not only at the next full `getdents` scan. Full scans are
    /// disabled (`enum_every` huge) so the birth can *only* be found by the probe.
    #[test]
    fn birth_probe_catches_sequential_birth_within_one_cycle() {
        let mut g = new_test();
        proc_config(&mut g).enum_every = 1_000_000; // no full re-scan → births only via probe
        proc_config(&mut g).probe_width = 1_000_000; // window spans (max_live, ns_last_pid]
        g.cycle(); // generation 0 → full scan seeds the maintained live set
        assert!(
            g.procs().as_slice().iter().any(|p| p.pid == 1),
            "set seeded"
        );

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id();

        g.cycle(); // generation 1 → SKIP cycle: probe must discover the new child
        let caught = g.procs().as_slice().iter().any(|p| p.pid == child_pid);

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            caught,
            "sequential birth (pid {child_pid}) must be caught by the probe within 1 cycle"
        );
    }

    /// §3b + cadence: a process that dies vanishes the **same** (skip) cycle via the pool
    /// `ESRCH` → re-tombstone → compact → survivor-rebuild path — it never lingers until
    /// the next full scan, and no phantom row is left behind.
    #[test]
    fn death_caught_same_cycle_on_skip() {
        let mut g = new_test();
        proc_config(&mut g).enum_every = 1_000_000; // force skip cycles after the first
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id();

        g.cycle(); // generation 0 → full scan sees the (still-live) child
        assert!(
            g.procs().as_slice().iter().any(|p| p.pid == child_pid),
            "child must be observed while alive"
        );

        child.kill().unwrap();
        child.wait().unwrap();

        g.cycle(); // generation 1 → SKIP cycle: held read ESRCH ⇒ death caught now
        let procs = g.procs().as_slice();
        assert!(
            !procs.iter().any(|p| p.pid == child_pid),
            "death must be caught the same skip cycle, not deferred to the next full scan"
        );
        assert!(
            procs.iter().all(|p| p.state != b'?'),
            "no phantom row may survive the death"
        );
    }

    /// Full privileged cycle through the real [`Gatherer`]: source selection picks BPF, and the
    /// source-agnostic tail (CPU%, cmdline via `/proc`, tree build) runs over the iterator's
    /// rows. In particular the BPF-provided uid must survive the cmdline refresh (the
    /// `source_provides_uid` guard) and the cmdline must still resolve. Skips without caps;
    /// run under `tools/caprun cargo test`.
    #[test]
    fn bpf_mode_end_to_end() {
        let proc_dir = ProcDir::open().expect("open /proc");
        let mut g = Gatherer::with_source(crate::sys::page_size(), proc_dir, true);
        if !g.is_privileged() {
            eprintln!("not privileged (no caps?) — skipping bpf end-to-end");
            return;
        }

        g.cycle();
        g.cycle(); // second cycle exercises CPU% windowing + cmdline cadence in bpf mode

        let procs = g.procs().as_slice();
        assert!(procs.iter().any(|p| p.pid == 1), "pid 1 missing");
        assert!(
            procs.windows(2).all(|w| w[0].pid < w[1].pid),
            "rows must be PID-sorted"
        );
        assert_ne!(g.first_root(), NONE, "tree must have a root");
        assert_eq!(g.pool_overflow(), 0, "no fd pool in bpf mode");

        let me = std::process::id();
        let mine = *procs.iter().find(|p| p.pid == me).expect("self");
        assert_eq!(
            mine.uid,
            unsafe { libc::getuid() },
            "uid kept from the iterator"
        );
        assert!(
            !g.cmdline(&mine).is_empty(),
            "cmdline must resolve via /proc in bpf mode"
        );

        // Every non-root parent index resolves to the right PID (tree integrity).
        for p in procs {
            if p.parent_idx != NONE {
                assert_eq!(procs[p.parent_idx as usize].pid, p.ppid);
            }
        }
    }
}
