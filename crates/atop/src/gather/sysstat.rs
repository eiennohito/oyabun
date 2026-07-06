//! Per-cycle system-wide sampler: CPU (delta-based), memory, swap, load, uptime, core count.
//!
//! CPU usage is the only delta-based field — it needs earlier cumulative `/proc/stat`
//! counters to compute a rate — so this holds that carried state. Everything else is read
//! fresh each cycle. The result lands in the cycle's [`SystemStats`].
//!
//! **Windowed, not single-interval.** A bare `cur − prev` delta at the 500 ms / 100 Hz
//! cadence resolves only ~50 jiffies, so a transient burst dominates one sample then
//! vanishes — the bar jumps. Instead this keeps a ring of the last N snapshots (a ~10 s
//! window, matching the per-process CPU window) and rates the two window endpoints:
//! `(counter[now] − counter[now − window]) / (total[now] − total[now − window])`. O(1) per
//! sample — store the snapshot, subtract the endpoints — and the user/sys/iowait breakdown
//! is preserved because every field is differenced the same way.

use super::config::REFRESH_MS;
use super::ring::Ring;
use crate::procs::SystemStats;
use crate::sys::{self, RawCpuCounters};

/// System CPU averaging window as wall-clock time (matches the per-process CPU window).
const SYS_CPU_WINDOW_MS: u64 = 10_000;
/// Snapshot ring depth: the window in samples at the current cadence. The oldest retained
/// snapshot is this many cycles back, so the rate spans ~`SYS_CPU_WINDOW_MS`.
const SYS_CPU_SAMPLES: usize = {
    let n = (SYS_CPU_WINDOW_MS / REFRESH_MS) as usize;
    if n < 2 { 2 } else { n }
};

/// Samples system-wide stats once per cycle, carrying a ring of recent `/proc/stat` CPU
/// counters so the displayed rate is a window average rather than one jumpy interval.
pub(crate) struct SystemSampler {
    /// Recent cumulative CPU counters; the ring's oldest is the window's far endpoint.
    ring: Ring<RawCpuCounters, SYS_CPU_SAMPLES>,
    num_cores: u32,
}

impl SystemSampler {
    pub(crate) fn new() -> Self {
        Self {
            ring: Ring::new(),
            num_cores: sys::num_cpus(),
        }
    }

    /// Read current counters, compute basis-point rates over the window (older endpoint =
    /// the oldest retained snapshot), and fill `sys` with CPU + memory + load stats. The
    /// first call has no endpoint to difference against, so it only seeds the ring.
    pub(crate) fn update(&mut self, sys: &mut SystemStats) {
        let cur = sys::read_cpu_counters();
        if let Some(oldest) = self.ring.oldest() {
            let d_user = cur.user.wrapping_sub(oldest.user) + cur.nice.wrapping_sub(oldest.nice);
            let d_sys = cur.system.wrapping_sub(oldest.system)
                + cur.irq.wrapping_sub(oldest.irq)
                + cur.softirq.wrapping_sub(oldest.softirq);
            let d_iowait = cur.iowait.wrapping_sub(oldest.iowait);
            let d_total = cur.total().wrapping_sub(oldest.total()).max(1);

            sys.cpu_user_bp = u32::try_from(d_user * 10000 / d_total).unwrap_or(u32::MAX);
            sys.cpu_sys_bp = u32::try_from(d_sys * 10000 / d_total).unwrap_or(u32::MAX);
            sys.cpu_iowait_bp = u32::try_from(d_iowait * 10000 / d_total).unwrap_or(u32::MAX);
        }
        self.ring.push(cur);
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
