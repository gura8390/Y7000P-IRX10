//! Explicit-only NVML sensor fallback. The official NVIDIA/go-nvml nvml.h
//! defines these public C signatures/units. No GPU library is opened by Default.
use crate::{
    gpu_sensors::{GpuSample, insert_memory},
    ownership::wide,
};
use anyhow::{Context, Result, ensure};
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
type Init = unsafe extern "C" fn() -> u32;
type Count = unsafe extern "C" fn(*mut u32) -> u32;
type ByIndex = unsafe extern "C" fn(u32, *mut *mut c_void) -> u32;
type Name = unsafe extern "C" fn(*mut c_void, *mut i8, u32) -> u32;
type Scalar = unsafe extern "C" fn(*mut c_void, *mut u32) -> u32;
type Domain = unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> u32;
type MemoryFn = unsafe extern "C" fn(*mut c_void, *mut Memory) -> u32;
type UtilFn = unsafe extern "C" fn(*mut c_void, *mut Utilization) -> u32;
#[repr(C)]
#[derive(Default)]
struct Memory {
    total: u64,
    free: u64,
    used: u64,
}
#[repr(C)]
#[derive(Default)]
struct Utilization {
    gpu: u32,
    memory: u32,
}
fn checked(status: u32) -> Result<()> {
    ensure!(status == 0, "NVML sensor status {status}");
    Ok(())
}
struct Session {
    module: HMODULE,
    shutdown: Option<Init>,
    handle: *mut c_void,
    _thread: PhantomData<Rc<()>>,
}
impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            if let Some(shutdown) = self.shutdown {
                shutdown();
            }
            FreeLibrary(self.module);
        }
    }
}
impl Session {
    fn symbol(&self, name: &[u8]) -> Result<unsafe extern "system" fn() -> isize> {
        unsafe { GetProcAddress(self.module, name.as_ptr()) }
            .context("NVML sensor function is absent")
    }
    fn open(index: u32, expected_name: &str) -> Result<Self> {
        // NVAPI/NVML enumeration indices are not interchangeable on multi-GPU
        // systems. This reviewed local path permits exactly one NVIDIA device.
        ensure!(
            index == 0,
            "NVML sensor mapping requires the reviewed single-NVIDIA index"
        );
        let name = wide("nvml.dll")?;
        let module =
            unsafe { LoadLibraryExW(name.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        ensure!(!module.is_null(), "System NVML library unavailable");
        let mut s = Self {
            module,
            shutdown: None,
            handle: ptr::null_mut(),
            _thread: PhantomData,
        };
        let init: Init = unsafe { std::mem::transmute(s.symbol(b"nvmlInit_v2\0")?) };
        let shutdown: Init = unsafe { std::mem::transmute(s.symbol(b"nvmlShutdown\0")?) };
        checked(unsafe { init() })?;
        s.shutdown = Some(shutdown);
        let count: Count = unsafe { std::mem::transmute(s.symbol(b"nvmlDeviceGetCount_v2\0")?) };
        let mut n = 0;
        checked(unsafe { count(&mut n) })?;
        ensure!(
            n == 1,
            "NVML/NVAPI physical identity is ambiguous; need PCI identity for multiple NVIDIA GPUs"
        );
        let get: ByIndex =
            unsafe { std::mem::transmute(s.symbol(b"nvmlDeviceGetHandleByIndex_v2\0")?) };
        checked(unsafe { get(0, &mut s.handle) })?;
        ensure!(!s.handle.is_null(), "Null NVML sensor handle");
        let get_name: Name = unsafe { std::mem::transmute(s.symbol(b"nvmlDeviceGetName\0")?) };
        let mut bytes = [0i8; 128];
        checked(unsafe { get_name(s.handle, bytes.as_mut_ptr(), 128) })?;
        ensure!(bytes.contains(&0), "Unterminated NVML sensor name");
        let actual = unsafe { CStr::from_ptr(bytes.as_ptr()) }.to_string_lossy();
        ensure!(
            same_name(&actual, expected_name),
            "NVML physical GPU identity differs from confirmed selection"
        );
        Ok(s)
    }
    fn scalar(&self, name: &[u8]) -> Result<u32> {
        let f: Scalar = unsafe { std::mem::transmute(self.symbol(name)?) };
        let mut v = 0;
        checked(unsafe { f(self.handle, &mut v) })?;
        Ok(v)
    }
    fn domain(&self, name: &[u8], domain: u32) -> Result<u32> {
        let f: Domain = unsafe { std::mem::transmute(self.symbol(name)?) };
        let mut v = 0;
        checked(unsafe { f(self.handle, domain, &mut v) })?;
        Ok(v)
    }
    fn memory(&self) -> Result<Memory> {
        let f: MemoryFn =
            unsafe { std::mem::transmute(self.symbol(b"nvmlDeviceGetMemoryInfo\0")?) };
        let mut v = Memory::default();
        checked(unsafe { f(self.handle, &mut v) })?;
        ensure!(
            v.total > 0 && v.free <= v.total && v.used <= v.total,
            "Invalid NVML memory counters"
        );
        Ok(v)
    }
    fn utilization(&self) -> Result<Utilization> {
        let f: UtilFn =
            unsafe { std::mem::transmute(self.symbol(b"nvmlDeviceGetUtilizationRates\0")?) };
        let mut v = Utilization::default();
        checked(unsafe { f(self.handle, &mut v) })?;
        ensure!(v.gpu <= 100 && v.memory <= 100, "Invalid NVML utilization");
        Ok(v)
    }
}
fn same_name(a: &str, b: &str) -> bool {
    a.strip_prefix("NVIDIA ").unwrap_or(a) == b.strip_prefix("NVIDIA ").unwrap_or(b)
}
#[derive(Default)]
pub(crate) struct NvmlSampler {
    session: Option<Session>,
    identity: Option<(u32, String)>,
}
impl NvmlSampler {
    pub(crate) fn supplement(&mut self, sample: &mut GpuSample) -> Result<()> {
        if [
            "GpuPower",
            "GpuCoreTemperature",
            "GpuFrequency",
            "GpuMemoryFrequency",
            "GpuVideoFrequency",
            "GpuVramTotal",
            "GpuVramUsed",
            "GpuUtilization",
        ]
        .iter()
        .all(|key| sample.values.get(*key).and_then(|v| v.value).is_some())
        {
            return Ok(());
        }
        let identity = (sample.index, sample.name.clone());
        if self.identity.as_ref() != Some(&identity) {
            self.session = None;
            self.identity = None;
            self.session = Some(Session::open(sample.index, &sample.name)?);
            self.identity = Some(identity);
        }
        let s = self.session.as_ref().expect("opened NVML sensor session");
        fn missing(sample: &GpuSample, key: &str) -> bool {
            sample.values.get(key).and_then(|v| v.value).is_none()
        }
        fn add(sample: &mut GpuSample, key: &str, unit: &str, value: Result<f64>) {
            match value {
                Ok(v) => {
                    if let Err(e) = sample.insert(key, v, unit, "NVML native sensor getter") {
                        sample
                            .errors
                            .insert(format!("nvml.{key}"), format!("{e:#}"));
                    }
                }
                Err(e) => {
                    sample
                        .errors
                        .insert(format!("nvml.{key}"), format!("{e:#}"));
                }
            }
        }
        // Power is board/device power, milliwatts, not percent of TDP.
        if missing(sample, "GpuPower") {
            add(
                sample,
                "GpuPower",
                "W",
                s.scalar(b"nvmlDeviceGetPowerUsage\0")
                    .map(|v| v as f64 / 1000.),
            );
        }
        if missing(sample, "GpuCoreTemperature") {
            add(
                sample,
                "GpuCoreTemperature",
                "°C",
                s.domain(b"nvmlDeviceGetTemperature\0", 0).and_then(|v| {
                    ensure!(v > 0 && v < 150, "Invalid NVML temperature");
                    Ok(v as f64)
                }),
            );
        }
        for (key, domain) in [
            ("GpuFrequency", 0),
            ("GpuMemoryFrequency", 2),
            ("GpuVideoFrequency", 3),
        ] {
            if missing(sample, key) {
                add(
                    sample,
                    key,
                    "MHz",
                    s.domain(b"nvmlDeviceGetClockInfo\0", domain)
                        .map(|v| v as f64),
                );
            }
        }
        if missing(sample, "GpuVramTotal") || missing(sample, "GpuVramUsed") {
            match s.memory() {
                Ok(v) => insert_memory(sample, v.total, v.used, "NVML memory bytes")?,
                Err(e) => {
                    sample.errors.insert("nvml.memory".into(), format!("{e:#}"));
                }
            }
        }
        if missing(sample, "GpuUtilization") {
            match s.utilization() {
                Ok(v) => add(sample, "GpuUtilization", "%", Ok(v.gpu as f64)),
                Err(e) => {
                    sample
                        .errors
                        .insert("nvml.utilization".into(), format!("{e:#}"));
                }
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn official_memory_and_utilization_layouts_and_identity() {
        assert_eq!(std::mem::size_of::<Memory>(), 24);
        assert_eq!(std::mem::size_of::<Utilization>(), 8);
        assert!(same_name(
            "NVIDIA GeForce RTX 5060 Laptop GPU",
            "GeForce RTX 5060 Laptop GPU"
        ));
        assert!(!same_name(
            "GeForce RTX 5060 Laptop GPU",
            "GeForce RTX 5070 Laptop GPU"
        ));
        let s = NvmlSampler::default();
        assert!(s.session.is_none());
        assert!(s.identity.is_none());
    }
}
