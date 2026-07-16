//! `io_uring` backend with a **persistent stat-fd pool** and a **fixed landing pad**.
//!
//! A `/proc/<pid>/stat` fd is installed once as an `io_uring` **direct (fixed)
//! descriptor** and re-read every cycle with a single `ReadFixed` at offset 0 (a
//! procfs single-show file regenerates fresh content on a repeated offset-0 read —
//! validated). No per-cycle open, no per-cycle close: the `osq_lock`/`uring_lock`
//! storm that fixed-fd closes drove from `io-wq` is gone.
//!
//! ```text
//!   cached PID:  ReadFixed(fixed_idx, slot, off 0)                    (1 SQE)
//!   new PID:     OpenAt(/stat, file_index) -[IO_LINK]-> ReadFixed     (2 SQEs, no Close)
//!   dead PID:    register_files_update(idx, -1)  (eviction, low volume)
//! ```
//!
//! **Landing pad.** Reads target a small, fixed `MmapRegion` of `READ_SLOTS × STAT_SLOT`
//! bytes, registered **once** as the single fixed buffer. Each in-flight read claims a slot
//! from a free-list; `reap` parses it, copies the ~15 B `comm` inline into the process row,
//! and frees the slot. So **pinned memory is constant** (~`READ_SLOTS × STAT_SLOT`), independent of
//! PID count — the arena no longer counts against the per-process lock limit, which is
//! what lets a non-root `perf` fit its own ring alongside us. In-flight depth is bounded
//! by `READ_SLOTS`; at high PID counts a cycle takes a few more wait rounds (negligible
//! against the 500 ms interval).
//!
//! A held read returning `ESRCH` (the incarnation died / the PID was reused) closes the
//! slot and reads the current incarnation transiently this cycle; it gets a fresh
//! persistent slot next cycle. When more PIDs are live than the pool holds (low
//! `RLIMIT_NOFILE`), the overflow uses the shared transient read.
//!
//! `uid`/`cmdline` are not read here — they are the `ProcTable`'s job (plain syscalls on a
//! coarse interval). This backend produces only the volatile stat fields.

use std::io;

use io_uring::{IoUring, opcode, squeue, types};

use thoop::MmapRegion;

use super::config::STAT_SLOT;
use super::parse;
use super::syscall::read_transient;
use crate::fxhash::PidMap;
use crate::log::debug_log;
use crate::procs::Procs;
use crate::sys::{self, ProcPath};

/// The landing pad is the only registered buffer, at this fixed index.
const READ_BUF_INDEX: u16 = 0;

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
/// current incarnation transiently after the ring drains.
#[derive(Clone, Copy)]
struct ReopenJob {
    pid_idx: usize,
    pid: u32,
}

/// Per-cycle scratch for an in-flight stat chain, indexed by `fixed_idx`.
#[derive(Clone, Copy, Default)]
struct Ctx {
    pid: u32,
    pid_idx: u32,
    /// Landing-pad slot this chain's `ReadFixed` targets (freed on completion).
    read_slot: u32,
    stat_len: u32,
    pending: u8,
    /// The `OpenAt` of a new chain failed (process gone before open) — the slot was
    /// never installed, so it is freed without a close.
    open_failed: bool,
    /// Raw CQE result of the open (diagnostic: the error code when `open_failed` is true).
    open_res: i32,
    /// The `ReadFixed` returned data.
    read_ok: bool,
    /// Raw CQE result of the read (diagnostic: the error code when `read_ok` is false).
    read_res: i32,
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
    /// One huge-page mapping serving two non-overlapping regions:
    /// `[0, scratch_off)` = `read_slots` registered read slots (the only **pinned**
    /// bytes — `io_uring` writes `ReadFixed` results here, `comm` is copied out into the
    /// arena); `[scratch_off, scratch_off + STAT_SLOT)` = a dedicated, **unregistered**
    /// scratch slot carved from the huge page's free tail, used by the post-drain
    /// transient reads. Never grows. Splitting the pinned region from the scratch this
    /// way removes the old "slot 0 is free only because the ring drained" invariant.
    read_pad: MmapRegion,
    /// Byte offset of the transient scratch slot — the first byte past the registered
    /// read region (`read_slots × STAT_SLOT`).
    scratch_off: usize,
    /// Free read-slot indices (`0..read_slots`), all within the registered region.
    read_free: Vec<u32>,
    /// Per-cycle scratch queues, kept on `self` to reuse capacity across cycles
    /// (cleared at the start of `collect`). Drained by index so the drain can borrow
    /// other `self` fields without a take-and-restore dance.
    done_buf: Vec<u32>,
    overflow_buf: Vec<(usize, u32)>,
    /// PIDs whose held read hit `ESRCH` (reuse/death): re-read transiently after the
    /// ring drains.
    reopen_buf: Vec<ReopenJob>,
    cur_gen: u32,
    /// Max CQEs to wait for per round (the I/O↔parse overlap dial). Default = a full
    /// ring, so each round submits the batch and waits for **all** of it once (no
    /// overlap; parse is cheap). Lowering it reaps+parses a chunk while later reads
    /// complete. Outstanding CQEs per round ≤ `READ_SLOTS × 2`, so the default never
    /// under-waits.
    batch_cap: usize,
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

impl UringBackend {
    /// Probe `io_uring`: build a ring, register a sparse direct-descriptor table sized
    /// to the pool, and register the landing pad as the single fixed buffer. Returns
    /// `None` if any step is unsupported (old kernel, restricted seccomp) — caller
    /// falls back. Does **not** touch the process buffer (the pad is the only registered buffer).
    pub fn probe(
        pool_cap: u32,
        ring_entries: u32,
        batch_cap: usize,
        read_slots: usize,
    ) -> Option<Self> {
        let ring = build_ring(ring_entries).ok()?;
        // One direct-fd slot per held PID (stat only — cmdline left the chain).
        ring.submitter().register_files_sparse(pool_cap).ok()?;

        // A huge mapping rounded up to ≥ 2 MiB: the read slots use the first
        // `read_slots × STAT_SLOT` bytes, leaving the rest of the huge page as free tail.
        // The `+1` slot guarantees the tail has room for the scratch slot even when the
        // read region is itself a whole-huge-page multiple (e.g. a large `ATOP_READ_SLOTS`).
        let registered_len = read_slots * STAT_SLOT;
        let read_pad = MmapRegion::huge(registered_len + STAT_SLOT);
        // Register ONLY the read region — pinned (locked) memory stays bounded to
        // `read_slots × STAT_SLOT` regardless of the (larger) mapping. The scratch slot
        // lives past it, unregistered, and is read into with a plain `read` (no ReadFixed),
        // so it never needs to be a registered buffer.
        let iov = libc::iovec {
            iov_base: read_pad.as_ptr().cast(),
            iov_len: registered_len,
        };
        // SAFETY: the iovec points at the live, owned landing pad, which outlives the
        // backend (and never moves — it is fixed-size, never grown).
        unsafe { ring.submitter().register_buffers(&[iov]) }.ok()?;

        let cap = pool_cap as usize;
        let mut free_fixed = Vec::with_capacity(cap);
        for s in (0..pool_cap).rev() {
            free_fixed.push(s);
        }
        let read_slots = u32::try_from(read_slots).expect("read_slots fits u32 (clamped)");
        let mut read_free = Vec::with_capacity(read_slots as usize);
        for s in (0..read_slots).rev() {
            read_free.push(s);
        }
        Some(Self {
            ring,
            held: PidMap::default(),
            free_fixed,
            ctxs: vec![Ctx::default(); cap],
            paths: vec![ProcPath::new(); cap],
            stat_path: ProcPath::new(),
            read_pad,
            scratch_off: registered_len,
            read_free,
            done_buf: Vec::new(),
            overflow_buf: Vec::new(),
            reopen_buf: Vec::new(),
            cur_gen: 0,
            batch_cap,
        })
    }

    /// Re-read every live PID's stat into `procs`. Returns the overflow count
    /// (PIDs that exceeded the pool and used the transient fallback).
    pub fn collect(&mut self, pids: &[u32], procs: &mut Procs, page_size: u64) -> io::Result<u32> {
        self.cur_gen = self.cur_gen.wrapping_add(1);
        let cur_gen = self.cur_gen;
        let mut next = 0usize;
        // Outstanding **CQEs**, not chains: a cached read emits 1, a new chain 2 (the
        // linked open + read — and a failed open still posts an `ECANCELED` read CQE, so
        // 2 is exact either way). Waiting on this count collapses wakeups: submit a batch,
        // wait once for all of it, drain, parse.
        let mut outstanding = 0usize;
        self.overflow_buf.clear();
        self.reopen_buf.clear();

        while next < pids.len() || outstanding > 0 {
            outstanding += self.fill(pids, &mut next, cur_gen);
            if outstanding == 0 {
                break; // only overflow PIDs remain (pool was full)
            }
            // Default `batch_cap` = a full ring ⇒ wait for the entire outstanding batch
            // (one wakeup/round). A smaller cap re-enables I/O↔parse overlap (the dial).
            let want = outstanding.min(self.batch_cap);
            self.submit_and_wait(want)?;
            outstanding -= self.reap_and_process(procs, page_size);
        }

        // Reopen (reuse/death) reads, then the overflow tail, both read transiently into
        // the dedicated scratch slot and copy `comm` inline into the row. The scratch lives in
        // the unregistered tail, never in `read_free`, so it is always safe to use — no
        // dependence on the ring being drained. No `start_time` re-check is needed on these
        // by-path reads: a recycled PID is genuinely that PID now (the row is keyed by PID
        // number and the slow fields self-heal — the `ProcTable` resets CPU/cmdline on the
        // start_time/ticks change this same cycle), and kill-safety is owned independently
        // by `kill_verified` (pidfd + start_time re-check). Drained by index so each read
        // can borrow `self`.
        let scratch_off = self.scratch_off;
        for i in 0..self.reopen_buf.len() {
            let job = self.reopen_buf[i];
            let scratch = self.read_pad.slice_mut(scratch_off, STAT_SLOT);
            if !read_transient(
                job.pid,
                job.pid_idx,
                procs,
                page_size,
                &mut self.stat_path,
                scratch,
            ) {
                debug_log!(
                    "[uring] reopen transient failed: pid={} idx={}",
                    job.pid,
                    job.pid_idx
                );
                procs.tombstone(job.pid_idx);
            }
        }
        for i in 0..self.overflow_buf.len() {
            let (idx, pid) = self.overflow_buf[i];
            let scratch = self.read_pad.slice_mut(scratch_off, STAT_SLOT);
            if !read_transient(pid, idx, procs, page_size, &mut self.stat_path, scratch) {
                debug_log!("[uring] overflow transient failed: pid={pid} idx={idx}");
                procs.tombstone(idx);
            }
        }
        let overflow = u32::try_from(self.overflow_buf.len()).unwrap_or(u32::MAX);

        self.evict(cur_gen);
        Ok(overflow)
    }

    /// Submit stat chains until the SQ fills, the fixed pool is exhausted, or no landing
    /// slot is free. Returns the number of **CQEs** the submitted chains will produce
    /// (cached read = 1, new open+read chain = 2) — the count [`collect`] waits on.
    ///
    /// [`collect`]: Self::collect
    #[allow(clippy::cast_possible_truncation)] // STAT_SLOT is a small const
    fn fill(&mut self, pids: &[u32], next: &mut usize, cur_gen: u32) -> usize {
        let mut cqes = 0usize;
        let mut sq = self.ring.submission();

        while *next < pids.len() {
            // A new chain needs 2 SQEs; require room for the larger case.
            if sq.capacity() - sq.len() < 2 {
                break;
            }
            let idx = *next;
            let pid = pids[idx];

            if let Some(h) = self.held.get_mut(&pid) {
                // Cached: single ReadFixed at offset 0 into a fresh landing slot.
                let Some(read_slot) = self.read_free.pop() else {
                    // Pad full: break with `*next` unchanged so this same PID is retried on
                    // the next `fill`, after `reap_and_process` frees slots. Not a skip.
                    break;
                };
                h.seen_gen = cur_gen;
                let fixed = h.fixed_idx;
                let ptr = self.read_pad.write_ptr(read_slot as usize * STAT_SLOT);
                let read = opcode::ReadFixed::new(
                    types::Fixed(fixed),
                    ptr,
                    STAT_SLOT as u32,
                    READ_BUF_INDEX,
                )
                .offset(0)
                .build()
                .user_data(pack(fixed, OP_READ));
                // SAFETY: landing slot outlives completion; SQ has room (checked).
                unsafe { sq.push(&read).expect("sq push") };
                self.ctxs[fixed as usize] = Ctx {
                    pid,
                    pid_idx: idx as u32,
                    read_slot,
                    pending: 1,
                    ..Ctx::default()
                };
                cqes += 1;
            } else if !self.free_fixed.is_empty() && !self.read_free.is_empty() {
                // New: OpenAt installs the direct descriptor, linked ReadFixed reads it.
                let fixed = self.free_fixed.pop().expect("free_fixed non-empty");
                let read_slot = self.read_free.pop().expect("read_free non-empty");
                let ptr = self.read_pad.write_ptr(read_slot as usize * STAT_SLOT);
                let path_ptr = self.paths[fixed as usize].write(pid, b"stat");
                let dest = types::DestinationSlot::try_from_slot_target(fixed).expect("slot fits");
                let open = opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), path_ptr)
                    .flags(libc::O_RDONLY)
                    .file_index(Some(dest))
                    .build()
                    .user_data(pack(fixed, OP_OPEN))
                    .flags(squeue::Flags::IO_LINK);
                let read = opcode::ReadFixed::new(
                    types::Fixed(fixed),
                    ptr,
                    STAT_SLOT as u32,
                    READ_BUF_INDEX,
                )
                .offset(0)
                .build()
                .user_data(pack(fixed, OP_READ));
                // SAFETY: path + landing slot outlive completion; SQ room checked (≥2).
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
                    read_slot,
                    pending: 2,
                    ..Ctx::default()
                };
                cqes += 2;
            } else if self.free_fixed.is_empty() {
                // fd-pool exhausted (not pad-full — `read_free` still has slots here):
                // this PID gets no persistent descriptor, so defer it to the transient
                // overflow pass. `*next` advances — overflow is a real disposition, not a retry.
                debug_log!(
                    "[uring] overflow: pid={pid} idx={idx} held={} free_fixed=0",
                    self.held.len()
                );
                self.overflow_buf.push((idx, pid));
            } else {
                // Fixed slot free but the landing pad is full: break with `*next` unchanged
                // so this PID is retried after the reap frees slots (a retry, not a skip).
                break;
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

    /// Drain ready completions; process each PID once both its CQEs land, copying `comm`
    /// out of its landing slot inline into the row and freeing the slot. Returns the number
    /// of **CQEs** reaped (chains complete in 1 or 2 CQEs).
    #[allow(clippy::cast_possible_truncation)] // STAT_SLOT is a small const
    fn reap_and_process(&mut self, procs: &mut Procs, page_size: u64) -> usize {
        self.done_buf.clear();
        let mut reaped = 0usize;
        for cqe in self.ring.completion() {
            reaped += 1;
            let (fixed, op) = unpack(cqe.user_data());
            let res = cqe.result();
            // `user_data` is echoed by the kernel, so `fixed` is always one we submitted
            // (< pool_cap == ctxs.len()). Guard anyway: a stale/spurious CQE would otherwise
            // panic the gatherer on an out-of-bounds index. It is still counted as reaped
            // (it was drained from the CQ); discarding it just skips processing.
            let Some(ctx) = self.ctxs.get_mut(fixed) else {
                continue;
            };
            if op == OP_OPEN {
                ctx.open_res = res;
                if res < 0 {
                    ctx.open_failed = true;
                }
            } else {
                ctx.read_res = res;
                if res > 0 {
                    ctx.stat_len = u32::try_from(res).unwrap_or(0).min(STAT_SLOT as u32);
                    ctx.read_ok = true;
                }
            }
            ctx.pending -= 1;
            if ctx.pending == 0 {
                self.done_buf.push(u32::try_from(fixed).unwrap_or(0));
            }
        }

        // Drain by index so the failure branches can borrow `self.ring`/`self.held` while
        // `done_buf` keeps its capacity. `Ctx` is `Copy`, so the indexed read takes no
        // lasting borrow.
        let n = self.done_buf.len();
        for i in 0..n {
            let fixed = self.done_buf[i];
            let ctx = self.ctxs[fixed as usize];
            let idx = ctx.pid_idx as usize;
            if ctx.read_ok {
                let slot = self
                    .read_pad
                    .bytes(ctx.read_slot as usize * STAT_SLOT, ctx.stat_len as usize);
                if let Some(f) = parse::parse_stat(slot) {
                    // New chain (open_res > 0): two checks before accepting.
                    if ctx.open_res > 0 {
                        // 1. PID cross-check: the stat line's PID must match what we
                        //    asked for. A mismatch means the read hit a stale fd.
                        if f.parsed_pid != ctx.pid {
                            debug_log!(
                                "[uring] PID MISMATCH: expected={} parsed={} fixed={fixed}",
                                ctx.pid,
                                f.parsed_pid,
                            );
                            self.held.remove(&ctx.pid);
                            let _ = self.ring.submitter().register_files_update(fixed, &[-1]);
                            self.free_fixed.push(fixed);
                            self.read_free.push(ctx.read_slot);
                            procs.tombstone(idx);
                            continue;
                        }
                        // 2. Thread-leader check: reject non-leader threads whose TID
                        //    falls in the probe window (`/proc` VFS gotcha — `open`
                        //    resolves any task, `getdents` returns only TGIDs).
                        //    `pidfd_open(pid, 0)` — one syscall, EINVAL for non-leaders.
                        if !sys::is_thread_group_leader(ctx.pid) {
                            self.held.remove(&ctx.pid);
                            let _ = self.ring.submitter().register_files_update(fixed, &[-1]);
                            self.free_fixed.push(fixed);
                            self.read_free.push(ctx.read_slot);
                            procs.tombstone(idx);
                            continue;
                        }
                    }
                    f.write_into(procs.row_mut(idx), page_size, slot);
                } else {
                    debug_log!(
                        "[uring] parse failed: pid={} len={} first_bytes={:?}",
                        ctx.pid,
                        ctx.stat_len,
                        &slot[..slot.len().min(40)],
                    );
                    procs.tombstone(idx);
                }
                self.read_free.push(ctx.read_slot);
            } else if ctx.open_failed {
                // New PID vanished before open — slot never installed.
                debug_log!(
                    "[uring] open failed: pid={} fixed={fixed} open_res={}",
                    ctx.pid,
                    ctx.open_res,
                );
                self.held.remove(&ctx.pid);
                self.free_fixed.push(fixed);
                self.read_free.push(ctx.read_slot);
                procs.tombstone(idx);
            } else {
                // Installed fd read failed — log the raw CQE result for diagnosis.
                debug_log!(
                    "[uring] read failed: pid={} fixed={fixed} res={} open_failed={}",
                    ctx.pid,
                    ctx.read_res,
                    ctx.open_failed,
                );
                let _ = self.ring.submitter().register_files_update(fixed, &[-1]);
                self.held.remove(&ctx.pid);
                self.free_fixed.push(fixed);
                self.read_free.push(ctx.read_slot);
                self.reopen_buf.push(ReopenJob {
                    pid_idx: idx,
                    pid: ctx.pid,
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
    use crate::gather::config::{READ_SLOTS, RING_ENTRIES};
    use std::time::{Duration, Instant};
    use thoop::Arena;

    /// A wired, arena-backed `Procs` for backend tests. The returned `Box<Arena>` must be
    /// kept alive alongside the `Procs` (the buffer caches its pinned address).
    fn test_procs() -> (Box<Arena>, Procs) {
        let arena = Box::new(Arena::new(0));
        let mut procs = Procs::new(&arena, 256);
        procs.wire(&arena);
        (arena, procs)
    }

    fn enum_pids() -> Vec<u32> {
        let dir = crate::sys::ProcDir::open().unwrap();
        let mut dent = vec![0u8; 64 * 1024];
        let mut pids = Vec::new();
        dir.read_pids(&mut dent, &mut pids);
        pids.sort_unstable();
        pids
    }

    fn name_of(procs: &Procs, pid: u32) -> Option<String> {
        procs
            .as_slice()
            .iter()
            .find(|p| p.pid == pid)
            .map(|p| String::from_utf8_lossy(p.comm()).into_owned())
    }

    /// The core invariant the design rests on: a held fixed descriptor, re-read with
    /// `ReadFixed(offset=0)` on a later cycle, regenerates *fresh* stat content — no
    /// reopen. We burn CPU between cycles and assert our own ticks advanced.
    #[test]
    fn persistent_fd_rereads_fresh_content() {
        let page_size = crate::sys::page_size();
        let me = std::process::id();
        let pids = vec![me];
        let (_aa, mut a) = test_procs();
        let Some(mut uring) =
            UringBackend::probe(64, RING_ENTRIES, RING_ENTRIES as usize, READ_SLOTS)
        else {
            eprintln!("io_uring unavailable — skipping");
            return;
        };

        a.push_tombstone(me);
        uring.collect(&pids, &mut a, page_size).unwrap();
        a.compact();
        let ticks1 = a
            .as_slice()
            .iter()
            .find(|p| p.pid == me)
            .expect("self c1")
            .ticks;
        assert_eq!(uring.held.len(), 1, "self fd held after cycle 1");
        let fixed1 = uring.held[&me].fixed_idx;

        // Burn CPU to advance our utime.
        let start = Instant::now();
        let mut x = 0u64;
        while start.elapsed() < Duration::from_millis(300) {
            x = std::hint::black_box(x.wrapping_mul(2_654_435_761).wrapping_add(1));
        }
        std::hint::black_box(x);

        let (_ab, mut b) = test_procs();
        b.push_tombstone(me);
        uring.collect(&pids, &mut b, page_size).unwrap();
        b.compact();
        let ticks2 = b
            .as_slice()
            .iter()
            .find(|p| p.pid == me)
            .expect("self c2")
            .ticks;
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
        let (_arena, mut a) = test_procs();
        let Some(mut uring) =
            UringBackend::probe(4, RING_ENTRIES, RING_ENTRIES as usize, READ_SLOTS)
        else {
            eprintln!("io_uring unavailable — skipping");
            return;
        };
        for &p in &pids {
            a.push_tombstone(p);
        }
        let overflow = uring.collect(&pids, &mut a, page_size).unwrap();
        a.compact();

        assert!(pids.len() > 10, "need a populated system for this test");
        assert!(overflow > 0, "pool_cap=4 must overflow on a real system");
        assert!(
            a.as_slice().len() > 10,
            "overflow path still collects all PIDs"
        );
        assert!(name_of(&a, 1).is_some(), "pid 1 present despite overflow");
    }

    /// A live set far larger than the landing pad must still complete: the bounded pad
    /// forces many fill/reap rounds and recycles its slots. Uses a tiny `read_slots` (8)
    /// so the multi-round path is exercised regardless of host PID count, and a pool big
    /// enough that no PID overflows — every PID must land via slot recycling alone.
    #[test]
    fn bounded_pad_recycles_slots_across_rounds() {
        let page_size = crate::sys::page_size();
        let pids = enum_pids();
        let (_arena, mut a) = test_procs();
        let Some(mut uring) = UringBackend::probe(
            crate::gather::config::MAX_POOL,
            RING_ENTRIES,
            RING_ENTRIES as usize,
            8,
        ) else {
            eprintln!("io_uring unavailable — skipping");
            return;
        };
        for &p in &pids {
            a.push_tombstone(p);
        }
        let overflow = uring.collect(&pids, &mut a, page_size).unwrap();
        a.compact();
        assert!(pids.len() > 8, "need more PIDs than the 8-slot pad");
        assert_eq!(
            overflow, 0,
            "pool large enough — no fd-pool overflow expected"
        );
        assert!(
            a.as_slice().len() > 8,
            "all {} pids must land across rounds with only 8 read slots, got {}",
            pids.len(),
            a.as_slice().len()
        );
        assert!(name_of(&a, 1).is_some(), "pid 1 present");
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
        let (_arena, mut a) = test_procs();
        let Some(mut uring) =
            UringBackend::probe(64, RING_ENTRIES, RING_ENTRIES as usize, READ_SLOTS)
        else {
            eprintln!("io_uring unavailable — skipping");
            return;
        };
        for &p in &pids {
            a.push_tombstone(p);
        }
        uring.collect(&pids, &mut a, page_size).unwrap();
        a.compact();

        assert!(name_of(&a, me).is_some(), "live self must survive");
        assert!(
            !a.as_slice().iter().any(|p| p.pid == dead),
            "dead PID must be tombstoned, not left as a phantom row"
        );
        assert!(
            a.as_slice().iter().all(|p| p.state != b'?'),
            "no phantom row (state '?') may survive compact"
        );
    }
}
