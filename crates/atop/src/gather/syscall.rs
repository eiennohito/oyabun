//! Synchronous `open`/`read`/`fstat`/`close` backend.
//!
//! The universal fallback (older kernels, restricted seccomp, containers) and the
//! correctness oracle for the `io_uring` backend. Reads directly into the arena.

use crate::arena::StringRef;
use crate::gather::{SLOT_SIZE, parse};
use crate::snapshot::Snapshot;
use crate::sys::ProcPath;

pub struct SyscallBackend;

impl SyscallBackend {
    #[allow(clippy::unused_self)] // symmetric with UringBackend::collect(&mut self)
    pub fn collect(&mut self, pids: &[u32], snap: &mut Snapshot, page_size: u64) {
        let mut stat_path = ProcPath::new();
        let mut cmd_path = ProcPath::new();
        for (i, &pid) in pids.iter().enumerate() {
            // --- /proc/<pid>/stat ---
            let path_ptr = stat_path.write(pid, b"stat");

            // SAFETY: valid C path, read-only.
            let fd = unsafe { libc::open(path_ptr, libc::O_RDONLY | libc::O_CLOEXEC) };
            if fd < 0 {
                continue;
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

            // --- /proc/<pid>/cmdline ---
            snap.procs[i].cmdline = read_cmdline(pid, &mut cmd_path, snap);
        }
    }
}

/// Read and process `/proc/<pid>/cmdline` into the arena. Returns the `StringRef`
/// for the cleaned command line, or `EMPTY` on any failure.
fn read_cmdline(pid: u32, path: &mut ProcPath, snap: &mut Snapshot) -> StringRef {
    let path_ptr = path.write(pid, b"cmdline");
    // SAFETY: valid C path, read-only.
    let fd = unsafe { libc::open(path_ptr, libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return StringRef::EMPTY;
    }
    let off = u32::try_from(snap.strings.alloc(SLOT_SIZE)).expect("arena offset fits u32");
    let ptr = snap.strings.write_ptr(off as usize);
    // SAFETY: ptr is a writable SLOT_SIZE region.
    let n = unsafe { libc::read(fd, ptr.cast(), SLOT_SIZE) };
    unsafe { libc::close(fd) };
    if n <= 0 {
        return StringRef::EMPTY;
    }
    let raw_len = usize::try_from(n).unwrap_or(0).min(SLOT_SIZE);
    let buf = snap
        .strings
        .bytes_mut(off, u32::try_from(raw_len).unwrap_or(0));
    let clean_len = parse::clean_cmdline(buf);
    if clean_len == 0 {
        return StringRef::EMPTY;
    }
    StringRef {
        offset: off,
        len: clean_len,
    }
}
