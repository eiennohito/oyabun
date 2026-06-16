//! `io_uring` backend with a **persistent stat-fd pool** — the close-storm killer.
//!
//! A `/proc/<pid>/stat` fd is installed once as an `io_uring` **direct (fixed)
//! descriptor** and re-read every cycle with a single `ReadFixed` at offset 0 (a
//! procfs single-show file regenerates fresh content on a repeated offset-0 read —
//! validated). No per-cycle open, no per-cycle close: the `osq_lock`/`uring_lock`
//! storm that fixed-fd closes drove from `io-wq` is gone.
//!
//! ```text
//!   cached PID:  ReadFixed(fixed_idx, off 0)                         (1 SQE)
//!   new PID:     OpenAt(/stat, file_index) -[IO_LINK]-> ReadFixed    (2 SQEs, no Close)
//!   dead PID:    register_files_update(idx, -1)  (eviction, low volume)
//! ```
//!
//! A held read returning `ESRCH` (the incarnation died / the PID was reused) closes
//! the slot and reads the current incarnation transiently this cycle; it gets a fresh
//! persistent slot next cycle. When more PIDs are live than the pool holds (low
//! `RLIMIT_NOFILE`), the overflow uses the shared transient read (a non-fixed close
//! hits `files->file_lock` briefly, never `uring_lock`).
//!
//! `uid`/`cmdline` are not read here — they are `ProcCache`'s job (plain syscalls on a
//! coarse cadence). This backend produces only the volatile stat fields.

use std::io;

use io_uring::{IoUring, opcode, squeue, types};

use std::collections::HashSet;

use crate::gather::syscall::{read_transient, reread_into};
use crate::gather::{PidMap, STAT_SLOT, STAT_SLOT_LONG, parse};
use crate::snapshot::Snapshot;
use crate::sys::ProcPath;

// CQE `user_data` packs `(fixed_idx << 1) | op`.
const OP_OPEN: u64 = 0;
const OP_READ: u64 = 1;

fn pack(fixed: u32, op: u64) -> u64 {
    (u64::from(fixed) << 1) | op
}

fn unpack(user_data: u64) -> (usize, u64) {
    ((user_data >> 1) as usize, user_data & 1)
}

/// A held direct descriptor for one PID's `/proc/<pid>/stat`.
struct Held {
    fixed_idx: u32,
    /// Tracker generation when last submitted, for evicting vanished PIDs.
    seen_gen: u32,
}

/// A deferred re-read for a PID whose held read hit `ESRCH` (death/reuse): re-read the
/// current incarnation transiently, into the slot the failed read already claimed.
#[derive(Clone, Copy)]
struct ReopenJob {
    pid_idx: usize,
    pid: u32,
    stat_off: u32,
    stat_cap: u32,
}

/// Per-cycle scratch for an in-flight stat chain, indexed by `fixed_idx`.
#[derive(Clone, Copy, Default)]
struct Ctx {
    pid: u32,
    pid_idx: u32,
    stat_off: u32,
    stat_cap: u32,
    stat_len: u32,
    pending: u8,
    /// The `OpenAt` of a new chain failed (process gone before open) — the slot was
    /// never installed, so it is freed without a close.
    open_failed: bool,
    /// The `ReadFixed` returned data.
    read_ok: bool,
}

pub struct UringBackend {
    ring: IoUring,
    /// PID → held direct descriptor.
    held: PidMap<Held>,
    /// Free fixed-file indices (`0..pool_cap`).
    free_fixed: Vec<u32>,
    /// Per-`fixed_idx` in-flight scratch.
    ctxs: Vec<Ctx>,
    /// Per-`fixed_idx` path storage for `OpenAt` (must outlive the open completion).
    paths: Vec<ProcPath>,
    /// Path scratch for transient (overflow / reopen) reads.
    stat_path: ProcPath,
    /// Per-cycle scratch queues, kept on `self` only to reuse their capacity across
    /// cycles (cleared at the start of `collect`). They are drained by index so the
    /// drain can borrow other `self` fields (the stat path, the ring) without a
    /// take-and-restore dance.
    done_buf: Vec<u32>,
    overflow_buf: Vec<(usize, u32)>,
    /// PIDs whose held read hit `ESRCH` (reuse/death): re-read transiently into the
    /// already-claimed slot after the ring drains — no new alloc, so the up-front
    /// reserve is never exceeded mid-cycle.
    reopen_buf: Vec<ReopenJob>,
    cur_gen: u32,
    /// Max CQEs to wait for per round (the I/O↔parse overlap dial, §1). Default = a full
    /// ring (`RING_ENTRIES`), so each round submits the batch and waits for **all** of it
    /// once (no overlap; parse is now cheap). Lowering it reaps+parses a chunk while later
    /// reads complete. Since a single `fill` submits ≤ `RING_ENTRIES` SQEs and each SQE
    /// yields one CQE, outstanding CQEs ≤ `RING_ENTRIES`, so the default never under-waits.
    batch_cap: usize,
    /// Registered buffer descriptors, one per snapshot (index = `buf_index`).
    bufs: [libc::iovec; 2],
}

/// Build the ring with modern single-issuer flags, probing a descending ladder and
/// falling back cleanly. `io_uring_setup` accepts or rejects a whole flag set (`EINVAL`
/// on an unknown flag), so we probe richest-first rather than per-flag. The gatherer
/// thread is the **sole** submitter — it creates *and* enters the ring — which is exactly
/// what `SINGLE_ISSUER`/`DEFER_TASKRUN` require: they bind the submitter task to the ring
/// **creator**, so the ring MUST be built on the gatherer thread (in `Gatherer::run`).
///
/// - `SINGLE_ISSUER` (6.0): assert one submitter → enables related fast paths.
/// - `DEFER_TASKRUN` (6.1, needs `SINGLE_ISSUER`): defer completion task-work to the
///   `enter`-to-wait call instead of running it async / via IPI. Safe here because every
///   `collect` round reaches `submit_and_wait` — the flag's "must periodically wait"
///   invariant holds by construction.
/// - `COOP_TASKRUN` (5.19): don't IPI the submitter task for task-work; run it on the
///   next ring exit. Cheaper wakeups.
///
/// The ladder must stay ordered by **descending kernel-version requirement** (6.1 → 6.0 →
/// 5.19 → none) so the first rung an old kernel accepts is the richest one it supports; a
/// new flag is added to the top, not spliced into the middle.
fn build_ring(entries: u32) -> io::Result<IoUring> {
    if let Ok(r) = IoUring::builder()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .setup_coop_taskrun()
        .build(entries)
    {
        return Ok(r);
    }
    if let Ok(r) = IoUring::builder()
        .setup_single_issuer()
        .setup_coop_taskrun()
        .build(entries)
    {
        return Ok(r);
    }
    if let Ok(r) = IoUring::builder().setup_coop_taskrun().build(entries) {
        return Ok(r);
    }
    IoUring::new(entries)
}

// SAFETY: the backend is owned solely by the gatherer thread and never shared. Its
// `iovec`s point at snapshot arenas (themselves `Send`) managed by that thread.
unsafe impl Send for UringBackend {}

fn iovec_of(snap: &Snapshot) -> libc::iovec {
    libc::iovec {
        iov_base: snap.strings.as_ptr().cast(),
        iov_len: snap.strings.capacity(),
    }
}

impl UringBackend {
    /// Probe `io_uring`: build a ring, register a sparse direct-descriptor table sized
    /// to the pool, and register both snapshot arenas as fixed buffers. Returns `None`
    /// if any step is unsupported (old kernel, restricted seccomp) — caller falls back.
    pub fn probe(
        pool_cap: u32,
        ring_entries: u32,
        batch_cap: usize,
        front: &Snapshot,
        back: &Snapshot,
    ) -> Option<Self> {
        let ring = build_ring(ring_entries).ok()?;
        // One direct-fd slot per held PID (stat only — cmdline left the chain).
        ring.submitter().register_files_sparse(pool_cap).ok()?;

        let bufs = [iovec_of(front), iovec_of(back)];
        // SAFETY: both iovecs point at live, mmap'd snapshot arenas that outlive the
        // backend (the snapshots are kept alive for the whole program).
        unsafe { ring.submitter().register_buffers(&bufs) }.ok()?;

        let cap = pool_cap as usize;
        let mut free_fixed = Vec::with_capacity(cap);
        for s in (0..pool_cap).rev() {
            free_fixed.push(s);
        }
        Some(Self {
            ring,
            held: PidMap::default(),
            free_fixed,
            ctxs: vec![Ctx::default(); cap],
            paths: vec![ProcPath::new(); cap],
            stat_path: ProcPath::new(),
            done_buf: Vec::new(),
            overflow_buf: Vec::new(),
            reopen_buf: Vec::new(),
            cur_gen: 0,
            batch_cap,
            bufs,
        })
    }

    /// Re-point a registered buffer after its arena grew (moved). Cheap & rare.
    pub fn update_buffer(&mut self, buf_index: u16, ptr: *mut u8, len: usize) {
        self.bufs[buf_index as usize] = libc::iovec {
            iov_base: ptr.cast(),
            iov_len: len,
        };
        let _ = self.ring.submitter().unregister_buffers();
        // SAFETY: bufs reference live snapshot arenas.
        let _ = unsafe { self.ring.submitter().register_buffers(&self.bufs) };
    }

    /// Re-read every live PID's stat into the snapshot. Returns the overflow count
    /// (PIDs that exceeded the pool and used the transient fallback).
    pub fn collect(
        &mut self,
        pids: &[u32],
        snap: &mut Snapshot,
        page_size: u64,
        long_stat: &mut HashSet<u32>,
    ) -> io::Result<u32> {
        self.cur_gen = self.cur_gen.wrapping_add(1);
        let cur_gen = self.cur_gen;
        let mut next = 0usize;
        // Outstanding **CQEs**, not chains: a cached read emits 1, a new chain 2 (the
        // linked open + read — and a failed open still posts an `ECANCELED` read CQE, so
        // 2 is exact either way). Waiting on this count is what collapses ~51 wakeups/cycle
        // to ~1: submit the whole batch, wait once for all of it, drain, parse.
        let mut outstanding = 0usize;
        self.overflow_buf.clear();
        self.reopen_buf.clear();

        while next < pids.len() || outstanding > 0 {
            outstanding += self.fill(pids, snap, &mut next, cur_gen, long_stat);
            if outstanding == 0 {
                break; // only overflow PIDs remain (pool was full)
            }
            // Default `batch_cap` = a full ring ⇒ wait for the entire outstanding batch
            // (one wakeup/round). A smaller cap re-enables I/O↔parse overlap (the dial).
            let want = outstanding.min(self.batch_cap);
            self.submit_and_wait(want)?;
            outstanding -= self.reap_and_process(snap, page_size, long_stat);
        }

        // The ring is fully drained: no ReadFixed targets the arena, so the transient
        // reads below can safely grow it. Reopen (reuse/death) reads, then the overflow
        // tail (PIDs that never got a persistent slot). Drained by index so each read
        // can borrow `self.stat_path`.
        for i in 0..self.reopen_buf.len() {
            let job = self.reopen_buf[i];
            if !reread_into(
                job.pid,
                job.pid_idx,
                snap,
                page_size,
                long_stat,
                &mut self.stat_path,
                (job.stat_off, job.stat_cap),
            ) {
                snap.tombstone(job.pid_idx); // incarnation truly gone — no phantom row
            }
        }

        for i in 0..self.overflow_buf.len() {
            let (idx, pid) = self.overflow_buf[i];
            if !read_transient(pid, idx, snap, page_size, long_stat, &mut self.stat_path) {
                snap.tombstone(idx); // transient overflow read failed
            }
        }
        let overflow = u32::try_from(self.overflow_buf.len()).unwrap_or(u32::MAX);

        self.evict(cur_gen);
        Ok(overflow)
    }

    /// Submit stat chains for PIDs until the SQ fills or the pool is exhausted.
    /// Returns the number of **CQEs** the submitted chains will produce (cached read = 1,
    /// new open+read chain = 2) — the count [`collect`](Self::collect) waits on.
    #[allow(clippy::cast_possible_truncation)] // slot sizes are small compile-time consts
    fn fill(
        &mut self,
        pids: &[u32],
        snap: &mut Snapshot,
        next: &mut usize,
        cur_gen: u32,
        long_stat: &HashSet<u32>,
    ) -> usize {
        let buf_index = snap.buf_index;
        let mut cqes = 0usize;
        let mut sq = self.ring.submission();

        while *next < pids.len() {
            // A new chain needs 2 SQEs; require room for the larger case.
            if sq.capacity() - sq.len() < 2 {
                break;
            }
            let idx = *next;
            let pid = pids[idx];

            let stat_sz = if long_stat.contains(&pid) {
                STAT_SLOT_LONG
            } else {
                STAT_SLOT
            };

            if let Some(h) = self.held.get_mut(&pid) {
                // Cached: single ReadFixed at offset 0 on the held descriptor.
                h.seen_gen = cur_gen;
                let fixed = h.fixed_idx;
                let off = u32::try_from(snap.strings.alloc(stat_sz)).expect("arena offset fits");
                let ptr = snap.strings.write_ptr(off as usize);
                let read =
                    opcode::ReadFixed::new(types::Fixed(fixed), ptr, stat_sz as u32, buf_index)
                        .offset(0)
                        .build()
                        .user_data(pack(fixed, OP_READ));
                // SAFETY: arena slot outlives completion; SQ has room (checked).
                unsafe { sq.push(&read).expect("sq push") };
                self.ctxs[fixed as usize] = Ctx {
                    pid,
                    pid_idx: idx as u32,
                    stat_off: off,
                    stat_cap: stat_sz as u32,
                    pending: 1,
                    ..Ctx::default()
                };
                cqes += 1;
            } else if let Some(fixed) = self.free_fixed.pop() {
                // New: OpenAt installs the direct descriptor, linked ReadFixed reads it.
                let off = u32::try_from(snap.strings.alloc(stat_sz)).expect("arena offset fits");
                let ptr = snap.strings.write_ptr(off as usize);
                let path_ptr = self.paths[fixed as usize].write(pid, b"stat");
                let dest = types::DestinationSlot::try_from_slot_target(fixed).expect("slot fits");
                let open = opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), path_ptr)
                    .flags(libc::O_RDONLY)
                    .file_index(Some(dest))
                    .build()
                    .user_data(pack(fixed, OP_OPEN))
                    .flags(squeue::Flags::IO_LINK);
                let read =
                    opcode::ReadFixed::new(types::Fixed(fixed), ptr, stat_sz as u32, buf_index)
                        .offset(0)
                        .build()
                        .user_data(pack(fixed, OP_READ));
                // SAFETY: path + arena slot outlive completion; SQ room checked (≥2).
                unsafe {
                    sq.push(&open).expect("sq push");
                    sq.push(&read).expect("sq push");
                }
                self.held.insert(
                    pid,
                    Held {
                        fixed_idx: fixed,
                        seen_gen: cur_gen,
                    },
                );
                self.ctxs[fixed as usize] = Ctx {
                    pid,
                    pid_idx: idx as u32,
                    stat_off: off,
                    stat_cap: stat_sz as u32,
                    pending: 2,
                    ..Ctx::default()
                };
                cqes += 2;
            } else {
                // Pool full: defer to the transient overflow pass.
                self.overflow_buf.push((idx, pid));
            }
            *next += 1;
        }
        cqes
    }

    /// Submit the queued SQEs and block until `want` CQEs are ready (`io_uring_enter`
    /// with `GETEVENTS`, `min_complete = want`), retrying on `EINTR`.
    fn submit_and_wait(&self, want: usize) -> io::Result<()> {
        loop {
            match self.ring.submit_and_wait(want) {
                Ok(_) => return Ok(()),
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Drain ready completions; process each PID once both its CQEs land. Returns the
    /// number of **CQEs** reaped (what [`collect`](Self::collect) subtracts from the
    /// outstanding-CQE counter — chains complete in 1 or 2 CQEs).
    #[allow(clippy::cast_possible_truncation)] // STAT_SLOT is a small const
    fn reap_and_process(
        &mut self,
        snap: &mut Snapshot,
        page_size: u64,
        long_stat: &mut HashSet<u32>,
    ) -> usize {
        self.done_buf.clear();
        let mut reaped = 0usize;
        for cqe in self.ring.completion() {
            reaped += 1;
            let (fixed, op) = unpack(cqe.user_data());
            let res = cqe.result();
            let ctx = &mut self.ctxs[fixed];
            if op == OP_OPEN {
                if res < 0 {
                    ctx.open_failed = true;
                }
            } else if res > 0 {
                ctx.stat_len = u32::try_from(res).unwrap_or(0).min(ctx.stat_cap);
                ctx.read_ok = true;
            }
            ctx.pending -= 1;
            if ctx.pending == 0 {
                self.done_buf.push(u32::try_from(fixed).unwrap_or(0));
            }
        }

        // Drain by index so the reopen branch can borrow `self.ring`/`self.held` while
        // `done_buf` keeps its capacity. `Ctx` is `Copy`, so the indexed read takes no
        // lasting borrow.
        let n = self.done_buf.len();
        for i in 0..n {
            let fixed = self.done_buf[i];
            let ctx = self.ctxs[fixed as usize];
            let idx = ctx.pid_idx as usize;
            if ctx.read_ok {
                // No `start_time` re-check is needed here (unlike a re-open-by-path
                // backend). An open `/proc/<pid>/stat` fd pins the kernel `struct pid`,
                // so the PID number cannot be recycled while we hold the descriptor:
                // a successful read is always the same incarnation, and a dead task
                // yields `ESRCH` (the read-fail branch below), never another process's
                // stat. Reuse only becomes possible after we close the slot on `ESRCH`.
                if ctx.stat_len == ctx.stat_cap && ctx.stat_cap == STAT_SLOT as u32 {
                    long_stat.insert(ctx.pid);
                }
                let slice = snap.strings.bytes(ctx.stat_off, ctx.stat_len);
                if let Some(f) = parse::parse_stat(slice, ctx.stat_off) {
                    f.write_into(&mut snap.procs[idx], page_size);
                } else {
                    snap.tombstone(idx); // unparseable read → no phantom row
                }
            } else if ctx.open_failed {
                // New PID vanished before open — slot never installed.
                self.held.remove(&ctx.pid);
                self.free_fixed.push(fixed);
                snap.tombstone(idx); // speculative/new PID gone before open
            } else {
                // Installed fd read failed (`ESRCH`: the incarnation exited; closing the
                // fd now unpins the PID, so a reused incarnation is read fresh below).
                // The transient re-read is *deferred* to after the ring fully drains —
                // calling it here could grow the arena (relocating the registered
                // buffer) while other ReadFixeds are still in flight.
                let _ = self.ring.submitter().register_files_update(fixed, &[-1]);
                self.held.remove(&ctx.pid);
                self.free_fixed.push(fixed);
                self.reopen_buf.push(ReopenJob {
                    pid_idx: idx,
                    pid: ctx.pid,
                    stat_off: ctx.stat_off,
                    stat_cap: ctx.stat_cap,
                });
            }
        }
        reaped
    }

    /// Close direct descriptors for PIDs not submitted this generation (vanished).
    fn evict(&mut self, cur_gen: u32) {
        let Self {
            ring,
            held,
            free_fixed,
            ..
        } = self;
        held.retain(|_, h| {
            if h.seen_gen == cur_gen {
                true
            } else {
                let _ = ring.submitter().register_files_update(h.fixed_idx, &[-1]);
                free_fixed.push(h.fixed_idx);
                false
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gather::{BYTES_PER_PID, INIT_BUF, RING_ENTRIES};
    use std::time::{Duration, Instant};

    fn enum_pids() -> Vec<u32> {
        let dir = crate::sys::ProcDir::open().unwrap();
        let mut dent = vec![0u8; 64 * 1024];
        let mut pids = Vec::new();
        dir.read_pids(&mut dent, &mut pids);
        pids.sort_unstable();
        pids
    }

    fn name_of(snap: &Snapshot, pid: u32) -> Option<String> {
        snap.procs
            .iter()
            .find(|p| p.pid == pid)
            .map(|p| String::from_utf8_lossy(snap.strings.get(p.name)).into_owned())
    }

    /// The core invariant the design rests on: a held fixed descriptor, re-read with
    /// `ReadFixed(offset=0)` on a later cycle, regenerates *fresh* stat content — no
    /// reopen. We burn CPU between cycles and assert our own ticks advanced.
    #[test]
    fn persistent_fd_rereads_fresh_content() {
        let page_size = crate::sys::page_size();
        let me = std::process::id();
        let pids = vec![me];
        let mut a = Snapshot::new(INIT_BUF, 0);
        let mut b = Snapshot::new(INIT_BUF, 1);
        let Some(mut uring) = UringBackend::probe(64, RING_ENTRIES, RING_ENTRIES as usize, &a, &b)
        else {
            eprintln!("io_uring unavailable — skipping");
            return;
        };
        let mut long = HashSet::new();

        a.push_tombstone(me);
        uring.collect(&pids, &mut a, page_size, &mut long).unwrap();
        a.compact();
        let ticks1 = a.procs.iter().find(|p| p.pid == me).expect("self c1").ticks;
        assert_eq!(uring.held.len(), 1, "self fd held after cycle 1");
        let fixed1 = uring.held[&me].fixed_idx;

        // Burn CPU to advance our utime.
        let start = Instant::now();
        let mut x = 0u64;
        while start.elapsed() < Duration::from_millis(300) {
            x = std::hint::black_box(x.wrapping_mul(2_654_435_761).wrapping_add(1));
        }
        std::hint::black_box(x);

        b.push_tombstone(me);
        uring.collect(&pids, &mut b, page_size, &mut long).unwrap();
        b.compact();
        let ticks2 = b.procs.iter().find(|p| p.pid == me).expect("self c2").ticks;
        assert_eq!(uring.held.len(), 1, "fd reused, not reopened");
        assert_eq!(uring.held[&me].fixed_idx, fixed1, "same persistent slot");
        assert!(
            ticks2 > ticks1,
            "persistent fd must re-read fresh ticks: {ticks1} -> {ticks2}"
        );
    }

    /// With a pool smaller than the live PID count, the surplus uses the transient
    /// fallback — `pool_overflow` is reported and the snapshot is still complete.
    #[test]
    fn overflow_falls_back_and_stays_correct() {
        let page_size = crate::sys::page_size();
        let pids = enum_pids();
        let mut a = Snapshot::new(INIT_BUF, 0);
        let b = Snapshot::new(INIT_BUF, 1);
        let Some(mut uring) = UringBackend::probe(4, RING_ENTRIES, RING_ENTRIES as usize, &a, &b)
        else {
            eprintln!("io_uring unavailable — skipping");
            return;
        };
        let mut long = HashSet::new();
        for &p in &pids {
            a.push_tombstone(p);
        }
        if a.strings.reserve(pids.len() * BYTES_PER_PID) {
            uring.update_buffer(a.buf_index, a.strings.as_ptr(), a.strings.capacity());
        }
        let overflow = uring.collect(&pids, &mut a, page_size, &mut long).unwrap();
        a.compact();

        assert!(pids.len() > 10, "need a populated system for this test");
        assert!(overflow > 0, "pool_cap=4 must overflow on a real system");
        assert!(a.procs.len() > 10, "overflow path still collects all PIDs");
        assert!(name_of(&a, 1).is_some(), "pid 1 present despite overflow");
    }

    /// Hygiene (§3b, load-bearing for the birth probe): a PID whose open/read fails —
    /// here a reaped-then-requested dead PID — must be re-tombstoned, never left as a
    /// phantom row (real pid, `state '?'`, empty everything) that `compact` keeps.
    #[test]
    fn failed_read_leaves_no_phantom_row() {
        let page_size = crate::sys::page_size();
        let me = std::process::id();
        // Reap a child so its PID number is genuinely dead (open → ENOENT).
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let dead = child.id();
        child.kill().unwrap();
        child.wait().unwrap();

        let mut pids = vec![me, dead];
        pids.sort_unstable();
        let mut a = Snapshot::new(INIT_BUF, 0);
        let b = Snapshot::new(INIT_BUF, 1);
        let Some(mut uring) = UringBackend::probe(64, RING_ENTRIES, RING_ENTRIES as usize, &a, &b)
        else {
            eprintln!("io_uring unavailable — skipping");
            return;
        };
        let mut long = HashSet::new();
        for &p in &pids {
            a.push_tombstone(p);
        }
        uring.collect(&pids, &mut a, page_size, &mut long).unwrap();
        a.compact();

        assert!(name_of(&a, me).is_some(), "live self must survive");
        assert!(
            !a.procs.iter().any(|p| p.pid == dead),
            "dead PID must be tombstoned, not left as a phantom row"
        );
        assert!(
            a.procs.iter().all(|p| p.state != b'?'),
            "no phantom row (state '?') may survive compact"
        );
    }
}
