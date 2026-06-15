//! Synchronous `open`/`read`/`fstat`/`close` backend.
//!
//! The universal fallback (older kernels, restricted seccomp, containers) and the
//! correctness oracle for the `io_uring` backend. Reads directly into the arena.

use crate::gather::{SLOT_SIZE, parse};
use crate::snapshot::Snapshot;
use crate::sys::ProcPath;

pub struct SyscallBackend;

impl SyscallBackend {
    pub fn collect(pids: &[u32], snap: &mut Snapshot, page_size: u64) {
        let mut path = ProcPath::new();
        for (i, &pid) in pids.iter().enumerate() {
            let path_ptr = path.write(pid, b"stat");

            // SAFETY: valid C path, read-only.
            let fd = unsafe { libc::open(path_ptr, libc::O_RDONLY | libc::O_CLOEXEC) };
            if fd < 0 {
                continue; // process vanished — leave tombstone, consume no arena
            }

            let off = u32::try_from(snap.strings.alloc(SLOT_SIZE)).expect("arena offset fits u32");
            let ptr = snap.strings.write_ptr(off as usize);

            // SAFETY: ptr is a writable SLOT_SIZE region; fd is open.
            let (len, uid) = unsafe {
                let n = libc::read(fd, ptr.cast(), SLOT_SIZE);
                let mut st: libc::stat = std::mem::zeroed();
                let uid = if libc::fstat(fd, &raw mut st) == 0 {
                    st.st_uid
                } else {
                    u32::MAX
                };
                libc::close(fd);
                if n <= 0 {
                    continue;
                }
                let len = usize::try_from(n).unwrap_or(0).min(SLOT_SIZE);
                (u32::try_from(len).unwrap_or(0), uid)
            };

            let slice = snap.strings.bytes(off, len);
            if let Some(f) = parse::parse_stat(slice, off) {
                f.write_into(&mut snap.procs[i], uid, page_size);
            }
        }
    }
}
