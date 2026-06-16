//! Synchronous backend with a **persistent stat-fd pool**.
//!
//! The universal fallback (older kernels, restricted seccomp, containers) and the
//! correctness oracle for the `io_uring` backend. A `/proc/<pid>/stat` fd is held
//! open across cycles and re-read with `lseek(0)+read` — eliminating the per-cycle
//! open/close churn. PIDs that vanish from enumeration have their fd closed
//! (generation eviction). On a held read returning `ESRCH` (the incarnation died or
//! the PID was reused) the fd is reopened. When more PIDs are live than the pool can
//! hold (low `RLIMIT_NOFILE`), the overflow falls back to transient open→read→close.
//!
//! Reads land in a reused scratch buffer (not the snapshot arena); `parse_stat` then
//! copies the ~15 B `comm` into the arena. `uid`/`cmdline` are **not** read here — they
//! are slow-changing fields owned by `ProcCache`. This backend produces only the
//! volatile stat fields (comm + numerics, fresh every cycle).

use std::os::fd::RawFd;

use crate::gather::parse::{self};
use crate::gather::{PidMap, STAT_SLOT};
use crate::snapshot::Snapshot;
use crate::sys::ProcPath;

/// A held `/proc/<pid>/stat` fd plus the incarnation it was opened against.
struct HeldFd {
    fd: RawFd,
    /// Start time of the incarnation this fd belongs to (PID-reuse discriminator).
    start_time: u64,
    /// Tracker generation when last re-read, for evicting vanished PIDs.
    seen_gen: u32,
}

pub struct SyscallBackend {
    held: PidMap<HeldFd>,
    /// Max held fds (real fds count against `RLIMIT_NOFILE`).
    pool_cap: usize,
    cur_gen: u32,
    stat_path: ProcPath,
    /// Reused stat read buffer (no per-PID allocation); `comm` is copied out of it
    /// into the arena by `parse`.
    scratch: Vec<u8>,
}

impl SyscallBackend {
    pub fn new(pool_cap: u32) -> Self {
        Self {
            held: PidMap::default(),
            pool_cap: pool_cap as usize,
            cur_gen: 0,
            stat_path: ProcPath::new(),
            scratch: vec![0u8; STAT_SLOT],
        }
    }

    /// Re-read every live PID's stat into the snapshot. `pids` is sorted; `snap` has a
    /// tombstone per PID at the matching index. Returns the count of PIDs that could
    /// not get a persistent slot and used the transient fallback (overflow).
    pub fn collect(&mut self, pids: &[u32], snap: &mut Snapshot, page_size: u64) -> u32 {
        self.cur_gen = self.cur_gen.wrapping_add(1);
        let cur_gen = self.cur_gen;
        let mut overflow = 0u32;

        for (i, &pid) in pids.iter().enumerate() {
            match self.held.get_mut(&pid) {
                Some(h) => {
                    // SAFETY: held fd is ours; rewind to regenerate the single-show file.
                    unsafe { libc::lseek(h.fd, 0, libc::SEEK_SET) };
                    match fill_stat(h.fd, i, snap, page_size, &mut self.scratch) {
                        Some(start_time) if start_time == h.start_time => {
                            h.seen_gen = cur_gen;
                        }
                        _ => {
                            // ESRCH / parse-fail / reuse (start_time changed): drop the
                            // stale fd and reopen the current incarnation by path. (The
                            // start_time check is belt-and-suspenders: a held fd pins the
                            // kernel struct pid, so a successful read is always the same
                            // incarnation and a dead task reads as ESRCH.)
                            close_fd(h.fd);
                            self.held.remove(&pid);
                            if self
                                .open_and_fill(pid, i, snap, page_size, cur_gen)
                                .is_none()
                            {
                                snap.tombstone(i); // death/reopen-fail: no phantom row
                            }
                        }
                    }
                }
                None => {
                    if self.held.len() < self.pool_cap {
                        if self
                            .open_and_fill(pid, i, snap, page_size, cur_gen)
                            .is_none()
                        {
                            snap.tombstone(i); // vanished before open
                        }
                    } else {
                        // Pool full: transient read, no persistent slot.
                        overflow += 1;
                        if !read_transient(
                            pid,
                            i,
                            snap,
                            page_size,
                            &mut self.stat_path,
                            &mut self.scratch,
                        ) {
                            snap.tombstone(i); // transient read failed
                        }
                    }
                }
            }
        }

        self.evict(cur_gen);
        overflow
    }

    /// Open `/proc/<pid>/stat`, fill the snapshot, and install the fd into the pool.
    /// Returns the parsed `start_time` on success.
    fn open_and_fill(
        &mut self,
        pid: u32,
        idx: usize,
        snap: &mut Snapshot,
        page_size: u64,
        cur_gen: u32,
    ) -> Option<u64> {
        let ptr = self.stat_path.write(pid, b"stat");
        // SAFETY: valid C path, read-only.
        let fd = unsafe { libc::open(ptr, libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        if let Some(st) = fill_stat(fd, idx, snap, page_size, &mut self.scratch) {
            self.held.insert(
                pid,
                HeldFd {
                    fd,
                    start_time: st,
                    seen_gen: cur_gen,
                },
            );
            Some(st)
        } else {
            close_fd(fd);
            None
        }
    }

    /// Close fds for PIDs not seen this generation (vanished from `/proc`).
    fn evict(&mut self, cur_gen: u32) {
        self.held.retain(|_, h| {
            if h.seen_gen == cur_gen {
                true
            } else {
                close_fd(h.fd);
                false
            }
        });
    }
}

impl Drop for SyscallBackend {
    fn drop(&mut self) {
        for h in self.held.values() {
            close_fd(h.fd);
        }
    }
}

fn close_fd(fd: RawFd) {
    // SAFETY: our fd, closed exactly once (removed from the map at the same time).
    unsafe { libc::close(fd) };
}

/// Read an open stat `fd` (positioned at 0) into `scratch`, parse it, and write the
/// result into `snap.procs[idx]` — copying `comm` out of `scratch` into `snap.strings`.
/// Returns the parsed `start_time`, or `None` on read failure (ESRCH/EOF) or parse
/// failure. Does **not** close `fd`.
fn fill_stat(
    fd: RawFd,
    idx: usize,
    snap: &mut Snapshot,
    page_size: u64,
    scratch: &mut [u8],
) -> Option<u64> {
    // SAFETY: scratch is a valid writable region; fd is open.
    let n = unsafe { libc::read(fd, scratch.as_mut_ptr().cast(), scratch.len()) };
    if n <= 0 {
        return None;
    }
    let len = usize::try_from(n).unwrap_or(0).min(scratch.len());
    let f = parse::parse_stat(&scratch[..len])?;
    let start_time = f.start_time;
    f.write_into(
        &mut snap.procs[idx],
        page_size,
        &scratch[..len],
        &mut snap.strings,
    );
    Some(start_time)
}

/// Transient open→read→close of `/proc/<pid>/stat` (the overflow path, shared with the
/// `io_uring` backend for overflow and post-drain reopen). Reads into `scratch`. A
/// non-fixed close hits `files->file_lock` briefly — never the `uring_lock` that drove
/// the storm.
pub(crate) fn read_transient(
    pid: u32,
    idx: usize,
    snap: &mut Snapshot,
    page_size: u64,
    path: &mut ProcPath,
    scratch: &mut [u8],
) -> bool {
    let ptr = path.write(pid, b"stat");
    // SAFETY: valid C path, read-only.
    let fd = unsafe { libc::open(ptr, libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return false;
    }
    let ok = fill_stat(fd, idx, snap, page_size, scratch).is_some();
    close_fd(fd);
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gather::INIT_BUF;
    use std::time::{Duration, Instant};

    fn enum_pids() -> Vec<u32> {
        let dir = crate::sys::ProcDir::open().unwrap();
        let mut dent = vec![0u8; 64 * 1024];
        let mut pids = Vec::new();
        dir.read_pids(&mut dent, &mut pids);
        pids.sort_unstable();
        pids
    }

    /// A held fd re-read with `lseek(0)+read` regenerates fresh content across cycles
    /// (the persistent-fd floor for kernels without `io_uring`).
    #[test]
    fn persistent_fd_rereads_fresh_content() {
        let page_size = crate::sys::page_size();
        let me = std::process::id();
        let pids = vec![me];
        let mut backend = SyscallBackend::new(64);

        let mut a = Snapshot::new(INIT_BUF);
        a.push_tombstone(me);
        backend.collect(&pids, &mut a, page_size);
        a.compact();
        let ticks1 = a.procs.iter().find(|p| p.pid == me).expect("self c1").ticks;
        assert_eq!(backend.held.len(), 1, "self fd held after cycle 1");
        let fd1 = backend.held[&me].fd;

        let start = Instant::now();
        let mut x = 0u64;
        while start.elapsed() < Duration::from_millis(300) {
            x = std::hint::black_box(x.wrapping_mul(2_654_435_761).wrapping_add(1));
        }
        std::hint::black_box(x);

        let mut b = Snapshot::new(INIT_BUF);
        b.push_tombstone(me);
        backend.collect(&pids, &mut b, page_size);
        b.compact();
        let ticks2 = b.procs.iter().find(|p| p.pid == me).expect("self c2").ticks;
        assert_eq!(backend.held.len(), 1, "fd reused, not reopened");
        assert_eq!(
            backend.held[&me].fd, fd1,
            "same persistent fd across cycles"
        );
        assert!(
            ticks2 > ticks1,
            "held fd must re-read fresh ticks: {ticks1} -> {ticks2}"
        );
    }

    /// Vanished PIDs are evicted (their held fds closed); the pool tracks only the
    /// PIDs still present.
    #[test]
    fn evicts_vanished_pids() {
        let page_size = crate::sys::page_size();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id();
        let me = std::process::id();
        let mut backend = SyscallBackend::new(64);

        let pids = {
            let mut v = vec![me, child_pid];
            v.sort_unstable();
            v
        };
        let mut a = Snapshot::new(INIT_BUF);
        for &p in &pids {
            a.push_tombstone(p);
        }
        backend.collect(&pids, &mut a, page_size);
        assert_eq!(backend.held.len(), 2, "both PIDs held");

        child.kill().unwrap();
        child.wait().unwrap();

        // Next cycle enumerates only self → the child's fd is evicted.
        let only_me = vec![me];
        let mut b = Snapshot::new(INIT_BUF);
        b.push_tombstone(me);
        backend.collect(&only_me, &mut b, page_size);
        assert_eq!(backend.held.len(), 1, "vanished PID evicted");
        assert!(backend.held.contains_key(&me));
    }

    /// Pool smaller than the live PID count → surplus uses the transient fallback,
    /// reported as overflow; the snapshot stays complete.
    #[test]
    fn overflow_falls_back_and_stays_correct() {
        let page_size = crate::sys::page_size();
        let pids = enum_pids();
        let mut backend = SyscallBackend::new(4);
        let mut a = Snapshot::new(INIT_BUF);
        for &p in &pids {
            a.push_tombstone(p);
        }
        let overflow = backend.collect(&pids, &mut a, page_size);
        a.compact();

        assert!(pids.len() > 10, "need a populated system for this test");
        assert!(overflow > 0, "pool_cap=4 must overflow");
        assert_eq!(backend.held.len(), 4, "pool holds exactly its capacity");
        assert!(
            a.procs.iter().any(|p| p.pid == 1),
            "pid 1 present despite overflow"
        );
    }

    /// Hygiene (§3b): a dead PID requested from the backend must be re-tombstoned, never
    /// left as a phantom row (real pid, `state '?'`) that `compact` keeps. Mirrors the
    /// uring backend's assertion so the oracle agrees on the failure path.
    #[test]
    fn failed_read_leaves_no_phantom_row() {
        let page_size = crate::sys::page_size();
        let me = std::process::id();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let dead = child.id();
        child.kill().unwrap();
        child.wait().unwrap();

        let mut pids = vec![me, dead];
        pids.sort_unstable();
        let mut backend = SyscallBackend::new(64);
        let mut a = Snapshot::new(INIT_BUF);
        for &p in &pids {
            a.push_tombstone(p);
        }
        backend.collect(&pids, &mut a, page_size);
        a.compact();

        assert!(
            a.procs.iter().any(|p| p.pid == me),
            "live self must survive"
        );
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
