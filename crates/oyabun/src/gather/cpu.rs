//! Per-process CPU%: a per-core rate over real elapsed time (`top`/`htop` "Irix mode"),
//! computed from a bounded ring of recent sample windows with exact integer math.
//!
//! One full core = 10000 basis points (100%); a multi-threaded process can exceed it.
//! The displayed number is a short box average sized only to cancel the inherent ±1-jiffy
//! quantization; *history* is the sparkline's job, served from the same ring
//! ([`CpuRing::series`]); the ring's depth is set by the sparkline length, not by the average.
//! The ring is indexed by sampling cycle: cycles a PID skipped are
//! zero-filled lazily, and a ring holding only zeros is never touched (quiescent).
//!
//! Each interval also carries the time the process spent runnable but queued for a CPU
//! (scheduler wait), in the same clock ticks as the run time so the two divide exactly. Wait
//! may be *unknown* for an interval (the source did not measure it): that is a state of its
//! own, never a zero. A ring is quiescent only when both run and wait are zero across it — an
//! interval that did not run but waited is not idle.

use std::time::Duration;

use super::config::REFRESH_MS;
use super::ring::Ring;

/// Below this elapsed interval the rate is too quantized (sub-jiffy) to be
/// meaningful — carry the windowed values forward instead of sampling.
pub(crate) const MIN_SAMPLE: Duration = Duration::from_millis(100);

/// Wall-clock width of the CPU% box average (and of the display-state "R" override, see
/// [`CpuRing::had_ticks`]): short enough that steps settle quickly and an idle process
/// reverts to its real state within a couple of seconds, long enough to damp ±1-jiffy noise.
const AVG_WINDOW_MS: u64 = 2_000;
/// [`AVG_WINDOW_MS`] in samples at the current refresh interval.
pub(crate) const AVG_SAMPLES: usize = {
    let n = (AVG_WINDOW_MS / REFRESH_MS) as usize;
    if n == 0 { 1 } else { n }
};

/// Cells of the per-process CPU sparkline; one cell is one sample interval.
pub(crate) const SPARK_CELLS: usize = 8;

/// Ring depth: the sparkline is the longest consumer (the average window is shorter).
const DEPTH: usize = if SPARK_CELLS > AVG_SAMPLES {
    SPARK_CELLS
} else {
    AVG_SAMPLES
};

/// Scheduler wait against the run time it is compared with, summed over any set of processes
/// and intervals (clock ticks). `run` counts only processes whose wait was *measured*, so
/// the share is over what was observed: a process with unknown wait adds to neither side
/// rather than diluting the share with run time that has no wait attached.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Waiting {
    /// Ticks spent runnable but queued for a CPU.
    pub(crate) wait: u32,
    /// Ticks spent on a CPU, of the processes whose wait was measured.
    pub(crate) run: u32,
}

impl Waiting {
    pub(crate) fn add(&mut self, o: Self) {
        self.wait = self.wait.saturating_add(o.wait);
        self.run = self.run.saturating_add(o.run);
    }

    /// The share of the time the work wanted a CPU that it spent waiting for one, basis
    /// points; `None` when nothing was measured (no run, no wait).
    pub(crate) fn share_bp(self) -> Option<u32> {
        let total = u64::from(self.wait) + u64::from(self.run);
        (total > 0).then(|| u32::try_from(u64::from(self.wait) * 10_000 / total).unwrap_or(10_000))
    }
}

/// One measured interval: tick delta, the per-core jiffies it spanned, and the scheduler wait
/// in the same ticks (`None` = not measured).
#[derive(Clone, Copy, Default)]
struct Sample {
    ticks: u32,
    jiff: u32,
    wait: Option<u32>,
}

impl Sample {
    fn rate(self) -> u32 {
        rate(u64::from(self.ticks), u64::from(self.jiff))
    }

    fn waiting(self) -> Waiting {
        self.wait.map_or_else(Waiting::default, |wait| Waiting {
            wait,
            run: self.ticks,
        })
    }
}

/// One sparkline cell: the interval's CPU rate (its height) and its scheduler wait (its color).
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Debug)]
pub(crate) struct SeriesCell {
    /// Basis points of one core.
    pub(crate) rate: u32,
    pub(crate) waiting: Waiting,
}

impl SeriesCell {
    pub(crate) fn add(&mut self, o: Self) {
        self.rate = self.rate.saturating_add(o.rate);
        self.waiting.add(o.waiting);
    }

    /// Remove a contribution previously [`add`](Self::add)ed (a member leaving the set).
    pub(crate) fn sub(&mut self, o: Self) {
        self.rate = self.rate.saturating_sub(o.rate);
        self.waiting.wait = self.waiting.wait.saturating_sub(o.waiting.wait);
        self.waiting.run = self.waiting.run.saturating_sub(o.waiting.run);
    }

    /// Whether the cell's bar has anything to show: it ran, or it waited without running.
    pub(crate) fn is_active(self) -> bool {
        self.rate > 0 || self.waiting.wait > 0
    }
}

/// Per-interval cells, oldest → newest, of one process's recent history: what the
/// sparkline draws. One `Copy`/`Hash` value so the renderer can key a cell on it.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Debug)]
pub(crate) struct CpuSeries(pub(crate) [SeriesCell; SPARK_CELLS]);

impl CpuSeries {
    /// The rates alone, oldest → newest.
    #[cfg(test)]
    pub(crate) fn rates(&self) -> [u32; SPARK_CELLS] {
        self.0.map(|c| c.rate)
    }
}

/// What one [`CpuRing::sample`] call observed.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Sampled {
    /// Rate (bp) of this cycle's interval; 0 when no interval was measured.
    pub(crate) rate: u32,
    /// This cycle's scheduler wait; empty when no interval was measured or wait is unknown.
    pub(crate) waiting: Waiting,
    /// Whether the ring was written (false ⇒ quiescent or nothing measured).
    pub(crate) wrote: bool,
}

/// Bounded per-PID CPU history: a ring of the last [`DEPTH`] intervals plus exact `u64`
/// running sums over the newest [`AVG_SAMPLES`] (the displayed average) and over the whole
/// ring (to detect quiescence).
///
/// `Flat` (`Copy`, no heap) so it lives on huge pages in a `GenStore<CpuRing>`. It is the
/// **hot** per-PID record — touched on every sample — and is deliberately a *separate* store
/// from the cold `PidMeta` so a CPU update loads only this, not metadata cache lines.
/// Liveness/incarnation bookkeeping (`seen_gen`, `start_time`) lives in `PidSlot`, not here.
#[derive(Clone, Copy)]
pub(crate) struct CpuRing {
    /// `utime + stime` at the last sample.
    prev_ticks: u64,
    /// Cumulative scheduler wait (ticks) at the last sample; `None` when the last cycle did not
    /// measure it, so the next reading only re-baselines (a delta spanning unmeasured
    /// intervals would be attributed to one).
    prev_wait: Option<u64>,
    /// The recent per-interval samples (storage + wraparound handled by [`Ring`]).
    ring: Ring<Sample, DEPTH>,
    /// Sampling cycle of the newest ring entry. Cycles after it were skipped as quiescent
    /// and are implicitly zero.
    last_cycle: u64,
    /// Sums over the newest [`AVG_SAMPLES`] entries.
    avg_ticks: u64,
    avg_jiff: u64,
    /// Tick sum over the whole ring.
    ring_ticks: u64,
    /// Measured-wait sum over the whole ring. With `ring_ticks`: both 0 ⇔ every recorded
    /// interval was idle.
    ring_wait: u64,
}

/// What a ring last saw of its process: the cumulative counters its next delta is taken from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CpuBaseline {
    /// `utime + stime` ticks at the last sample.
    pub(crate) ticks: u64,
    /// Cumulative scheduler wait ticks at the last sample, `None` when it was not measured.
    pub(crate) wait: Option<u64>,
}

impl CpuRing {
    pub(crate) fn baseline(&self) -> CpuBaseline {
        CpuBaseline {
            ticks: self.prev_ticks,
            wait: self.prev_wait,
        }
    }

    pub(crate) fn new(prev_ticks: u64, prev_wait: Option<u64>, cycle: u64) -> Self {
        Self {
            prev_ticks,
            prev_wait,
            ring: Ring::new(),
            last_cycle: cycle,
            avg_ticks: 0,
            avg_jiff: 0,
            ring_ticks: 0,
            ring_wait: 0,
        }
    }

    /// Discard history (PID reuse / counter reset), re-baselining at the current counters.
    fn reset(&mut self, ticks: u64, wait: Option<u64>, cycle: u64) {
        *self = Self::new(ticks, wait, cycle);
    }

    /// Fold cycle `cycle`'s observation into the ring. `reused` ⇒ a new incarnation took this
    /// PID, so discard the dead one's history. Otherwise, given a real measurement window
    /// (`jiff`) for an already-`known` PID, take the tick delta — or reset if the counter went
    /// backwards (wrap). A brand-new PID just keeps its baseline (first real rate lands next
    /// interval); a too-soon/first cycle (no window) carries the windowed values forward.
    ///
    /// `wait` is the cumulative scheduler wait in ticks, `None` when this cycle did not measure
    /// it. Its delta needs a baseline from the *previous* cycle; a counter that went backwards
    /// (a thread exited and took its wait with it) is a zero delta, not a reset.
    ///
    /// An idle interval (no run, no wait) on a ring that holds only zeros writes nothing
    /// (quiescent). Otherwise the cycles skipped since the last write are zero-filled first,
    /// then the delta pushed.
    pub(crate) fn sample(
        &mut self,
        ticks: u64,
        wait: Option<u64>,
        jiff: Option<u32>,
        cycle: u64,
        reused: bool,
        known: bool,
    ) -> Sampled {
        if reused {
            self.reset(ticks, wait, cycle);
            return Sampled::default();
        }
        let Some(j) = jiff else {
            return Sampled::default();
        };
        if !known {
            self.last_cycle = cycle;
            return Sampled::default();
        }
        if ticks < self.prev_ticks {
            self.reset(ticks, wait, cycle);
            return Sampled::default();
        }
        let delta = u32::try_from(ticks - self.prev_ticks).unwrap_or(u32::MAX);
        self.prev_ticks = ticks;
        let wait_delta = match (wait, self.prev_wait) {
            (Some(now), Some(before)) => {
                Some(u32::try_from(now.saturating_sub(before)).unwrap_or(u32::MAX))
            }
            _ => None,
        };
        self.prev_wait = wait;
        let rate = rate(u64::from(delta), u64::from(j));
        let waiting = Sample {
            ticks: delta,
            jiff: j,
            wait: wait_delta,
        }
        .waiting();
        if delta == 0 && wait_delta.unwrap_or(0) == 0 && self.ring_ticks == 0 && self.ring_wait == 0
        {
            return Sampled {
                rate,
                waiting,
                wrote: false,
            };
        }
        let gap = cycle.saturating_sub(self.last_cycle).saturating_sub(1);
        for _ in 0..gap.min(DEPTH as u64) {
            self.push(0, j, None);
        }
        self.push(delta, j, wait_delta);
        self.last_cycle = cycle;
        Sampled {
            rate,
            waiting,
            wrote: true,
        }
    }

    fn push(&mut self, ticks: u32, jiff: u32, wait: Option<u32>) {
        // The sample falling out of the short window is the AVG_SAMPLES-th newest *before*
        // this push (when the ring is at least that long).
        if let Some(i) = self.ring.recent_index(AVG_SAMPLES) {
            let s = self.ring.slot(i);
            self.avg_ticks -= u64::from(s.ticks);
            self.avg_jiff -= u64::from(s.jiff);
        }
        if let Some(evicted) = self.ring.push(Sample { ticks, jiff, wait }) {
            self.ring_ticks -= u64::from(evicted.ticks);
            self.ring_wait -= u64::from(evicted.wait.unwrap_or(0));
        }
        self.ring_ticks += u64::from(ticks);
        self.ring_wait += u64::from(wait.unwrap_or(0));
        self.avg_ticks += u64::from(ticks);
        self.avg_jiff += u64::from(jiff);
    }

    /// Box average over the newest [`AVG_SAMPLES`] intervals (exact integer math).
    pub(crate) fn avg(&self) -> u32 {
        rate(self.avg_ticks, self.avg_jiff)
    }

    /// Whether any of the last `n` pushed samples recorded nonzero CPU ticks — i.e. the
    /// process executed at some point in that recent window. Drives the display-state "R"
    /// override: a point-sampled state answers "on-CPU this microsecond?", which flickers;
    /// this answers "active recently?", which is stable. `n` is clamped to the samples held,
    /// so a young ring (fewer than `n` samples) just examines what it has.
    pub(crate) fn had_ticks(&self, n: usize) -> bool {
        let count = n.min(self.ring.len());
        (1..=count).any(|k| {
            let i = self.ring.recent_index(k).expect("k ≤ len");
            self.ring.slot(i).ticks > 0
        })
    }

    /// The last [`SPARK_CELLS`] intervals as of sampling cycle `cycle`, oldest → newest.
    /// Cycles after the ring's last write were skipped as quiescent, so they read as zero —
    /// a skipped ring never shows stale bars.
    pub(crate) fn series(&self, cycle: u64) -> CpuSeries {
        let mut out = [SeriesCell::default(); SPARK_CELLS];
        let skipped = usize::try_from(cycle.saturating_sub(self.last_cycle)).unwrap_or(usize::MAX);
        // Cell `c` counted from the newest is the `(c + 1 - skipped)`-th newest ring entry.
        for (c, slot) in out.iter_mut().rev().enumerate() {
            let Some(k) = (c + 1).checked_sub(skipped).filter(|k| *k > 0) else {
                continue;
            };
            if let Some(i) = self.ring.recent_index(k) {
                let sample = self.ring.slot(i);
                *slot = SeriesCell {
                    rate: sample.rate(),
                    waiting: sample.waiting(),
                };
            }
        }
        CpuSeries(out)
    }
}

/// Nanoseconds per clock tick — the divisor turning kernel ns into `/proc`-style jiffies.
/// Computed **once** from `CLK_TCK` (constant for the process) and stored by each source, so the
/// per-row conversion is a single division, not this plus the `1e9/CLK_TCK` division each time.
/// `CLK_TCK` is ~always 100 (⇒ 10 ms), where the division is exact, matching the kernel's
/// `nsec_to_clock_t`. A non-dividing `CLK_TCK` rounds slightly; immaterial to CPU% (already
/// ±1-jiffy quantized) and to start-time identity (a mismatch only makes `kill` refuse — safe).
///
/// If profiling ever shows the per-row `ns / ns_per_tick` division is hot (unlikely: ~2 per
/// process per cycle at a ~2 Hz interval), the divisor is a single value — swap in a precomputed
/// libdivide-style magic multiply+shift here without touching call sites.
pub(crate) fn ns_per_tick(clk_tck: u64) -> u64 {
    (1_000_000_000 / clk_tck.max(1)).max(1)
}

/// Per-core elapsed time in jiffies, floored at 1 to keep [`rate`] division safe.
pub(crate) fn elapsed_jiffies(elapsed: Duration, clk_tck: u64) -> u64 {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a ring one cycle at a time with a fixed 50-jiffy (500 ms @ 100 Hz) window,
    /// accumulating the tick counter like `/proc` would.
    struct Driver {
        ring: CpuRing,
        ticks: u64,
        wait: u64,
        cycle: u64,
    }

    impl Driver {
        fn new() -> Self {
            Self {
                ring: CpuRing::new(0, Some(0), 0),
                ticks: 0,
                wait: 0,
                cycle: 0,
            }
        }
        /// One cycle in which the process burned `delta` ticks and waited for none.
        fn step(&mut self, delta: u64) -> Sampled {
            self.step_wait(delta, 0)
        }
        /// One cycle in which the process burned `delta` ticks and sat `wait` ticks queued.
        fn step_wait(&mut self, delta: u64, wait: u64) -> Sampled {
            self.cycle += 1;
            self.ticks += delta;
            self.wait += wait;
            self.ring.sample(
                self.ticks,
                Some(self.wait),
                Some(50),
                self.cycle,
                false,
                true,
            )
        }
        /// One cycle in which the wait was not measured.
        fn step_unmeasured(&mut self, delta: u64) -> Sampled {
            self.cycle += 1;
            self.ticks += delta;
            self.ring
                .sample(self.ticks, None, Some(50), self.cycle, false, true)
        }
        fn series(&self) -> [u32; SPARK_CELLS] {
            self.ring.series(self.cycle).rates()
        }
        fn cells(&self) -> [SeriesCell; SPARK_CELLS] {
            self.ring.series(self.cycle).0
        }
    }

    #[test]
    fn elapsed_jiffies_is_per_core() {
        // 500 ms at 100 Hz = 50 jiffies on one core, regardless of core count.
        assert_eq!(elapsed_jiffies(Duration::from_millis(500), 100), 50);
        assert_eq!(elapsed_jiffies(Duration::from_secs(1), 100), 100);
        // Floored at 1 so cpu_rate never divides by zero.
        assert_eq!(elapsed_jiffies(Duration::from_micros(1), 100), 1);
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
    fn average_is_exact_over_the_short_window() {
        let mut d = Driver::new();
        d.step(25); // 50%
        d.step(50); // 100% → (25+50)/(100) = 75%
        assert_eq!(d.ring.avg(), 7500);
    }

    #[test]
    fn average_evicts_after_the_short_window() {
        let mut d = Driver::new();
        for _ in 0..AVG_SAMPLES {
            d.step(10); // 20%
        }
        assert_eq!(d.ring.avg(), 2000);
        // Step response: settles in exactly AVG_SAMPLES samples, linearly.
        for n in 1..=AVG_SAMPLES {
            d.step(50);
            let want = u32::try_from(2000 + (8000 * n) / AVG_SAMPLES).unwrap();
            assert_eq!(d.ring.avg(), want, "after {n} busy samples");
        }
        assert_eq!(d.ring.avg(), 10000, "old samples must be fully evicted");
    }

    #[test]
    fn had_ticks_tracks_recent_activity() {
        let mut d = Driver::new();
        assert!(!d.ring.had_ticks(4), "an empty ring has no recent activity");
        d.step(0);
        assert!(!d.ring.had_ticks(4));
        d.step(3); // ran this interval
        assert!(d.ring.had_ticks(4), "recent nonzero ticks ⇒ active");
        assert!(d.ring.had_ticks(1), "the very last sample was active");
        d.step(0);
        assert!(
            d.ring.had_ticks(4),
            "active sample still within the 4-wide window"
        );
        assert!(!d.ring.had_ticks(1), "the last sample alone was idle");
    }

    #[test]
    fn idle_ring_is_not_written() {
        let mut d = Driver::new();
        for _ in 0..10 {
            let s = d.step(0);
            assert_eq!(
                s,
                Sampled {
                    rate: 0,
                    waiting: Waiting::default(),
                    wrote: false
                }
            );
        }
        assert!(d.series().iter().all(|r| *r == 0));
    }

    #[test]
    fn lazy_gap_fill_restores_alignment() {
        let mut d = Driver::new();
        d.step(50); // 100%
        for _ in 0..DEPTH {
            d.step(0); // drains, then goes quiet
        }
        let quiet = d.step(0);
        assert!(!quiet.wrote);
        // Skip 3 more quiescent cycles, then burn: the burst must land in the newest cell
        // with zeros (not stale history) before it.
        for _ in 0..3 {
            assert!(!d.step(0).wrote);
        }
        let s = d.step(25);
        assert_eq!(
            s,
            Sampled {
                rate: 5000,
                waiting: Waiting { wait: 0, run: 25 },
                wrote: true
            }
        );
        let mut want = [0u32; SPARK_CELLS];
        want[SPARK_CELLS - 1] = 5000;
        assert_eq!(d.series(), want);
    }

    #[test]
    fn gap_fill_preserves_history_position() {
        // A ring whose newest write is `g` cycles old reads the same as one that pushed the
        // zeros explicitly.
        let mut d = Driver::new();
        d.step(50);
        d.step(50);
        let mut want = d.series();
        want.rotate_left(1);
        want[SPARK_CELLS - 1] = 0;
        // Observe one cycle later without sampling.
        assert_eq!(d.ring.series(d.cycle + 1).rates(), want);
    }

    #[test]
    fn nonzero_ring_drains_then_goes_quiet() {
        let mut d = Driver::new();
        d.step(50);
        // Every cycle until the burst leaves the ring must write (so the sparkline scrolls).
        for n in 1..=DEPTH {
            let s = d.step(0);
            assert!(s.wrote, "cycle {n} after the burst must scroll the ring");
        }
        assert!(d.series().iter().all(|r| *r == 0), "drained");
        assert!(!d.step(0).wrote, "drained ring is quiescent");
        assert_eq!(d.ring.avg(), 0);
    }

    #[test]
    fn series_corrects_for_unpushed_trailing_cycles() {
        let mut d = Driver::new();
        d.step(50);
        assert_eq!(d.series()[SPARK_CELLS - 1], 10000);
        // Many cycles later without a push, a stale ring must read all-zero.
        assert_eq!(
            d.ring.series(d.cycle + SPARK_CELLS as u64).rates(),
            [0; SPARK_CELLS]
        );
        assert_eq!(d.ring.series(d.cycle + 1).rates()[SPARK_CELLS - 2], 10000);
    }

    #[test]
    fn series_is_oldest_to_newest_per_sample_rate() {
        let mut d = Driver::new();
        d.step(5); // 10%
        d.step(25); // 50%
        d.step(100); // 200% — beyond one core is kept, not clamped
        let s = d.series();
        assert_eq!(&s[SPARK_CELLS - 3..], &[1000, 5000, 20000]);
        assert!(s[..SPARK_CELLS - 3].iter().all(|r| *r == 0));
    }

    #[test]
    fn sampled_rate_is_this_cycles_interval() {
        let mut d = Driver::new();
        assert_eq!(d.step(25).rate, 5000);
        assert_eq!(d.step(50).rate, 10000, "not the average");
        assert_eq!(d.step(0).rate, 0);
        // No window measured ⇒ no rate, no write.
        let s = d
            .ring
            .sample(d.ticks + 9, Some(d.wait), None, d.cycle + 1, false, true);
        assert_eq!(s, Sampled::default());
    }

    #[test]
    fn wait_share_is_wait_over_wait_plus_run() {
        let mut d = Driver::new();
        let s = d.step_wait(30, 10); // waited 10, ran 30 → 25%
        assert_eq!(s.waiting, Waiting { wait: 10, run: 30 });
        assert_eq!(s.waiting.share_bp(), Some(2500));
        let cell = d.cells()[SPARK_CELLS - 1];
        assert_eq!(cell.rate, 6000);
        assert_eq!(cell.waiting.share_bp(), Some(2500));
        assert_eq!(Waiting::default().share_bp(), None, "nothing measured");
    }

    #[test]
    fn wait_only_interval_is_not_idle() {
        let mut d = Driver::new();
        // Never ran, but queued: written, visible, not quiescent.
        let s = d.step_wait(0, 5);
        assert!(s.wrote, "a zero-run nonzero-wait interval is not idle");
        let cell = d.cells()[SPARK_CELLS - 1];
        assert!(cell.is_active());
        assert_eq!(cell.rate, 0);
        assert_eq!(cell.waiting.share_bp(), Some(10_000), "all wait, no run");
        // It keeps scrolling (not quiescent) until the wait drains out of the ring.
        for n in 1..=DEPTH {
            assert!(d.step(0).wrote, "cycle {n} after the wait must scroll");
        }
        assert!(!d.step(0).wrote, "drained ring is quiescent again");
        assert!(d.cells().iter().all(|c| !c.is_active()));
    }

    #[test]
    fn wait_fills_gap_lazily_with_zeros() {
        let mut d = Driver::new();
        for _ in 0..4 {
            assert!(!d.step(0).wrote);
        }
        d.step_wait(10, 10);
        let cells = d.cells();
        assert!(cells[..SPARK_CELLS - 1].iter().all(|c| !c.is_active()));
        assert_eq!(cells[SPARK_CELLS - 1].waiting.share_bp(), Some(5000));
    }

    #[test]
    fn backwards_wait_counter_is_a_zero_delta_not_a_reset() {
        let mut d = Driver::new();
        d.step_wait(10, 8);
        d.wait -= 6; // a waiting thread exited and took its wait with it
        let s = d.step(10);
        assert_eq!(s.waiting, Waiting { wait: 0, run: 10 });
        assert!(s.wrote);
        assert_eq!(d.series()[SPARK_CELLS - 2], 2000, "history is kept");
        // And counting resumes from the lower baseline.
        let s = d.step_wait(10, 5);
        assert_eq!(s.waiting.wait, 5);
    }

    #[test]
    fn unmeasured_wait_is_unknown_not_zero() {
        let mut d = Driver::new();
        let s = d.step_unmeasured(20);
        assert!(s.wrote);
        assert_eq!(s.waiting, Waiting::default());
        assert_eq!(d.cells()[SPARK_CELLS - 1].waiting.share_bp(), None);
        assert_eq!(d.cells()[SPARK_CELLS - 1].rate, 4000, "run still counts");
        // The first measured cycle after a gap only re-baselines: its interval is unknown.
        d.wait = 100;
        let s = d.step_wait(10, 0);
        assert_eq!(s.waiting, Waiting::default());
        // The next one has a baseline and measures.
        let s = d.step_wait(10, 5);
        assert_eq!(s.waiting, Waiting { wait: 5, run: 10 });
    }
}
