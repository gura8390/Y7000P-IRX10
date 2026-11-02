//! Native NVIDIA inventory. Loading/initialization is explicit; no GPU setter exists here.
use crate::ownership::wide;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    ffi::{CStr, c_void},
    ptr,
};
use windows_sys::Win32::{
    Foundation::FreeLibrary,
    System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
};

type Init = unsafe extern "C" fn() -> u32;
type Count = unsafe extern "C" fn(*mut u32) -> u32;
type ByIndex = unsafe extern "C" fn(u32, *mut *mut c_void) -> u32;
type Name = unsafe extern "C" fn(*mut c_void, *mut i8, u32) -> u32;
#[derive(Debug, Clone, Serialize)]
pub struct NvidiaDevice {
    pub index: u32,
    pub name: String,
    pub source: &'static str,
}
pub fn inventory() -> Result<Vec<NvidiaDevice>> {
    let name = wide("nvml.dll")?;
    let library =
        unsafe { LoadLibraryExW(name.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    ensure!(!library.is_null(), "NVIDIA NVML system library unavailable");
    struct Library(windows_sys::Win32::Foundation::HMODULE);
    impl Drop for Library {
        fn drop(&mut self) {
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }
    let _library = Library(library);
    unsafe fn symbol(
        library: windows_sys::Win32::Foundation::HMODULE,
        name: &[u8],
    ) -> Result<unsafe extern "system" fn() -> isize> {
        unsafe { GetProcAddress(library, name.as_ptr()) }
            .context("Required NVML capability is unavailable")
    }
    let init: Init = unsafe { std::mem::transmute(symbol(library, b"nvmlInit_v2\0")?) };
    let shutdown: Init = unsafe { std::mem::transmute(symbol(library, b"nvmlShutdown\0")?) };
    let count: Count = unsafe { std::mem::transmute(symbol(library, b"nvmlDeviceGetCount_v2\0")?) };
    let by_index: ByIndex =
        unsafe { std::mem::transmute(symbol(library, b"nvmlDeviceGetHandleByIndex_v2\0")?) };
    let get_name: Name = unsafe { std::mem::transmute(symbol(library, b"nvmlDeviceGetName\0")?) };
    ensure!(unsafe { init() } == 0, "NVML initialization failed");
    struct Session(Init);
    impl Drop for Session {
        fn drop(&mut self) {
            unsafe {
                (self.0)();
            }
        }
    }
    let _session = Session(shutdown);
    let mut length = 0;
    ensure!(
        unsafe { count(&mut length) } == 0 && length <= 64,
        "NVML device count failed"
    );
    let mut result = Vec::new();
    for index in 0..length {
        let mut handle = ptr::null_mut();
        ensure!(
            unsafe { by_index(index, &mut handle) } == 0 && !handle.is_null(),
            "NVML device handle unavailable"
        );
        let mut bytes = [0i8; 128];
        ensure!(
            unsafe { get_name(handle, bytes.as_mut_ptr(), bytes.len() as u32) } == 0,
            "NVML GPU name unavailable"
        );
        ensure!(bytes.contains(&0), "NVML returned an unterminated name");
        result.push(NvidiaDevice {
            index,
            name: unsafe { CStr::from_ptr(bytes.as_ptr()) }
                .to_string_lossy()
                .into_owned(),
            source: "NVML native read-only",
        });
    }
    Ok(result)
}
