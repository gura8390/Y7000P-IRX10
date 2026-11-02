//! EnergyDrv notifications. Opening/registering is explicit; no hook or driver
//! installation is performed. The physical actor owns both handles until stop.
use crate::{
    domain::ExplicitAction,
    ownership::{OwnerToken, wide},
};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{ptr, rc::Rc};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::*,
    System::{
        IO::DeviceIoControl,
        Threading::{CreateEventW, ResetEvent, WaitForSingleObject},
    },
};
const BIND: u32 = 0x8310_20d8;
const VALUE: u32 = 0x8310_20cc;

pub fn event_packet(handle: u64) -> Result<[u8; 16]> {
    ensure!(
        handle != 0 && handle != u64::MAX,
        "Invalid driver event handle"
    );
    let mut packet = [0; 16];
    packet[..8].copy_from_slice(&handle.to_le_bytes());
    Ok(packet)
}
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub active: bool,
    pub raw_keys: Vec<u32>,
    pub owned_handles_closed: bool,
}
impl Snapshot {
    pub fn stopped() -> Self {
        Self {
            active: false,
            raw_keys: vec![],
            owned_handles_closed: true,
        }
    }
}
pub trait Port {
    fn guard(&mut self) -> Result<()>;
    fn event_value(&self) -> u64;
    fn bind(&mut self, packet: &[u8; 16]) -> Result<()>;
    fn clear_register(&mut self) -> Result<()>;
    fn take_signaled(&mut self) -> Result<Option<u32>>;
    fn close_file(&mut self) -> Result<()>;
    fn close_event(&mut self) -> Result<()>;
}
pub struct Session<P: Port> {
    port: P,
    attempted: bool,
    active: bool,
    closed: bool,
}
impl<P: Port> Session<P> {
    pub fn new(port: P) -> Self {
        Self {
            port,
            attempted: false,
            active: false,
            closed: false,
        }
    }
    pub fn start(&mut self) -> Result<Snapshot> {
        if self.active {
            return Ok(Snapshot {
                active: true,
                raw_keys: vec![],
                owned_handles_closed: false,
            });
        }
        ensure!(
            !self.closed && !self.attempted,
            "A partial/closed driver key registration must be stopped before retry"
        );
        self.port.guard()?;
        let packet = event_packet(self.port.event_value())?;
        self.attempted = true;
        self.port.bind(&packet)?;
        self.port.clear_register()?;
        self.active = true;
        Ok(Snapshot {
            active: true,
            raw_keys: vec![],
            owned_handles_closed: false,
        })
    }
    pub fn read(&mut self) -> Result<Snapshot> {
        ensure!(
            self.active && !self.closed,
            "Driver key registration is not active"
        );
        Ok(Snapshot {
            active: true,
            raw_keys: self.port.take_signaled()?.into_iter().collect(),
            owned_handles_closed: false,
        })
    }
    pub fn stop(&mut self) -> Result<Snapshot> {
        if !self.closed {
            self.port.close_file()?;
            self.port.close_event()?;
            self.closed = true;
            self.active = false;
        }
        Ok(Snapshot::stopped())
    }
}
pub(crate) struct NativePort {
    file: Option<HANDLE>,
    event: Option<HANDLE>,
    _owner: Rc<OwnerToken>,
    action: ExplicitAction,
}
impl NativePort {
    pub(crate) fn open(owner: Rc<OwnerToken>, action: ExplicitAction) -> Result<Self> {
        action.check_write_boundary()?;
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        ensure!(
            !event.is_null(),
            "Driver notification event creation failed: {}",
            std::io::Error::last_os_error()
        );
        let mut port = Self {
            file: None,
            event: Some(event),
            _owner: owner,
            action,
        };
        let file = unsafe {
            CreateFileW(
                wide("\\\\.\\EnergyDrv")?.as_ptr(),
                FILE_READ_DATA | FILE_WRITE_DATA,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        ensure!(
            file != INVALID_HANDLE_VALUE,
            "EnergyDrv key notification device is unavailable: {}",
            std::io::Error::last_os_error()
        );
        port.file = Some(file);
        Ok(port)
    }
    fn value(&self) -> Result<u32> {
        let input = 0u32;
        let (mut value, mut returned) = (0u32, 0u32);
        ensure!(
            unsafe {
                DeviceIoControl(
                    self.file
                        .ok_or_else(|| anyhow::anyhow!("Driver key file is closed"))?,
                    VALUE,
                    (&input as *const u32).cast(),
                    4,
                    (&mut value as *mut u32).cast(),
                    4,
                    &mut returned,
                    ptr::null_mut(),
                )
            } != 0,
            "EnergyDrv key register read failed: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            returned == 4,
            "EnergyDrv key register returned a truncated value"
        );
        Ok(value)
    }
}
impl Port for NativePort {
    fn guard(&mut self) -> Result<()> {
        self.action.check_write_boundary()
    }
    fn event_value(&self) -> u64 {
        self.event.map_or(0, |v| v as usize as u64)
    }
    fn bind(&mut self, packet: &[u8; 16]) -> Result<()> {
        self.guard()?;
        let mut returned = 0;
        ensure!(
            unsafe {
                DeviceIoControl(
                    self.file.unwrap(),
                    BIND,
                    packet.as_ptr().cast(),
                    16,
                    ptr::null_mut(),
                    0,
                    &mut returned,
                    ptr::null_mut(),
                )
            } != 0,
            "EnergyDrv event registration failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }
    fn clear_register(&mut self) -> Result<()> {
        self.value()?;
        Ok(())
    }
    fn take_signaled(&mut self) -> Result<Option<u32>> {
        let event = self
            .event
            .ok_or_else(|| anyhow::anyhow!("Driver key event is closed"))?;
        match unsafe { WaitForSingleObject(event, 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let value = self.value()?;
                ensure!(
                    unsafe { ResetEvent(event) } != 0,
                    "Driver key event reset failed"
                );
                Ok(Some(value))
            }
            _ => anyhow::bail!(
                "Driver key event poll failed: {}",
                std::io::Error::last_os_error()
            ),
        }
    }
    fn close_file(&mut self) -> Result<()> {
        if let Some(file) = self.file {
            ensure!(
                unsafe { CloseHandle(file) } != 0,
                "Driver key device close was not confirmed"
            );
            self.file = None;
        }
        Ok(())
    }
    fn close_event(&mut self) -> Result<()> {
        ensure!(self.file.is_none(), "Device must close before event");
        if let Some(event) = self.event {
            ensure!(
                unsafe { CloseHandle(event) } != 0,
                "Driver key event close was not confirmed"
            );
            self.event = None;
        }
        Ok(())
    }
}
impl Drop for NativePort {
    fn drop(&mut self) {
        if self.close_file().is_ok() {
            let _ = self.close_event();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        calls: Vec<&'static str>,
        expired: bool,
        signaled: Option<u32>,
        fail_clear: bool,
        fail_close: bool,
    }
    impl Port for Fake {
        fn guard(&mut self) -> Result<()> {
            ensure!(!self.expired, "expired");
            Ok(())
        }
        fn event_value(&self) -> u64 {
            0x1234_5678_90ab_cdef
        }
        fn bind(&mut self, p: &[u8; 16]) -> Result<()> {
            assert_eq!(p, event_packet(self.event_value()).unwrap().as_slice());
            self.calls.push("bind");
            Ok(())
        }
        fn clear_register(&mut self) -> Result<()> {
            self.calls.push("clear");
            ensure!(!self.fail_clear, "read failed after registration");
            Ok(())
        }
        fn take_signaled(&mut self) -> Result<Option<u32>> {
            if self.signaled.is_some() {
                self.calls.push("read");
            }
            Ok(self.signaled.take())
        }
        fn close_file(&mut self) -> Result<()> {
            self.calls.push("close-file");
            ensure!(!self.fail_close, "close unconfirmed");
            Ok(())
        }
        fn close_event(&mut self) -> Result<()> {
            self.calls.push("close-event");
            Ok(())
        }
    }
    fn fake() -> Fake {
        Fake {
            calls: vec![],
            expired: false,
            signaled: None,
            fail_clear: false,
            fail_close: false,
        }
    }
    #[test]
    fn packet_is_full_64bit_handle_and_zero_reserved_tail() {
        let p = event_packet(0x1234567890abcdef).unwrap();
        assert_eq!(&p[..8], &0x1234567890abcdefu64.to_le_bytes());
        assert_eq!(&p[8..], &[0; 8]);
        assert!(event_packet(0).is_err());
    }
    #[test]
    fn explicit_registration_clears_once_and_idle_poll_does_not_issue_ioctl() {
        let mut s = Session::new(fake());
        assert!(s.port.calls.is_empty());
        s.start().unwrap();
        s.start().unwrap();
        assert_eq!(s.port.calls, vec!["bind", "clear"]);
        assert!(s.read().unwrap().raw_keys.is_empty());
        assert_eq!(s.port.calls, vec!["bind", "clear"]);
        s.port.signaled = Some(0xf1234567);
        assert_eq!(s.read().unwrap().raw_keys, vec![0xf1234567]);
        assert_eq!(s.port.calls, vec!["bind", "clear", "read"]);
        assert!(s.stop().unwrap().owned_handles_closed);
        assert_eq!(&s.port.calls[3..], &["close-file", "close-event"]);
        s.stop().unwrap();
        assert_eq!(s.port.calls.len(), 5);
    }
    #[test]
    fn registration_expiry_and_partial_failure_never_release_event_before_device() {
        let mut f = fake();
        f.expired = true;
        let mut s = Session::new(f);
        assert!(s.start().is_err());
        assert!(s.port.calls.is_empty());
        s.port.expired = false;
        s.port.fail_clear = true;
        assert!(s.start().is_err());
        assert!(s.start().is_err());
        s.port.fail_close = true;
        assert!(s.stop().is_err());
        assert!(!s.port.calls.contains(&"close-event"));
        s.port.fail_close = false;
        assert!(s.stop().unwrap().owned_handles_closed);
    }
}
