//! `io_uring` backend: batched, zero-copy `/proc` reads with continuous I/O↔parse
//! overlap.
//!
//! Per PID we submit a linked chain plus an independent statx:
//! ```text
//!   OpenAt(direct slot) -[IO_LINK]->  ReadFixed(slot → arena) -[IO_HARDLINK]-> Close(slot)
//!   Statx(→ uid)
//! ```
//! `IO_LINK` on open cancels the chain if the process vanished before open.
//! `IO_HARDLINK` on read guarantees `Close` runs even if the read errors, so an
//! installed direct descriptor is never leaked.
//!
//! Concurrency is a bounded in-flight window over a slot free-list: we keep filling
//! the submission queue while slots are free, then reap whatever completed and parse
//! it — so the kernel reads the next PIDs while we parse the last ones. Each PID's
//! arena region persists for the snapshot (names point into it); only the direct-fd
//! slots and statx buffers recycle.

use std::io;

use io_uring::{IoUring, opcode, squeue, types};

use crate::gather::{SLOT_SIZE, parse};
use crate::snapshot::Snapshot;
use crate::sys::ProcPath;

// CQE `user_data` packs `(slot << 2) | op` — a 2-bit op tag below the slot index.
const OP_OPEN: u64 = 0;
const OP_READ: u64 = 1;
const OP_CLOSE: u64 = 2;
const OP_STATX: u64 = 3;

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
    buf_off: u32,
    read_len: u32,
    pending: u8,
    read_ok: bool,
    statx_ok: bool,
}

pub struct UringBackend {
    ring: IoUring,
    n_slots: u32,
    free_slots: Vec<u32>,
    slots: Vec<SlotState>,
    statx: Vec<libc::statx>,
    /// Per-slot path storage; must stay live until the slot's open completes.
    paths: Vec<ProcPath>,
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
        ring.submitter().register_files_sparse(n_slots).ok()?;

        let bufs = [iovec_of(front), iovec_of(back)];
        // SAFETY: both iovecs point at live, mmap'd snapshot arenas that outlive
        // the backend (the snapshots are kept alive for the whole program).
        unsafe { ring.submitter().register_buffers(&bufs) }.ok()?;

        let z: libc::statx = unsafe { std::mem::zeroed() };
        Some(Self {
            free_slots: Vec::with_capacity(n_slots as usize),
            slots: vec![SlotState::default(); n_slots as usize],
            statx: vec![z; n_slots as usize],
            paths: vec![ProcPath::new(); n_slots as usize],
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
            paths,
            ..
        } = self;
        let buf_index = snap.buf_index;
        let mut sq = ring.submission();

        while *next < pids.len() {
            if sq.capacity() - sq.len() < 4 {
                break;
            }
            let Some(slot) = free_slots.pop() else {
                break;
            };
            let s = slot as usize;
            let pid = pids[*next];
            let off = u32::try_from(snap.strings.alloc(SLOT_SIZE)).expect("arena offset fits u32");
            let buf_ptr = snap.strings.write_ptr(off as usize);
            let path_ptr = paths[s].write(pid, b"stat");

            let dest = types::DestinationSlot::try_from_slot_target(slot).expect("slot < n_slots");

            let open = opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), path_ptr)
                .flags(libc::O_RDONLY)
                .file_index(Some(dest))
                .build()
                .user_data(pack(slot, OP_OPEN))
                .flags(squeue::Flags::IO_LINK);
            let read =
                opcode::ReadFixed::new(types::Fixed(slot), buf_ptr, SLOT_SIZE_U32, buf_index)
                    .build()
                    .user_data(pack(slot, OP_READ))
                    .flags(squeue::Flags::IO_HARDLINK);
            let close = opcode::Close::new(types::Fixed(slot))
                .build()
                .user_data(pack(slot, OP_CLOSE));
            let stx = opcode::Statx::new(
                types::Fd(libc::AT_FDCWD),
                path_ptr,
                std::ptr::from_mut(&mut statx[s]).cast(),
            )
            .mask(libc::STATX_UID)
            .build()
            .user_data(pack(slot, OP_STATX));

            // SAFETY: path/arena/statx buffers all outlive completion; SQ has room
            // for 4 entries (checked above), so no chain is split across submits.
            unsafe {
                let _ = sq.push(&open);
                let _ = sq.push(&read);
                let _ = sq.push(&close);
                let _ = sq.push(&stx);
            }
            slots[s] = SlotState {
                pid_idx: u32::try_from(*next).expect("pid index fits u32"),
                buf_off: off,
                pending: 4,
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

    /// Drain ready completions; parse a PID once all four of its CQEs arrive.
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
                OP_READ if res > 0 => {
                    st.read_len = u32::try_from(res).unwrap_or(0).min(SLOT_SIZE_U32);
                    st.read_ok = true;
                }
                OP_STATX => st.statx_ok = res >= 0,
                _ => {} // OP_OPEN / OP_CLOSE: result not needed
            }
            st.pending -= 1;
            if st.pending != 0 {
                continue;
            }

            if st.read_ok && st.statx_ok {
                let (i, off, len) = (st.pid_idx as usize, st.buf_off, st.read_len);
                let uid = statx[slot].stx_uid;
                if let Some(f) = parse::parse_stat(snap.strings.bytes(off, len), off) {
                    f.write_into(&mut snap.procs[i], uid, page_size);
                }
            }
            free_slots.push(u32::try_from(slot).unwrap_or(0));
            *completed += 1;
        }
    }
}
