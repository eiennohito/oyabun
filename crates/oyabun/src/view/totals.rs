//! What a row adds up: per-process totals, and the sums over native subtrees that a collapsed
//! row's numbers are built from.

use super::row::{RowKind, ViewRow};
use crate::gather::Waiting;
use crate::procs::{GpuMetrics, ProcessEntry};

/// A sum over some set of processes. One value for every row kind, so a collapsed process, a
/// folded group, and a plain process row all read the same fields.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Totals {
    pub(crate) threads: u32,
    /// Basis points, summed over the 2 s average of each process.
    pub(crate) cpu_pct: u32,
    /// Basis points of this cycle's interval alone — what the row's series records.
    pub(crate) cpu_now: u32,
    /// Scheduler wait and the run it is compared with over the same interval, summed — the row
    /// series' color. Wait and run are summed separately and only then divided, so a busy member
    /// outweighs a nearly idle one.
    pub(crate) wait_now: Waiting,
    /// Proportional set size where a process has a measurement, else its resident set.
    pub(crate) mem_bytes: u64,
    /// Some process in the sum is reported by resident set (no proportional value yet), so the
    /// memory is an upper-bound mix rather than pure PSS.
    pub(crate) mem_estimated: bool,
    /// `None` when no process in the set has a GPU context; `Unknown` metrics propagate.
    pub(crate) gpu: Option<GpuMetrics>,
    /// Processes covered.
    pub(crate) count: u32,
}

impl Totals {
    /// One process on its own. Memory starts as its resident set, flagged as a fallback; the
    /// sampler's estimates refine it afterwards.
    pub(crate) fn of(p: &ProcessEntry, gpu: Option<GpuMetrics>) -> Self {
        Self {
            threads: p.num_threads,
            cpu_pct: p.cpu_pct,
            cpu_now: p.cpu_now,
            wait_now: p.wait_now,
            mem_bytes: p.mem_bytes,
            mem_estimated: p.mem_bytes > 0,
            gpu,
            count: 1,
        }
    }

    pub(crate) fn add(&mut self, o: &Totals) {
        self.threads = self.threads.saturating_add(o.threads);
        self.cpu_pct = self.cpu_pct.saturating_add(o.cpu_pct);
        self.cpu_now = self.cpu_now.saturating_add(o.cpu_now);
        self.wait_now.add(o.wait_now);
        self.mem_bytes = self.mem_bytes.saturating_add(o.mem_bytes);
        self.mem_estimated |= o.mem_estimated;
        self.gpu = match (self.gpu, o.gpu) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (a, None) => a,
            (None, b) => b,
        };
        self.count = self.count.saturating_add(o.count);
    }
}

/// Per-process own totals, aligned to the process buffer and reused across cycles. What a
/// collapsed row adds up is summed from these by walking its hidden subtrees. Hidden sets are
/// disjoint, so all the walks together touch each process at most once — and none at all while
/// nothing is collapsed.
#[derive(Default)]
pub(crate) struct ProcessTotals {
    own: Vec<Totals>,
}

impl ProcessTotals {
    /// Recompute every process's own totals from the latest cycle. Memory starts as the
    /// resident-set fallback; [`apply_estimates`](Self::apply_estimates) refines it.
    pub(crate) fn rebuild(
        &mut self,
        procs: &[ProcessEntry],
        gpu: impl Fn(u32) -> Option<GpuMetrics>,
    ) {
        self.own.clear();
        self.own
            .extend(procs.iter().map(|p| Totals::of(p, gpu(p.pid))));
    }

    /// Refine memory with the sampler's proportional estimates: `estimates` is called for every
    /// process that has one. Touches only those processes.
    pub(crate) fn apply_estimates(&mut self, estimates: impl FnOnce(&mut dyn FnMut(usize, u64))) {
        let own = &mut self.own;
        estimates(&mut |idx, bytes| {
            own[idx].mem_bytes = bytes;
            own[idx].mem_estimated = false;
        });
    }

    pub(crate) fn own(&self, proc_idx: usize) -> Totals {
        self.own[proc_idx]
    }

    /// What `row` adds up: a plain process its own totals, a collapsed row the sum over the
    /// whole native subtrees of its hidden roots (a slice of `roots`). `walk` is scratch.
    pub(crate) fn of_row(
        &self,
        row: &ViewRow,
        roots: &[u32],
        procs: &[ProcessEntry],
        walk: &mut Vec<u32>,
    ) -> Totals {
        match row.kind {
            RowKind::Process { proc_idx } if !row.is_collapsed() => self.own(proc_idx),
            RowKind::Process { .. } | RowKind::Group { .. } => {
                let hidden = row.hidden();
                self.subtrees(
                    &roots[hidden.start as usize..hidden.end as usize],
                    procs,
                    walk,
                )
            }
        }
    }

    /// Sum over every process in the native subtrees under `roots`.
    fn subtrees(&self, roots: &[u32], procs: &[ProcessEntry], walk: &mut Vec<u32>) -> Totals {
        let mut sum = Totals::default();
        crate::tree::for_each_in_subtrees(procs, roots, walk, |iu| {
            sum.add(&self.own[iu as usize]);
        });
        sum
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::GpuMetric;

    fn gpu(pct: GpuMetric<u32>) -> GpuMetrics {
        GpuMetrics {
            pct,
            mem_bytes: GpuMetric::Value(1),
        }
    }

    #[test]
    fn gpu_none_is_identity_and_unknown_propagates() {
        let mut t = Totals::default();
        t.add(&Totals::default());
        assert_eq!(t.gpu, None);
        let mut a = Totals {
            gpu: Some(gpu(GpuMetric::Value(5))),
            ..Totals::default()
        };
        a.add(&Totals::default());
        assert_eq!(a.gpu, Some(gpu(GpuMetric::Value(5))));
        a.add(&Totals {
            gpu: Some(gpu(GpuMetric::Unknown)),
            ..Totals::default()
        });
        assert_eq!(a.gpu.unwrap().pct, GpuMetric::Unknown);
    }

    #[test]
    fn subtree_sums_cover_whole_subtrees_and_flag_fallback() {
        // 0 → {1 → {3}, 2}
        let mut procs = vec![ProcessEntry::TOMBSTONE; 4];
        procs[0].first_child = 1;
        procs[1].next_sibling = 2;
        procs[1].first_child = 3;
        for (pid, p) in (1u32..).zip(procs.iter_mut()) {
            p.pid = pid;
            p.cpu_pct = pid * 100;
            p.num_threads = 2;
            p.mem_bytes = 1000;
        }
        let mut t = ProcessTotals::default();
        t.rebuild(&procs, |_| None);
        // Only process 3 has a proportional value.
        t.apply_estimates(|f| f(3, 400));
        let mut walk = Vec::new();
        let all = t.subtrees(&[0], &procs, &mut walk);
        assert_eq!(all.cpu_pct, 1000);
        assert_eq!(all.count, 4);
        assert_eq!(all.threads, 8);
        assert_eq!(all.mem_bytes, 3400);
        assert!(all.mem_estimated);
        let one = t.subtrees(&[1], &procs, &mut walk);
        assert_eq!(one.mem_bytes, 1400);
        assert!(one.mem_estimated, "1 itself is a fallback");
        let leaf = t.subtrees(&[3], &procs, &mut walk);
        assert_eq!(leaf.mem_bytes, 400);
        assert!(!leaf.mem_estimated, "a measured process carries no marker");
        let two_roots = t.subtrees(&[3, 2], &procs, &mut walk);
        assert_eq!(two_roots.count, 2);
        assert_eq!(t.own(1).cpu_pct, 200);
    }
}
