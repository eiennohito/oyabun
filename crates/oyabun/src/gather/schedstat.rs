//! Scheduler wait for the unprivileged `/proc` source: how long a process's threads were
//! runnable but queued for a CPU.
//!
//! **Why this is a separate, budgeted pass.** `/proc/<pid>/stat` has no run-queue delay, and
//! `/proc/<pid>/schedstat` is per *task* (the leader's own), so a multi-threaded process needs
//! one read per thread plus a thread listing. Doing that for every process every cycle would
//! turn a cheap scan into thousands of reads for numbers nobody looks at. Only processes whose
//! numbers are on screen are asked for (the viewport's rows and the hidden sets of visible
//! collapsed rows), largest CPU first, under a per-cycle budget of file reads. A process not
//! read this cycle has *unknown* wait for the interval — it is never filled with zero, and the
//! CPU ring does not bridge the gap, because a delta spanning several intervals would be
//! attributed to one.
//!
//! The privileged BPF source does not need this: its task iterator already walks every thread.

use super::cpu::{CpuBaseline, ns_per_tick};
use crate::procs::ProcessEntry;
use crate::sys::{ProcDir, ProcPath};

/// File reads allowed per cycle: a single-threaded process costs one, a multi-threaded one a
/// thread listing plus one per thread. Small reads of a procfs file are microseconds, so this
/// bounds the pass to about a millisecond while covering a screenful and its hidden sets.
const READ_BUDGET: u32 = 192;

/// Scratch for one `schedstat` line: `<run_ns> <wait_ns> <timeslices>` — three u64s at most.
const LINE_CAP: usize = 96;

/// `getdents` scratch for a thread listing.
const DENT_CAP: usize = 16 * 1024;

/// The fields of one `schedstat` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Schedstat {
    /// Time on a CPU, ns.
    pub(crate) run_ns: u64,
    /// Time runnable but queued for a CPU, ns.
    pub(crate) wait_ns: u64,
}

impl Schedstat {
    /// Parse `<run_ns> <wait_ns> <timeslices>`. Anything else — a short line, a non-number —
    /// is `None`, so a kernel that formats it differently reads as unmeasured, not zero.
    pub(crate) fn parse(bytes: &[u8]) -> Option<Self> {
        let mut fields = bytes
            .split(u8::is_ascii_whitespace)
            .filter(|f| !f.is_empty());
        let run_ns = parse_u64(fields.next()?)?;
        let wait_ns = parse_u64(fields.next()?)?;
        parse_u64(fields.next()?)?; // timeslices: must be present, unused
        Some(Self { run_ns, wait_ns })
    }
}

fn parse_u64(field: &[u8]) -> Option<u64> {
    if field.is_empty() {
        return None;
    }
    field.iter().try_fold(0u64, |acc, &b| {
        b.is_ascii_digit()
            .then(|| acc.checked_mul(10)?.checked_add(u64::from(b - b'0')))
            .flatten()
    })
}

/// Total wait over the threads `read` can reach; `None` when none could be read (the file is
/// absent without scheduler statistics, or the process is gone). A thread that exits between
/// the listing and its read just drops out of the sum — the same way an exited thread's
/// wait vanishes between cycles, which the ring treats as a zero delta.
fn sum_wait_ns(tids: &[u32], mut read: impl FnMut(u32) -> Option<Schedstat>) -> Option<u64> {
    tids.iter()
        .filter_map(|&tid| read(tid))
        .map(|s| s.wait_ns)
        .reduce(u64::saturating_add)
}

/// The cumulative wait to carry forward without reading, if `row` provably did not queue since
/// `baseline`: its CPU ticks are unchanged and it is not runnable (`R`), and the baseline holds
/// a measured wait to carry. `None` ⇒ read it.
fn cannot_have_waited(row: &ProcessEntry, baseline: Option<CpuBaseline>) -> Option<u64> {
    let b = baseline?;
    (row.ticks == b.ticks && row.state != b'R')
        .then_some(b.wait)
        .flatten()
}

/// Reads scheduler wait for a wanted set of processes under a per-cycle budget. Reuses its
/// buffers: a settled cycle allocates nothing.
pub(crate) struct WaitSampler {
    ns_per_tick: u64,
    path: ProcPath,
    line: [u8; LINE_CAP],
    dent: Vec<u8>,
    tids: Vec<u32>,
}

impl WaitSampler {
    pub(crate) fn new(clk_tck: u64) -> Self {
        Self {
            ns_per_tick: ns_per_tick(clk_tck),
            path: ProcPath::new(),
            line: [0; LINE_CAP],
            dent: vec![0; DENT_CAP],
            tids: Vec::new(),
        }
    }

    /// Set `wait_ticks` on the rows of the `wanted` PIDs, in priority order, until the read
    /// budget is spent. `procs` is PID-sorted; a PID no longer in it is skipped. A process too
    /// costly for what is left of the budget is skipped, but cheaper ones after it still fit.
    ///
    /// `baseline` gives what the CPU ring last recorded for a row. A process that cannot have
    /// accrued wait is not read, and costs no budget: its ticks did not move this interval and
    /// it is not runnable now, so it was asleep (or stopped) throughout and its cumulative
    /// wait stands at the baseline — the row gets that value, a *measured* zero for the
    /// interval. The one gap is a wake-run-sleep wholly between two sampling instants that
    /// burned under a tick: any wait it accrued is not seen now and lands in the interval of
    /// the next real read. Bounded — it is one interval's worth, never lost — and only for
    /// processes that barely ran.
    pub(crate) fn fill(
        &mut self,
        procs: &mut [ProcessEntry],
        wanted: &[u32],
        baseline: impl Fn(&ProcessEntry) -> Option<CpuBaseline>,
    ) {
        let mut budget = READ_BUDGET;
        for &pid in wanted {
            if budget == 0 {
                break;
            }
            let Ok(idx) = procs.binary_search_by_key(&pid, |p| p.pid) else {
                continue;
            };
            if let Some(unchanged) = cannot_have_waited(&procs[idx], baseline(&procs[idx])) {
                procs[idx].wait_ticks = Some(unchanged);
                continue;
            }
            let threads = procs[idx].num_threads.max(1);
            let cost = if threads == 1 {
                1
            } else {
                threads.saturating_add(1)
            };
            if cost > budget {
                continue;
            }
            budget -= cost;
            procs[idx].wait_ticks = self
                .process_wait_ns(pid, threads)
                .map(|ns| ns / self.ns_per_tick);
        }
    }

    /// The process's total wait: its own file when single-threaded, else the sum over its
    /// listed threads.
    fn process_wait_ns(&mut self, pid: u32, threads: u32) -> Option<u64> {
        if threads == 1 {
            return self.read_one(pid).map(|s| s.wait_ns);
        }
        let dir = ProcDir::open_tasks(pid).ok()?;
        dir.read_pids(&mut self.dent, &mut self.tids);
        let tids = std::mem::take(&mut self.tids);
        let total = sum_wait_ns(&tids, |tid| self.read_one(tid));
        self.tids = tids;
        total
    }

    /// `/proc/<id>/schedstat` for a process or a thread id (every tid has its own directory).
    fn read_one(&mut self, id: u32) -> Option<Schedstat> {
        let ptr = self.path.write(id, b"schedstat");
        // SAFETY: valid NUL-terminated path in `self.path`, read-only open.
        let fd = unsafe { libc::open(ptr, libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        // SAFETY: `line` is a valid writable buffer of the given length; `fd` is ours.
        let n = unsafe { libc::read(fd, self.line.as_mut_ptr().cast(), self.line.len()) };
        // SAFETY: our fd, closed once.
        unsafe { libc::close(fd) };
        let n = usize::try_from(n).ok()?;
        Schedstat::parse(&self.line[..n])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_schedstat_line() {
        assert_eq!(
            Schedstat::parse(b"123456789 987654 42\n"),
            Some(Schedstat {
                run_ns: 123_456_789,
                wait_ns: 987_654
            })
        );
        assert_eq!(
            Schedstat::parse(b"0 0 0\n"),
            Some(Schedstat {
                run_ns: 0,
                wait_ns: 0
            })
        );
    }

    #[test]
    fn rejects_malformed_lines() {
        assert_eq!(Schedstat::parse(b""), None);
        assert_eq!(Schedstat::parse(b"12 34\n"), None, "timeslices missing");
        assert_eq!(Schedstat::parse(b"12 x 3\n"), None);
        assert_eq!(Schedstat::parse(b"-1 2 3\n"), None);
        assert_eq!(
            Schedstat::parse(b"99999999999999999999 1 1\n"),
            None,
            "u64 overflow"
        );
    }

    #[test]
    fn thread_waits_are_summed_and_unreadable_threads_drop_out() {
        let read = |tid: u32| match tid {
            10 => Some(Schedstat {
                run_ns: 1,
                wait_ns: 100,
            }),
            11 => Some(Schedstat {
                run_ns: 1,
                wait_ns: 250,
            }),
            _ => None, // exited between the listing and the read
        };
        assert_eq!(sum_wait_ns(&[10, 11, 12], read), Some(350));
        assert_eq!(sum_wait_ns(&[12], read), None, "nothing readable = unknown");
        assert_eq!(sum_wait_ns(&[], read), None);
    }

    fn row(pid: u32, threads: u32) -> ProcessEntry {
        let mut e = ProcessEntry::TOMBSTONE;
        e.pid = pid;
        e.num_threads = threads;
        e
    }

    #[test]
    fn reads_own_process_across_threads_and_respects_the_budget() {
        let me = std::process::id();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let workers: Vec<_> = (0..3)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                })
            })
            .collect();

        let mut sampler = WaitSampler::new(100);
        // Thread count as the stat parse would report it: this process has many threads.
        let threads = u32::try_from(
            std::fs::read_dir("/proc/self/task")
                .expect("task dir")
                .count(),
        )
        .unwrap();
        assert!(threads >= 4);

        let mut procs = [row(1, 1), row(me, threads)];
        sampler.fill(&mut procs, &[me], |_| None);
        let multi = procs[1].wait_ticks;
        // Schedstats may be compiled out; then everything stays unknown rather than zero.
        if std::fs::read("/proc/self/schedstat").is_ok() {
            assert!(multi.is_some(), "own process must read via its threads");
            let leader = sampler.read_one(me).expect("leader schedstat");
            assert!(
                multi.unwrap() + 1 >= leader.wait_ns / sampler.ns_per_tick,
                "the process sum covers at least the leader"
            );
        }

        // A budget that cannot cover the multi-threaded process skips it, not blanks it wrongly.
        let mut procs = [row(me, READ_BUDGET + 1)];
        sampler.fill(&mut procs, &[me], |_| None);
        assert_eq!(procs[0].wait_ticks, None, "over budget = unknown");

        // A cheaper process later in the list still fits after a costly one is skipped.
        let mut procs = [row(1, 1), row(me, READ_BUDGET + 1)];
        sampler.fill(&mut procs, &[me, 1], |_| None);
        assert_eq!(procs[1].wait_ticks, None);

        // PIDs absent from the buffer are ignored.
        sampler.fill(&mut procs, &[u32::MAX], |_| None);

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for w in workers {
            w.join().unwrap();
        }
    }

    #[test]
    fn processes_that_cannot_have_waited_are_carried_not_read() {
        // A PID that cannot be read: a read yields unknown, so `Some` proves the skip.
        const GHOST: u32 = 4_000_000_000;
        let base = |wait| Some(CpuBaseline { ticks: 5, wait });
        let mut e = row(GHOST, 1);
        e.ticks = 5;
        e.state = b'S';

        let mut sampler = WaitSampler::new(100);
        let mut run = |e: ProcessEntry, baseline: Option<CpuBaseline>| {
            let mut rows = [e];
            sampler.fill(&mut rows, &[GHOST], |_| baseline);
            rows[0].wait_ticks
        };
        assert_eq!(
            run(e, base(Some(7))),
            Some(7),
            "asleep, no ticks: baseline carried"
        );

        let mut running = e;
        running.state = b'R';
        assert_eq!(run(running, base(Some(7))), None, "runnable: must be read");
        let mut ticked = e;
        ticked.ticks = 6;
        assert_eq!(
            run(ticked, base(Some(7))),
            None,
            "ran this interval: must be read"
        );
        assert_eq!(
            run(e, base(None)),
            None,
            "no measured baseline: must be read"
        );
        assert_eq!(run(e, None), None, "unknown process: must be read");
    }

    #[test]
    fn carried_processes_cost_no_read_budget() {
        let me = std::process::id();
        // More skippable rows than the budget, ahead of one that must be read.
        let quiet = READ_BUDGET + 8;
        let mut procs: Vec<_> = (0..quiet)
            .map(|i| {
                let mut e = row(4_000_000_000 + i, 1);
                e.state = b'S';
                e
            })
            .collect();
        procs.insert(0, row(me, 1));
        let mut wanted: Vec<u32> = procs.iter().skip(1).map(|p| p.pid).collect();
        wanted.push(me);
        let mut sampler = WaitSampler::new(100);
        sampler.fill(&mut procs, &wanted, |p| {
            (p.pid != me).then_some(CpuBaseline {
                ticks: 0,
                wait: Some(1),
            })
        });
        assert!(procs[1..].iter().all(|p| p.wait_ticks == Some(1)));
        if std::fs::read("/proc/self/schedstat").is_ok() {
            assert!(
                procs[0].wait_ticks.is_some(),
                "the real read still fit the budget"
            );
        }
    }
}
