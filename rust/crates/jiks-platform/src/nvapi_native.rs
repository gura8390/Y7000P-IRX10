//! Native NVAPI transport. Explicit calls may wake a GPU; never used by import,
//! general device enumeration, or the idle sampler. Private packets are validated
//! against the fixed-fork ABI in gpu.rs before crossing the DLL boundary.
use crate::{
    domain::{ExplicitAction, monotonic_ms},
    gpu::{self, AbiBuffer, NvApiTransport, NvFunction, NvStatus},
    ownership::{OwnerToken, wide},
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    ffi::{CStr, c_void},
    marker::PhantomData,
    ptr,
    rc::Rc,
};
use windows_sys::Win32::{
    Foundation::{FreeLibrary, HMODULE},
    System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
};
type Query = unsafe extern "C" fn(u32) -> *mut c_void;
type Init = unsafe extern "C" fn() -> i32;
type Enumerate = unsafe extern "C" fn(*mut *mut c_void, *mut u32) -> i32;
type Name = unsafe extern "C" fn(*mut c_void, *mut i8) -> i32;
type Structured = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
type Enable = unsafe extern "C" fn(*mut c_void) -> i32;

struct Library {
    module: HMODULE,
    query: Query,
    unload: Init,
    initialized: bool,
    _thread: PhantomData<Rc<()>>,
}
impl Library {
    fn open() -> Result<Self> {
        let name = wide("nvapi64.dll")?;
        let module =
            unsafe { LoadLibraryExW(name.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        ensure!(!module.is_null(), "System NVAPI64 library unavailable");
        // Free even if resolving the bootstrap ABI fails.
        let result = (|| {
            let exported = unsafe { GetProcAddress(module, b"nvapi_QueryInterface\0".as_ptr()) }
                .context("NVAPI query interface unavailable")?;
            let query: Query = unsafe { std::mem::transmute(exported) };
            let init_ptr = unsafe { query(NvFunction::Initialize as u32) };
            let unload_ptr = unsafe { query(NvFunction::Unload as u32) };
            ensure!(
                !init_ptr.is_null() && !unload_ptr.is_null(),
                "NVAPI bootstrap capabilities unavailable"
            );
            let init: Init = unsafe { std::mem::transmute(init_ptr) };
            let unload: Init = unsafe { std::mem::transmute(unload_ptr) };
            NvStatus::checked(unsafe { init() })?;
            Ok(Self {
                module,
                query,
                unload,
                initialized: true,
                _thread: PhantomData,
            })
        })();
        if result.is_err() {
            unsafe { FreeLibrary(module) };
        }
        result
    }
    fn resolve(&self, function: NvFunction) -> Result<*mut c_void> {
        let value = unsafe { (self.query)(function as u32) };
        if value.is_null() {
            return Err(NvStatus(-3).into());
        }
        Ok(value)
    }
    fn devices(&self) -> Result<Vec<(GpuDevice, *mut c_void)>> {
        let enumerate: Enumerate =
            unsafe { std::mem::transmute(self.resolve(NvFunction::EnumPhysicalGpus)?) };
        let name: Name = unsafe { std::mem::transmute(self.resolve(NvFunction::GetFullName)?) };
        let mut handles = [ptr::null_mut(); gpu::MAX_PHYSICAL_GPUS];
        let mut count = 0;
        NvStatus::checked(unsafe { enumerate(handles.as_mut_ptr(), &mut count) })?;
        ensure!(
            count as usize <= handles.len(),
            "NVAPI physical GPU count exceeds native array"
        );
        let mut result = vec![];
        for (index, handle) in handles.into_iter().take(count as usize).enumerate() {
            ensure!(
                !handle.is_null(),
                "NVAPI returned a null physical GPU handle"
            );
            let mut buffer = [0i8; gpu::SHORT_STRING_BYTES];
            NvStatus::checked(unsafe { name(handle, buffer.as_mut_ptr()) })?;
            ensure!(buffer.contains(&0), "Unterminated NVAPI GPU name");
            let name = unsafe { CStr::from_ptr(buffer.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            result.push((
                GpuDevice {
                    index: index as u32,
                    name,
                    source: "Native NVAPI physical handle; explicit read can wake GPU".into(),
                },
                handle,
            ));
        }
        Ok(result)
    }
}
impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            if self.initialized {
                (self.unload)();
            }
            FreeLibrary(self.module)
        };
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct GpuDevice {
    pub index: u32,
    pub name: String,
    pub source: String,
}
pub fn inventory() -> Result<Vec<GpuDevice>> {
    Ok(Library::open()?
        .devices()?
        .into_iter()
        .map(|(device, _)| device)
        .collect())
}
pub(crate) struct NativeNvApi<'a> {
    library: Library,
    handle: *mut c_void,
    owner: Option<&'a OwnerToken>,
    _action: Option<&'a ExplicitAction>,
    deadline_tick_ms: Option<u64>,
    performance_requires_ac: bool,
}
impl<'a> NativeNvApi<'a> {
    fn status(&mut self) -> crate::gpu_status::GpuStatusSnapshot {
        let requested = monotonic_ms();
        let mut status = crate::gpu_status::read(self, requested);
        status.completed_at_tick_ms = monotonic_ms();
        status
    }
    fn current_pstate(&self) -> Result<u32> {
        let pointer = self.performance_pointer(crate::gpu_pstate::GET_CURRENT_PSTATE)?;
        let get: unsafe extern "C" fn(*mut c_void, *mut u32) -> i32 =
            unsafe { std::mem::transmute(pointer) };
        crate::gpu_pstate::current(|value| {
            NvStatus::checked(unsafe { get(self.handle, value) }).map(|_| ())
        })
    }
    fn performance_pointer(&self, id: u32) -> Result<*mut c_void> {
        let p = unsafe { (self.library.query)(id) };
        ensure!(
            !p.is_null(),
            "GPU performance function unavailable ({id:#x})"
        );
        Ok(p)
    }
    fn performance_write_guard(&self) -> Result<()> {
        ensure!(
            self.owner.is_some(),
            "GPU performance write requires exclusive owner"
        );
        self._action
            .context("GPU performance write requires explicit action")?
            .check_write_boundary()?;
        ensure!(
            !self.performance_requires_ac || crate::ac_connected()?,
            "AC disconnected before GPU performance setter"
        );
        ensure!(
            self.deadline_tick_ms.is_some_and(|d| monotonic_ms() < d),
            "GPU performance deadline expired before native setter"
        );
        Ok(())
    }
    pub(crate) fn open(
        index: u32,
        expected_name: &str,
        owner: Option<&'a OwnerToken>,
        action: Option<&'a ExplicitAction>,
        deadline_tick_ms: Option<u64>,
    ) -> Result<Self> {
        ensure!(
            !expected_name.is_empty(),
            "GPU name must be confirmed with its explicit physical index"
        );
        let library = Library::open()?;
        let (device, handle) = library
            .devices()?
            .into_iter()
            .find(|(device, _)| device.index == index)
            .context("Selected physical GPU is absent")?;
        ensure!(
            crate::gpu_awake::names_match(&device.name, expected_name),
            "Physical GPU identity changed after selection"
        );
        Ok(Self {
            library,
            handle,
            owner,
            _action: action,
            deadline_tick_ms,
            performance_requires_ac: false,
        })
    }
}
impl crate::gpu_status::StatusTransport for NativeNvApi<'_> {
    fn coproc_info(&mut self, packet: &mut AbiBuffer) -> Result<()> {
        let (bytes, version) = match packet.version() {
            8 => (108, 8),
            7 => (96, 7),
            _ => anyhow::bail!("Unverified NVAPI coprocessor ABI"),
        };
        packet.validate(bytes, version)?;
        let pointer = self.performance_pointer(crate::gpu_status::GET_COPROC_INFO)?;
        let get: Structured = unsafe { std::mem::transmute(pointer) };
        NvStatus::checked(unsafe { get(self.handle, packet.as_mut_ptr()) })
    }
    fn active_applications(
        &mut self,
        apps: &mut [crate::gpu_status::ActiveApplication],
        count: &mut u32,
    ) -> Result<()> {
        ensure!(
            apps.len() == crate::gpu_status::MAX_APPLICATIONS
                && *count == apps.len() as u32
                && apps
                    .iter()
                    .all(|a| a.version == crate::gpu_status::APPLICATION_VERSION),
            "Invalid NVAPI active application packet"
        );
        type QueryApps = unsafe extern "C" fn(
            *mut c_void,
            *mut crate::gpu_status::ActiveApplication,
            *mut u32,
        ) -> i32;
        let pointer = self.performance_pointer(crate::gpu_status::QUERY_ACTIVE_APPS)?;
        let get: QueryApps = unsafe { std::mem::transmute(pointer) };
        NvStatus::checked(unsafe { get(self.handle, apps.as_mut_ptr(), count) })?;
        ensure!(
            *count as usize <= apps.len(),
            "NVAPI active application count overflow"
        );
        Ok(())
    }
    fn process_exists(&mut self, pid: u32) -> Result<bool> {
        use windows_sys::Win32::{
            Foundation::{
                CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, WAIT_OBJECT_0, WAIT_TIMEOUT,
            },
            System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
        };
        // A query-only SYNCHRONIZE handle verifies that an app has not exited,
        // matching the old Process.GetProcessById filtering without reading its
        // command line, environment, executable path or private process data.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            let error = unsafe { GetLastError() };
            if error == ERROR_INVALID_PARAMETER {
                return Ok(false);
            }
            anyhow::bail!("GPU active process existence query failed (Win32 {error})");
        }
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        let error = if wait != WAIT_OBJECT_0 && wait != WAIT_TIMEOUT {
            Some(unsafe { GetLastError() })
        } else {
            None
        };
        unsafe { CloseHandle(handle) };
        match wait {
            WAIT_OBJECT_0 => Ok(false),
            WAIT_TIMEOUT => Ok(true),
            _ => anyhow::bail!(
                "GPU active process existence wait failed (Win32 {})",
                error.unwrap_or(0)
            ),
        }
    }
    fn connected_display_count(&mut self) -> Result<u32> {
        type Displays = unsafe extern "C" fn(
            *mut c_void,
            *mut crate::gpu_status::DisplayId,
            *mut u32,
            u32,
        ) -> i32;
        let pointer = self.performance_pointer(crate::gpu_status::GET_CONNECTED_DISPLAY_IDS)?;
        let get: Displays = unsafe { std::mem::transmute(pointer) };
        let mut count = 0;
        // ConnectedIdsFlag.None=0: cached connected devices, exactly as the
        // original fallback. No uncached probe, display setter or DDC request.
        NvStatus::checked(unsafe { get(self.handle, ptr::null_mut(), &mut count, 0) })?;
        ensure!(
            count <= crate::gpu_status::MAX_DISPLAYS,
            "NVAPI connected display count exceeds bound"
        );
        if count == 0 {
            return Ok(0);
        }
        let mut displays = vec![crate::gpu_status::DisplayId::default(); count as usize];
        NvStatus::checked(unsafe { get(self.handle, displays.as_mut_ptr(), &mut count, 0) })?;
        ensure!(
            count as usize <= displays.len(),
            "NVAPI connected display count grew beyond buffer"
        );
        ensure!(
            displays
                .iter()
                .take(count as usize)
                .all(|d| d.version == crate::gpu_status::DISPLAY_VERSION),
            "NVAPI connected display version mismatch"
        );
        Ok(count)
    }
}
impl crate::gpu_pstate::PerformanceTransport for NativeNvApi<'_> {
    fn guard(&mut self, deadline: u64, locking: bool) -> Result<()> {
        self.performance_requires_ac = locking;
        self.performance_write_guard()?;
        ensure!(monotonic_ms() < deadline, "GPU P-state deadline expired");
        ensure!(
            !locking || crate::ac_connected()?,
            "AC required to lock a GPU P-state"
        );
        Ok(())
    }
    fn preflight(&mut self, dynamic: bool) -> Result<()> {
        self.performance_pointer(crate::gpu_pstate::SET_RATED_TDP)?;
        self.library.resolve(NvFunction::GetClockBoostLock)?;
        self.library.resolve(NvFunction::SetClockBoostLock)?;
        if dynamic {
            self.performance_pointer(crate::gpu_pstate::ENABLE_DYNAMIC_PSTATES)?;
        }
        Ok(())
    }
    fn set_rated_tdp(&mut self, payload: &mut AbiBuffer) -> Result<()> {
        payload.validate(12, 1)?;
        ensure!(
            payload.get(4) == 1 && [0, 3].contains(&payload.get(8)),
            "Invalid rated-TDP packet"
        );
        let p = self.performance_pointer(crate::gpu_pstate::SET_RATED_TDP)?;
        self.performance_write_guard()?;
        let f: Structured = unsafe { std::mem::transmute(p) };
        NvStatus::checked(unsafe { f(self.handle, payload.as_mut_ptr()) })
    }
    fn enable_dynamic(&mut self) -> Result<()> {
        let p = self.performance_pointer(crate::gpu_pstate::ENABLE_DYNAMIC_PSTATES)?;
        self.performance_write_guard()?;
        let f: Enable = unsafe { std::mem::transmute(p) };
        NvStatus::checked(unsafe { f(self.handle) })
    }
}
impl crate::gpu_sensors::SensorTransport for NativeNvApi<'_> {
    fn call(
        &mut self,
        function: crate::gpu_sensors::SensorFunction,
        payload: &mut crate::gpu_sensors::SensorPacket,
    ) -> Result<()> {
        payload.validate(function)?;
        let p = self.performance_pointer(function.id())?;
        if function == crate::gpu_sensors::SensorFunction::ThermalSettings {
            type Thermal = unsafe extern "C" fn(*mut c_void, u32, *mut c_void) -> i32;
            let f: Thermal = unsafe { std::mem::transmute(p) };
            NvStatus::checked(unsafe { f(self.handle, 15, payload.as_mut_ptr()) })
        } else {
            let f: Structured = unsafe { std::mem::transmute(p) };
            NvStatus::checked(unsafe { f(self.handle, payload.as_mut_ptr()) })
        }
    }
    fn tachometer(&mut self) -> Result<u32> {
        type Tach = unsafe extern "C" fn(*mut c_void, *mut u32) -> i32;
        let f: Tach = unsafe { std::mem::transmute(self.performance_pointer(0x5f608315)?) };
        let mut value = 0;
        NvStatus::checked(unsafe { f(self.handle, &mut value) })?;
        Ok(value)
    }
}
#[derive(Default)]
pub(crate) struct GpuSampler {
    native: Option<NativeNvApi<'static>>,
    identity: Option<(u32, String)>,
    thermal_mask: Option<u32>,
    nvml: crate::nvml_sensors::NvmlSampler,
}
impl GpuSampler {
    pub(crate) fn sample(
        &mut self,
        index: u32,
        name: &str,
    ) -> Result<crate::gpu_sensors::GpuSample> {
        let tick = monotonic_ms();
        let identity = (index, name.to_string());
        if self.identity.as_ref() != Some(&identity) {
            self.native = None;
            self.identity = None;
            self.thermal_mask = None;
            self.nvml = Default::default();
            self.native = Some(NativeNvApi::open(index, name, None, None, None)?);
            self.identity = Some(identity);
        }
        // Query power state first on the same session. A confirmed sleeping
        // GPU needs no temperature/P-state/NVML requests on this iteration.
        let status = self
            .native
            .as_mut()
            .expect("opened sensor session")
            .status();
        if status.state == crate::gpu_status::GpuState::PoweredOff {
            let mut sample = crate::gpu_sensors::GpuSample::new(index, name, tick);
            sample.status = Some(status);
            sample.completed_at_tick_ms = monotonic_ms();
            return Ok(sample);
        }
        let mut sample = crate::gpu_sensors::read(
            self.native.as_mut().expect("opened sensor session"),
            index,
            name,
            tick,
            &mut self.thermal_mask,
        );
        // Public NVML fallback is optional; errors remain visible and never erase
        // independently verified NVAPI measurements.
        if let Err(error) = self.nvml.supplement(&mut sample) {
            sample.errors.insert("nvml".into(), format!("{error:#}"));
        }
        match self
            .native
            .as_ref()
            .expect("opened sensor session")
            .current_pstate()
        {
            Ok(value) => sample.performance_state = Some(value),
            Err(error) => {
                sample
                    .errors
                    .insert("current_pstate".into(), format!("{error:#}"));
            }
        }
        sample.status = Some(status);
        sample.completed_at_tick_ms = monotonic_ms();
        Ok(sample)
    }
}
pub fn read_performance(index: u32, name: &str) -> Result<serde_json::Value> {
    let requested = monotonic_ms();
    let mut f = NativeNvApi::open(index, name, None, None, None)?;
    let mut result = crate::gpu_pstate::read(&mut f)?;
    match f.current_pstate() {
        Ok(value) => result["current_pstate"] = serde_json::json!(value),
        Err(error) => result["current_pstate_error"] = serde_json::json!(format!("{error:#}")),
    };
    result["requested_at_tick_ms"] = serde_json::json!(requested);
    result["completed_at_tick_ms"] = serde_json::json!(monotonic_ms());
    Ok(result)
}
pub(crate) fn set_performance(
    owner: &OwnerToken,
    action: &ExplicitAction,
    index: u32,
    name: &str,
    state: Option<u32>,
    deadline: u64,
) -> Result<serde_json::Value> {
    let mut f = NativeNvApi::open(index, name, Some(owner), Some(action), Some(deadline))?;
    crate::gpu_pstate::execute(&mut f, state, deadline)
}
fn validate_packet(function: NvFunction, payload: &AbiBuffer) -> Result<()> {
    let version = payload.version();
    match function {
        NvFunction::GetPstates20 => {
            ensure!((1..=3).contains(&version), "Unknown Pstate ABI");
            payload.validate(
                if version == 1 {
                    gpu::PSTATE_V1_BYTES
                } else {
                    gpu::PSTATE_V2_BYTES
                },
                version,
            )
        }
        NvFunction::SetPstates20 => payload.validate(gpu::PSTATE_V1_BYTES, 1),
        NvFunction::GetClockBoostMask => payload.validate(gpu::MASK_BYTES, 1),
        NvFunction::GetClockBoostRanges => payload.validate(gpu::RANGES_BYTES, 1),
        NvFunction::GetClockBoostTable | NvFunction::SetClockBoostTable => {
            ensure!((1..=2).contains(&version), "Unknown curve ABI");
            payload.validate(gpu::TABLE_BYTES, version)
        }
        NvFunction::GetClientClkVfPointsStatus => {
            ensure!((1..=2).contains(&version), "Unknown V/F point ABI");
            payload.validate(gpu::POINTS_BYTES, version)
        }
        NvFunction::GetClockBoostLock | NvFunction::SetClockBoostLock => {
            payload.validate(gpu::LOCK_BYTES, 2)
        }
        NvFunction::EnableOverclockedPstates => payload.validate(4, 0),
        _ => anyhow::bail!("Bootstrap function cannot use the structured GPU transport"),
    }
}
impl NvApiTransport for NativeNvApi<'_> {
    fn call(&mut self, function: NvFunction, payload: &mut AbiBuffer) -> Result<()> {
        validate_packet(function, payload)?;
        if function.is_write() {
            ensure!(
                self.owner.is_some() && self._action.is_some(),
                "Native NVAPI write requires exclusive owner and explicit action"
            );
            crate::native::reject_other_controllers()?;
            ensure!(
                self.deadline_tick_ms
                    .is_some_and(|deadline| monotonic_ms() < deadline),
                "GPU action expired before the next physical setter"
            );
        }
        let address = self.library.resolve(function)?;
        if function.is_write() {
            self._action
                .context("NVAPI explicit action missing")?
                .check_write_boundary()?;
            ensure!(
                self.deadline_tick_ms.is_some_and(|d| monotonic_ms() < d),
                "GPU action expired after resolving native setter"
            );
        }
        let status = if function == NvFunction::EnableOverclockedPstates {
            let function: Enable = unsafe { std::mem::transmute(address) };
            unsafe { function(self.handle) }
        } else {
            ensure!(function.structured(), "Wrong NVAPI function signature");
            let function: Structured = unsafe { std::mem::transmute(address) };
            unsafe { function(self.handle, payload.as_mut_ptr()) }
        };
        NvStatus::checked(status)
    }
}
pub fn read_overclock(index: u32, expected_name: &str) -> Result<serde_json::Value> {
    let mut transport = NativeNvApi::open(index, expected_name, None, None, None)?;
    let snapshot = gpu::read_snapshot(&mut transport)?;
    Ok(snapshot_json(&snapshot))
}
pub(crate) fn snapshot_json(snapshot: &gpu::GpuSnapshot) -> serde_json::Value {
    let core = snapshot.pstates.range(0);
    let memory = snapshot.pstates.range(4);
    serde_json::json!({"overclock":snapshot.overclock_info(),"core_range":{"min_mhz":core.min_mhz,"max_mhz":core.max_mhz,"driver_reported":core.driver_reported},"memory_range":{"min_mhz":memory.min_mhz,"max_mhz":memory.max_mhz,"driver_reported":memory.driver_reported},"private_curve_available":snapshot.curve.is_some(),"source":gpu::ABI_SOURCE,"read_only":true})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_function_shapes_and_packet_sizes_reject_cross_abi_without_loading_driver() {
        assert!(
            validate_packet(
                NvFunction::GetClockBoostTable,
                &AbiBuffer::new(gpu::TABLE_BYTES, 2)
            )
            .is_ok()
        );
        assert!(
            validate_packet(
                NvFunction::GetClockBoostTable,
                &AbiBuffer::new(gpu::POINTS_BYTES, 2)
            )
            .is_err()
        );
        assert!(
            validate_packet(NvFunction::EnableOverclockedPstates, &AbiBuffer::new(4, 0)).is_ok()
        );
        assert!(validate_packet(NvFunction::EnumPhysicalGpus, &AbiBuffer::new(4, 0)).is_err());
        assert!(
            validate_packet(
                NvFunction::SetPstates20,
                &AbiBuffer::new(gpu::PSTATE_V2_BYTES, 3)
            )
            .is_err()
        );
    }
}
