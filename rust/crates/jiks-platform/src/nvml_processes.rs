//! Explicit NVML process reads can wake a dGPU; never startup/idle discovery.
//! ABI: NVIDIA/go-nvml pkg/nvml/nvml.h, nvmlProcessInfo_t (24 bytes).
use crate::ownership::wide;
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeSet,
    ffi::{CStr, c_void},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{FreeLibrary, HMODULE},
    System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
};
type Init = unsafe extern "C" fn() -> u32;
type Count = unsafe extern "C" fn(*mut u32) -> u32;
type ByIndex = unsafe extern "C" fn(u32, *mut *mut c_void) -> u32;
type Name = unsafe extern "C" fn(*mut c_void, *mut i8, u32) -> u32;
struct Library {
    module: HMODULE,
    shutdown: Option<Init>,
}
impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            if let Some(shutdown) = self.shutdown {
                shutdown();
            }
            FreeLibrary(self.module);
        }
    }
}
impl Library {
    fn open() -> Result<Self> {
        let name = wide("nvml.dll")?;
        let module =
            unsafe { LoadLibraryExW(name.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        ensure!(!module.is_null(), "NVIDIA NVML system library unavailable");
        let mut lib = Self {
            module,
            shutdown: None,
        };
        let init: Init = unsafe { std::mem::transmute(lib.symbol(b"nvmlInit_v2\0")?) };
        let shutdown: Init = unsafe { std::mem::transmute(lib.symbol(b"nvmlShutdown\0")?) };
        let status = unsafe { init() };
        ensure!(status == 0, "NVML initialization failed ({status})");
        lib.shutdown = Some(shutdown);
        Ok(lib)
    }
    fn symbol(&self, name: &[u8]) -> Result<unsafe extern "system" fn() -> isize> {
        unsafe { GetProcAddress(self.module, name.as_ptr()) }
            .context("Required NVML capability is unavailable")
    }
    fn count(&self) -> Result<u32> {
        let get: Count = unsafe { std::mem::transmute(self.symbol(b"nvmlDeviceGetCount_v2\0")?) };
        let mut count = 0;
        let status = unsafe { get(&mut count) };
        ensure!(
            status == 0 && count <= 64,
            "NVML device count failed ({status})"
        );
        Ok(count)
    }
    fn device(&self, index: u32) -> Result<(*mut c_void, String)> {
        let get: ByIndex =
            unsafe { std::mem::transmute(self.symbol(b"nvmlDeviceGetHandleByIndex_v2\0")?) };
        let name: Name = unsafe { std::mem::transmute(self.symbol(b"nvmlDeviceGetName\0")?) };
        let mut handle = ptr::null_mut();
        let status = unsafe { get(index, &mut handle) };
        ensure!(
            status == 0 && !handle.is_null(),
            "NVML device handle unavailable ({status})"
        );
        let mut bytes = [0i8; 128];
        let status = unsafe { name(handle, bytes.as_mut_ptr(), bytes.len() as u32) };
        ensure!(
            status == 0 && bytes.contains(&0),
            "NVML device name unavailable/unterminated ({status})"
        );
        Ok((
            handle,
            unsafe { CStr::from_ptr(bytes.as_ptr()) }
                .to_string_lossy()
                .into_owned(),
        ))
    }
}
// Both *_v3 functions take nvmlProcessInfo_t, the public 24-byte v2 layout,
// not nvmlProcessDetail_v1_t's 32-byte protected-memory ABI.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
struct ProcessInfo {
    pid: u32,
    used_gpu_memory: u64,
    gpu_instance_id: u32,
    compute_instance_id: u32,
}
type Processes = unsafe extern "C" fn(*mut c_void, *mut u32, *mut ProcessInfo) -> u32;
fn enumerate_processes(
    mut call: impl FnMut(*mut u32, *mut ProcessInfo) -> u32,
) -> Result<Vec<u32>> {
    let mut capacity = 0;
    let status = call(&mut capacity, ptr::null_mut());
    ensure!(
        status == 0 || status == 7,
        "NVML process enumeration failed ({status})"
    );
    if status == 0 && capacity == 0 {
        return Ok(vec![]);
    }
    for _ in 0..3 {
        ensure!(
            capacity > 0 && capacity <= 4096,
            "NVML process count outside bounds"
        );
        let mut buffer = vec![ProcessInfo::default(); capacity as usize];
        let mut count = capacity;
        let status = call(&mut count, buffer.as_mut_ptr());
        if status == 7 {
            ensure!(
                count > capacity,
                "NVML insufficient-size response did not grow"
            );
            capacity = count;
            continue;
        }
        ensure!(
            status == 0 && count <= capacity,
            "NVML process list failed/inconsistent ({status})"
        );
        return Ok(buffer
            .into_iter()
            .take(count as usize)
            .map(|p| p.pid)
            .filter(|p| *p > 4)
            .collect());
    }
    anyhow::bail!("NVML process list kept changing beyond bounded allocation retries")
}
pub fn read(index: u32, expected_name: &str) -> Result<serde_json::Value> {
    let lib = Library::open()?;
    ensure!(
        lib.count()? == 1 && index == 0,
        "Cross-NVAPI/NVML process identity requires the single local NVIDIA adapter; multiple-GPU mapping needs PCI identity"
    );
    let (handle, name) = lib.device(index)?;
    let canonical = |value: &str| {
        value
            .strip_prefix("NVIDIA ")
            .unwrap_or(value)
            .to_ascii_lowercase()
    };
    ensure!(
        canonical(&name) == canonical(expected_name),
        "Selected GPU name changed across NVAPI/NVML inventory"
    );
    let mut pids = BTreeSet::new();
    let mut errors = Vec::new();
    for symbol in [
        b"nvmlDeviceGetGraphicsRunningProcesses_v3\0".as_slice(),
        b"nvmlDeviceGetComputeRunningProcesses_v3\0".as_slice(),
    ] {
        let result = (|| -> Result<Vec<u32>> {
            let get: Processes = unsafe { std::mem::transmute(lib.symbol(symbol)?) };
            enumerate_processes(|count, buffer| unsafe { get(handle, count, buffer) })
        })();
        match result {
            Ok(values) => pids.extend(values),
            Err(e) => errors.push(format!(
                "{}: {e:#}",
                String::from_utf8_lossy(&symbol[..symbol.len() - 1])
            )),
        }
    }
    ensure!(
        errors.len() < 2,
        "Both NVML graphics/compute process APIs failed: {:?}",
        errors
    );
    let mut processes = Vec::new();
    let mut inaccessible = Vec::new();
    for pid in pids {
        match crate::gpu_management::process_metadata(pid) {
            Ok(value) => processes.push(value),
            Err(_) => inaccessible.push(pid),
        }
    }
    Ok(
        serde_json::json!({"index":index,"name":name,"processes":processes,"inaccessible_pids":inaccessible,"enumeration_errors":errors,"complete":errors.is_empty()&&inaccessible.is_empty(),"source":"Explicit NVML graphics+compute process lists; WDDM per-process VRAM not available"}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_process_packet_layout_matches_nvml_v3_function_argument() {
        assert_eq!(std::mem::size_of::<ProcessInfo>(), 24);
        assert_eq!(std::mem::offset_of!(ProcessInfo, used_gpu_memory), 8);
        assert_eq!(std::mem::offset_of!(ProcessInfo, gpu_instance_id), 16);
    }
    #[test]
    fn list_growth_is_bounded_and_error_never_becomes_empty_success() {
        let mut call_count = 0;
        let result = enumerate_processes(|count, buffer| {
            call_count += 1;
            unsafe {
                if buffer.is_null() {
                    *count = 1;
                    7
                } else if call_count == 2 {
                    *count = 2;
                    7
                } else {
                    *count = 2;
                    (*buffer).pid = 10;
                    (*buffer.add(1)).pid = 11;
                    0
                }
            }
        });
        assert_eq!(result.unwrap(), vec![10, 11]);
        assert_eq!(call_count, 3);
        assert!(enumerate_processes(|_, _| 3).is_err());
        assert!(
            enumerate_processes(|count, _| {
                unsafe { *count = 4097 };
                7
            })
            .is_err()
        );
    }
}
