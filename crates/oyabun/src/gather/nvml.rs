//! Optional NVIDIA telemetry through a runtime-loaded NVML.
//!
//! This module deliberately has no CUDA/NVML build dependency. The small stable C ABI used
//! here is declared locally and resolved from `libnvidia-ml.so.1`; any probe failure simply
//! leaves the rest of oyabun in its original CPU-only mode.

use std::ffi::{c_char, c_uint, c_void};
use std::mem;
use std::ptr;

use crate::fxhash::PidMap;
use crate::procs::{GpuMetric, GpuMetrics, GpuProcessStats, SystemStats, VramStats};

/// Coarse interval, in cycles, for the GPU process-list + VRAM refresh. The client set and each
/// client's framebuffer footprint change slowly, and enumerating them is the second-largest NVML
/// cost, so re-reading every cycle is waste; ~8 cycles ≈ 4 s at the gather interval. A GPU
/// process born between refreshes is invisible until the next one — a brief, acceptable delay.
const CLIENT_REFRESH_CYCLES: u64 = 8;

/// Interval, in cycles, of the expensive per-process utilization poll. That call is a ~1 ms
/// GSP busy-poll (it round-trips to the GPU's processor for fresh perfmon samples), so running
/// it every cycle dominated the profile. Two measured driver constants make a coarser poll
/// lossless: the driver produces a new per-process sample only every ~200 ms, and it retains
/// several seconds of them, drained by `last_seen_timestamp`. So a ~2 s poll (4 cycles at the
/// 500 ms gather interval) still reads the full backlog while cutting the busy-poll rate 4×.
/// Utilization is *held* between polls (like VRAM); the goal is ranking GPU users, for which
/// ~2 s staleness is imperceptible. A client-list rebuild resets held utilization, so a poll is
/// forced on any cycle that rebuilds (see `sample`) regardless of this interval — the two are
/// independent by construction, not by a numeric relationship between the two periods.
const PROCESS_UTIL_REFRESH_CYCLES: u64 = 4;

type Return = c_uint;
type Device = *mut c_void;

const SUCCESS: Return = 0;
const ERROR_NOT_FOUND: Return = 6;
const ERROR_INSUFFICIENT_SIZE: Return = 7;
const VALUE_NOT_AVAILABLE: u64 = u64::MAX;

type InitFn = unsafe extern "C" fn() -> Return;
type ShutdownFn = unsafe extern "C" fn() -> Return;
type CountFn = unsafe extern "C" fn(*mut c_uint) -> Return;
type HandleFn = unsafe extern "C" fn(c_uint, *mut Device) -> Return;
type UtilFn = unsafe extern "C" fn(Device, *mut Utilization) -> Return;
type MemoryFn = unsafe extern "C" fn(Device, *mut Memory) -> Return;
type ProcessUtilFn =
    unsafe extern "C" fn(Device, *mut ProcessUtilSample, *mut c_uint, u64) -> Return;
type ProcessesUtilInfoFn = unsafe extern "C" fn(Device, *mut ProcessesUtilInfo) -> Return;
type ProcessListV2Fn = unsafe extern "C" fn(Device, *mut c_uint, *mut ProcessInfoV2) -> Return;
type ProcessListV1Fn = unsafe extern "C" fn(Device, *mut c_uint, *mut ProcessInfoV1) -> Return;
type CloseFn = unsafe extern "C" fn(*mut c_void) -> libc::c_int;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Utilization {
    gpu: c_uint,
    memory: c_uint,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Memory {
    total: u64,
    free: u64,
    used: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessUtilSample {
    pid: c_uint,
    timestamp: u64,
    sm_util: c_uint,
    mem_util: c_uint,
    enc_util: c_uint,
    dec_util: c_uint,
}

/// `nvmlProcessUtilizationInfo_v1_t` — one element of the Device-Queries plural utilization
/// array. A superset of the GRID `ProcessUtilSample` (adds jpg/ofa engines); `repr(C)` trailing
/// padding brings it to the 40 bytes NVML strides by.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessUtilInfo {
    timestamp: u64,
    pid: c_uint,
    sm_util: c_uint,
    mem_util: c_uint,
    enc_util: c_uint,
    dec_util: c_uint,
    jpg_util: c_uint,
    ofa_util: c_uint,
}

/// `nvmlProcessesUtilizationInfo_v1_t` — the versioned container passed to
/// `nvmlDeviceGetProcessesUtilizationInfo`: the caller sets `version`, the array capacity, and
/// the backlog cursor; NVML fills the array and writes back the actual count.
#[repr(C)]
struct ProcessesUtilInfo {
    version: c_uint,
    process_samples_count: c_uint,
    last_seen_timestamp: u64,
    proc_util_array: *mut ProcessUtilInfo,
}

/// `NVML_STRUCT_VERSION(ProcessesUtilizationInfo, 1)` = struct size, with the version in the top
/// byte. Derived from `size_of` so it stays correct across pointer widths.
#[allow(clippy::cast_possible_truncation)] // struct size is a small compile-time constant (24)
const PROCESSES_UTIL_INFO_V1: c_uint =
    (mem::size_of::<ProcessesUtilInfo>() as c_uint) | (1u32 << 24);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessInfoV1 {
    pid: c_uint,
    used_gpu_memory: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessInfoV2 {
    pid: c_uint,
    used_gpu_memory: u64,
    gpu_instance_id: c_uint,
    compute_instance_id: c_uint,
}

#[derive(Clone, Copy)]
enum ProcessListFn {
    V2(ProcessListV2Fn),
    V1(ProcessListV1Fn),
}

/// The per-process utilization entry point, resolved preferring the general Device-Queries API
/// over the GRID one. Both cost the same ~1 ms GSP poll, but the Device-Queries call is
/// multi-process-correct and reports extra engines, whereas the GRID call is documented as
/// single-process-oriented; the GRID call is kept only as a fallback for drivers too old to
/// export the newer symbol.
#[derive(Clone, Copy)]
enum ProcessUtil {
    Info(ProcessesUtilInfoFn),
    Samples(ProcessUtilFn),
}

struct NvmlApi {
    library: *mut c_void,
    close: CloseFn,
    shutdown: ShutdownFn,
    device_get_utilization: Option<UtilFn>,
    device_get_memory: Option<MemoryFn>,
    process_get_utilization: Option<ProcessUtil>,
    compute_processes: Option<ProcessListFn>,
    graphics_processes: Option<ProcessListFn>,
}

impl NvmlApi {
    fn load() -> Option<(Self, Vec<Device>)> {
        // SAFETY: the name is NUL-terminated and the handle is checked before use.
        let library = unsafe {
            libc::dlopen(
                c"libnvidia-ml.so.1".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            )
        };
        if library.is_null() {
            return None;
        }

        // Resolve the lifecycle/discovery core first. A missing core symbol makes this library
        // unusable; optional telemetry symbols only make their individual metrics unavailable.
        let loaded = (|| {
            // SAFETY: each symbol is used with the ABI declared by NVIDIA's public nvml.h.
            let init = unsafe { symbol_any::<InitFn>(library, &[c"nvmlInit_v2", c"nvmlInit"]) }?;
            // SAFETY: same ABI argument as above.
            let shutdown = unsafe { symbol::<ShutdownFn>(library, c"nvmlShutdown") }?;
            // SAFETY: versioned and legacy functions have the same signature here.
            let count = unsafe {
                symbol_any::<CountFn>(library, &[c"nvmlDeviceGetCount_v2", c"nvmlDeviceGetCount"])
            }?;
            // SAFETY: versioned and legacy functions have the same signature here.
            let handle = unsafe {
                symbol_any::<HandleFn>(
                    library,
                    &[
                        c"nvmlDeviceGetHandleByIndex_v2",
                        c"nvmlDeviceGetHandleByIndex",
                    ],
                )
            }?;

            let devices = discover_devices(init, shutdown, count, handle)?;

            let api = Self {
                library,
                close: libc::dlclose,
                shutdown,
                // SAFETY: optional symbol signatures match nvml.h.
                device_get_utilization: unsafe {
                    symbol(library, c"nvmlDeviceGetUtilizationRates")
                },
                // The original memory ABI is stable and avoids a version-tagged structure.
                // SAFETY: optional symbol signature matches nvml.h.
                device_get_memory: unsafe { symbol(library, c"nvmlDeviceGetMemoryInfo") },
                // Prefer the general Device-Queries API; fall back to the GRID one on old drivers.
                // SAFETY: optional symbol signatures match nvml.h.
                process_get_utilization: unsafe { load_process_util(library) },
                // Prefer the current v3 entry point. Its public parameter remains
                // `nvmlProcessInfo_t` (the v2 layout), then fall back through older ABIs.
                compute_processes: unsafe {
                    load_process_list(
                        library,
                        c"nvmlDeviceGetComputeRunningProcesses_v3",
                        c"nvmlDeviceGetComputeRunningProcesses_v2",
                        c"nvmlDeviceGetComputeRunningProcesses",
                    )
                },
                graphics_processes: unsafe {
                    load_process_list(
                        library,
                        c"nvmlDeviceGetGraphicsRunningProcesses_v3",
                        c"nvmlDeviceGetGraphicsRunningProcesses_v2",
                        c"nvmlDeviceGetGraphicsRunningProcesses",
                    )
                },
            };
            Some((api, devices))
        })();

        if loaded.is_none() {
            // Initialization failures and zero-device paths already shut down when necessary.
            // SAFETY: this handle came from `dlopen` and no function pointer escapes.
            unsafe { libc::dlclose(library) };
        }
        loaded
    }
}

fn discover_devices(
    init: InitFn,
    shutdown: ShutdownFn,
    count: CountFn,
    handle: HandleFn,
) -> Option<Vec<Device>> {
    // SAFETY: no arguments; NVML owns its internal state.
    if unsafe { init() } != SUCCESS {
        return None;
    }
    let mut n = 0;
    // SAFETY: `n` is a valid output pointer.
    if unsafe { count(&raw mut n) } != SUCCESS || n == 0 {
        // SAFETY: initialization succeeded, so shutdown is paired before returning.
        let _ = unsafe { shutdown() };
        return None;
    }
    let mut devices = Vec::with_capacity(n as usize);
    for index in 0..n {
        let mut device = ptr::null_mut();
        // SAFETY: index is below the count just returned and output is valid.
        if unsafe { handle(index, &raw mut device) } != SUCCESS || device.is_null() {
            // SAFETY: initialization succeeded.
            let _ = unsafe { shutdown() };
            return None;
        }
        devices.push(device);
    }
    Some(devices)
}

impl Drop for NvmlApi {
    fn drop(&mut self) {
        // Function pointers must remain valid through shutdown, so unload is last.
        // SAFETY: successful probe paired this API with a successful initialization.
        let _ = unsafe { (self.shutdown)() };
        // SAFETY: handle came from `dlopen` and all NVML calls have ended.
        unsafe { (self.close)(self.library) };
    }
}

/// Resolve a typed function pointer. `dlsym` returns a data pointer on POSIX; copying its bits
/// into a same-sized function pointer is the conventional dynamic-loader bridge.
unsafe fn symbol<T: Copy>(library: *mut c_void, name: &std::ffi::CStr) -> Option<T> {
    // SAFETY: caller supplies a live loader handle and a NUL-terminated symbol.
    let raw = unsafe { libc::dlsym(library, name.as_ptr().cast::<c_char>()) };
    if raw.is_null() {
        None
    } else {
        debug_assert_eq!(mem::size_of::<T>(), mem::size_of::<*mut c_void>());
        // SAFETY: `T` is the exact C function-pointer type for this symbol.
        Some(unsafe { mem::transmute_copy(&raw) })
    }
}

unsafe fn symbol_any<T: Copy>(library: *mut c_void, names: &[&std::ffi::CStr]) -> Option<T> {
    names.iter().find_map(|name| {
        // SAFETY: inherited from this function's contract.
        unsafe { symbol(library, name) }
    })
}

/// Resolve the per-process utilization entry point, preferring the Device-Queries plural API and
/// falling back to the GRID one (see [`ProcessUtil`]).
unsafe fn load_process_util(library: *mut c_void) -> Option<ProcessUtil> {
    // SAFETY: names correspond to these exact public ABIs.
    unsafe { symbol::<ProcessesUtilInfoFn>(library, c"nvmlDeviceGetProcessesUtilizationInfo") }
        .map(ProcessUtil::Info)
        .or_else(|| {
            unsafe { symbol::<ProcessUtilFn>(library, c"nvmlDeviceGetProcessUtilization") }
                .map(ProcessUtil::Samples)
        })
}

unsafe fn load_process_list(
    library: *mut c_void,
    v3: &std::ffi::CStr,
    v2: &std::ffi::CStr,
    v1: &std::ffi::CStr,
) -> Option<ProcessListFn> {
    // SAFETY: names correspond to these exact public ABI versions.
    unsafe { symbol::<ProcessListV2Fn>(library, v3) }
        .or_else(|| unsafe { symbol::<ProcessListV2Fn>(library, v2) })
        .map(ProcessListFn::V2)
        .or_else(|| unsafe { symbol::<ProcessListV1Fn>(library, v1) }.map(ProcessListFn::V1))
}

#[derive(Clone, Copy)]
struct DeviceState {
    handle: Device,
    last_seen_timestamp: u64,
}

#[derive(Clone, Copy, Default)]
struct PidMemory {
    pid: u32,
    bytes: u64,
    available: bool,
}

/// Per-client GPU state held across cycles. Both fields are *held* between the coarse refreshes
/// that produce them, because both read data that moves slower than the gather interval. `vram`
/// is refreshed on the client-list interval — a client whose GPU is idle still owns its
/// framebuffer. `util_bp` is refreshed on the (finer) per-process utilization interval and held
/// between those polls; it is forced to a true zero whenever the device reads idle, since then
/// there is genuinely nothing running to attribute.
#[derive(Clone, Copy, Default)]
struct ClientGpu {
    vram: GpuMetric<u64>,
    util_bp: GpuMetric<u32>,
}

/// Runtime NVML sampler. It is constructed only when initialization and fixed startup device
/// discovery both succeed.
pub(crate) struct NvmlSampler {
    api: NvmlApi,
    devices: Vec<DeviceState>,
    /// Normalized per-device utilization samples, reused across polls (the aggregation input,
    /// filled by whichever underlying API `ProcessUtil` resolved to).
    util_samples: Vec<ProcessUtilSample>,
    /// Raw scratch for the Device-Queries plural array, reused across polls.
    util_info: Vec<ProcessUtilInfo>,
    process_v2: Vec<ProcessInfoV2>,
    process_v1: Vec<ProcessInfoV1>,
    pid_memory: Vec<PidMemory>,
    empty_devices: Vec<u32>,
    empty_devices_available: bool,
    /// Known GPU clients (from the last coarse process-list refresh) → held VRAM + this cycle's
    /// utilization. Materialized into the sparse per-process view each cycle.
    clients: PidMap<ClientGpu>,
    /// Monotonic sample counter. Both intervals are pure functions of it (`cycle % PERIOD == 0`),
    /// so there is nothing to keep in phase; starts at 0 so the first sample both refreshes and
    /// polls.
    cycle: u64,
    /// A coarse refresh whose process-list query failed; retried next cycle regardless of the
    /// interval, so a transient NVML error costs at most one cycle rather than a full period.
    refresh_pending: bool,
}

impl NvmlSampler {
    pub(crate) fn probe() -> Option<Self> {
        let (api, handles) = NvmlApi::load()?;
        Some(Self {
            api,
            devices: handles
                .into_iter()
                .map(|handle| DeviceState {
                    handle,
                    last_seen_timestamp: 0,
                })
                .collect(),
            util_samples: Vec::new(),
            util_info: Vec::new(),
            process_v2: Vec::new(),
            process_v1: Vec::new(),
            pid_memory: Vec::new(),
            empty_devices: Vec::new(),
            empty_devices_available: false,
            clients: PidMap::default(),
            cycle: 0,
            refresh_pending: false,
        })
    }

    pub(crate) fn device_count(&self) -> u32 {
        u32::try_from(self.devices.len()).unwrap_or(u32::MAX)
    }

    pub(crate) fn empty_devices(&self) -> Option<&[u32]> {
        self.empty_devices_available
            .then_some(self.empty_devices.as_slice())
    }

    pub(crate) fn process_available(&self) -> bool {
        self.api.process_get_utilization.is_some()
            && self.api.compute_processes.is_some()
            && self.api.graphics_processes.is_some()
    }

    pub(crate) fn sample(
        &mut self,
        sys: &mut SystemStats,
        procs: &[crate::procs::ProcessEntry],
        gpu: &mut GpuProcessStats,
    ) {
        sys.gpu_count = self.device_count();

        if !self.process_available() {
            // No per-process capability, hence no client set to check: sample device telemetry
            // every cycle (as before) and leave the per-process view unavailable.
            self.sample_device_util(sys);
            self.sample_device_vram(sys);
            gpu.clear_unavailable();
            return;
        }

        let cycle = self.cycle;
        self.cycle = self.cycle.wrapping_add(1);

        // Coarse interval: rediscover the client set + per-client VRAM + device VRAM totals, all
        // held between refreshes because they move slowly. A failed query arms `refresh_pending`
        // so the next cycle retries instead of waiting a whole period. `rebuilt` records that the
        // client set (and its held utilization) was just reset.
        let rebuilt = if cycle.is_multiple_of(CLIENT_REFRESH_CYCLES) || self.refresh_pending {
            if self.refresh_clients(sys) {
                self.refresh_pending = false;
                true
            } else {
                self.refresh_pending = true;
                gpu.clear_unavailable();
                return;
            }
        } else {
            false
        };

        // Device utilization every cycle (a cheap counter read that also drives the system bar).
        // `active` controls the expensive per-process busy-poll: it is the *GPU-side*
        // activity signal, not CPU, because a rendering-idle desktop client is still CPU-busy
        // (CPU would never let us skip) while a long compute kernel runs GPU-busy at ~0% host CPU
        // (CPU would wrongly zero it). An unreadable device counts as active (never skip based on the
        // unknown).
        let active = self.sample_device_util(sys);

        // Per-process utilization is held between polls (the poll is the ~1 ms GSP busy-poll). A
        // rebuild just reset every client's held value to a default, so a poll is forced whenever
        // `rebuilt` — that coupling, not a numeric relationship between the two periods, is what
        // guarantees a rebuilt client is refilled the same cycle.
        if !active {
            // Idle device → a genuine zero, refreshed every cycle (device util is already read,
            // so this costs nothing and never holds a stale nonzero after work stops).
            for client in self.clients.values_mut() {
                client.util_bp = GpuMetric::Value(0);
            }
        } else if cycle.is_multiple_of(PROCESS_UTIL_REFRESH_CYCLES) || rebuilt {
            // Poll cycle → reset first so a client that went quiet since the last poll decays to
            // zero, then drain the backlog since each device's `last_seen_timestamp`.
            for client in self.clients.values_mut() {
                client.util_bp = GpuMetric::Value(0);
            }
            if !self.sample_process_util() {
                gpu.clear_unavailable();
                return;
            }
        }
        // Active but between polls: hold the last polled utilization.

        gpu.clear_available();
        self.materialize(procs, gpu);
    }

    /// Sum per-device SM utilization into the system bar and report whether any device is doing
    /// work. A device that fails to read is treated as active (do not skip based on an unknown) and
    /// nulls the bar total, matching the prior all-or-nothing system semantics.
    fn sample_device_util(&self, sys: &mut SystemStats) -> bool {
        let Some(get) = self.api.device_get_utilization else {
            sys.gpu_util_bp = None;
            return true; // cannot measure GPU activity → never skip the per-process poll
        };
        let mut total = Some(0u32);
        let mut active = false;
        for device in &self.devices {
            let mut util = Utilization::default();
            // SAFETY: handle was discovered at startup; output lives for the call.
            if unsafe { get(device.handle, &raw mut util) } == SUCCESS {
                if util.gpu > 0 {
                    active = true;
                }
                if let Some(t) = &mut total {
                    *t = t.saturating_add(util.gpu.saturating_mul(100));
                }
            } else {
                total = None;
                active = true;
            }
        }
        sys.gpu_util_bp = total;
        active
    }

    /// Sum per-device framebuffer totals into the system VRAM bar. Held between coarse refreshes.
    fn sample_device_vram(&self, sys: &mut SystemStats) {
        let Some(get) = self.api.device_get_memory else {
            sys.vram = None;
            return;
        };
        let mut vram = Some(VramStats::default());
        for device in &self.devices {
            let mut memory = Memory::default();
            // SAFETY: handle was discovered at startup; output lives for the call.
            if unsafe { get(device.handle, &raw mut memory) } == SUCCESS {
                if let Some(v) = &mut vram {
                    v.total = v.total.saturating_add(memory.total);
                    v.used = v.used.saturating_add(memory.used);
                }
            } else {
                vram = None;
            }
        }
        sys.vram = vram;
    }

    /// The expensive per-process utilization busy-poll. Runs only when a device is active and only
    /// on the coarse utilization interval. Each call retrieves every sample since the device's
    /// `last_seen_timestamp`, so skipped cycles lose no data — the next call drains the backlog.
    /// The two underlying APIs (`ProcessUtil::Info`/`Samples`) both fill the normalized
    /// `util_samples`, so the aggregation — latest sample per PID, summed across a client's GPUs —
    /// is shared.
    fn sample_process_util(&mut self) -> bool {
        let Some(get) = self.api.process_get_utilization else {
            return false;
        };
        let mut complete = true;
        for idx in 0..self.devices.len() {
            let handle = self.devices[idx].handle;
            let last = self.devices[idx].last_seen_timestamp;
            let ok = match get {
                ProcessUtil::Info(f) => {
                    query_util_info(f, handle, last, &mut self.util_info, &mut self.util_samples)
                }
                ProcessUtil::Samples(f) => {
                    query_process_util(f, handle, last, &mut self.util_samples).is_ok()
                }
            };
            if !ok {
                complete = false;
                continue;
            }
            self.util_samples
                .sort_unstable_by_key(|s| (s.pid, s.timestamp));
            let mut i = 0;
            while i < self.util_samples.len() {
                let pid = self.util_samples[i].pid;
                let mut latest = self.util_samples[i];
                i += 1;
                while i < self.util_samples.len() && self.util_samples[i].pid == pid {
                    latest = self.util_samples[i];
                    i += 1;
                }
                self.devices[idx].last_seen_timestamp =
                    self.devices[idx].last_seen_timestamp.max(latest.timestamp);
                if let Some(client) = self.clients.get_mut(&pid) {
                    let add = GpuMetric::Value(latest.sm_util.saturating_mul(100));
                    client.util_bp = client.util_bp.saturating_add(add);
                }
            }
        }
        complete
    }

    /// Coarse rediscovery of the GPU client set and each client's held VRAM, plus the device
    /// VRAM totals. Rebuilds [`clients`](Self::clients) from the compute + graphics process lists.
    fn refresh_clients(&mut self, sys: &mut SystemStats) -> bool {
        self.sample_device_vram(sys);
        self.clients.clear();
        self.empty_devices.clear();
        self.empty_devices_available = true;
        let (Some(compute), Some(graphics)) =
            (self.api.compute_processes, self.api.graphics_processes)
        else {
            self.empty_devices_available = false;
            return false;
        };

        let mut complete = true;
        for (index, device) in self.devices.iter().enumerate() {
            self.pid_memory.clear();
            let compute_ok = query_process_list(
                compute,
                device.handle,
                &mut self.process_v2,
                &mut self.process_v1,
                &mut self.pid_memory,
            );
            let graphics_ok = query_process_list(
                graphics,
                device.handle,
                &mut self.process_v2,
                &mut self.process_v1,
                &mut self.pid_memory,
            );
            if !compute_ok || !graphics_ok {
                complete = false;
                self.empty_devices_available = false;
                continue;
            }
            if self.pid_memory.is_empty() {
                self.empty_devices
                    .push(u32::try_from(index).unwrap_or(u32::MAX));
                continue;
            }

            // Compute and graphics lists can report the same context. Collapse each PID on
            // this physical device with max(VRAM), then sum those maxima across devices.
            self.pid_memory.sort_unstable_by_key(|p| p.pid);
            let mut i = 0;
            while i < self.pid_memory.len() {
                let pid = self.pid_memory[i].pid;
                let mut bytes = 0u64;
                let mut available = true;
                while i < self.pid_memory.len() && self.pid_memory[i].pid == pid {
                    bytes = bytes.max(self.pid_memory[i].bytes);
                    available &= self.pid_memory[i].available;
                    i += 1;
                }
                let add = if available {
                    GpuMetric::Value(bytes)
                } else {
                    GpuMetric::Unknown
                };
                let client = self.clients.entry(pid).or_default();
                client.vram = client.vram.saturating_add(add);
            }
        }
        complete
    }

    /// Publish the held client state into the sparse per-process view, filtered to PIDs live in
    /// the current buffer — a client that died between coarse refreshes simply does not appear.
    fn materialize(&self, procs: &[crate::procs::ProcessEntry], gpu: &mut GpuProcessStats) {
        for (&pid, client) in &self.clients {
            if procs.binary_search_by_key(&pid, |p| p.pid).is_ok() {
                gpu.add_live(
                    pid,
                    GpuMetrics {
                        pct: client.util_bp,
                        mem_bytes: client.vram,
                    },
                );
            }
        }
    }
}

fn query_process_util(
    get: ProcessUtilFn,
    device: Device,
    last_seen: u64,
    buf: &mut Vec<ProcessUtilSample>,
) -> Result<(), ()> {
    query_sized(
        buf,
        |ptr, count| {
            // SAFETY: the query helper supplies either null or a buffer of `count` elements.
            unsafe { get(device, ptr, count, last_seen) }
        },
        true,
    )
}

/// Device-Queries plural utilization: fill `buf` (the versioned container's backing array) with
/// samples since `last_seen` via the shared [`query_sized`] retry loop, then normalize into `out`
/// for the aggregation shared with the singular path.
fn query_util_info(
    get: ProcessesUtilInfoFn,
    device: Device,
    last_seen: u64,
    buf: &mut Vec<ProcessUtilInfo>,
    out: &mut Vec<ProcessUtilSample>,
) -> bool {
    // The versioned-container ABI threads the element count through a struct field rather than a
    // separate out-param, so bridge it into the shared sized-array helper: seed the current
    // capacity into the struct before the call, hand the actual/required count back after.
    let ok = query_sized(
        buf,
        |ptr, count| {
            let mut info = ProcessesUtilInfo {
                version: PROCESSES_UTIL_INFO_V1,
                // SAFETY: `count` is the helper's live capacity cell.
                process_samples_count: unsafe { *count },
                last_seen_timestamp: last_seen,
                proc_util_array: ptr,
            };
            // SAFETY: `version` is NVML's struct-version macro and the array is null or holds that
            // many elements, exactly as the ABI requires.
            let status = unsafe { get(device, &raw mut info) };
            // SAFETY: same live cell; report the actual/required count to the helper.
            unsafe { *count = info.process_samples_count };
            status
        },
        true,
    )
    .is_ok();
    if !ok {
        return false;
    }
    out.clear();
    out.extend(buf.iter().map(|p| ProcessUtilSample {
        pid: p.pid,
        timestamp: p.timestamp,
        sm_util: p.sm_util,
        mem_util: p.mem_util,
        enc_util: p.enc_util,
        dec_util: p.dec_util,
    }));
    true
}

fn query_process_list(
    get: ProcessListFn,
    device: Device,
    v2: &mut Vec<ProcessInfoV2>,
    v1: &mut Vec<ProcessInfoV1>,
    out: &mut Vec<PidMemory>,
) -> bool {
    match get {
        ProcessListFn::V2(f) => {
            if query_sized(
                v2,
                |ptr, count| {
                    // SAFETY: the query helper supplies a valid ABI-matched buffer.
                    unsafe { f(device, count, ptr) }
                },
                false,
            )
            .is_err()
            {
                return false;
            }
            out.extend(v2.iter().map(|p| PidMemory {
                pid: p.pid,
                bytes: p.used_gpu_memory,
                available: p.used_gpu_memory != VALUE_NOT_AVAILABLE,
            }));
        }
        ProcessListFn::V1(f) => {
            if query_sized(
                v1,
                |ptr, count| {
                    // SAFETY: the query helper supplies a valid ABI-matched buffer.
                    unsafe { f(device, count, ptr) }
                },
                false,
            )
            .is_err()
            {
                return false;
            }
            out.extend(v1.iter().map(|p| PidMemory {
                pid: p.pid,
                bytes: p.used_gpu_memory,
                available: p.used_gpu_memory != VALUE_NOT_AVAILABLE,
            }));
        }
    }
    true
}

/// Ceiling on a driver-reported element count. No real GPU has anywhere near this many processes
/// or buffered utilization samples; the cap turns an implausible or garbage count from the vendor
/// library into a graceful failure instead of a multi-gigabyte allocation attempt.
const MAX_QUERY_ELEMS: usize = 1 << 20;

/// NVML's variable-array convention: null first call reports required count with
/// `INSUFFICIENT_SIZE`; an already large high-water buffer usually completes in one call. The
/// reported count is bounded by [`MAX_QUERY_ELEMS`] before it is trusted for an allocation.
fn query_sized<T: Copy + Default>(
    buf: &mut Vec<T>,
    mut call: impl FnMut(*mut T, *mut c_uint) -> Return,
    not_found_is_empty: bool,
) -> Result<(), ()> {
    loop {
        let mut count = c_uint::try_from(buf.capacity()).unwrap_or(c_uint::MAX);
        let data = if count == 0 {
            ptr::null_mut()
        } else {
            buf.as_mut_ptr()
        };
        let status = call(data, &raw mut count);
        if status == SUCCESS || (not_found_is_empty && status == ERROR_NOT_FOUND) {
            let len = if status == SUCCESS { count as usize } else { 0 };
            if len > buf.capacity() {
                return Err(());
            }
            // SAFETY: NVML initialized exactly `len` elements on success.
            unsafe { buf.set_len(len) };
            return Ok(());
        }
        let needed = count as usize;
        if status != ERROR_INSUFFICIENT_SIZE || needed <= buf.capacity() || needed > MAX_QUERY_ELEMS
        {
            buf.clear();
            return Err(());
        }
        buf.reserve(needed - buf.len());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::procs::ProcessEntry;

    static DROP_ORDER: AtomicUsize = AtomicUsize::new(0);
    static PROBE_MODE: AtomicUsize = AtomicUsize::new(0);
    static PROBE_SHUTDOWNS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn probe_init() -> Return {
        if PROBE_MODE.load(Ordering::SeqCst) == 1 {
            1
        } else {
            SUCCESS
        }
    }

    unsafe extern "C" fn probe_shutdown() -> Return {
        PROBE_SHUTDOWNS.fetch_add(1, Ordering::SeqCst);
        SUCCESS
    }

    unsafe extern "C" fn probe_count(out: *mut c_uint) -> Return {
        let count = if PROBE_MODE.load(Ordering::SeqCst) == 2 {
            0
        } else {
            2
        };
        // SAFETY: discovery supplies a live output object.
        unsafe { out.write(count) };
        SUCCESS
    }

    unsafe extern "C" fn probe_handle(index: c_uint, out: *mut Device) -> Return {
        // SAFETY: discovery supplies a live output object.
        unsafe { out.write((index as usize + 1) as Device) };
        SUCCESS
    }

    unsafe extern "C" fn fake_shutdown() -> Return {
        assert_eq!(DROP_ORDER.swap(1, Ordering::SeqCst), 0);
        SUCCESS
    }

    unsafe extern "C" fn fake_close(_: *mut c_void) -> libc::c_int {
        assert_eq!(DROP_ORDER.swap(2, Ordering::SeqCst), 1);
        0
    }

    // Benign lifecycle fns for tests that don't assert drop order (the drop-order asserts in
    // `fake_shutdown`/`fake_close` are shared global state and would race across parallel tests).
    unsafe extern "C" fn noop_shutdown() -> Return {
        SUCCESS
    }

    unsafe extern "C" fn noop_close(_: *mut c_void) -> libc::c_int {
        0
    }

    fn device_index(device: Device) -> usize {
        device as usize - 1
    }

    unsafe extern "C" fn fake_util(device: Device, out: *mut Utilization) -> Return {
        let gpu = [40, 70][device_index(device)];
        // SAFETY: sampler passes a live output object.
        unsafe { out.write(Utilization { gpu, memory: 0 }) };
        SUCCESS
    }

    unsafe extern "C" fn fake_memory(device: Device, out: *mut Memory) -> Return {
        let used = [100, 200][device_index(device)];
        // SAFETY: sampler passes a live output object.
        unsafe {
            out.write(Memory {
                total: 1_000,
                free: 1_000 - used,
                used,
            });
        }
        SUCCESS
    }

    unsafe extern "C" fn fake_process_util(
        device: Device,
        out: *mut ProcessUtilSample,
        count: *mut c_uint,
        _: u64,
    ) -> Return {
        if out.is_null() {
            // SAFETY: sampler always supplies the count output.
            unsafe { count.write(1) };
            return ERROR_INSUFFICIENT_SIZE;
        }
        let index = device_index(device);
        // SAFETY: the preceding size query requested one element.
        unsafe {
            out.write(ProcessUtilSample {
                pid: 10,
                timestamp: [11, 22][index],
                sm_util: [20, 30][index],
                ..ProcessUtilSample::default()
            });
            count.write(1);
        }
        SUCCESS
    }

    /// Device-Queries plural equivalent of `fake_process_util`: same pid, timestamps and SM
    /// utilization, exercised through the versioned container + sized-array protocol.
    unsafe extern "C" fn fake_processes_util_info(
        device: Device,
        info: *mut ProcessesUtilInfo,
    ) -> Return {
        // SAFETY: the sampler always passes a live container.
        let info = unsafe { &mut *info };
        if info.proc_util_array.is_null() || info.process_samples_count == 0 {
            info.process_samples_count = 1;
            return ERROR_INSUFFICIENT_SIZE;
        }
        let index = device_index(device);
        // SAFETY: the preceding size query requested one element.
        unsafe {
            info.proc_util_array.write(ProcessUtilInfo {
                pid: 10,
                timestamp: [11, 22][index],
                sm_util: [20, 30][index],
                ..ProcessUtilInfo::default()
            });
        }
        info.process_samples_count = 1;
        SUCCESS
    }

    unsafe extern "C" fn fake_process_util_partial(
        device: Device,
        out: *mut ProcessUtilSample,
        count: *mut c_uint,
        last_seen: u64,
    ) -> Return {
        if device_index(device) == 1 {
            3 // NVML_ERROR_NOT_SUPPORTED (e.g. a MIG utilization query)
        } else {
            // SAFETY: forwards the exact ABI arguments to the complete fake.
            unsafe { fake_process_util(device, out, count, last_seen) }
        }
    }

    unsafe extern "C" fn fake_util_partial(device: Device, out: *mut Utilization) -> Return {
        if device_index(device) == 1 {
            15 // NVML_ERROR_GPU_IS_LOST
        } else {
            // SAFETY: forwards the exact ABI arguments to the complete fake.
            unsafe { fake_util(device, out) }
        }
    }

    unsafe extern "C" fn fake_compute(
        device: Device,
        count: *mut c_uint,
        out: *mut ProcessInfoV2,
    ) -> Return {
        if out.is_null() {
            // SAFETY: sampler always supplies the count output.
            unsafe { count.write(1) };
            return ERROR_INSUFFICIENT_SIZE;
        }
        // SAFETY: the preceding size query requested one element.
        unsafe {
            out.write(ProcessInfoV2 {
                pid: 10,
                used_gpu_memory: [100, 200][device_index(device)],
                ..ProcessInfoV2::default()
            });
            count.write(1);
        }
        SUCCESS
    }

    unsafe extern "C" fn fake_graphics(
        device: Device,
        count: *mut c_uint,
        out: *mut ProcessInfoV2,
    ) -> Return {
        if device_index(device) == 0 {
            if out.is_null() {
                // SAFETY: sampler always supplies the count output.
                unsafe { count.write(1) };
                return ERROR_INSUFFICIENT_SIZE;
            }
            // Same PID/device as compute, but less VRAM: dedup must retain 100, not sum 180.
            // SAFETY: the preceding size query requested one element.
            unsafe {
                out.write(ProcessInfoV2 {
                    pid: 10,
                    used_gpu_memory: 80,
                    ..ProcessInfoV2::default()
                });
                count.write(1);
            }
        } else {
            // SAFETY: sampler always supplies the count output.
            unsafe { count.write(0) };
        }
        SUCCESS
    }

    unsafe extern "C" fn fake_empty_processes(
        _: Device,
        count: *mut c_uint,
        _: *mut ProcessInfoV2,
    ) -> Return {
        // SAFETY: sampler always supplies the count output.
        unsafe { count.write(0) };
        SUCCESS
    }

    fn fake_sampler() -> NvmlSampler {
        let api = NvmlApi {
            library: ptr::dangling_mut(),
            close: fake_close,
            shutdown: fake_shutdown,
            device_get_utilization: Some(fake_util),
            device_get_memory: Some(fake_memory),
            process_get_utilization: Some(ProcessUtil::Samples(fake_process_util)),
            compute_processes: Some(ProcessListFn::V2(fake_compute)),
            graphics_processes: Some(ProcessListFn::V2(fake_graphics)),
        };
        NvmlSampler {
            api,
            devices: vec![
                DeviceState {
                    handle: 1usize as Device,
                    last_seen_timestamp: 0,
                },
                DeviceState {
                    handle: 2usize as Device,
                    last_seen_timestamp: 0,
                },
            ],
            util_samples: Vec::new(),
            util_info: Vec::new(),
            process_v2: Vec::new(),
            process_v1: Vec::new(),
            pid_memory: Vec::new(),
            empty_devices: Vec::new(),
            empty_devices_available: false,
            clients: PidMap::default(),
            cycle: 0,
            refresh_pending: false,
        }
    }

    #[test]
    fn abi_layouts_match_nvml_header() {
        assert_eq!(mem::size_of::<Utilization>(), 8);
        assert_eq!(mem::size_of::<Memory>(), 24);
        assert_eq!(mem::size_of::<ProcessInfoV1>(), 16);
        assert_eq!(mem::size_of::<ProcessInfoV2>(), 24);
        assert_eq!(mem::size_of::<ProcessUtilSample>(), 32);
        assert_eq!(mem::offset_of!(ProcessUtilSample, timestamp), 8);
        // Device-Queries plural utilization ABI.
        assert_eq!(mem::size_of::<ProcessUtilInfo>(), 40);
        assert_eq!(mem::offset_of!(ProcessUtilInfo, pid), 8);
        assert_eq!(mem::offset_of!(ProcessUtilInfo, sm_util), 12);
        assert_eq!(mem::size_of::<ProcessesUtilInfo>(), 24);
        assert_eq!(mem::offset_of!(ProcessesUtilInfo, last_seen_timestamp), 8);
        assert_eq!(mem::offset_of!(ProcessesUtilInfo, proc_util_array), 16);
        assert_eq!(PROCESSES_UTIL_INFO_V1, 0x0100_0018); // struct size 24, version 1 in top byte
    }

    #[test]
    fn absent_library_or_driver_is_a_silent_fallback() {
        // This is environment-independent: either a usable device exists or probe returns
        // None. Most CI hosts exercise the latter; importantly, neither branch can panic.
        let _ = NvmlSampler::probe();
    }

    #[test]
    fn missing_symbol_initialization_failure_and_zero_devices_fall_back() {
        // A loader handle for the current program is enough to test a guaranteed-missing name.
        // SAFETY: null filename is the documented `dlopen` handle for the main program.
        let library = unsafe { libc::dlopen(ptr::null(), libc::RTLD_NOW) };
        assert!(!library.is_null());
        // SAFETY: the handle is live and the symbol name is NUL-terminated.
        assert!(unsafe { symbol::<InitFn>(library, c"oya_symbol_that_must_not_exist") }.is_none());
        // SAFETY: closes the handle acquired above.
        unsafe { libc::dlclose(library) };

        PROBE_SHUTDOWNS.store(0, Ordering::SeqCst);
        PROBE_MODE.store(1, Ordering::SeqCst);
        assert!(discover_devices(probe_init, probe_shutdown, probe_count, probe_handle).is_none());
        assert_eq!(PROBE_SHUTDOWNS.load(Ordering::SeqCst), 0);

        PROBE_MODE.store(2, Ordering::SeqCst);
        assert!(discover_devices(probe_init, probe_shutdown, probe_count, probe_handle).is_none());
        assert_eq!(PROBE_SHUTDOWNS.load(Ordering::SeqCst), 1);

        PROBE_MODE.store(0, Ordering::SeqCst);
        let devices =
            discover_devices(probe_init, probe_shutdown, probe_count, probe_handle).unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(PROBE_SHUTDOWNS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unavailable_vram_sentinel_never_becomes_a_huge_value() {
        let mut values = [PidMemory {
            pid: 7,
            bytes: VALUE_NOT_AVAILABLE,
            available: false,
        }];
        for p in values.iter_mut().filter(|p| !p.available) {
            p.bytes = 0;
        }
        assert_eq!(values[0].bytes, 0);
        assert!(!values[0].available);
    }

    #[test]
    fn multi_gpu_sum_dedup_timestamp_and_drop_order() {
        DROP_ORDER.store(0, Ordering::SeqCst);
        {
            let mut sampler = fake_sampler();
            let mut sys = SystemStats::default();
            let mut p10 = ProcessEntry::TOMBSTONE;
            p10.pid = 10;
            let mut p20 = ProcessEntry::TOMBSTONE;
            p20.pid = 20;
            let procs = [p10, p20];

            let mut gpu = GpuProcessStats::default();
            sampler.sample(&mut sys, &procs, &mut gpu);

            assert_eq!(sys.gpu_count, 2);
            assert_eq!(sys.gpu_util_bp, Some(11_000));
            assert_eq!(sys.vram.map(|v| (v.used, v.total)), Some((300, 2_000)));
            assert_eq!(
                gpu.live(10),
                Some(GpuMetrics {
                    pct: GpuMetric::Value(5_000),
                    mem_bytes: GpuMetric::Value(300),
                })
            );
            assert_eq!(gpu.live(20), None);
            assert_eq!(sampler.devices[0].last_seen_timestamp, 11);
            assert_eq!(sampler.devices[1].last_seen_timestamp, 22);
            assert_eq!(sampler.empty_devices(), Some([].as_slice()));
        }
        assert_eq!(DROP_ORDER.load(Ordering::SeqCst), 2);

        DROP_ORDER.store(0, Ordering::SeqCst);
        {
            let mut sampler = fake_sampler();
            sampler.api.device_get_utilization = Some(fake_util_partial);
            sampler.api.process_get_utilization =
                Some(ProcessUtil::Samples(fake_process_util_partial));
            let mut sys = SystemStats::default();
            let mut proc = ProcessEntry::TOMBSTONE;
            proc.pid = 10;
            let mut gpu = GpuProcessStats::default();
            sampler.sample(&mut sys, std::slice::from_ref(&proc), &mut gpu);
            assert_eq!(sys.gpu_util_bp, None);
            assert!(!gpu.available());
        }
        assert_eq!(DROP_ORDER.load(Ordering::SeqCst), 2);

        DROP_ORDER.store(0, Ordering::SeqCst);
        {
            let mut sampler = fake_sampler();
            sampler.api.compute_processes = Some(ProcessListFn::V2(fake_empty_processes));
            sampler.api.graphics_processes = Some(ProcessListFn::V2(fake_empty_processes));
            let mut sys = SystemStats::default();
            let mut gpu = GpuProcessStats::default();
            sampler.sample(&mut sys, &[], &mut gpu);
            assert_eq!(sampler.empty_devices(), Some([0, 1].as_slice()));
        }
        assert_eq!(DROP_ORDER.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn idle_device_skips_per_process_poll_and_holds_vram() {
        static UTIL_CALLS: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn idle_device_util(_: Device, out: *mut Utilization) -> Return {
            // SAFETY: sampler passes a live output object. gpu = 0 → device idle.
            unsafe { out.write(Utilization::default()) };
            SUCCESS
        }
        unsafe extern "C" fn counting_process_util(
            device: Device,
            out: *mut ProcessUtilSample,
            count: *mut c_uint,
            last: u64,
        ) -> Return {
            UTIL_CALLS.fetch_add(1, Ordering::SeqCst);
            // SAFETY: forwards the exact ABI arguments to the complete fake.
            unsafe { fake_process_util(device, out, count, last) }
        }

        UTIL_CALLS.store(0, Ordering::SeqCst);
        let mut sampler = fake_sampler();
        sampler.api.shutdown = noop_shutdown;
        sampler.api.close = noop_close;
        sampler.api.device_get_utilization = Some(idle_device_util);
        sampler.api.process_get_utilization = Some(ProcessUtil::Samples(counting_process_util));
        let mut sys = SystemStats::default();
        let mut p10 = ProcessEntry::TOMBSTONE;
        p10.pid = 10;
        let procs = [p10];
        let mut gpu = GpuProcessStats::default();
        sampler.sample(&mut sys, &procs, &mut gpu);

        // Device reads idle, so the expensive per-process busy-poll never ran, yet the coarse
        // process-list still discovered the client's held VRAM (100 + 200 across the two GPUs)
        // and its utilization is a true zero.
        assert_eq!(
            UTIL_CALLS.load(Ordering::SeqCst),
            0,
            "an idle device must not trigger the per-process utilization poll"
        );
        assert_eq!(sys.gpu_util_bp, Some(0));
        assert_eq!(
            gpu.live(10),
            Some(GpuMetrics {
                pct: GpuMetric::Value(0),
                mem_bytes: GpuMetric::Value(300),
            })
        );
        assert_eq!(sys.vram.map(|v| (v.used, v.total)), Some((300, 2_000)));
    }

    #[test]
    fn process_list_refreshes_on_a_coarse_interval() {
        static LIST_CALLS: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn counting_compute(
            device: Device,
            count: *mut c_uint,
            out: *mut ProcessInfoV2,
        ) -> Return {
            LIST_CALLS.fetch_add(1, Ordering::SeqCst);
            // SAFETY: forwards the exact ABI arguments to the complete fake.
            unsafe { fake_compute(device, count, out) }
        }

        LIST_CALLS.store(0, Ordering::SeqCst);
        let mut sampler = fake_sampler();
        sampler.api.shutdown = noop_shutdown;
        sampler.api.close = noop_close;
        sampler.api.compute_processes = Some(ProcessListFn::V2(counting_compute));
        let mut sys = SystemStats::default();
        let mut p10 = ProcessEntry::TOMBSTONE;
        p10.pid = 10;
        let procs = [p10];
        let mut gpu = GpuProcessStats::default();

        sampler.sample(&mut sys, &procs, &mut gpu);
        let after_first = LIST_CALLS.load(Ordering::SeqCst);
        assert!(
            after_first > 0,
            "the first sample must discover the client set"
        );

        // The next full interval of cycles reuses the held client set — no re-enumeration.
        for _ in 0..CLIENT_REFRESH_CYCLES - 1 {
            sampler.sample(&mut sys, &procs, &mut gpu);
        }
        assert_eq!(
            LIST_CALLS.load(Ordering::SeqCst),
            after_first,
            "the process list must be held between coarse refreshes"
        );

        // One more cycle crosses the interval boundary and refreshes again.
        sampler.sample(&mut sys, &procs, &mut gpu);
        assert!(
            LIST_CALLS.load(Ordering::SeqCst) > after_first,
            "the coarse interval must re-enumerate after CLIENT_REFRESH_CYCLES"
        );
    }

    #[test]
    fn process_util_polls_on_coarse_interval_and_holds_between() {
        static UTIL_POLLS: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn counting_util(
            device: Device,
            out: *mut ProcessUtilSample,
            count: *mut c_uint,
            last: u64,
        ) -> Return {
            // Count only the real fill, not the null size-probe, so one increment == one device poll.
            if !out.is_null() {
                UTIL_POLLS.fetch_add(1, Ordering::SeqCst);
            }
            // SAFETY: forwards the exact ABI arguments to the complete fake.
            unsafe { fake_process_util(device, out, count, last) }
        }

        UTIL_POLLS.store(0, Ordering::SeqCst);
        let mut sampler = fake_sampler();
        sampler.api.shutdown = noop_shutdown;
        sampler.api.close = noop_close;
        sampler.api.process_get_utilization = Some(ProcessUtil::Samples(counting_util));
        let mut sys = SystemStats::default();
        let mut p10 = ProcessEntry::TOMBSTONE;
        p10.pid = 10;
        let procs = [p10];
        let mut gpu = GpuProcessStats::default();

        // First (active) cycle polls the per-process utilization.
        sampler.sample(&mut sys, &procs, &mut gpu);
        let after_first = UTIL_POLLS.load(Ordering::SeqCst);
        assert!(
            after_first > 0,
            "the first sample must poll per-process utilization"
        );
        assert_eq!(gpu.live(10).map(|m| m.pct), Some(GpuMetric::Value(5_000)));

        // The rest of the interval window reuses the held value — no new polls, still materialized.
        for _ in 0..PROCESS_UTIL_REFRESH_CYCLES - 1 {
            sampler.sample(&mut sys, &procs, &mut gpu);
        }
        assert_eq!(
            UTIL_POLLS.load(Ordering::SeqCst),
            after_first,
            "utilization is held between polls, not re-polled every cycle"
        );
        assert_eq!(
            gpu.live(10).map(|m| m.pct),
            Some(GpuMetric::Value(5_000)),
            "the held utilization is still materialized on non-poll cycles"
        );

        // Crossing the interval boundary re-polls (reset-then-fill, so a quiet client would decay).
        sampler.sample(&mut sys, &procs, &mut gpu);
        assert!(
            UTIL_POLLS.load(Ordering::SeqCst) > after_first,
            "the poll re-runs after PROCESS_UTIL_REFRESH_CYCLES"
        );
    }

    #[test]
    fn device_queries_plural_util_path_aggregates_like_grid() {
        // The preferred Device-Queries API (versioned container + sized array) must produce the
        // same per-client aggregation as the GRID path: pid 10 summed across both GPUs to 5000.
        let mut sampler = fake_sampler();
        sampler.api.shutdown = noop_shutdown;
        sampler.api.close = noop_close;
        sampler.api.process_get_utilization = Some(ProcessUtil::Info(fake_processes_util_info));
        let mut sys = SystemStats::default();
        let mut p10 = ProcessEntry::TOMBSTONE;
        p10.pid = 10;
        let procs = [p10];
        let mut gpu = GpuProcessStats::default();

        sampler.sample(&mut sys, &procs, &mut gpu);

        assert_eq!(
            gpu.live(10),
            Some(GpuMetrics {
                pct: GpuMetric::Value(5_000),
                mem_bytes: GpuMetric::Value(300),
            })
        );
        assert_eq!(sampler.devices[0].last_seen_timestamp, 11);
        assert_eq!(sampler.devices[1].last_seen_timestamp, 22);
    }
}
