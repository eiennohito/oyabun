//! The unprivileged `/proc` observation source: a maintained live set, a cadenced `getdents`
//! re-scan, a skip-cycle birth probe, and an `io_uring`/syscall [`Backend`] reading each live
//! PID's stat into the row buffer. The peer of `bpf::BpfSource` behind the gatherer's source
//! enum — it owns everything the cycle does *before* the source-agnostic per-PID table / tree
//! build.

use std::time::Instant;

use super::config::{Config, RING_ENTRIES, force_syscall, pool_capacity};
use super::source::{CycleResult, Source, SourceCtx};
use super::syscall::SyscallBackend;
use super::table::PidIndex;
use super::uring::UringBackend;
use crate::procs::Procs;
use crate::sys::{self, ProcDir};

/// The I/O mechanism a [`ProcSource`] uses to read each live PID's stat: `io_uring` when the
/// kernel supports it, the syscall floor otherwise. Both hold a persistent stat-fd pool.
enum Backend {
    Uring(Box<UringBackend>),
    Syscall(SyscallBackend),
}

impl Backend {
    /// Build the I/O backend on the **calling thread**, which is the single thread that will
    /// also submit on it — the `io_uring` backend's single-issuer flags bind the ring's
    /// submitter task to its creator. Probes `io_uring` (`ATOP_FORCE_SYSCALL` or any probe
    /// failure falls back to the syscall backend). The backend owns its own fixed landing pad,
    /// so probing needs nothing from the process buffer.
    fn from_config(pool_cap: u32, config: &Config) -> Backend {
        if force_syscall() {
            return Backend::Syscall(SyscallBackend::new(pool_cap));
        }
        match UringBackend::probe(pool_cap, RING_ENTRIES, config.batch_cap, config.read_slots) {
            Some(u) => Backend::Uring(Box::new(u)),
            None => Backend::Syscall(SyscallBackend::new(pool_cap)),
        }
    }

    /// Re-read every live PID's stat into `procs`. Returns the overflow count (PIDs that
    /// exceeded the persistent-fd pool and used the transient fallback).
    fn collect(&mut self, pids: &[u32], procs: &mut Procs, page_size: u64) -> std::io::Result<u32> {
        match self {
            Backend::Uring(u) => u.collect(pids, procs, page_size),
            Backend::Syscall(s) => Ok(s.collect(pids, procs, page_size)),
        }
    }
}

/// The unprivileged `/proc` observation source: maintained live set + cadenced `getdents`
/// re-scan + skip-cycle birth probe, with an `io_uring`/syscall [`Backend`] reading each PID's
/// stat. This is everything the gatherer's cycle does *before* the per-PID table / tree build —
/// the peer to `bpf::BpfSource`.
pub(crate) struct ProcSource {
    /// I/O backend, built once on the owning thread (the sole ring submitter).
    backend: Backend,
    proc_dir: ProcDir,
    pids: Vec<u32>,
    dent_buf: Vec<u8>,
    /// Persistent-fd pool capacity, retained for the mid-run `io_uring`→syscall downgrade.
    pool_cap: u32,
    pub(crate) config: Config,
    /// `pid_max` (the PID-counter wrap point), read once at startup — bounds the probe
    /// window so it never generates an impossible PID.
    pid_max: u32,
}

impl Source for ProcSource {
    fn populate(&mut self, procs: &mut Procs, ctx: SourceCtx<'_>) -> CycleResult {
        let (now, pool_overflow) = self.scan(procs, ctx.index, ctx.page_size, ctx.prev_gen);
        CycleResult {
            now,
            pool_overflow,
            short_lived: 0,
        }
    }
}

impl ProcSource {
    pub(crate) fn new(proc_dir: ProcDir) -> Self {
        let pool_cap = pool_capacity();
        let config = Config::from_env();
        let backend = Backend::from_config(pool_cap, &config);
        Self {
            backend,
            proc_dir,
            pids: Vec::new(),
            dent_buf: vec![0u8; 64 * 1024],
            pool_cap,
            config,
            pid_max: sys::read_pid_max(),
        }
    }

    /// Fill `procs` with the live set's stat fields, leaving it **compacted and PID-sorted**.
    /// `prev_gen` (the just-finished generation) drives the enumeration cadence and the
    /// skip-cycle leader gate; `index` is the per-PID table's index, read by that gate to tell
    /// a known PID from a probe-introduced non-leader thread. Returns the sample instant and
    /// the pool-overflow count.
    pub(crate) fn scan(
        &mut self,
        procs: &mut Procs,
        index: &PidIndex,
        page_size: u64,
        prev_gen: u64,
    ) -> (Instant, u32) {
        // Enumeration cadence (§3): a full `getdents` re-scan every K cycles is the resync
        // that catches any birth the probe missed (non-sequential, burst > W, post-wrap);
        // skip cycles reuse the maintained live set plus a cheap sequential-birth probe.
        // The set is rebuilt from survivors below, so deaths drop the same cycle (held read →
        // `ESRCH` → re-tombstone → compact) — the full scan is only for births, never pruning.
        if prev_gen.is_multiple_of(self.config.enum_every) {
            self.proc_dir.read_pids(&mut self.dent_buf, &mut self.pids);
            self.pids.sort_unstable();
            self.pids.dedup();
        } else {
            self.probe_births();
        }
        let now = Instant::now();

        Self::prepare(procs, &self.pids);

        let overflow = if let Ok(overflow) = self.backend.collect(&self.pids, procs, page_size) {
            overflow
        } else {
            // io_uring failed mid-cycle: drop to syscall permanently and redo the fill.
            self.backend = Backend::Syscall(SyscallBackend::new(self.pool_cap));
            Self::prepare(procs, &self.pids);
            self.backend
                .collect(&self.pids, procs, page_size)
                .unwrap_or(0)
        };

        // Before compact: rows with real PIDs but unfilled (state='?' = TOMBSTONE default) are
        // rows `fill` never submitted a read for AND never tombstoned.
        #[cfg(debug_assertions)]
        {
            for (i, r) in procs.as_slice().iter().enumerate() {
                if r.pid != 0 && r.state == b'?' {
                    eprintln!(
                        "[UNFILLED] idx={i} pid={} ppid={} state=? mem={} ticks={} start_time={}",
                        r.pid, r.ppid, r.mem_bytes, r.ticks, r.start_time,
                    );
                }
            }
        }

        procs.compact();

        // Thread-leader backstop (syscall backend): the uring backend rejects non-leader
        // threads inline; the syscall backend has no inline filter, so this post-compact pass
        // catches any non-leader thread a probe introduced (uring: a no-op — already gone).
        if !prev_gen.is_multiple_of(self.config.enum_every) {
            let mut re_compact = false;
            for row in procs.as_mut_slice() {
                if index.get(row.pid).is_none() && !sys::is_thread_group_leader(row.pid) {
                    row.pid = 0;
                    re_compact = true;
                }
            }
            if re_compact {
                procs.compact();
            }
        }

        // Rebuild the maintained live set from survivors (PID-sorted, since `compact` preserves
        // order): deaths and probe misses fall out now, confirmed births stay.
        self.pids.clear();
        self.pids.extend(procs.as_slice().iter().map(|p| p.pid));

        (now, overflow)
    }

    /// Skip-cycle birth probe (§3a): append up to `probe_width` candidate PIDs just above
    /// the highest live PID, bounded by the kernel's allocation frontier (`ns_last_pid`).
    /// PIDs are allocated near-monotonically, so a freshly-forked process almost always
    /// takes a number above the current max; `collect` opens these speculatively — survivors
    /// join the maintained set, misses fail their open and are re-tombstoned (§3b, the
    /// hygiene that makes speculative opens safe). In steady state `ns_last_pid` equals our
    /// max, so the window is empty and the probe costs **zero** opens. A burst > W, a
    /// non-sequential birth, or a post-wrap low PID waits for the next full scan — a probe
    /// hit-rate, never a correctness, concern, because the full scan is the backstop.
    ///
    /// **Precondition**: `self.pids` is sorted ascending and holds the current live set.
    fn probe_births(&mut self) {
        debug_assert!(
            self.pids.windows(2).all(|w| w[0] <= w[1]),
            "probe_births requires a sorted live set"
        );
        let w = self.config.probe_width;
        let Some(&anchor) = self.pids.last() else {
            return;
        };
        let frontier = sys::read_ns_last_pid().unwrap_or(anchor.saturating_add(w));
        let hi = anchor.saturating_add(w).min(frontier).min(self.pid_max);
        for cand in anchor.saturating_add(1)..=hi {
            self.pids.push(cand);
        }
        #[cfg(debug_assertions)]
        {
            let n_added = hi.saturating_sub(anchor);
            if n_added > 0 {
                eprintln!("[probe] anchor={anchor} frontier={frontier} hi={hi} added={n_added}");
            }
        }
    }

    /// Reset and pre-size the process buffer, seeding a tombstone for every PID in sorted
    /// order. `reserve` keeps the per-cycle tombstone fill from relocating the buffer chunk
    /// mid-fill — a performance choice, since a grow self-heals harmlessly.
    fn prepare(procs: &mut Procs, pids: &[u32]) {
        procs.clear();
        procs.reserve(pids.len());
        for &pid in pids {
            procs.push_tombstone(pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fxhash::PidMap;
    use crate::procs::ProcessEntry;
    use crate::sys::ProcDir;
    use thoop::Arena;

    fn enum_pids() -> Vec<u32> {
        let dir = ProcDir::open().unwrap();
        let mut dent = vec![0u8; 64 * 1024];
        let mut pids = Vec::new();
        dir.read_pids(&mut dent, &mut pids);
        pids.sort_unstable();
        pids
    }

    /// A wired, arena-backed `Procs` for direct-backend tests. The returned `Box<Arena>` must
    /// outlive the `Procs` (the buffer caches its pinned address).
    fn test_procs() -> (Box<Arena>, Procs) {
        let arena = Box::new(Arena::new(0));
        let mut procs = Procs::new(&arena, 256);
        procs.wire(&arena);
        (arena, procs)
    }

    fn name_of(p: &ProcessEntry) -> String {
        String::from_utf8_lossy(p.comm()).into_owned()
    }

    /// The `io_uring` backend must agree with the syscall oracle on stable **stat**
    /// fields for processes both scans observed. (uid/cmdline are no longer backend
    /// fields — the `ProcTable` owns them; CPU/ticks/mem can drift between scans; PID reuse
    /// can churn the set — so we anchor on PID 1 and self, and require broad agreement
    /// on the overlap.)
    #[test]
    fn uring_matches_syscall_backend() {
        let page_size = crate::sys::page_size();
        let pids = enum_pids();
        let pool_cap = pool_capacity();

        let (_aa, mut a) = test_procs();
        let (_ab, mut b) = test_procs();

        let Some(mut uring) = UringBackend::probe(
            pool_cap,
            RING_ENTRIES,
            RING_ENTRIES as usize,
            crate::gather::config::READ_SLOTS,
        ) else {
            eprintln!("io_uring unavailable — skipping oracle comparison");
            return;
        };

        for &pid in &pids {
            a.push_tombstone(pid);
        }
        SyscallBackend::new(pool_cap).collect(&pids, &mut a, page_size);
        a.compact();

        for &pid in &pids {
            b.push_tombstone(pid);
        }
        uring
            .collect(&pids, &mut b, page_size)
            .expect("uring collect");
        b.compact();

        assert!(
            b.as_slice().len() > 10,
            "uring found too few: {}",
            b.as_slice().len()
        );

        let by_pid: PidMap<ProcessEntry> = b.as_slice().iter().map(|p| (p.pid, *p)).collect();

        let mut common = 0;
        let mut name_matches = 0;
        for pa in a.as_slice() {
            if let Some(pb) = by_pid.get(&pa.pid) {
                common += 1;
                if name_of(pa) == name_of(pb) {
                    name_matches += 1;
                }
                if pa.pid == 1 || pa.pid == std::process::id() {
                    assert_eq!(pa.ppid, pb.ppid, "ppid mismatch for pid {}", pa.pid);
                    assert_eq!(
                        pa.is_kthread, pb.is_kthread,
                        "is_kthread mismatch for pid {}",
                        pa.pid
                    );
                    assert_eq!(name_of(pa), name_of(pb), "name mismatch pid {}", pa.pid);
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
}
