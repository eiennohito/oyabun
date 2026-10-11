use std::path::{Path, PathBuf};

use crate::fxhash::PidMap;
use crate::procs::ProcessEntry;

/// Address-space walks permitted per gather cycle. Only bootstrap (a member never walked) and
/// the staggered backstop below produce walks now, so a settled desktop walks approximately
/// nothing and a freshly-folded large application spreads its bootstrap walks over a few cycles.
const READ_BUDGET: u32 = 16;

/// Staggered period, in generations, of the forced re-walk of an otherwise-stable member. The
/// per-member estimate is exact for private (heap) growth — the common case — and drifts only
/// when the *shared* mapping set changes (a library mapped or unmapped), which the free
/// resident-set reading cannot reveal. This backstop is the sole steady-state source of walks, so
/// it also bounds how long that shared-set drift can persist. Re-walks are **staggered by PID**
/// (see [`MemorySampler::backstop_due`]): a whole application folded in one cycle would otherwise
/// come due together and walk as a thundering herd every period. Spread out, at most
/// ~`members / BACKSTOP_GENS` walk per cycle; per-member staleness is bounded to this many cycles.
const BACKSTOP_GENS: u64 = 256;

/// The shared/private split of a member's resident set from one address-space walk, from which
/// proportional set size is re-estimated each cycle against the free live resident reading.
/// `Rss = private + shared_full` and `Pss = private + shared_pss`, so caching only the shared
/// components lets the private part — and thus PSS — track resident growth without a re-walk.
#[derive(Clone, Copy)]
struct Split {
    /// Resident bytes in shared mappings (`Shared_Clean + Shared_Dirty`) at the walk.
    shared_full: u64,
    /// The proportional share of those shared mappings (`Pss − private`) at the walk.
    shared_pss: u64,
}

impl Split {
    /// Estimate PSS at the current resident set: attribute all resident change since the walk to
    /// private pages (exact for heap growth — the shared set is what the backstop re-walks), then
    /// recombine with the cached shared proportional share. Clamped into `[0, rss_now]`.
    fn estimate(self, rss_now: u64) -> u64 {
        let private_now = rss_now.saturating_sub(self.shared_full);
        private_now.saturating_add(self.shared_pss).min(rss_now)
    }
}

/// Per-process cache. Keyed by PID and validated by `start_time`, so a reused PID never inherits a
/// stale reading; entries whose process is not live this generation are evicted in
/// [`MemorySampler::begin_cycle`].
struct MemberSample {
    start_time: u64,
    /// The split from the last walk, or `None` when the address space was unreadable.
    split: Option<Split>,
    /// Generation of the last walk, for the staggered backstop.
    read_gen: u64,
}

/// Estimate-based proportional-memory sampler for the processes on screen.
///
/// Every row's memory is proportional set size (a collapsed row sums it over what it hides),
/// and reading a process's PSS forces a kernel walk of its whole address space — the dominant
/// interactive cost on a real desktop if done every cycle. The key invariant is that the *shape*
/// of a process's footprint — its shared-vs-private split — changes far more slowly than its
/// size. So PSS is estimated, not re-read: each process caches that split from one walk, then
/// every cycle recombines it with the free live resident reading (resident growth is attributed
/// to private pages, exact for heap growth). The expensive walk runs only to bootstrap a
/// process's split and, on a staggered backstop, to catch a change in the shared mapping set —
/// the one thing the resident reading cannot reveal — under a per-cycle budget. A process
/// without a split yet is reported by its resident set and flagged, per process, so one
/// unreadable process never degrades a whole row. See `docs/application-memory-gotchas.md`.
pub(crate) struct MemorySampler {
    proc_root: PathBuf,
    members: PidMap<MemberSample>,
    /// Reused `(proc_idx, priority)` scratch for the per-cycle candidate ranking — cleared and
    /// refilled each `sample_visible` so the sampler allocates nothing on a settled desktop.
    candidates: Vec<(usize, u64)>,
    generation: u64,
    budget: u32,
    #[cfg(test)]
    reads: std::cell::Cell<usize>,
}

impl MemorySampler {
    pub(crate) fn system() -> Self {
        Self::with_root(PathBuf::from("/proc"))
    }

    pub(crate) fn with_root(proc_root: PathBuf) -> Self {
        Self {
            proc_root,
            members: PidMap::default(),
            candidates: Vec::new(),
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

    /// Spend the cycle's remaining read budget on `wanted` (indices into `procs`): the stale
    /// ones, ranked by resident size, largest first, so a freshly shown big subtree spends the
    /// budget where it matters most and spreads the rest across the next few cycles. Returns
    /// whether anything was read (the caller then recomputes its totals). Cheap to call
    /// repeatedly within a generation — processes with a fresh split trigger no reads.
    pub(crate) fn sample(&mut self, wanted: &[usize], procs: &[ProcessEntry]) -> bool {
        // Reuse the scratch buffer across calls (no per-frame allocation). Taking it out by value
        // sidesteps the borrow conflict between `self.refresh_priority` (&self) and pushing.
        let mut candidates = std::mem::take(&mut self.candidates);
        candidates.clear();
        for &proc_idx in wanted {
            if let Some(priority) = self.refresh_priority(&procs[proc_idx]) {
                candidates.push((proc_idx, priority));
            }
        }
        candidates.sort_unstable_by_key(|&(_, priority)| std::cmp::Reverse(priority));
        let mut read = false;
        for &(proc_idx, _) in &candidates {
            if self.budget == 0 {
                break;
            }
            self.budget -= 1;
            self.read_member(&procs[proc_idx]);
            read = true;
        }
        self.candidates = candidates;
        read
    }

    /// Report the current PSS estimate of each `wanted` process (index into `procs`) that has a
    /// usable split, as `(index, bytes)`. One hash lookup per wanted process, so the cost follows
    /// the viewport, not the cache (which keeps every entry sampled so far, so scrolling back to
    /// a sampled process shows its number without a re-walk) nor the process table. A process
    /// absent here has no proportional value and falls back to its resident set.
    pub(crate) fn estimates(
        &self,
        procs: &[ProcessEntry],
        wanted: &[usize],
        mut f: impl FnMut(usize, u64),
    ) {
        for &idx in wanted {
            let proc = &procs[idx];
            if let Some(sample) = self.members.get(&proc.pid)
                && sample.start_time == proc.start_time
                && let Some(split) = sample.split
            {
                f(idx, split.estimate(proc.mem_bytes));
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn read_count(&self) -> usize {
        self.reads.get()
    }

    /// Whether a member needs an address-space walk this cycle, priced by resident set (largest
    /// first) if so. A member with no address space never walks. One with a cached split walks
    /// only when its staggered backstop is due — resident *size* changes are estimated, not
    /// re-walked. A never-walked member (or a reused PID) walks once to bootstrap its split.
    fn refresh_priority(&self, proc: &ProcessEntry) -> Option<u64> {
        if proc.mem_bytes == 0 {
            return None;
        }
        match self.members.get(&proc.pid) {
            Some(sample) if sample.start_time == proc.start_time => self
                .backstop_due(proc.pid, sample.read_gen)
                .then_some(proc.mem_bytes),
            _ => Some(proc.mem_bytes),
        }
    }

    /// Whether a resident-stable member is due its staggered shared-page-drift backstop this
    /// cycle. Keyed on `(generation + pid) % BACKSTOP_GENS` so each PID's re-read slot lands on
    /// a different cycle — a group folded all at once no longer re-reads as one herd. The
    /// `read_gen != generation` guard prevents a second read within the same cycle it was just
    /// read (`sample_visible` may run several times per gather, once per input event).
    fn backstop_due(&self, pid: u32, read_gen: u64) -> bool {
        read_gen != self.generation
            && self.generation.wrapping_add(u64::from(pid)) % BACKSTOP_GENS == 0
    }

    fn read_member(&mut self, proc: &ProcessEntry) {
        #[cfg(test)]
        self.reads.set(self.reads.get() + 1);
        let split = read_split(&self.proc_root, proc.pid);
        self.members.insert(
            proc.pid,
            MemberSample {
                start_time: proc.start_time,
                split,
                read_gen: self.generation,
            },
        );
    }
}

/// Walk a member's `smaps_rollup` into its shared/private split. `None` when the address space is
/// unreadable (permission, or a racing exit) or the rollup lacks a `Pss:` line.
fn read_split(proc_root: &Path, pid: u32) -> Option<Split> {
    let text =
        std::fs::read_to_string(proc_root.join(pid.to_string()).join("smaps_rollup")).ok()?;
    let (mut pss, mut shared_full, mut private) = (None, 0u64, 0u64);
    for line in text.lines() {
        if let Some(v) = field_bytes(line, "Pss:") {
            pss = Some(v);
        } else if let Some(v) = field_bytes(line, "Shared_Clean:") {
            shared_full = shared_full.saturating_add(v);
        } else if let Some(v) = field_bytes(line, "Shared_Dirty:") {
            shared_full = shared_full.saturating_add(v);
        } else if let Some(v) = field_bytes(line, "Private_Clean:") {
            private = private.saturating_add(v);
        } else if let Some(v) = field_bytes(line, "Private_Dirty:") {
            private = private.saturating_add(v);
        }
    }
    Some(Split {
        shared_full,
        // Proportional share of the shared pages: Pss minus the fully-counted private pages.
        shared_pss: pss?.saturating_sub(private),
    })
}

/// Parse a `smaps_rollup` `"<label>   <n> kB"` line into bytes; `None` if it is not that label.
fn field_bytes(line: &str, label: &str) -> Option<u64> {
    line.strip_prefix(label)?
        .split_ascii_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .map(|kib| kib.saturating_mul(1024))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        root: PathBuf,
        sampler: MemorySampler,
        procs: Vec<ProcessEntry>,
    }

    impl Fixture {
        /// `n` members, PIDs `1..=n`, each with `rss_mib` MiB resident and a `smaps_rollup`
        /// whose split reports `pss_mib` MiB proportional at that resident set. Every process wanted.
        fn new(name: &str, n: u32, rss_mib: u64, pss_mib: u64) -> Self {
            let root = std::env::temp_dir().join(format!("oya-mem-{name}-{}", std::process::id()));
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
                write_rollup(&proc_root, pid, rss_mib, pss_mib);
            }
            Self {
                sampler: MemorySampler::with_root(proc_root),
                root,
                procs,
            }
        }

        fn cycle(&mut self, generation: u64) {
            self.sampler.begin_cycle(generation, &self.procs);
            let wanted: Vec<usize> = (0..self.procs.len()).collect();
            self.sampler.sample(&wanted, &self.procs);
        }

        /// Sum of per-process memory as a row would show it: the estimate where the sampler has
        /// a split, else the resident fallback.
        fn total(&self) -> u64 {
            let mut own: Vec<u64> = self.procs.iter().map(|p| p.mem_bytes).collect();
            let all: Vec<usize> = (0..self.procs.len()).collect();
            self.sampler
                .estimates(&self.procs, &all, |idx, bytes| own[idx] = bytes);
            own.iter().sum()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Write a coherent `smaps_rollup` modelling the whole footprint as shared pages
    /// (`Shared_Clean = Rss`) with proportional share `pss_mib`. The estimate reproduces `pss_mib`
    /// exactly at this resident set and attributes any later growth to private pages.
    fn write_rollup(proc_root: &Path, pid: u32, rss_mib: u64, pss_mib: u64) {
        let dir = proc_root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        let rss = rss_mib * 1024;
        let pss = pss_mib * 1024;
        fs::write(
            dir.join("smaps_rollup"),
            format!(
                "Rss: {rss} kB\nPss: {pss} kB\nShared_Clean: {rss} kB\n\
                 Shared_Dirty: 0 kB\nPrivate_Clean: 0 kB\nPrivate_Dirty: 0 kB\n"
            ),
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
    fn estimates_cover_only_the_wanted_processes_and_the_cache_is_kept() {
        let mut f = Fixture::new("viewport", 3, 100, 40);
        f.cycle(1);
        let mut seen = Vec::new();
        f.sampler
            .estimates(&f.procs, &[1], |idx, bytes| seen.push((idx, bytes)));
        assert_eq!(seen, [(1, 40 * 1024 * 1024)]);
        // Scrolling elsewhere and back needs no further walk.
        seen.clear();
        f.sampler
            .estimates(&f.procs, &[0, 2], |idx, _| seen.push((idx, 0)));
        assert_eq!(seen.len(), 2);
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
    fn large_resident_jump_is_estimated_without_rewalk() {
        let mut f = Fixture::new("jump", 3, 100, 40);
        f.cycle(1);
        assert_eq!(f.sampler.read_count(), 3);
        // One member's resident set jumps +500 MiB (private allocation). No new rollup, no re-walk:
        // the cached split scales, attributing the growth to private pages one-to-one.
        f.procs[1].mem_bytes += 500 * 1024 * 1024;
        f.cycle(2);
        assert_eq!(
            f.sampler.read_count(),
            3,
            "resident growth is estimated from the cached split, not re-walked"
        );
        // member 1: (600 − 100) + 40 = 540 MiB; the two unchanged members stay at 40 MiB.
        assert_eq!(f.total(), (40 + 540 + 40) * 1024 * 1024);
    }

    #[test]
    fn per_cycle_budget_bounds_reads_and_converges() {
        let members = READ_BUDGET + 8;
        let mut f = Fixture::new("budget", members, 100, 10);
        f.cycle(1);
        assert_eq!(f.sampler.read_count(), READ_BUDGET as usize);
        // The unread members fall back to their resident set, per process; the read ones are
        // already proportional (no whole-group fallback).
        let (read, unread) = (u64::from(READ_BUDGET), u64::from(members - READ_BUDGET));
        assert_eq!(f.total(), (read * 10 + unread * 100) * 1024 * 1024);

        f.cycle(2);
        assert_eq!(f.sampler.read_count(), members as usize);
        // All members read → proportional total.
        assert_eq!(f.total(), u64::from(members) * 10 * 1024 * 1024);
    }

    #[test]
    fn staggered_backstop_catches_drift_without_herd() {
        let mut f = Fixture::new("backstop", 2, 100, 40);
        f.cycle(1);
        assert_eq!(f.sampler.read_count(), 2);
        // The shared mapping set shifts its proportional share with no resident-set change, so the
        // estimate cannot see it — only the backstop re-walk can.
        write_rollup(&f.root.join("proc"), 1, 100, 55);
        write_rollup(&f.root.join("proc"), 2, 100, 55);
        // Over one full stagger window each member re-walks exactly once, and no single cycle
        // re-walks both — the re-walks are spread by PID rather than bursting as one herd.
        let mut prev = f.sampler.read_count();
        let mut max_per_cycle = 0;
        for generation in 2..=BACKSTOP_GENS + 2 {
            f.cycle(generation);
            let now = f.sampler.read_count();
            max_per_cycle = max_per_cycle.max(now - prev);
            prev = now;
        }
        assert_eq!(
            f.sampler.read_count(),
            4,
            "each member re-read exactly once in the window"
        );
        assert_eq!(
            max_per_cycle, 1,
            "staggered: at most one drift re-read per cycle, no herd"
        );
        assert_eq!(f.total(), 2 * 55 * 1024 * 1024);
    }

    #[test]
    fn unreadable_member_falls_back_alone() {
        let mut f = Fixture::new("fallback", 3, 100, 40);
        fs::remove_file(f.root.join("proc/2/smaps_rollup")).unwrap();
        f.cycle(1);
        // Only the unreadable member reports its resident set; the rest stay proportional.
        assert_eq!(f.total(), (40 + 100 + 40) * 1024 * 1024);
    }

    #[test]
    fn empty_member_contributes_zero_without_forcing_fallback() {
        let mut f = Fixture::new("zombie", 3, 100, 40);
        f.procs[1].mem_bytes = 0; // zombie / racing exit: no address space
        fs::remove_file(f.root.join("proc/2/smaps_rollup")).unwrap();
        f.cycle(1);
        // The empty member contributes nothing; the two readable members sum proportionally.
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
        write_rollup(&f.root.join("proc"), 1, 100, 70);
        f.cycle(2);
        assert_eq!(
            f.sampler.read_count(),
            2,
            "reused PID re-walks from scratch"
        );
        assert_eq!(f.total(), 70 * 1024 * 1024);
    }
}
