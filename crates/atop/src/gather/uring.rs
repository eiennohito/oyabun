//! `io_uring` backend: batched, zero-copy `/proc` reads with continuous I/O↔parse
//! overlap.
//!
//! Per PID we submit two linked chains plus an independent statx:
//! ```text
//!   OpenAt(/stat, fd=2s)  -[IO_LINK]->  ReadFixed(→ arena) -[IO_HARDLINK]-> Close(fd=2s)
//!   OpenAt(/cmdline, fd=2s+1) -[IO_LINK]-> ReadFixed(→ arena) -[IO_HARDLINK]-> Close(fd=2s+1)
//!   Statx(→ uid)
//! ```
//! `IO_LINK` on open cancels the chain if the process vanished before open.
//! `IO_HARDLINK` on read guarantees `Close` runs even if the read errors, so an
//! installed direct descriptor is never leaked.
//!
//! Concurrency is a bounded in-flight window over a slot free-list: we keep filling
//! the submission queue while slots are free, then reap whatever completed and parse
//! it — so the kernel reads the next PIDs while we parse the last ones. Each PID's
//! arena regions persist for the snapshot (names/cmdline point into them); only the
//! direct-fd slots and statx buffers recycle.

use std::io;

use io_uring::{IoUring, opcode, squeue, types};

use crate::arena::StringRef;
use crate::gather::{SLOT_SIZE, parse};
use crate::snapshot::Snapshot;
use crate::sys::ProcPath;

// CQE `user_data` packs `(slot << 2) | op` — 2-bit op tag below the slot index.
// We only need to distinguish: stat read, cmdline read, statx, and everything else.
const OP_STAT_READ: u64 = 0;
const OP_CMD_READ: u64 = 1;
const OP_STATX: u64 = 2;
const OP_OTHER: u64 = 3;

/// `io_uring` ops per PID: stat(open+read+close) + cmd(open+read+close) + statx = 7.
/// Each SQE produces exactly one CQE, so SQ capacity and pending count are the same.
const OPS_PER_PID: usize = 7;

fn pack(slot: u32, op: u64) -> u64 {
    (u64::from(slot) << 2) | op
}

fn unpack(user_data: u64) -> (usize, u64) {
    ((user_data >> 2) as usize, user_data & 0b11)
}

#[allow(clippy::cast_possible_truncation)] // SLOT_SIZE is a small compile-time const
const SLOT_SIZE_U32: u32 = SLOT_SIZE as u32;

#[derive(Clone, Copy, Default)]
struct SlotState {
    pid_idx: u32,
    stat_off: u32,
    stat_len: u32,
    cmd_off: u32,
    cmd_len: u32,
    pending: u8,
    stat_ok: bool,
    cmd_ok: bool,
    statx_ok: bool,
}

pub struct UringBackend {
    ring: IoUring,
    n_slots: u32,
    free_slots: Vec<u32>,
    slots: Vec<SlotState>,
    statx: Vec<libc::statx>,
    /// Per-slot path storage for `/proc/<pid>/stat`.
    stat_paths: Vec<ProcPath>,
    /// Per-slot path storage for `/proc/<pid>/cmdline`.
    cmd_paths: Vec<ProcPath>,
    /// Registered buffer descriptors, one per snapshot (index = `buf_index`).
    bufs: [libc::iovec; 2],
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
    /// Probe `io_uring`: build a ring, register a sparse direct-descriptor table, and
    /// register both snapshot arenas as fixed buffers. Returns `None` if any step
    /// is unsupported (old kernel, restricted seccomp) — caller falls back.
    pub fn probe(
        n_slots: u32,
        ring_entries: u32,
        front: &Snapshot,
        back: &Snapshot,
    ) -> Option<Self> {
        let ring = IoUring::new(ring_entries).ok()?;
        // Two direct-fd slots per PID slot: one for stat, one for cmdline.
        ring.submitter()
            .register_files_sparse(n_slots.checked_mul(2)?)
            .ok()?;

        let bufs = [iovec_of(front), iovec_of(back)];
        // SAFETY: both iovecs point at live, mmap'd snapshot arenas that outlive
        // the backend (the snapshots are kept alive for the whole program).
        unsafe { ring.submitter().register_buffers(&bufs) }.ok()?;

        let z: libc::statx = unsafe { std::mem::zeroed() };
        let ns = n_slots as usize;
        Some(Self {
            free_slots: Vec::with_capacity(ns),
            slots: vec![SlotState::default(); ns],
            statx: vec![z; ns],
            stat_paths: vec![ProcPath::new(); ns],
            cmd_paths: vec![ProcPath::new(); ns],
            bufs,
            n_slots,
            ring,
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

    pub fn collect(&mut self, pids: &[u32], snap: &mut Snapshot, page_size: u64) -> io::Result<()> {
        let total = pids.len();
        let mut next = 0usize;
        let mut completed = 0usize;

        self.free_slots.clear();
        for s in (0..self.n_slots).rev() {
            self.free_slots.push(s);
        }

        while completed < total {
            self.fill(pids, snap, &mut next);
            self.submit_and_wait()?;
            self.reap(snap, page_size, &mut completed);
        }
        Ok(())
    }

    /// Submit linked chains for new PIDs until slots or SQ space run out.
    fn fill(&mut self, pids: &[u32], snap: &mut Snapshot, next: &mut usize) {
        let Self {
            ring,
            free_slots,
            slots,
            statx,
            stat_paths,
            cmd_paths,
            ..
        } = self;
        let buf_index = snap.buf_index;
        let mut sq = ring.submission();

        while *next < pids.len() {
            if sq.capacity() - sq.len() < OPS_PER_PID {
                break;
            }
            let Some(slot) = free_slots.pop() else {
                break;
            };
            let s = slot as usize;
            let pid = pids[*next];

            // Two arena allocations per PID: stat + cmdline.
            let stat_off =
                u32::try_from(snap.strings.alloc(SLOT_SIZE)).expect("arena offset fits u32");
            let stat_ptr = snap.strings.write_ptr(stat_off as usize);
            let cmd_off =
                u32::try_from(snap.strings.alloc(SLOT_SIZE)).expect("arena offset fits u32");
            let cmd_ptr = snap.strings.write_ptr(cmd_off as usize);

            let stat_path_ptr = stat_paths[s].write(pid, b"stat");
            let cmd_path_ptr = cmd_paths[s].write(pid, b"cmdline");

            // Direct-fd slots: slot*2 for stat, slot*2+1 for cmdline.
            let stat_fd = slot * 2;
            let cmd_fd = slot * 2 + 1;
            let stat_dest =
                types::DestinationSlot::try_from_slot_target(stat_fd).expect("slot fits");
            let cmd_dest = types::DestinationSlot::try_from_slot_target(cmd_fd).expect("slot fits");

            // Chain 1: stat open → read → close
            let stat_open = opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), stat_path_ptr)
                .flags(libc::O_RDONLY)
                .file_index(Some(stat_dest))
                .build()
                .user_data(pack(slot, OP_OTHER))
                .flags(squeue::Flags::IO_LINK);
            let stat_read =
                opcode::ReadFixed::new(types::Fixed(stat_fd), stat_ptr, SLOT_SIZE_U32, buf_index)
                    .build()
                    .user_data(pack(slot, OP_STAT_READ))
                    .flags(squeue::Flags::IO_HARDLINK);
            let stat_close = opcode::Close::new(types::Fixed(stat_fd))
                .build()
                .user_data(pack(slot, OP_OTHER));

            // Chain 2: cmdline open → read → close
            let cmd_open = opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), cmd_path_ptr)
                .flags(libc::O_RDONLY)
                .file_index(Some(cmd_dest))
                .build()
                .user_data(pack(slot, OP_OTHER))
                .flags(squeue::Flags::IO_LINK);
            let cmd_read =
                opcode::ReadFixed::new(types::Fixed(cmd_fd), cmd_ptr, SLOT_SIZE_U32, buf_index)
                    .build()
                    .user_data(pack(slot, OP_CMD_READ))
                    .flags(squeue::Flags::IO_HARDLINK);
            let cmd_close = opcode::Close::new(types::Fixed(cmd_fd))
                .build()
                .user_data(pack(slot, OP_OTHER));

            // Independent: statx for uid
            let stx = opcode::Statx::new(
                types::Fd(libc::AT_FDCWD),
                stat_path_ptr,
                std::ptr::from_mut(&mut statx[s]).cast(),
            )
            .mask(libc::STATX_UID)
            .build()
            .user_data(pack(slot, OP_STATX));

            // SAFETY: path/arena/statx buffers all outlive completion; SQ has room
            // for OPS_PER_PID entries (checked above), so no chain is split.
            unsafe {
                let _ = sq.push(&stat_open);
                let _ = sq.push(&stat_read);
                let _ = sq.push(&stat_close);
                let _ = sq.push(&cmd_open);
                let _ = sq.push(&cmd_read);
                let _ = sq.push(&cmd_close);
                let _ = sq.push(&stx);
            }
            slots[s] = SlotState {
                pid_idx: u32::try_from(*next).expect("pid index fits u32"),
                stat_off,
                cmd_off,
                #[allow(clippy::cast_possible_truncation)] // OPS_PER_PID = 7
                pending: OPS_PER_PID as u8,
                ..SlotState::default()
            };
            *next += 1;
        }
    }

    fn submit_and_wait(&self) -> io::Result<()> {
        loop {
            match self.ring.submit_and_wait(1) {
                Ok(_) => return Ok(()),
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Drain ready completions; parse a PID once all seven of its CQEs arrive.
    fn reap(&mut self, snap: &mut Snapshot, page_size: u64, completed: &mut usize) {
        let Self {
            ring,
            free_slots,
            slots,
            statx,
            ..
        } = self;
        for cqe in ring.completion() {
            let (slot, op) = unpack(cqe.user_data());
            let res = cqe.result();
            let st = &mut slots[slot];
            match op {
                OP_STAT_READ if res > 0 => {
                    st.stat_len = u32::try_from(res).unwrap_or(0).min(SLOT_SIZE_U32);
                    st.stat_ok = true;
                }
                OP_CMD_READ if res > 0 => {
                    st.cmd_len = u32::try_from(res).unwrap_or(0).min(SLOT_SIZE_U32);
                    st.cmd_ok = true;
                }
                OP_STATX => st.statx_ok = res >= 0,
                _ => {}
            }
            st.pending -= 1;
            if st.pending != 0 {
                continue;
            }

            let i = st.pid_idx as usize;
            if st.stat_ok && st.statx_ok {
                let uid = statx[slot].stx_uid;
                if let Some(f) =
                    parse::parse_stat(snap.strings.bytes(st.stat_off, st.stat_len), st.stat_off)
                {
                    f.write_into(&mut snap.procs[i], uid, page_size);
                }
            }
            if st.cmd_ok && st.cmd_len > 0 {
                let buf = snap.strings.bytes_mut(st.cmd_off, st.cmd_len);
                let clean_len = parse::clean_cmdline(buf);
                if clean_len > 0 {
                    snap.procs[i].cmdline = StringRef {
                        offset: st.cmd_off,
                        len: clean_len,
                    };
                }
            }
            free_slots.push(u32::try_from(slot).unwrap_or(0));
            *completed += 1;
        }
    }
}
