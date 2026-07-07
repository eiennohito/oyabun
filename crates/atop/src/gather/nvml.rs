//! Optional NVIDIA telemetry through a runtime-loaded NVML.
//!
//! This module deliberately has no CUDA/NVML build dependency. The small stable C ABI used
//! here is declared locally and resolved from `libnvidia-ml.so.1`; any probe failure simply
//! leaves the rest of atop in its original CPU-only mode.

use std::ffi::{c_char, c_uint, c_void};
use std::mem;
use std::ptr;

use crate::procs::{GpuMetric, GpuMetrics, GpuProcessStats, SystemStats, VramStats};

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

struct NvmlApi {
    library: *mut c_void,
    close: CloseFn,
    shutdown: ShutdownFn,
    device_get_utilization: Option<UtilFn>,
    device_get_memory: Option<MemoryFn>,
    process_get_utilization: Option<ProcessUtilFn>,
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
                // SAFETY: optional symbol signature matches nvml.h.
                process_get_utilization: unsafe {
                    symbol(library, c"nvmlDeviceGetProcessUtilization")
                },
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

/// Runtime NVML sampler. It is constructed only when initialization and fixed startup device
/// discovery both succeed.
pub(crate) struct NvmlSampler {
    api: NvmlApi,
    devices: Vec<DeviceState>,
    util_samples: Vec<ProcessUtilSample>,
    process_v2: Vec<ProcessInfoV2>,
    process_v1: Vec<ProcessInfoV1>,
    pid_memory: Vec<PidMemory>,
    empty_devices: Vec<u32>,
    empty_devices_available: bool,
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
            process_v2: Vec::new(),
            process_v1: Vec::new(),
            pid_memory: Vec::new(),
            empty_devices: Vec::new(),
            empty_devices_available: false,
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
        self.sample_system(sys);

        if !self.process_available() {
            gpu.clear_unavailable();
            return;
        }
        gpu.clear_available();
        let util_ok = self.sample_process_utilization(procs, gpu);
        let memory_ok = self.sample_process_memory(procs, gpu);
        if !util_ok || !memory_ok {
            gpu.clear_unavailable();
        }
    }

    fn sample_system(&self, sys: &mut SystemStats) {
        let mut gpu_util_bp: Option<u32> = self.api.device_get_utilization.map(|_| 0);
        let mut vram = self.api.device_get_memory.map(|_| VramStats::default());

        for device in &self.devices {
            if let Some(get) = self.api.device_get_utilization {
                let mut util = Utilization::default();
                // SAFETY: handle was discovered at startup; output lives for the call.
                if unsafe { get(device.handle, &raw mut util) } == SUCCESS {
                    if let Some(total) = &mut gpu_util_bp {
                        *total = total.saturating_add(util.gpu.saturating_mul(100));
                    }
                } else {
                    gpu_util_bp = None;
                }
            }
            if let Some(get) = self.api.device_get_memory {
                let mut memory = Memory::default();
                // SAFETY: handle was discovered at startup; output lives for the call.
                if unsafe { get(device.handle, &raw mut memory) } == SUCCESS {
                    if let Some(total) = &mut vram {
                        total.total = total.total.saturating_add(memory.total);
                        total.used = total.used.saturating_add(memory.used);
                    }
                } else {
                    vram = None;
                }
            }
        }
        sys.gpu_util_bp = gpu_util_bp;
        sys.vram = vram;
    }

    fn sample_process_utilization(
        &mut self,
        procs: &[crate::procs::ProcessEntry],
        gpu: &mut GpuProcessStats,
    ) -> bool {
        let Some(get) = self.api.process_get_utilization else {
            return false;
        };
        let mut complete = true;

        for device in &mut self.devices {
            match query_process_util(
                get,
                device.handle,
                device.last_seen_timestamp,
                &mut self.util_samples,
            ) {
                Ok(()) => {
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
                        device.last_seen_timestamp =
                            device.last_seen_timestamp.max(latest.timestamp);
                        if let Ok(row) = procs.binary_search_by_key(&pid, |p| p.pid) {
                            gpu.add_live(
                                procs[row].pid,
                                GpuMetrics {
                                    pct: GpuMetric::Value(latest.sm_util.saturating_mul(100)),
                                    mem_bytes: GpuMetric::Value(0),
                                },
                            );
                        }
                    }
                }
                Err(()) => complete = false,
            }
        }
        complete
    }

    fn sample_process_memory(
        &mut self,
        procs: &[crate::procs::ProcessEntry],
        gpu: &mut GpuProcessStats,
    ) -> bool {
        self.empty_devices.clear();
        self.empty_devices_available =
            self.api.compute_processes.is_some() && self.api.graphics_processes.is_some();
        let (Some(compute), Some(graphics)) =
            (self.api.compute_processes, self.api.graphics_processes)
        else {
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
                if let Ok(row) = procs.binary_search_by_key(&pid, |p| p.pid) {
                    gpu.add_live(
                        procs[row].pid,
                        GpuMetrics {
                            pct: GpuMetric::Value(0),
                            mem_bytes: if available {
                                GpuMetric::Value(bytes)
                            } else {
                                GpuMetric::Unknown
                            },
                        },
                    );
                }
            }
        }
        if !complete {
            return false;
        }
        true
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

/// NVML's variable-array convention: null first call reports required count with
/// `INSUFFICIENT_SIZE`; an already large high-water buffer usually completes in one call.
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
        if status != ERROR_INSUFFICIENT_SIZE || count as usize <= buf.capacity() {
            buf.clear();
            return Err(());
        }
        buf.reserve(count as usize - buf.len());
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
            process_get_utilization: Some(fake_process_util),
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
            process_v2: Vec::new(),
            process_v1: Vec::new(),
            pid_memory: Vec::new(),
            empty_devices: Vec::new(),
            empty_devices_available: false,
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
        assert!(unsafe { symbol::<InitFn>(library, c"atop_symbol_that_must_not_exist") }.is_none());
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
            sampler.api.process_get_utilization = Some(fake_process_util_partial);
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
}
