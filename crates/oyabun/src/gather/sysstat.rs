//! Per-cycle system-wide sampler: windowed CPU, pressure, swap-in rate, plus the memory
//! snapshot and the per-interval CPU history the header graph draws.
//!
//! CPU, pressure, and swap-in are rates, so they need earlier cumulative counters; this holds
//! that carried state. Everything else is read fresh each cycle. The result lands in the
//! cycle's [`SystemStats`].
//!
//! **A short window, not one interval and not a long average.** A bare `cur − prev` delta at
//! the 500 ms / 100 Hz interval resolves only ~50 jiffies per core-second, so one burst
//! dominates a sample then vanishes. Averaging over ~10 s instead smears a step into a ramp
//! and hides what is happening *now*. The headline numbers use a ~2 s window — long enough to
//! cancel quantization, short enough to settle quickly — and the graph's per-interval cells
//! carry the history. O(1) per sample: store the snapshot, subtract the endpoints.
//!
//! **Time comes from the caller.** Pressure is a stall *duration* over wall time, so each
//! sample carries a monotonic timestamp supplied with the snapshot. Replay supplies the
//! recorded wall clock, so a replayed pressure reading is identical to the live one.

use std::time::Instant;

use super::config::REFRESH_MS;
use super::cpu::MIN_SAMPLE;
use super::cpu_history::{CpuCell, CpuHistory};
use super::ring::Ring;
use crate::procs::{CpuWait, SystemStats};
use crate::sys::{self, RawCpuCounters, RawPsi, RawSystemSnapshot};

/// Averaging window for the headline numbers, as wall-clock time.
const WINDOW_MS: u64 = 2_000;
/// Snapshot ring depth: one sample per window interval. The sample being processed is the
/// near endpoint and is pushed after the rates are taken, so the ring's oldest is exactly
/// one window back.
const WINDOW_SAMPLES: usize = {
    let intervals = (WINDOW_MS / REFRESH_MS) as usize;
    if intervals < 1 { 1 } else { intervals }
};

/// [`MIN_SAMPLE`] in the sampler's timestamp unit.
#[allow(clippy::cast_possible_truncation)] // 100 ms in µs
const MIN_SAMPLE_US: u64 = MIN_SAMPLE.as_micros() as u64;

/// One cycle's cumulative readings, stamped with when they were taken.
#[derive(Clone, Copy, Default)]
struct Sample {
    /// Microseconds since the sampler's first sample.
    t_us: u64,
    cpu: RawCpuCounters,
    psi: RawPsi,
    pswpin: Option<u64>,
    runnable: u32,
}

/// Pressure over an interval, per resource: share of the interval some task was stalled, in
/// basis points. `None` where the kernel has no PSI for that resource.
struct Stalls {
    cpu: Option<u32>,
    memory: Option<u32>,
    io: Option<u32>,
}

impl Sample {
    fn elapsed_us_since(&self, older: &Sample) -> u64 {
        self.t_us.saturating_sub(older.t_us).max(1)
    }

    fn stalls_since(&self, older: &Sample) -> Stalls {
        let elapsed = self.elapsed_us_since(older);
        let share = |new: Option<u64>, old: Option<u64>| {
            new.map(|n| {
                let stalled = n.wrapping_sub(old.unwrap_or(n));
                u32::try_from((stalled.saturating_mul(10_000) / elapsed).min(10_000))
                    .unwrap_or(10_000)
            })
        };
        Stalls {
            cpu: share(self.psi.cpu, older.psi.cpu),
            memory: share(self.psi.memory, older.psi.memory),
            io: share(self.psi.io, older.psi.io),
        }
    }

    /// Millicores of a jiffy-counter delta between `older` and `self`: the delta as a share of
    /// all accounted jiffies, times the core count. The jiffy clock is its own denominator, so
    /// no wall time is involved.
    fn millicores(&self, older: &Sample, cores: u32, pick: fn(&RawCpuCounters) -> u64) -> u32 {
        let d_total = self.cpu.total().wrapping_sub(older.cpu.total()).max(1);
        let d = pick(&self.cpu).wrapping_sub(pick(&older.cpu));
        u32::try_from(d.saturating_mul(u64::from(cores) * 1000) / d_total).unwrap_or(u32::MAX)
    }
}

/// Samples system-wide stats once per cycle, carrying a ring of recent cumulative readings so
/// the displayed rates are window averages rather than one jumpy interval.
pub(crate) struct SystemSampler {
    /// Recent samples; the ring's oldest is the window's far endpoint.
    ring: Ring<Sample, WINDOW_SAMPLES>,
    /// Origin of [`Sample::t_us`]: the first sample's timestamp.
    epoch: Option<Instant>,
    num_cores: u32,
    page_size: u64,
    history: CpuHistory,
}

impl SystemSampler {
    pub(crate) fn new() -> Self {
        let num_cores = sys::num_cpus();
        Self {
            ring: Ring::new(),
            epoch: None,
            num_cores,
            page_size: sys::page_size(),
            history: CpuHistory::new(num_cores),
        }
    }

    /// Override the core count (replay streams carry their own `num_cores`).
    pub(crate) fn set_num_cores(&mut self, n: u32) {
        self.num_cores = n;
        self.history.set_num_cores(n);
    }

    pub(crate) fn history(&self) -> &CpuHistory {
        &self.history
    }

    pub(crate) fn history_mut(&mut self) -> &mut CpuHistory {
        &mut self.history
    }

    /// Fill `sys` from the provided raw snapshot taken at `now`, and push the interval's cell
    /// onto the history. The first call has no earlier sample to difference against, so it only
    /// seeds the ring (rates read zero; pressure *availability* is still reported).
    pub(crate) fn update(&mut self, sys: &mut SystemStats, snap: RawSystemSnapshot, now: Instant) {
        let epoch = *self.epoch.get_or_insert(now);
        let cur = Sample {
            t_us: u64::try_from(now.saturating_duration_since(epoch).as_micros())
                .unwrap_or(u64::MAX),
            cpu: snap.cpu,
            psi: snap.psi,
            pswpin: snap.pswpin,
            runnable: snap.runnable,
        };
        sys.num_cores = self.num_cores;
        sys.cpu_has_steal = cur.cpu.steal > 0;

        // A too-soon cycle (e.g. the forced one after a kill) would put a sub-jiffy-quantized
        // interval into the ring and the graph, shorten the 2 s window, and break "one cell =
        // one refresh". Like the per-process table, carry the rate fields forward untouched and
        // refresh only the instantaneous ones.
        let too_soon = self
            .ring
            .recent_index(1)
            .is_some_and(|i| cur.t_us.saturating_sub(self.ring.slot(i).t_us) < MIN_SAMPLE_US);
        if !too_soon {
            self.sample_rates(sys, cur);
        }

        let mem = snap.mem;
        sys.mem_total = mem.total;
        sys.mem_available = mem.available;
        sys.mem_free = mem.free;
        sys.mem_shmem = mem.shmem;
        sys.swap_total = mem.swap_total;
        sys.swap_used = mem.swap_total.saturating_sub(mem.swap_free);
    }

    /// Window rates and the graph cell for a real (not too-soon) sample, which is then pushed.
    fn sample_rates(&mut self, sys: &mut SystemStats, cur: Sample) {
        let cores = self.num_cores;
        // Rates over the window; without an earlier sample, zero (or "available, no stalls").
        let (window, stalls) = match self.ring.oldest() {
            Some(old) => (Some(old), cur.stalls_since(&old)),
            None => (None, cur.stalls_since(&cur)),
        };
        // Runnable work averaged over the same window as the other headline numbers: the
        // instantaneous count flips every cycle, which would repaint the line each time.
        let run_avg = self.runnable_avg(cur.runnable);
        sys.cpu_busy_mc = window.map_or(0, |o| cur.millicores(&o, cores, RawCpuCounters::busy));
        sys.cpu_kernel_mc = window.map_or(0, |o| cur.millicores(&o, cores, RawCpuCounters::kernel));
        sys.cpu_steal_mc = window.map_or(0, |o| cur.millicores(&o, cores, |c| c.steal));
        sys.cpu_wait = stalls
            .cpu
            .map_or(CpuWait::RunQueue(run_avg), CpuWait::Pressure);
        sys.mem_wait_bp = stalls.memory;
        sys.io_wait_bp = stalls.io;
        sys.swap_in_bps = window.map_or(0, |o| {
            // Absent counter, or one that went backwards (reset): no rate, never a wrapped one.
            let (Some(new), Some(old_pages)) = (cur.pswpin, o.pswpin) else {
                return 0;
            };
            new.saturating_sub(old_pages)
                .saturating_mul(self.page_size)
                .saturating_mul(1_000_000)
                / cur.elapsed_us_since(&o)
        });

        // The graph's cell covers just the newest interval.
        if let Some(prev) = self.ring.recent_index(1).map(|i| self.ring.slot(i)) {
            let pressure_bp = cur.stalls_since(&prev).cpu.unwrap_or_else(|| {
                // No PSI: how far the run queue overshoots the core count.
                let over = u64::from(run_avg.saturating_sub(cores));
                u32::try_from((over * 10_000 / u64::from(cores.max(1))).min(10_000))
                    .unwrap_or(10_000)
            });
            self.history.push(CpuCell {
                busy_mc: cur.millicores(&prev, cores, RawCpuCounters::busy),
                pressure_bp,
            });
        }
        self.ring.push(cur);
    }

    /// Mean runnable count over the window's samples plus `latest` (not yet in the ring),
    /// rounded to nearest.
    fn runnable_avg(&self, latest: u32) -> u32 {
        let mut sum = u64::from(latest);
        let mut n = 1u64;
        for k in 1..=self.ring.len() {
            if let Some(i) = self.ring.recent_index(k) {
                sum += u64::from(self.ring.slot(i).runnable);
                n += 1;
            }
        }
        u32::try_from((sum + n / 2) / n).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::sys::MemInfo;

    /// A 4-core machine; counters in jiffies (100 Hz => 400 jiffies/s total).
    fn snap(user: u64, system: u64, steal: u64, idle: u64) -> RawSystemSnapshot {
        RawSystemSnapshot {
            cpu: RawCpuCounters {
                user,
                system,
                idle,
                steal,
                ..RawCpuCounters::default()
            },
            runnable: 1,
            mem: MemInfo::default(),
            psi: RawPsi::default(),
            pswpin: Some(0),
        }
    }

    fn sampler() -> (SystemSampler, Instant) {
        let mut s = SystemSampler::new();
        s.set_num_cores(4);
        (s, Instant::now())
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn busy_kernel_and_steal_are_in_cores_over_the_window() {
        let (mut s, t0) = sampler();
        let mut sys = SystemStats::default();
        s.update(&mut sys, snap(0, 0, 0, 1000), t0);
        // 1 s later: 400 jiffies total = user 120 + system 80 + steal 40 + idle 160.
        s.update(&mut sys, snap(120, 80, 40, 1160), at(t0, 1000));
        assert_eq!(sys.cpu_busy_mc, 2000, "(120+80)/400 of 4 cores");
        assert_eq!(sys.cpu_kernel_mc, 800);
        assert_eq!(sys.cpu_steal_mc, 400, "steal is reported, not counted busy");
    }

    #[test]
    fn pressure_is_stall_time_over_wall_time() {
        let (mut s, t0) = sampler();
        let mut sys = SystemStats::default();
        let with_psi = |cpu: u64, mem: Option<u64>| {
            let mut x = snap(0, 0, 0, 0);
            x.psi = RawPsi {
                cpu: Some(cpu),
                memory: mem,
                io: None,
            };
            x
        };
        s.update(&mut sys, with_psi(1_000_000, Some(5)), t0);
        assert_eq!(
            sys.cpu_wait,
            CpuWait::Pressure(0),
            "available but not yet measured"
        );
        // 2 s later, CPU stalled 200 ms (10%), memory 0, IO file absent.
        s.update(&mut sys, with_psi(1_200_000, Some(5)), at(t0, 2000));
        assert_eq!(sys.cpu_wait, CpuWait::Pressure(1000));
        assert_eq!(sys.mem_wait_bp, Some(0));
        assert_eq!(sys.io_wait_bp, None, "absent PSI stays absent, not zero");
    }

    #[test]
    fn without_psi_cpu_waits_are_the_run_queue() {
        let (mut s, t0) = sampler();
        let mut sys = SystemStats::default();
        let mut x = snap(0, 0, 0, 100);
        x.runnable = 9;
        s.update(&mut sys, x, t0);
        assert_eq!(sys.cpu_wait, CpuWait::RunQueue(9));
        assert_eq!(sys.mem_wait_bp, None);
    }

    #[test]
    fn window_spans_two_seconds_not_the_whole_history() {
        let (mut s, t0) = sampler();
        let mut sys = SystemStats::default();
        // 10 intervals: busy for the first 4 only; the rest idle. The window is the last 4.
        let mut user = 0;
        let mut idle = 0;
        s.update(&mut sys, snap(0, 0, 0, 0), t0);
        for i in 1..=10u64 {
            if i <= 4 {
                user += 200;
            } else {
                idle += 200;
            }
            s.update(&mut sys, snap(user, 0, 0, idle), at(t0, i * 500));
        }
        assert_eq!(sys.cpu_busy_mc, 0, "the burst is older than the 2 s window");
    }

    #[test]
    fn swap_in_rate_is_bytes_per_second() {
        let (mut s, t0) = sampler();
        let mut sys = SystemStats::default();
        s.update(&mut sys, snap(0, 0, 0, 0), t0);
        let mut x = snap(0, 0, 0, 0);
        x.pswpin = Some(100);
        s.update(&mut sys, x, at(t0, 2000));
        assert_eq!(sys.swap_in_bps, 100 * s.page_size / 2);
    }

    #[test]
    fn each_interval_pushes_one_graph_cell() {
        let (mut s, t0) = sampler();
        s.history_mut().set_visible(8);
        let mut sys = SystemStats::default();
        s.update(&mut sys, snap(0, 0, 0, 0), t0);
        assert_eq!(s.history().visible_cells().count(), 0, "seed only");
        s.update(&mut sys, snap(100, 0, 0, 100), at(t0, 500));
        let cells: Vec<_> = s.history().visible_cells().collect();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].busy_mc, 2000, "100 of 200 jiffies on 4 cores");
    }

    #[test]
    fn run_queue_overload_colors_cells_when_psi_is_absent() {
        let (mut s, t0) = sampler();
        s.history_mut().set_visible(8);
        let mut sys = SystemStats::default();
        let seed = |runnable| {
            let mut x = snap(0, 0, 0, 0);
            x.runnable = runnable;
            x
        };
        s.update(&mut sys, seed(6), t0);
        let mut x = snap(10, 0, 0, 10);
        x.runnable = 6; // steady 6 on 4 cores: 2 over = 50% overload
        s.update(&mut sys, x, at(t0, 500));
        assert_eq!(
            s.history().visible_cells().next().unwrap().pressure_bp,
            5000
        );
    }

    #[test]
    fn too_soon_update_leaves_ring_graph_and_rates_alone() {
        let (mut s, t0) = sampler();
        s.history_mut().set_visible(8);
        let mut sys = SystemStats::default();
        s.update(&mut sys, snap(0, 0, 0, 0), t0);
        s.update(&mut sys, snap(100, 0, 0, 100), at(t0, 500));
        let busy = sys.cpu_busy_mc;
        assert_eq!(s.history().visible_len(), 1);

        // A forced gather 20 ms later: memory is refreshed, the sampled state is untouched.
        let mut x = snap(101, 0, 0, 100);
        x.mem.available = 123;
        s.update(&mut sys, x, at(t0, 520));
        assert_eq!(s.history().visible_len(), 1, "no bogus graph cell");
        assert_eq!(sys.cpu_busy_mc, busy, "rates carried forward");
        assert_eq!(sys.mem_available, 123, "instantaneous fields still refresh");

        // The next regular sample differences against the last *real* one, not the too-soon one.
        s.update(&mut sys, snap(200, 0, 0, 200), at(t0, 1000));
        assert_eq!(s.history().visible_len(), 2);
        let last = s.history().visible_cells().last().unwrap();
        assert_eq!(last.busy_mc, 2000, "100 of 200 jiffies on 4 cores");
    }

    #[test]
    fn run_queue_is_averaged_over_the_window() {
        let (mut s, t0) = sampler();
        let mut sys = SystemStats::default();
        for (i, runnable) in [0u32, 4, 0, 4, 0].into_iter().enumerate() {
            let mut x = snap(0, 0, 0, 100 * i as u64);
            x.runnable = runnable;
            s.update(&mut sys, x, at(t0, i as u64 * 500));
        }
        // Samples 0,4,0,4,0 over the 2 s window: a steady 1.6, shown as 2 — not 0 or 4.
        assert_eq!(sys.cpu_wait, CpuWait::RunQueue(2));
    }

    #[test]
    fn swap_in_rate_is_absent_or_zero_never_wrapped() {
        let (mut s, t0) = sampler();
        let mut sys = SystemStats::default();
        let mut x = snap(0, 0, 0, 0);
        x.pswpin = Some(1000);
        s.update(&mut sys, x, t0);
        // Counter went backwards (reset): no rate, not ~2^64 pages.
        x.pswpin = Some(10);
        s.update(&mut sys, x, at(t0, 1000));
        assert_eq!(sys.swap_in_bps, 0);
        // Unreadable now: no rate either.
        x.pswpin = None;
        s.update(&mut sys, x, at(t0, 2000));
        assert_eq!(sys.swap_in_bps, 0);
    }
}
