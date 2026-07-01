//! Per-process CPU%: a per-core rate over real elapsed time (`top`/`htop` "Irix mode"),
//! computed from a bounded ring of recent sample windows with exact integer math.
//!
//! One full core = 10000 basis points (100%); a multi-threaded process can exceed it.
//! [`CpuRing`] keeps the recent window so the inherent ±1-jiffy quantization is damped into
//! a stable moving average, while still capturing spikes as a separate peak.

use std::time::Duration;

use super::config::REFRESH_MS;

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
pub(crate) const MIN_SAMPLE: Duration = Duration::from_millis(100);

/// One measured interval: tick delta and the per-core jiffies it spanned.
#[derive(Clone, Copy, Default)]
struct Sample {
    ticks: u32,
    jiff: u32,
}

/// Bounded per-PID CPU history: a ring of the last [`CPU_WINDOW`] intervals plus exact `u64`
/// running sums. The displayed CPU% is the sum-weighted moving **average** (stable);
/// [`peak`](Self::peak) is the max single-interval rate still in the window (captures a spike
/// for up to [`CPU_WINDOW`] intervals after it happens).
///
/// `Flat` (`Copy`, no heap) so it lives on huge pages in a `GenStore<CpuRing>`. It is the
/// **hot** per-PID record — touched on every sample — and is deliberately a *separate* store
/// from the cold `PidMeta` so a CPU update loads only this, not metadata cache lines.
/// Liveness/incarnation bookkeeping (`seen_gen`, `start_time`) lives in `PidSlot`, not here.
#[derive(Clone, Copy)]
pub(crate) struct CpuRing {
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
}

impl CpuRing {
    pub(crate) fn new(prev_ticks: u64) -> Self {
        Self {
            prev_ticks,
            samples: [Sample::default(); CPU_WINDOW],
            next: 0,
            len: 0,
            sum_ticks: 0,
            sum_jiff: 0,
            peak_bp: 0,
            peak_at: usize::MAX,
        }
    }

    /// Discard history (PID reuse / counter reset), re-baselining at `ticks`.
    fn reset(&mut self, ticks: u64) {
        *self = Self::new(ticks);
    }

    /// Fold one cycle's observation into the ring. `reused` ⇒ a new incarnation took this PID,
    /// so discard the dead one's history. Otherwise, given a real measurement window (`jiff`)
    /// for an already-`known` PID, push the tick delta — or reset if the counter went
    /// backwards (wrap). A brand-new PID just keeps its baseline (first real rate lands next
    /// interval); a too-soon/first cycle (no window) carries the windowed values forward.
    pub(crate) fn sample(&mut self, ticks: u64, jiff: Option<u32>, reused: bool, known: bool) {
        if reused {
            self.reset(ticks);
            return;
        }
        let Some(j) = jiff else { return };
        if !known {
            return;
        }
        if ticks < self.prev_ticks {
            self.reset(ticks);
        } else {
            let delta = u32::try_from(ticks - self.prev_ticks).unwrap_or(u32::MAX);
            self.push(delta, j);
            self.prev_ticks = ticks;
        }
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
    pub(crate) fn avg(&self) -> u32 {
        rate(self.sum_ticks, self.sum_jiff)
    }

    /// Max single-interval rate still in the window. O(1) in the common case;
    /// O(`CPU_WINDOW`) only when the previous peak sample is evicted (~1/window).
    pub(crate) fn peak(&mut self) -> u32 {
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
    fn moving_average_is_exact() {
        let mut h = CpuRing::new(0);
        h.push(25, 50); // 50%
        h.push(50, 50); // 100% → (25+50)/(100) = 75%
        assert_eq!(h.avg(), 7500);
    }

    #[test]
    fn moving_average_evicts_old_samples() {
        let mut h = CpuRing::new(0);
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
        let mut h = CpuRing::new(0);
        h.push(5, 50); // 10%
        h.push(50, 50); // 100% spike
        h.push(5, 50); // 10%
        assert_eq!(h.peak(), 10000, "peak must hold the spike");
        assert!(h.avg() < 5000, "average must stay damped, got {}", h.avg());
    }
}
