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

pub fn num_cpus() -> u32 {
    // SAFETY: sysconf with a valid name.
    let v = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    u32::try_from(v).unwrap_or(1).max(1)
}

/// Soft `RLIMIT_NOFILE` — the ceiling on open fds for this process, which bounds the
/// persistent-fd pool (a held fd per tracked PID). Containers commonly cap this at
/// 256/512; tests can lower it with `ulimit -n`. Falls back to 1024 (the historic
/// default) if the query fails.
pub fn nofile_soft_limit() -> u64 {
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: valid resource id and a writable rlimit out-param.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut rlim) } == 0 {
        rlim.rlim_cur
    } else {
        1024
    }
}

// ---------------------------------------------------------------------------
// System-wide stat readers (tiny files, read into stack buffers)
// ---------------------------------------------------------------------------

/// Read a small procfs file into a stack buffer. Returns the valid slice.
fn read_proc_file<const N: usize>(path: *const libc::c_char, buf: &mut [u8; N]) -> &[u8] {
    // SAFETY: valid C path, read-only.
    let fd = unsafe { libc::open(path, libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return &[];
    }
    // SAFETY: buf is a valid writable region.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), N) };
    unsafe { libc::close(fd) };
    if n <= 0 {
        return &[];
    }
    &buf[..usize::try_from(n).unwrap_or(0).min(N)]
}

/// Cumulative CPU jiffies from the aggregate `cpu` line of `/proc/stat`.
#[derive(Clone, Copy, Default)]
pub struct RawCpuCounters {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl RawCpuCounters {
    pub fn total(&self) -> u64 {
        self.user
            + self.nice
            + self.system
            + self.idle
            + self.iowait
            + self.irq
            + self.softirq
            + self.steal
    }
}

/// Parse the first (`cpu`) line of `/proc/stat`.
pub fn read_cpu_counters() -> RawCpuCounters {
    let mut buf = [0u8; 512];
    let data = read_proc_file(c"/proc/stat".as_ptr(), &mut buf);
    // First line: "cpu  <user> <nice> <sys> <idle> <iowait> <irq> <softirq> <steal> ..."
    let line = data.split(|&b| b == b'\n').next().unwrap_or(&[]);
    let mut it = line.split(|&b| b == b' ').filter(|f| !f.is_empty());
    it.next(); // skip "cpu"
    let mut n = || it.next().and_then(parse_u64_bytes).unwrap_or(0);
    let user = n();
    let nice = n();
    let system = n();
    let idle = n();
    let iowait = n();
    let irq = n();
    let softirq = n();
    let steal = n();
    RawCpuCounters {
        user,
        nice,
        system,
        idle,
        iowait,
        irq,
        softirq,
        steal,
    }
}

/// Memory stats from `/proc/meminfo` (bytes).
#[derive(Clone, Copy, Default)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
    pub buffers: u64,
    pub cached: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

pub fn read_meminfo() -> MemInfo {
    let mut buf = [0u8; 2048];
    let data = read_proc_file(c"/proc/meminfo".as_ptr(), &mut buf);
    let mut info = MemInfo::default();
    for line in data.split(|&b| b == b'\n') {
        let Some((key, val)) = meminfo_line(line) else {
            continue;
        };
        // Values in /proc/meminfo are in kB (1024 bytes).
        let bytes = val.saturating_mul(1024);
        match key {
            b"MemTotal" => info.total = bytes,
            b"MemAvailable" => info.available = bytes,
            b"Buffers" => info.buffers = bytes,
            b"Cached" => info.cached = bytes,
            b"SwapTotal" => info.swap_total = bytes,
            b"SwapFree" => info.swap_free = bytes,
            _ => {}
        }
    }
    info
}

/// Parse `"Key:    1234 kB\n"` → `(key, value_in_kB)`.
fn meminfo_line(line: &[u8]) -> Option<(&[u8], u64)> {
    let colon = line.iter().position(|&b| b == b':')?;
    let key = &line[..colon];
    let rest = &line[colon + 1..];
    let num = rest
        .split(|&b| b == b' ')
        .find(|f| !f.is_empty())
        .and_then(parse_u64_bytes)?;
    Some((key, num))
}

/// Load averages × 100 and uptime in seconds.
pub fn read_loadavg() -> [u32; 3] {
    let mut buf = [0u8; 128];
    let data = read_proc_file(c"/proc/loadavg".as_ptr(), &mut buf);
    let mut fields = data.split(|&b| b == b' ');
    let parse_load = |f: Option<&[u8]>| -> u32 {
        let s = f.unwrap_or(&[]);
        // "1.23" → 123
        let dot = s.iter().position(|&b| b == b'.').unwrap_or(s.len());
        let whole = parse_u64_bytes(&s[..dot]).unwrap_or(0);
        let frac_bytes = s.get(dot + 1..).unwrap_or(&[]);
        let digit = |b: u8| {
            if b.is_ascii_digit() {
                u64::from(b - b'0')
            } else {
                0
            }
        };
        let d0 = frac_bytes.first().copied().unwrap_or(b'0');
        let d1 = frac_bytes.get(1).copied().unwrap_or(b'0');
        let frac = digit(d0) * 10 + digit(d1);
        u32::try_from(whole * 100 + frac).unwrap_or(u32::MAX)
    };
    [
        parse_load(fields.next()),
        parse_load(fields.next()),
        parse_load(fields.next()),
    ]
}

pub fn read_uptime_secs() -> u64 {
    let mut buf = [0u8; 64];
    let data = read_proc_file(c"/proc/uptime".as_ptr(), &mut buf);
    // "12345.67 ..."
    let field = data.split(|&b| b == b' ').next().unwrap_or(&[]);
    let dot = field.iter().position(|&b| b == b'.').unwrap_or(field.len());
    parse_u64_bytes(&field[..dot]).unwrap_or(0)
}

fn parse_u64_bytes(b: &[u8]) -> Option<u64> {
    if b.is_empty() {
        return None;
    }
    let mut n: u64 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u64::from(c - b'0'))?;
    }
    Some(n)
}

/// Kernel `PID_MAX_LIMIT` (2^22 on 64-bit). A real PID never exceeds it; used to
/// reject a pathologically long numeric dirent name that would otherwise wrap.
const PID_MAX_LIMIT: u32 = 1 << 22;

/// `/proc/sys/kernel/ns_last_pid` — the last PID the kernel allocated in this namespace
/// (the allocation frontier). New PIDs appear just above it (allocation is near-monotonic,
/// wrapping at [`read_pid_max`]). The gatherer's skip-cycle birth probe reads this to bound
/// the probe window — in steady state it equals our highest live PID, so the window is
/// empty and the probe costs zero opens. `None` if the file is unreadable (older kernel /
/// restricted), in which case the probe falls back to a blind fixed-width window.
pub fn read_ns_last_pid() -> Option<u32> {
    let mut buf = [0u8; 32];
    let data = read_proc_file(c"/proc/sys/kernel/ns_last_pid".as_ptr(), &mut buf);
    let line = data.split(|&b| b == b'\n').next().unwrap_or(&[]);
    u32::try_from(parse_u64_bytes(line)?).ok()
}

/// `/proc/sys/kernel/pid_max` — the value at which the PID counter wraps back to the low
/// range. Read once at startup; bounds the birth-probe window so it never generates an
/// impossible PID. Falls back to [`PID_MAX_LIMIT`] if unreadable.
pub fn read_pid_max() -> u32 {
    let mut buf = [0u8; 32];
    let data = read_proc_file(c"/proc/sys/kernel/pid_max".as_ptr(), &mut buf);
    let line = data.split(|&b| b == b'\n').next().unwrap_or(&[]);
    parse_u64_bytes(line)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(PID_MAX_LIMIT)
}

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
        assert!(
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
/// `/proc/<pid>/stat`'s start-time still matches `start_time`. Race-safe: the signal
/// goes through the pidfd (anchored to the kernel `task_struct`, not the PID number),
/// so even if the PID is recycled between `pidfd_open` and the `start_time` check,
/// `pidfd_send_signal` targets the original (now-dead) process and harmlessly fails
/// with ESRCH. The `start_time` check prevents signaling a process that exited and
/// was replaced — the pidfd makes it safe, the `start_time` makes it correct.
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

/// Is this task a thread group leader (a real process, not a non-leader thread)?
/// Uses `pidfd_open(pid, 0)` — one syscall, no file I/O. Returns `EINVAL` for
/// non-leader threads (kernel ≥ 5.3, the same floor as `io_uring`). The birth probe
/// uses this to reject threads whose TID falls in the speculative window: `open`
/// resolves any `/proc/<tid>` via VFS lookup, but only TGIDs appear in `getdents`.
#[allow(clippy::cast_possible_truncation)] // syscall returns c_long; fd fits i32
pub fn is_thread_group_leader(pid: u32) -> bool {
    // SAFETY: pidfd_open is a thin wrapper around a PID lookup; flags=0 restricts
    // to thread group leaders (EINVAL for non-leaders).
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    if fd >= 0 {
        unsafe { libc::close(fd) };
        true
    } else {
        false
    }
}

/// Field 22 of `/proc/<pid>/stat` — process start time (jiffies since boot), the
/// discriminator that distinguishes PID reuse. `None` if the PID is gone.
///
/// Intentionally duplicates the field-22 parse from `gather::parse::parse_stat` —
/// `sys` is a low-level module that must not depend on `gather`, and this path uses
/// a small stack buffer rather than the arena.
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
    fn cpu_counters_are_nonzero() {
        let c = read_cpu_counters();
        assert!(c.total() > 0, "total jiffies must be > 0");
        assert!(c.idle > 0, "idle jiffies should be > 0 on a live system");
    }

    #[test]
    fn meminfo_has_total() {
        let m = read_meminfo();
        assert!(m.total > 0, "MemTotal must be > 0");
        assert!(m.available > 0, "MemAvailable must be > 0");
        assert!(m.available <= m.total);
    }

    #[test]
    fn loadavg_parses() {
        let l = read_loadavg();
        // On any live system, load average is some positive number.
        assert!(l[0] > 0 || l[1] > 0 || l[2] > 0, "all loads zero?");
    }

    #[test]
    fn uptime_positive() {
        assert!(read_uptime_secs() > 0);
    }

    #[test]
    fn pid_max_and_ns_last_pid_are_plausible() {
        let pid_max = read_pid_max();
        assert!(pid_max >= 1 << 15, "pid_max implausibly small: {pid_max}");
        // ns_last_pid may be unreadable in odd environments; if present it must be sane.
        if let Some(last) = read_ns_last_pid() {
            assert!(last >= 1, "ns_last_pid should be positive");
            assert!(
                last <= pid_max,
                "ns_last_pid {last} should not exceed pid_max {pid_max}"
            );
        }
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
