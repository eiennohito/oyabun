use std::path::{Path, PathBuf};

use crate::application::AppGroup;
use crate::fxhash::PidMap;
use crate::procs::ProcessEntry;

/// Proportional reads permitted per gather cycle. A newly-folded large application refreshes
/// over a few cycles instead of one spike; a settled desktop reads approximately nothing.
const READ_BUDGET: u32 = 16;

/// Generations between forced refreshes of an otherwise-stable member. A member's proportional
/// share drifts when a *shared* page is (un)mapped elsewhere — invisible to the resident-set
/// gate — so a slow periodic re-read backstops it. At the gather interval this is on the order
/// of tens of seconds, and the value moves slowly enough that the staleness is imperceptible.
const BACKSTOP_GENS: u64 = 64;

/// A resident-set move below this fraction of host memory is ignored as a refresh trigger:
/// it cannot move a total shown in gigabytes. Significance is absolute (against host memory),
/// not fractional against the process, so a large jump on a big process refreshes while a large
/// *relative* jiggle on a tiny one does not.
const SIGNIFICANCE_DIVISOR: u64 = 2048; // ~0.05% of host memory
const MIN_SIGNIFICANCE: u64 = 1 << 20; // 1 MiB floor for tiny hosts

/// Per-member proportional-set-size cache. Keyed by PID and validated by `start_time`, so a
/// reused PID never inherits a stale reading; entries whose process is not live this generation
/// are evicted in [`MemorySampler::begin_cycle`].
struct MemberSample {
    start_time: u64,
    /// Last proportional read: `Some(pss)`, or `None` when the address space was unreadable.
    pss: Option<u64>,
    /// Resident set *at the moment of that read* — the staleness gate compares against this, not
    /// the previous cycle, so slow accumulation still crosses the threshold and refreshes.
    rss_at_read: u64,
    /// Generation of the last read, for the slow-drift backstop.
    read_gen: u64,
}

/// Change-gated proportional-memory sampler for folded application rows.
///
/// Correct application memory sums proportional set size across **every** member, and each read
/// forces a kernel walk of the whole address space — the dominant interactive cost on a real
/// desktop if done every cycle. The governing invariant is that application memory changes
/// slowly, so it is change-gated rather than recomputed: a member whose resident set has not
/// moved keeps its cached proportional value, candidates are prioritized by absolute
/// resident-set change against host memory, a per-cycle budget bounds the reads, and a slow
/// refresh backstops shared-page drift. See `docs/application-memory-gotchas.md`.
pub(crate) struct MemorySampler {
    proc_root: PathBuf,
    members: PidMap<MemberSample>,
    generation: u64,
    budget: u32,
    #[cfg(test)]
    reads: std::cell::Cell<usize>,
}

/// One member's contribution to a group total, resolved from the cache after the budgeted reads.
enum Contribution {
    /// A usable proportional value (private + shared-recombined bytes).
    Pss(u64),
    /// The member has no address space (zombie / racing exit): contributes zero, never forces
    /// the whole-group resident fallback.
    Empty,
    /// The member is live with resident memory but has no proportional value yet (never read
    /// within budget, or unreadable). Any such member drops the whole group to resident-set
    /// addition — mixing proportional and resident units in one total would be incoherent.
    Missing,
}

impl MemorySampler {
    pub(crate) fn system() -> Self {
        Self::with_root(PathBuf::from("/proc"))
    }

    pub(crate) fn with_root(proc_root: PathBuf) -> Self {
        Self {
            proc_root,
            members: PidMap::default(),
            generation: 0,
            budget: READ_BUDGET,
            #[cfg(test)]
            reads: std::cell::Cell::new(0),
        }
    }

    /// Advance to a new gather generation: reset the per-cycle read budget and evict cached
    /// members whose exact incarnation is no longer live (death detected, or PID reused).
    pub(crate) fn begin_cycle(&mut self, generation: u64, live: &[ProcessEntry]) {
        self.generation = generation;
        self.budget = READ_BUDGET;
        self.members.retain(|pid, sample| {
            live.binary_search_by(|p| p.pid.cmp(pid))
                .is_ok_and(|idx| live[idx].start_time == sample.start_time)
        });
    }

    /// Refresh and write `mem_bytes` for the given visible collapsed groups. Stale members
    /// across all of them are prioritized by absolute resident-set change and read highest-first
    /// until the shared budget is spent; every group's total is then recomputed from the cache.
    /// Cheap to call repeatedly within a generation — fresh members trigger no reads.
    pub(crate) fn sample_visible(
        &mut self,
        groups: &mut [AppGroup],
        visible: &[usize],
        procs: &[ProcessEntry],
        mem_total: u64,
    ) {
        let threshold = significance(mem_total);
        let mut candidates: Vec<(usize, u64)> = Vec::new(); // (proc_idx, priority)
        for &group_idx in visible {
            for &proc_idx in &groups[group_idx].members {
                let proc = &procs[proc_idx];
                if let Some(priority) = self.refresh_priority(proc, threshold) {
                    candidates.push((proc_idx, priority));
                }
            }
        }
        // Highest absolute resident-set change first; a fresh fold of a large application spreads
        // its reads across the next few cycles rather than spiking one.
        candidates.sort_unstable_by_key(|&(_, priority)| std::cmp::Reverse(priority));
        for (proc_idx, _) in candidates {
            if self.budget == 0 {
                break;
            }
            self.budget -= 1;
            self.read_member(&procs[proc_idx]);
        }

        for &group_idx in visible {
            let total = self.group_total(&groups[group_idx], procs);
            groups[group_idx].mem_bytes = total;
        }
    }

    #[cfg(test)]
    pub(crate) fn read_count(&self) -> usize {
        self.reads.get()
    }

    /// The refresh priority of a member, or `None` if its cached value is still fresh. A member
    /// with no address space is never a candidate. Never-read members sort by their whole
    /// resident set (they have no value yet); read members by how far their resident set has
    /// moved since the last read, with the backstop forcing an eventual refresh.
    fn refresh_priority(&self, proc: &ProcessEntry, threshold: u64) -> Option<u64> {
        if proc.mem_bytes == 0 {
            return None;
        }
        match self.members.get(&proc.pid) {
            Some(sample) if sample.start_time == proc.start_time => {
                let delta = proc.mem_bytes.abs_diff(sample.rss_at_read);
                let backstop_due = self.generation.wrapping_sub(sample.read_gen) >= BACKSTOP_GENS;
                (delta >= threshold || backstop_due).then_some(delta)
            }
            _ => Some(proc.mem_bytes),
        }
    }

    fn read_member(&mut self, proc: &ProcessEntry) {
        #[cfg(test)]
        self.reads.set(self.reads.get() + 1);
        let pss = read_pss(&self.proc_root, proc.pid);
        self.members.insert(
            proc.pid,
            MemberSample {
                start_time: proc.start_time,
                pss,
                rss_at_read: proc.mem_bytes,
                read_gen: self.generation,
            },
        );
    }

    fn group_total(&self, group: &AppGroup, procs: &[ProcessEntry]) -> u64 {
        let mut pss_sum = 0u64;
        for &proc_idx in &group.members {
            match self.contribution(&procs[proc_idx]) {
                Contribution::Pss(pss) => pss_sum = pss_sum.saturating_add(pss),
                Contribution::Empty => {}
                Contribution::Missing => return rss_sum(group, procs),
            }
        }
        pss_sum
    }

    fn contribution(&self, proc: &ProcessEntry) -> Contribution {
        if proc.mem_bytes == 0 {
            return Contribution::Empty;
        }
        match self.members.get(&proc.pid) {
            Some(sample) if sample.start_time == proc.start_time => {
                sample.pss.map_or(Contribution::Missing, Contribution::Pss)
            }
            _ => Contribution::Missing,
        }
    }
}

fn significance(mem_total: u64) -> u64 {
    (mem_total / SIGNIFICANCE_DIVISOR).max(MIN_SIGNIFICANCE)
}

fn rss_sum(group: &AppGroup, procs: &[ProcessEntry]) -> u64 {
    group
        .members
        .iter()
        .fold(0u64, |sum, &idx| sum.saturating_add(procs[idx].mem_bytes))
}

fn read_pss(proc_root: &Path, pid: u32) -> Option<u64> {
    let text =
        std::fs::read_to_string(proc_root.join(pid.to_string()).join("smaps_rollup")).ok()?;
    let kib = text.lines().find_map(|line| {
        line.strip_prefix("Pss:")?
            .split_ascii_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    })?;
    Some(kib.saturating_mul(1024))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::AppGroupKey;
    use std::fs;

    const HOST_MEM: u64 = 16 * 1024 * 1024 * 1024;

    struct Fixture {
        root: PathBuf,
        sampler: MemorySampler,
        procs: Vec<ProcessEntry>,
        group: AppGroup,
    }

    impl Fixture {
        /// `n` members, PIDs `1..=n`, each with `rss_mib` MiB resident and a `smaps_rollup`
        /// reporting `pss_mib` MiB. Members are the whole group.
        fn new(name: &str, n: u32, rss_mib: u64, pss_mib: u64) -> Self {
            let root = std::env::temp_dir().join(format!("atop-mem-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let proc_root = root.join("proc");
            fs::create_dir_all(&proc_root).unwrap();
            let mut procs = Vec::new();
            for pid in 1..=n {
                let mut p = ProcessEntry::TOMBSTONE;
                p.pid = pid;
                p.start_time = u64::from(pid);
                p.mem_bytes = rss_mib * 1024 * 1024;
                procs.push(p);
                write_pss(&proc_root, pid, pss_mib);
            }
            let group = AppGroup {
                key: AppGroupKey {
                    owner_uid: 1,
                    canonical: "x".into(),
                },
                representative: 0,
                members: (0..n as usize).collect(),
                label: "x".into(),
                owner_uid: 1,
                persist_key: None,
                threads: 0,
                cpu_pct: 0,
                cpu_peak: 0,
                mem_bytes: 0,
                gpu: None,
            };
            Self {
                sampler: MemorySampler::with_root(proc_root),
                root,
                procs,
                group,
            }
        }

        fn cycle(&mut self, generation: u64) {
            self.sampler.begin_cycle(generation, &self.procs);
            self.sampler.sample_visible(
                std::slice::from_mut(&mut self.group),
                &[0],
                &self.procs,
                HOST_MEM,
            );
        }

        fn total(&self) -> u64 {
            self.group.mem_bytes
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn write_pss(proc_root: &Path, pid: u32, pss_mib: u64) {
        let dir = proc_root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("smaps_rollup"),
            format!("Rss: 999999 kB\nPss: {} kB\n", pss_mib * 1024),
        )
        .unwrap();
    }

    #[test]
    fn sums_proportional_set_size_across_members() {
        let mut f = Fixture::new("pss-sum", 3, 100, 40);
        f.cycle(1);
        assert_eq!(f.total(), 3 * 40 * 1024 * 1024);
        assert_eq!(f.sampler.read_count(), 3);
    }

    #[test]
    fn resident_stable_member_triggers_no_reread() {
        let mut f = Fixture::new("stable", 3, 100, 40);
        f.cycle(1);
        assert_eq!(f.sampler.read_count(), 3);
        // Resident set unchanged → no member qualifies for a fresh read.
        f.cycle(2);
        assert_eq!(f.sampler.read_count(), 3);
        assert_eq!(f.total(), 3 * 40 * 1024 * 1024);
    }

    #[test]
    fn large_resident_jump_is_refreshed() {
        let mut f = Fixture::new("jump", 3, 100, 40);
        f.cycle(1);
        assert_eq!(f.sampler.read_count(), 3);
        // One member's resident set jumps well past the significance threshold and its
        // proportional reading grows to match.
        f.procs[1].mem_bytes = 100 * 1024 * 1024 + 500 * 1024 * 1024;
        write_pss(&f.root.join("proc"), 2, 240);
        f.cycle(2);
        assert_eq!(f.sampler.read_count(), 4, "only the jumped member re-reads");
        assert_eq!(f.total(), (40 + 240 + 40) * 1024 * 1024);
    }

    #[test]
    fn small_resident_move_below_significance_is_ignored() {
        let mut f = Fixture::new("small", 3, 100, 40);
        f.cycle(1);
        // A move smaller than ~0.05% of host memory must not trigger a read.
        f.procs[1].mem_bytes += MIN_SIGNIFICANCE / 2;
        f.cycle(2);
        assert_eq!(f.sampler.read_count(), 3);
    }

    #[test]
    fn per_cycle_budget_bounds_reads_and_converges() {
        let members = READ_BUDGET + 8;
        let mut f = Fixture::new("budget", members, 100, 10);
        f.cycle(1);
        assert_eq!(f.sampler.read_count(), READ_BUDGET as usize);
        // Not every member has a proportional value yet → coherent resident-set fallback.
        assert_eq!(f.total(), u64::from(members) * 100 * 1024 * 1024);

        f.cycle(2);
        assert_eq!(f.sampler.read_count(), members as usize);
        // All members read → proportional total.
        assert_eq!(f.total(), u64::from(members) * 10 * 1024 * 1024);
    }

    #[test]
    fn slow_refresh_backstops_shared_page_drift() {
        let mut f = Fixture::new("backstop", 2, 100, 40);
        f.cycle(1);
        assert_eq!(f.sampler.read_count(), 2);
        // Proportional share shifts with no resident-set change (a shared page mapped elsewhere).
        write_pss(&f.root.join("proc"), 1, 55);
        write_pss(&f.root.join("proc"), 2, 55);
        // Before the backstop window the resident-stable members are not re-read.
        for generation in 2..BACKSTOP_GENS {
            f.cycle(generation);
        }
        assert_eq!(f.sampler.read_count(), 2);
        // Once the window elapses the backstop forces a refresh and the drift is caught.
        f.cycle(BACKSTOP_GENS + 1);
        assert_eq!(f.sampler.read_count(), 4);
        assert_eq!(f.total(), 2 * 55 * 1024 * 1024);
    }

    #[test]
    fn unreadable_member_drops_group_to_resident_addition() {
        let mut f = Fixture::new("fallback", 3, 100, 40);
        fs::remove_file(f.root.join("proc/2/smaps_rollup")).unwrap();
        f.cycle(1);
        // A live member with resident memory but no proportional value makes the whole group
        // fall back to resident-set addition rather than mix units.
        assert_eq!(f.total(), 3 * 100 * 1024 * 1024);
    }

    #[test]
    fn empty_member_contributes_zero_without_forcing_fallback() {
        let mut f = Fixture::new("zombie", 3, 100, 40);
        f.procs[1].mem_bytes = 0; // zombie / racing exit: no address space
        fs::remove_file(f.root.join("proc/2/smaps_rollup")).unwrap();
        f.cycle(1);
        // The empty member contributes nothing and its unreadable record does not trigger the
        // whole-group resident fallback; the two readable members still sum proportionally.
        assert_eq!(f.total(), 2 * 40 * 1024 * 1024);
    }

    #[test]
    fn reused_pid_does_not_inherit_stale_reading() {
        let mut f = Fixture::new("reuse", 1, 100, 40);
        f.cycle(1);
        assert_eq!(f.total(), 40 * 1024 * 1024);
        // Same PID, new incarnation (different start_time) with a different footprint.
        f.procs[0].start_time = 999;
        f.procs[0].mem_bytes = 100 * 1024 * 1024;
        write_pss(&f.root.join("proc"), 1, 70);
        f.cycle(2);
        assert_eq!(
            f.sampler.read_count(),
            2,
            "reused PID re-reads from scratch"
        );
        assert_eq!(f.total(), 70 * 1024 * 1024);
    }
}
