//! Per-cycle system-wide sampler: CPU (delta-based), memory, swap, load, uptime, core count.
//!
//! CPU usage is the only delta-based field — it needs the previous cumulative `/proc/stat`
//! counters to compute a rate — so this holds that one piece of carried state. Everything else
//! is read fresh each cycle. The result lands in the cycle's [`SystemStats`].

use crate::procs::SystemStats;
use crate::sys::{self, RawCpuCounters};

/// Samples system-wide stats once per cycle, carrying the prior `/proc/stat` CPU counters so
/// the next cycle can turn cumulative jiffies into a rate.
pub(crate) struct SystemSampler {
    prev: Option<RawCpuCounters>,
    num_cores: u32,
}

impl SystemSampler {
    pub(crate) fn new() -> Self {
        Self {
            prev: None,
            num_cores: sys::num_cpus(),
        }
    }

    /// Read current counters, compute basis-point rates from the delta, and fill
    /// `sys` with CPU + memory + load stats. The first call baselines only.
    pub(crate) fn update(&mut self, sys: &mut SystemStats) {
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
