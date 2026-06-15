//! System interface: sysconf constants, zero-alloc `/proc` enumeration and file
//! reads, and the uid→username map.

use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;

pub fn page_size() -> u64 {
    // SAFETY: sysconf with a valid name; always defined.
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(v).unwrap_or(4096).max(1)
}

/// Clock ticks per second (`CLK_TCK`) — the unit of `utime`/`stime` jiffies.
pub fn clk_tck() -> u64 {
    // SAFETY: sysconf with a valid name; always defined.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(v).unwrap_or(100).max(1)
}

/// Kernel `PID_MAX_LIMIT` (2^22 on 64-bit). A real PID never exceeds it; used to
/// reject a pathologically long numeric dirent name that would otherwise wrap.
const PID_MAX_LIMIT: u32 = 1 << 22;

/// Stack buffer for building `/proc/<pid>/<suffix>` C-string paths with no heap
/// allocation. 64 bytes fits `/proc/` (6) + a 10-digit PID + `/` + suffix + NUL.
#[derive(Clone)]
pub struct ProcPath([u8; 64]);

impl ProcPath {
    pub fn new() -> Self {
        Self([0; 64])
    }

    /// Write `/proc/<pid>/<suffix>\0`; returns a C-string pointer into `self`, valid
    /// until the next `write` or until `self` is dropped.
    pub fn write(&mut self, pid: u32, suffix: &[u8]) -> *const libc::c_char {
        let buf = &mut self.0;
        // "/proc/" (6) + up to 10 PID digits + "/" (1) + NUL (1) + suffix.
        debug_assert!(
            6 + 10 + 1 + 1 + suffix.len() <= buf.len(),
            "proc path overflow"
        );
        let mut i = 0;
        for &b in b"/proc/" {
            buf[i] = b;
            i += 1;
        }
        i += write_u32(&mut buf[i..], pid);
        buf[i] = b'/';
        i += 1;
        for &b in suffix {
            buf[i] = b;
            i += 1;
        }
        buf[i] = 0;
        buf.as_ptr().cast()
    }
}

impl Default for ProcPath {
    fn default() -> Self {
        Self::new()
    }
}

fn write_u32(buf: &mut [u8], v: u32) -> usize {
    let mut tmp = [0u8; 10];
    let mut n = 0;
    let mut x = v;
    loop {
        tmp[n] = b'0' + u8::try_from(x % 10).unwrap_or(0);
        x /= 10;
        n += 1;
        if x == 0 {
            break;
        }
    }
    for j in 0..n {
        buf[j] = tmp[n - 1 - j];
    }
    n
}

/// Send `sig` to `pid` **only if it is still the same process incarnation** — i.e.
/// `/proc/<pid>/stat`'s start-time still matches `start_time`. Race-free: a pidfd
/// pins the current occupant of `pid`, identity is re-checked, then the signal goes
/// through the pidfd (never a reused PID). Returns whether the signal was sent.
pub fn kill_verified(pid: u32, start_time: u64, sig: i32) -> bool {
    let Ok(pid_arg) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: pidfd_open with a valid pid and zero flags.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid_arg, 0) };
    let Ok(pidfd) = libc::c_int::try_from(raw) else {
        return false; // process already gone, or unsupported
    };

    let identity_ok = process_start_time(pid) == Some(start_time);
    let sent = identity_ok && {
        // SAFETY: pidfd is valid; null siginfo and zero flags are accepted.
        let r = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd,
                sig,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        r == 0
    };
    // SAFETY: closing our own fd exactly once.
    unsafe { libc::close(pidfd) };
    sent
}

/// Field 22 of `/proc/<pid>/stat` — process start time (jiffies since boot), the
/// discriminator that distinguishes PID reuse. `None` if the PID is gone.
fn process_start_time(pid: u32) -> Option<u64> {
    let mut path = ProcPath::new();
    let ptr = path.write(pid, b"stat");
    // SAFETY: valid C path, read-only.
    let fd = unsafe { libc::open(ptr, libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; 512];
    // SAFETY: reading into a local buffer of the given length.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    // SAFETY: our fd.
    unsafe { libc::close(fd) };
    let slot = buf.get(..usize::try_from(n).ok()?)?;

    // starttime is the 20th space-separated field after the comm's closing ')'.
    let close = slot.iter().rposition(|&b| b == b')')?;
    let mut fields = slot.get(close + 2..)?.split(|&b| b == b' ');
    for _ in 0..19 {
        fields.next()?;
    }
    let mut v: u64 = 0;
    for &c in fields.next()? {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(u64::from(c - b'0'))?;
    }
    Some(v)
}

/// An open handle to `/proc`, scanned repeatedly via `getdents64` into a caller
/// buffer — no per-entry `String`/`PathBuf` allocation (unlike `fs::read_dir`).
pub struct ProcDir {
    fd: RawFd,
}

impl ProcDir {
    pub fn open() -> io::Result<Self> {
        // SAFETY: constant path, standard flags.
        let fd = unsafe {
            libc::open(
                c"/proc".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    /// Fill `out` with the numeric PIDs under `/proc`, using `buf` as scratch.
    /// Non-numeric entries (`self`, `net`, …) are skipped. `out` is unsorted.
    pub fn read_pids(&self, buf: &mut [u8], out: &mut Vec<u32>) {
        out.clear();
        // SAFETY: valid fd; rewind to re-scan from the start.
        unsafe { libc::lseek(self.fd, 0, libc::SEEK_SET) };

        loop {
            // SAFETY: buf is a valid writable slice of the given length.
            let n = unsafe {
                libc::syscall(libc::SYS_getdents64, self.fd, buf.as_mut_ptr(), buf.len())
            };
            if n <= 0 {
                break; // 0 = end of directory, <0 = error (give up this scan)
            }
            let n = usize::try_from(n).unwrap_or(0);
            let mut off = 0;
            while off + 19 <= n {
                // struct linux_dirent64: d_ino(8) d_off(8) d_reclen(2) d_type(1) d_name[]
                let reclen = u16::from_ne_bytes([buf[off + 16], buf[off + 17]]) as usize;
                if reclen == 0 || off + reclen > n {
                    break;
                }
                if let Some(pid) = parse_pid_name(&buf[off + 19..off + reclen]) {
                    out.push(pid);
                }
                off += reclen;
            }
        }
    }
}

impl Drop for ProcDir {
    fn drop(&mut self) {
        // SAFETY: our fd, closed once.
        unsafe { libc::close(self.fd) };
    }
}

/// Parse a NUL-terminated dirent name as an all-digits PID, else `None`. Rejects
/// values above [`PID_MAX_LIMIT`] (checked arithmetic — no silent wrap).
fn parse_pid_name(name: &[u8]) -> Option<u32> {
    let mut pid: u32 = 0;
    let mut any = false;
    for &c in name {
        if c == 0 {
            break;
        }
        if !c.is_ascii_digit() {
            return None;
        }
        pid = pid.checked_mul(10)?.checked_add(u32::from(c - b'0'))?;
        if pid > PID_MAX_LIMIT {
            return None;
        }
        any = true;
    }
    any.then_some(pid)
}

/// Parse `/etc/passwd` once at startup to build the uid→username map.
pub fn read_uid_names() -> HashMap<u32, Box<str>> {
    let mut map = HashMap::new();
    let Ok(content) = std::fs::read_to_string("/etc/passwd") else {
        return map;
    };
    for line in content.lines() {
        let mut parts = line.split(':');
        let (Some(username), Some(_pw), Some(uid)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if let Ok(uid) = uid.parse::<u32>() {
            map.insert(uid, username.into());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates_pids_including_self_and_init() {
        let dir = ProcDir::open().expect("open /proc");
        let mut buf = vec![0u8; 64 * 1024];
        let mut pids = Vec::new();
        dir.read_pids(&mut buf, &mut pids);
        assert!(pids.len() > 10, "expected many pids, got {}", pids.len());
        assert!(pids.contains(&1), "pid 1 missing");
        assert!(pids.contains(&std::process::id()), "own pid missing");
    }

    #[test]
    fn parse_pid_name_rejects_non_numeric() {
        assert_eq!(parse_pid_name(b"123\0"), Some(123));
        assert_eq!(parse_pid_name(b"1\0junk"), Some(1));
        assert_eq!(parse_pid_name(b"net\0"), None);
        assert_eq!(parse_pid_name(b"self\0"), None);
        assert_eq!(parse_pid_name(b"\0"), None);
    }

    #[test]
    fn parse_pid_name_rejects_overflow() {
        assert_eq!(parse_pid_name(b"99999999999\0"), None); // would wrap u32
        assert_eq!(parse_pid_name(b"4194305\0"), None); // above PID_MAX_LIMIT
        assert_eq!(parse_pid_name(b"4194304\0"), Some(PID_MAX_LIMIT));
    }

    #[test]
    fn proc_path_builds_c_string() {
        let mut p = ProcPath::new();
        let ptr = p.write(1234, b"stat");
        let s = unsafe { std::ffi::CStr::from_ptr(ptr) };
        assert_eq!(s.to_bytes(), b"/proc/1234/stat");
    }

    #[test]
    fn start_time_of_self_is_stable() {
        let me = std::process::id();
        let a = process_start_time(me).expect("own start time");
        let b = process_start_time(me).expect("own start time");
        assert_eq!(a, b, "start time must be constant for a live process");
    }

    #[test]
    fn kill_verified_respects_identity() {
        let mut child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let start = process_start_time(pid).expect("child start time");

        // Mismatched start time (as if the PID had been reused) → refuse to signal.
        assert!(
            !kill_verified(pid, start.wrapping_add(1), libc::SIGTERM),
            "must not signal a different incarnation"
        );
        // Correct identity → signal is delivered.
        assert!(kill_verified(pid, start, libc::SIGTERM));

        let status = child.wait().expect("reap child");
        assert!(
            !status.success(),
            "child should have been terminated by signal"
        );
    }
}
