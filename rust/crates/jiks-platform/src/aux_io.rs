//! Small synchronous, read-query handles owned by the slow auxiliary actor.
//! Pending DeviceIoControl calls retain their buffers/handle on that thread.
use crate::ownership::wide;
use anyhow::{Result, ensure};
use std::{
    mem::{size_of, zeroed},
    ptr,
};
use windows_sys::Win32::{
    Devices::DeviceAndDriverInstallation::*,
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::*,
    System::IO::DeviceIoControl,
};
pub(crate) struct Device(HANDLE);
impl Device {
    pub(crate) fn open(path: &str, access: u32) -> Result<Self> {
        let handle = unsafe {
            CreateFileW(
                wide(path)?.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        ensure!(
            handle != INVALID_HANDLE_VALUE,
            "Read-query device open failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(handle))
    }
    pub(crate) fn query(&self, code: u32, input: &[u8], capacity: usize) -> Result<Vec<u8>> {
        ensure!(
            input.len() <= 65536 && (1..=65536).contains(&capacity),
            "Read IOCTL buffer bound exceeded"
        );
        let mut output = vec![0u8; capacity];
        let mut returned = 0;
        ensure!(
            unsafe {
                DeviceIoControl(
                    self.0,
                    code,
                    input.as_ptr().cast(),
                    input.len() as u32,
                    output.as_mut_ptr().cast(),
                    capacity as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            } != 0,
            "Read-query IOCTL {code:#x} failed: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            returned as usize <= capacity,
            "Read-query IOCTL returned an oversized response"
        );
        output.truncate(returned as usize);
        Ok(output)
    }
}
impl Drop for Device {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}
pub(crate) fn interfaces(guid: &windows_sys::core::GUID) -> Result<Vec<String>> {
    let info = unsafe {
        SetupDiGetClassDevsW(
            guid,
            ptr::null(),
            ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    ensure!(info != -1, "Read-query interface inventory failed");
    struct Info(HDEVINFO);
    impl Drop for Info {
        fn drop(&mut self) {
            unsafe { SetupDiDestroyDeviceInfoList(self.0) };
        }
    }
    let _guard = Info(info);
    let mut paths = vec![];
    for index in 0..64 {
        let mut data: SP_DEVICE_INTERFACE_DATA = unsafe { zeroed() };
        data.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        if unsafe { SetupDiEnumDeviceInterfaces(info, ptr::null(), guid, index, &mut data) } == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(259) {
                return Ok(paths);
            }
            return Err(error.into());
        }
        let mut size = 0;
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                info,
                &data,
                ptr::null_mut(),
                0,
                &mut size,
                ptr::null_mut(),
            )
        };
        ensure!(
            (8..=65536).contains(&size),
            "Auxiliary interface packet size invalid"
        );
        let mut words = vec![0u32; (size as usize).div_ceil(4)];
        words[0] = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        let capacity = (words.len() * 4) as u32;
        ensure!(
            unsafe {
                SetupDiGetDeviceInterfaceDetailW(
                    info,
                    &data,
                    words.as_mut_ptr().cast(),
                    capacity,
                    &mut size,
                    ptr::null_mut(),
                )
            } != 0
                && size <= capacity
                && size >= 6
                && size % 2 == 0,
            "Auxiliary interface packet truncated"
        );
        let units = unsafe {
            std::slice::from_raw_parts(words.as_ptr().cast::<u16>().add(2), (size as usize - 4) / 2)
        };
        let end = units
            .iter()
            .position(|v| *v == 0)
            .ok_or_else(|| anyhow::anyhow!("Unterminated auxiliary device path"))?;
        paths.push(String::from_utf16(&units[..end])?);
    }
    anyhow::bail!("Auxiliary interface inventory exceeded its bound")
}
pub(crate) fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(|| anyhow::anyhow!("Truncated native query response"))?
            .try_into()?,
    ))
}
