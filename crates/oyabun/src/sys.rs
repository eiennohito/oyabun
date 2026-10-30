//! System interface: sysconf constants, zero-alloc `/proc` enumeration and file
//! reads, and the uid→username map.

use std::io;
use std::os::fd::RawFd;

use crate::fxhash::FxMap;

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

/// The full effective-capability mask for the running kernel: `(1 << (cap_last_cap+1)) − 1`.
/// Read once at startup from `/proc/sys/kernel/cap_last_cap`; a process whose masked `CapEff`
/// equals this holds every capability (root-equivalent). Falls back to a 41-bit set
/// (`CAP_CHECKPOINT_RESTORE`-era) if the file is unreadable.
pub fn cap_full_mask() -> u64 {
    let mut buf = [0u8; 32];
    let data = read_proc_file(c"/proc/sys/kernel/cap_last_cap".as_ptr(), &mut buf);
    let line = data.split(|&b| b == b'\n').next().unwrap_or(&[]);
    let last = parse_u64_bytes(line).unwrap_or(40);
    if last >= 63 {
        u64::MAX
    } else {
        (1u64 << (last + 1)) - 1
    }
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

/// Read `/proc/<pid>/<suffix>` into a stack buffer; empty if the process is gone.
pub fn read_pid_file<'a, const N: usize>(
    pid: u32,
    suffix: &[u8],
    buf: &'a mut [u8; N],
) -> &'a [u8] {
    let mut path = ProcPath::new();
    read_proc_file(path.write(pid, suffix), buf)
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
    /// Every accounted jiffy; the denominator that turns a counter delta into cores.
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

    /// Jiffies this machine's CPUs spent executing work. Excludes `idle` and `iowait` (the CPU
    /// was free, merely with IO outstanding) and `steal` (the hypervisor ran someone else — our
    /// CPU time was *denied*, not used; it is reported on its own).
    pub fn busy(&self) -> u64 {
        self.user + self.nice + self.kernel()
    }

    /// Jiffies in the kernel on behalf of tasks or interrupts: `system + irq + softirq`.
    pub fn kernel(&self) -> u64 {
        self.system + self.irq + self.softirq
    }
}

/// First line: `"cpu  <user> <nice> <sys> <idle> <iowait> <irq> <softirq> <steal> ..."`.
fn parse_cpu_counters(data: &[u8]) -> RawCpuCounters {
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
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct MemInfo {
    pub total: u64,
    /// `MemFree`: pages nobody holds at all (not even as cache).
    pub free: u64,
    pub available: u64,
    /// tmpfs + SysV/POSIX shared memory: charged to no process, swappable.
    pub shmem: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

fn parse_meminfo(data: &[u8]) -> MemInfo {
    let mut info = MemInfo::default();
    for line in data.split(|&b| b == b'\n') {
        let Some((key, val)) = meminfo_line(line) else {
            continue;
        };
        // Values in /proc/meminfo are in kB (1024 bytes).
        let bytes = val.saturating_mul(1024);
        match key {
            b"MemTotal" => info.total = bytes,
            b"MemFree" => info.free = bytes,
            b"MemAvailable" => info.available = bytes,
            b"Shmem" => info.shmem = bytes,
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

/// Runnable scheduling entities *other than the reader*: `"0.52 0.58 0.59 3/1234 56789\n"` → 2.
///
/// The `running/total` fourth field of `/proc/loadavg` is the kernel's `nr_running`, which
/// counts the very task reading it — always at least 1 even on an idle machine. Subtracting it
/// here, once, at the read boundary keeps every consumer (and recordings) in "work waiting or
/// running besides us" terms. Same value as `/proc/stat`'s `procs_running`, but that line sits
/// after the per-core and interrupt lines (tens of KiB on many cores); this file is ~30 bytes.
fn parse_runnable(data: &[u8]) -> u32 {
    data.split(|&b| b == b' ')
        .nth(3)
        .and_then(|f| f.split(|&b| b == b'/').next())
        .and_then(parse_u64_bytes)
        .and_then(|n| u32::try_from(n).ok())
        .map_or(0, |n| n.saturating_sub(1))
}

/// Cumulative `some` stall time in microseconds for each pressure-stall resource. `None` when
/// the kernel does not expose that file (`CONFIG_PSI` off, or booted with `psi=0`) — absence is
/// a distinct state, never a fake zero.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct RawPsi {
    pub cpu: Option<u64>,
    pub memory: Option<u64>,
    pub io: Option<u64>,
}

/// `total=` (µs) of the `some` line: `"some avg10=0.05 avg60=0.03 avg300=0.08 total=634013983"`.
fn parse_psi_some_total(data: &[u8]) -> Option<u64> {
    let line = data
        .split(|&b| b == b'\n')
        .find(|l| l.starts_with(b"some "))?;
    line.split(|&b| b == b' ')
        .find_map(|f| f.strip_prefix(b"total="))
        .and_then(parse_u64_bytes)
}

/// `pswpin` (cumulative pages swapped in since boot) from `/proc/vmstat`; `None` if the counter
/// is not there.
fn parse_pswpin(data: &[u8]) -> Option<u64> {
    data.split(|&b| b == b'\n')
        .find_map(|l| l.strip_prefix(b"pswpin "))
        .and_then(parse_u64_bytes)
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

/// All raw system reads bundled for a single cycle — the source data that
/// [`SystemSampler`](crate::gather::sysstat::SystemSampler) consumes to produce
/// the windowed CPU, pressure, and memory figures the header shows.
#[derive(Clone, Copy)]
pub struct RawSystemSnapshot {
    pub cpu: RawCpuCounters,
    /// Runnable entities besides the reader at this instant (see [`parse_runnable`]).
    pub runnable: u32,
    pub mem: MemInfo,
    pub psi: RawPsi,
    /// Cumulative pages swapped in; `None` when not read (no swap, or unreadable), so a failed
    /// read can never masquerade as a counter that reset to zero.
    pub pswpin: Option<u64>,
}

/// One long-lived procfs file. procfs `seq_file`s regenerate their content for a read at offset
/// zero, so one descriptor serves every cycle via `pread` — the same property the per-PID stat
/// pool relies on. Replaces an open+read+close triple per file per cycle.
///
/// The file is probed once, on first use. A file the kernel does not expose (PSI off) stays
/// absent for the process lifetime rather than costing a failed `open` every cycle.
struct HeldFile {
    path: &'static std::ffi::CStr,
    state: HeldState,
}

enum HeldState {
    Unprobed,
    Open(RawFd),
    Absent,
}

impl HeldFile {
    const fn new(path: &'static std::ffi::CStr) -> Self {
        Self {
            path,
            state: HeldState::Unprobed,
        }
    }

    fn fd(&mut self) -> Option<RawFd> {
        if matches!(self.state, HeldState::Unprobed) {
            // SAFETY: valid C path, read-only.
            let fd = unsafe { libc::open(self.path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
            self.state = if fd < 0 {
                HeldState::Absent
            } else {
                HeldState::Open(fd)
            };
        }
        match self.state {
            HeldState::Open(fd) => Some(fd),
            _ => None,
        }
    }

    /// Whether the file exists (probing it if needed).
    fn present(&mut self) -> bool {
        self.fd().is_some()
    }

    /// One `pread` at offset 0: enough for files that fit the buffer in a single chunk.
    /// Empty when absent or unreadable.
    fn read<'b, const N: usize>(&mut self, buf: &'b mut [u8; N]) -> &'b [u8] {
        let Some(fd) = self.fd() else { return &[] };
        // SAFETY: `buf` is a valid writable region of N bytes; `fd` is ours.
        let n = unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), N, 0) };
        &buf[..usize::try_from(n).unwrap_or(0).min(N)]
    }

    /// `pread`s to EOF (or a full buffer), for multi-record files where one read returns only
    /// a page-sized chunk.
    fn read_all<'b, const N: usize>(&mut self, buf: &'b mut [u8; N]) -> &'b [u8] {
        let Some(fd) = self.fd() else { return &[] };
        let mut len = 0;
        while len < N {
            let off = libc::off_t::try_from(len).unwrap_or(0);
            // SAFETY: `buf[len..]` is a valid writable region of `N - len` bytes.
            let n = unsafe { libc::pread(fd, buf[len..].as_mut_ptr().cast(), N - len, off) };
            let Ok(n) = usize::try_from(n) else { break };
            if n == 0 {
                break;
            }
            len += n;
        }
        &buf[..len]
    }
}

impl Drop for HeldFile {
    fn drop(&mut self) {
        if let HeldState::Open(fd) = self.state {
            // SAFETY: our fd, closed once.
            unsafe { libc::close(fd) };
        }
    }
}

/// The system-wide procfs files, held open across cycles. One read per cycle gives the whole
/// [`RawSystemSnapshot`].
pub struct SystemFiles {
    stat: HeldFile,
    loadavg: HeldFile,
    meminfo: HeldFile,
    psi_cpu: HeldFile,
    psi_memory: HeldFile,
    psi_io: HeldFile,
    vmstat: HeldFile,
}

impl SystemFiles {
    pub fn new() -> Self {
        Self {
            stat: HeldFile::new(c"/proc/stat"),
            loadavg: HeldFile::new(c"/proc/loadavg"),
            meminfo: HeldFile::new(c"/proc/meminfo"),
            psi_cpu: HeldFile::new(c"/proc/pressure/cpu"),
            psi_memory: HeldFile::new(c"/proc/pressure/memory"),
            psi_io: HeldFile::new(c"/proc/pressure/io"),
            vmstat: HeldFile::new(c"/proc/vmstat"),
        }
    }

    /// Read `/proc/stat`, `/proc/loadavg`, `/proc/meminfo`, `/proc/pressure/*`, and (only on a
    /// machine with swap — `pswpin` can only move there) `/proc/vmstat`.
    pub fn read(&mut self) -> RawSystemSnapshot {
        // Only the aggregate first line is used; it is well inside the first 512 bytes.
        let mut stat = [0u8; 512];
        // ~30 bytes.
        let mut load = [0u8; 128];
        // ~1.6 KiB on current kernels and growing with new counters; leave generous headroom.
        let mut mem_buf = [0u8; 4096];
        let mut psi = [0u8; 256];
        let mem = parse_meminfo(self.meminfo.read(&mut mem_buf));
        let pswpin = if mem.swap_total > 0 && self.vmstat.present() {
            // ~4.3 KiB today; read to EOF.
            let mut vm = [0u8; 16384];
            parse_pswpin(self.vmstat.read_all(&mut vm))
        } else {
            None
        };
        RawSystemSnapshot {
            cpu: parse_cpu_counters(self.stat.read(&mut stat)),
            runnable: parse_runnable(self.loadavg.read(&mut load)),
            mem,
            psi: RawPsi {
                cpu: parse_psi_some_total(self.psi_cpu.read(&mut psi)),
                memory: parse_psi_some_total(self.psi_memory.read(&mut psi)),
                io: parse_psi_some_total(self.psi_io.read(&mut psi)),
            },
            pswpin,
        }
    }
}

// --- stream ↔ internal conversions (colocated so the two directions are reviewable together) ---

#[cfg(any(feature = "record", test))]
impl From<oya_stream::SystemStats> for RawSystemSnapshot {
    fn from(s: oya_stream::SystemStats) -> Self {
        Self {
            cpu: RawCpuCounters {
                user: s.stat_user,
                nice: s.stat_nice,
                system: s.stat_system,
                idle: s.stat_idle,
                iowait: s.stat_iowait,
                irq: s.stat_irq,
                softirq: s.stat_softirq,
                steal: s.stat_steal,
            },
            runnable: s.runnable,
            mem: MemInfo {
                total: s.mem_total,
                free: s.mem_free,
                available: s.mem_available,
                shmem: s.mem_shmem,
                swap_total: s.swap_total,
                swap_free: s.swap_free,
            },
            psi: RawPsi {
                cpu: s.psi_cpu,
                memory: s.psi_memory,
                io: s.psi_io,
            },
            pswpin: s.pswpin,
        }
    }
}

#[cfg(feature = "record")]
impl RawSystemSnapshot {
    /// Convert to the stream format, stamped with the given core count (which is a
    /// startup-time value, not part of the per-cycle raw snapshot).
    pub fn to_stream(self, num_cores: u32) -> oya_stream::SystemStats {
        oya_stream::SystemStats {
            num_cores,
            stat_user: self.cpu.user,
            stat_nice: self.cpu.nice,
            stat_system: self.cpu.system,
            stat_idle: self.cpu.idle,
            stat_iowait: self.cpu.iowait,
            stat_irq: self.cpu.irq,
            stat_softirq: self.cpu.softirq,
            stat_steal: self.cpu.steal,
            runnable: self.runnable,
            mem_total: self.mem.total,
            mem_free: self.mem.free,
            mem_available: self.mem.available,
            mem_shmem: self.mem.shmem,
            swap_total: self.mem.swap_total,
            swap_free: self.mem.swap_free,
            psi_cpu: self.psi.cpu,
            psi_memory: self.psi.memory,
            psi_io: self.psi.io,
            pswpin: self.pswpin,
        }
    }
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

    /// The `/proc/<pid>/task` directory: its numeric entries are the process's thread ids, so
    /// [`read_pids`](Self::read_pids) lists them. Fails if the process is gone.
    pub fn open_tasks(pid: u32) -> io::Result<Self> {
        let mut path = ProcPath::new();
        let ptr = path.write(pid, b"task");
        // SAFETY: valid NUL-terminated path in `path`, standard flags.
        let fd = unsafe { libc::open(ptr, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
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
pub fn read_uid_names() -> FxMap<u32, Box<str>> {
    let mut map = FxMap::default();
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
    fn cpu_counters_are_nonzero() {
        let c = SystemFiles::new().read().cpu;
        assert!(c.total() > 0, "total jiffies must be > 0");
        assert!(c.idle > 0, "idle jiffies should be > 0 on a live system");
    }

    #[test]
    fn meminfo_has_total() {
        let m = SystemFiles::new().read().mem;
        assert!(m.total > 0, "MemTotal must be > 0");
        assert!(m.available > 0, "MemAvailable must be > 0");
        assert!(m.available <= m.total);
    }

    #[test]
    fn live_snapshot_reads_succeed() {
        let mut files = SystemFiles::new();
        let s = files.read();
        // PSI may be absent (CONFIG_PSI off / psi=0); if present, totals are cumulative µs.
        if let Some(t) = s.psi.cpu {
            // The held descriptors re-read from offset 0 each cycle.
            assert!(files.read().psi.cpu.unwrap_or(0) >= t);
        }
        assert!(files.read().cpu.total() >= s.cpu.total());
    }

    #[test]
    fn cpu_counters_split_busy_from_idle_iowait_and_steal() {
        let c = parse_cpu_counters(b"cpu  10 20 30 400 50 6 7 8 0 0\ncpu0 1 2 3 4 5 6 7 8 0 0\n");
        assert_eq!(c.busy(), 10 + 20 + 30 + 6 + 7);
        assert_eq!(c.kernel(), 30 + 6 + 7);
        assert_eq!(c.steal, 8);
        assert_eq!(c.total(), 10 + 20 + 30 + 400 + 50 + 6 + 7 + 8);
    }

    #[test]
    fn meminfo_parses_shmem_and_swap() {
        let m = parse_meminfo(
            b"MemTotal:       65536 kB\nMemFree:  1 kB\nMemAvailable:   32768 kB\n\
              Shmem:            2048 kB\nSwapTotal:       8192 kB\nSwapFree:        6144 kB\n",
        );
        assert_eq!(
            m,
            MemInfo {
                total: 65536 * 1024,
                free: 1024,
                available: 32768 * 1024,
                shmem: 2048 * 1024,
                swap_total: 8192 * 1024,
                swap_free: 6144 * 1024,
            }
        );
    }

    #[test]
    fn runnable_is_loadavg_numerator() {
        assert_eq!(
            parse_runnable(b"0.52 0.58 0.59 3/1234 56789\n"),
            2,
            "the reader itself is not counted"
        );
        assert_eq!(parse_runnable(b"0.00 0.00 0.00 1/1234 56789\n"), 0);
        assert_eq!(parse_runnable(b""), 0);
    }

    #[test]
    fn psi_takes_some_total_and_distinguishes_absence() {
        let cpu = b"some avg10=0.05 avg60=0.03 avg300=0.08 total=634013983\n\
                    full avg10=0.00 avg60=0.00 avg300=0.00 total=77\n";
        assert_eq!(parse_psi_some_total(cpu), Some(634_013_983));
        assert_eq!(parse_psi_some_total(b""), None);
        assert_eq!(parse_psi_some_total(b"full avg10=0 total=5\n"), None);
    }

    #[test]
    fn pswpin_is_found_among_other_counters() {
        let v = b"pgpgin 1\npswpin 32\npswpout 7\npswpin_x 9\n";
        assert_eq!(parse_pswpin(v), Some(32));
        assert_eq!(parse_pswpin(b"pgpgin 1\n"), None);
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
}
