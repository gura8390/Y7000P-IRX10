//! Lenovo EnergyDrv protocol. Pure encoders are testable without opening a device.
use crate::{
    domain::ExplicitAction,
    ownership::{OwnerToken, wide},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{mem::size_of, ptr};
use windows_sys::Win32::{
    Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::*,
    System::IO::DeviceIoControl,
};
const SETTINGS: u32 = 0x831020e8;
const CHARGE: u32 = 0x831020f8;
const NIGHT: u32 = 0x83102150;
const KEYBOARD: u32 = 0x83102144;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChargeMode {
    Normal,
    Rapid,
    Conservation,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlwaysOnUsb {
    Off,
    Sleeping,
    Always,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EnergyAction {
    Battery(ChargeMode),
    NightCharge(bool),
    AlwaysOnUsb(AlwaysOnUsb),
    FnLock(bool),
    KeyboardLevel(u32),
    OneLevelWhite(bool),
}
#[derive(Debug, Clone, Serialize)]
pub struct EnergyStatus {
    pub battery_mode: ChargeMode,
    pub night_charge: Option<bool>,
    pub fn_lock: bool,
    pub always_on_usb: AlwaysOnUsb,
    pub keyboard_maximum: Option<u32>,
    pub keyboard_level: Option<u32>,
    pub one_level_white: Option<bool>,
}
fn charge_mode(raw: u32) -> ChargeMode {
    if raw & 0x20 != 0 {
        ChargeMode::Conservation
    } else if raw & 4 != 0 {
        ChargeMode::Rapid
    } else {
        ChargeMode::Normal
    }
}
fn usb_mode(raw: u32) -> AlwaysOnUsb {
    let raw = raw.swap_bytes();
    if raw & (1 << 31) == 0 {
        AlwaysOnUsb::Off
    } else if raw & (1 << 23) != 0 {
        AlwaysOnUsb::Always
    } else {
        AlwaysOnUsb::Sleeping
    }
}
fn commands(action: &EnergyAction, keyboard_capability: Option<u32>) -> Result<(u32, Vec<u32>)> {
    Ok(match action {
        EnergyAction::Battery(ChargeMode::Normal) => (CHARGE, vec![5, 8]),
        EnergyAction::Battery(ChargeMode::Rapid) => (CHARGE, vec![5, 7]),
        EnergyAction::Battery(ChargeMode::Conservation) => (CHARGE, vec![8, 3]),
        EnergyAction::NightCharge(on) => (NIGHT, vec![if *on { 0x80000012 } else { 0x12 }]),
        EnergyAction::AlwaysOnUsb(AlwaysOnUsb::Off) => (SETTINGS, vec![0xb, 0x12]),
        EnergyAction::AlwaysOnUsb(AlwaysOnUsb::Sleeping) => (SETTINGS, vec![0xa, 0x12]),
        EnergyAction::AlwaysOnUsb(AlwaysOnUsb::Always) => (SETTINGS, vec![0xa, 0x13]),
        // The verified 83NN is a current Legion generation: logical FnLock is inverted by its firmware.
        EnergyAction::FnLock(on) => (SETTINGS, vec![if *on { 0xf } else { 0xe }]),
        EnergyAction::OneLevelWhite(on) => (SETTINGS, vec![if *on { 0x8 } else { 0x9 }]),
        EnergyAction::KeyboardLevel(level) => {
            let capability = keyboard_capability
                .ok_or_else(|| anyhow::anyhow!("Keyboard capability unavailable"))?;
            ensure!(
                capability & 1 == 1 && capability >> 1 >= 1 && *level <= capability >> 1,
                "Keyboard level outside advertised capability"
            );
            (
                KEYBOARD,
                vec![((capability & !1) << 3) | (*level << 16) | 3],
            )
        }
    })
}
struct Driver(HANDLE);
impl Drop for Driver {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
impl Driver {
    fn open() -> Result<Self> {
        let path = wide("\\\\.\\EnergyDrv")?;
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        ensure!(
            handle != INVALID_HANDLE_VALUE,
            "EnergyDrv unavailable: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(handle))
    }
    fn invoke(&self, code: u32, input: u32) -> Result<u32> {
        let mut output = 0u32;
        let mut returned = 0u32;
        ensure!(
            unsafe {
                DeviceIoControl(
                    self.0,
                    code,
                    (&input as *const u32).cast(),
                    size_of::<u32>() as u32,
                    (&mut output as *mut u32).cast(),
                    size_of::<u32>() as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            } != 0,
            "EnergyDrv IOCTL failed: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            returned == 4,
            "EnergyDrv did not return a complete uint32 result"
        );
        Ok(output)
    }
    fn status(&self) -> Result<EnergyStatus> {
        let battery = self.invoke(CHARGE, 0xff)?;
        let settings = self.invoke(SETTINGS, 2)?;
        let night = self.invoke(NIGHT, 0x11).ok().and_then(night_charge);
        let capability = self.invoke(KEYBOARD, 1).ok().filter(|c| c & 1 == 1);
        let keyboard_level = capability
            .and_then(|cap| self.invoke(KEYBOARD, ((cap & !1) << 3) | 2).ok())
            .filter(|s| s & 1 == 1)
            .map(|s| (s & 0xf) >> 1);
        Ok(EnergyStatus {
            battery_mode: charge_mode(battery),
            night_charge: night,
            fn_lock: fn_lock(settings),
            always_on_usb: usb_mode(settings),
            keyboard_maximum: capability.map(|c| c >> 1),
            keyboard_level,
            one_level_white: one_level_white(settings),
        })
    }
}
pub fn read_status() -> Result<EnergyStatus> {
    Driver::open()?.status()
}
fn fn_lock(settings: u32) -> bool {
    settings & (1 << 10) == 0
}
fn night_charge(raw: u32) -> Option<bool> {
    (raw & 1 == 1).then_some(raw & (1 << 4) != 0)
}
fn one_level_white(settings: u32) -> Option<bool> {
    (settings & (1 << 4) != 0).then_some(settings & (1 << 5) != 0)
}
pub(crate) fn write(
    _owner: &OwnerToken,
    _permit: &ExplicitAction,
    action: &EnergyAction,
) -> Result<EnergyStatus> {
    let driver = Driver::open()?;
    if matches!(action, EnergyAction::OneLevelWhite(_)) {
        ensure!(
            one_level_white(driver.invoke(SETTINGS, 2)?).is_some(),
            "One-level white backlight is not advertised"
        );
    }
    let capability = if matches!(action, EnergyAction::KeyboardLevel(_)) {
        Some(driver.invoke(KEYBOARD, 1)?)
    } else {
        None
    };
    let (code, commands) = commands(action, capability)?;
    for input in commands {
        _permit.check_write_boundary()?;
        driver.invoke(code, input)?;
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let observed = driver.status()?;
    let verified = match action {
        EnergyAction::Battery(mode) => observed.battery_mode == *mode,
        EnergyAction::NightCharge(v) => observed.night_charge == Some(*v),
        EnergyAction::AlwaysOnUsb(v) => observed.always_on_usb == *v,
        EnergyAction::FnLock(v) => observed.fn_lock == *v,
        EnergyAction::KeyboardLevel(v) => observed.keyboard_level == Some(*v),
        EnergyAction::OneLevelWhite(v) => observed.one_level_white == Some(*v),
    };
    ensure!(
        verified,
        "EnergyDrv state did not verify; no success was assumed"
    );
    Ok(observed)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_encoders_match_existing_driver_contract() {
        assert_eq!(
            commands(&EnergyAction::Battery(ChargeMode::Conservation), None).unwrap(),
            (CHARGE, vec![8, 3])
        );
        assert_eq!(
            commands(&EnergyAction::Battery(ChargeMode::Normal), None).unwrap(),
            (CHARGE, vec![5, 8])
        );
        assert_eq!(
            commands(&EnergyAction::NightCharge(true), None).unwrap(),
            (NIGHT, vec![0x80000012])
        );
        assert_eq!(
            commands(&EnergyAction::FnLock(true), None).unwrap(),
            (SETTINGS, vec![0xf])
        );
        assert_eq!(
            commands(&EnergyAction::AlwaysOnUsb(AlwaysOnUsb::Always), None).unwrap(),
            (SETTINGS, vec![0xa, 0x13])
        );
        assert!(commands(&EnergyAction::KeyboardLevel(3), Some(5)).is_err());
    }
    #[test]
    fn state_bits_are_decoded_without_opening_driver() {
        assert_eq!(charge_mode(0x24), ChargeMode::Conservation);
        assert_eq!(charge_mode(4), ChargeMode::Rapid);
        assert_eq!(charge_mode(0), ChargeMode::Normal);
        assert_eq!(usb_mode(0), AlwaysOnUsb::Off);
        assert_eq!(
            usb_mode((0x80000000u32).swap_bytes()),
            AlwaysOnUsb::Sleeping
        );
        assert_eq!(usb_mode((0x80800000u32).swap_bytes()), AlwaysOnUsb::Always);
    }
    #[test]
    fn logical_truth_table_preserves_83nn_fn_inversion_night_and_one_level_white() {
        for on in [false, true] {
            assert_eq!(
                commands(&EnergyAction::FnLock(on), None).unwrap().1,
                vec![if on { 0xf } else { 0xe }]
            );
            assert_eq!(fn_lock(if on { 0 } else { 1 << 10 }), on);
            assert_eq!(
                commands(&EnergyAction::NightCharge(on), None).unwrap().1,
                vec![if on { 0x80000012 } else { 0x12 }]
            );
            assert_eq!(night_charge(1 | if on { 1 << 4 } else { 0 }), Some(on));
            assert_eq!(
                commands(&EnergyAction::OneLevelWhite(on), None).unwrap().1,
                vec![if on { 0x8 } else { 0x9 }]
            );
            assert_eq!(
                one_level_white((1 << 4) | if on { 1 << 5 } else { 0 }),
                Some(on)
            );
        }
        assert_eq!(one_level_white(0), None);
        assert_eq!(night_charge(0), None);
        assert!(commands(&EnergyAction::KeyboardLevel(0), Some(1)).is_err());
    }
}
