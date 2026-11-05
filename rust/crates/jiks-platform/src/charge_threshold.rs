//! The legacy PWRMGRV / IBMPmDrv charge-threshold protocol.
use crate::{
    domain::ExplicitAction,
    ownership::{OwnerToken, wide},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{io, mem::size_of_val, ptr};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_READ_DATA, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    },
    System::{IO::DeviceIoControl, Registry::*},
};

const DATA: &str = r"SOFTWARE\WOW6432Node\Lenovo\PWRMGRV\ConfKeys\Data";
const FIELDS: [&str; 4] = [
    "ChargeStartPercentage",
    "ChargeStopPercentage",
    "ChargeStartControl",
    "ChargeStopControl",
];
const MODE: u32 = 0x22261c;
const START_STATUS: u32 = 0x22262c;
const START: u32 = 0x222630;
const STOP_STATUS: u32 = 0x222634;
const STOP: u32 = 0x222638;

pub const MINIMUM: u32 = 2;
pub const MAXIMUM: u32 = 99;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Threshold {
    pub enabled: bool,
    pub start: u32,
    pub stop: u32,
}
impl Threshold {
    pub fn validate(self) -> Result<Self> {
        ensure!(
            (MINIMUM..=MAXIMUM).contains(&self.start)
                && (MINIMUM..=MAXIMUM).contains(&self.stop)
                && self.start < self.stop,
            "开始和停止阈值须在 2–99% 范围内，且开始阈值小于停止阈值"
        );
        Ok(self)
    }
    fn registry(self) -> [u32; 4] {
        let enabled = u32::from(self.enabled);
        [self.start, self.stop, enabled, enabled]
    }
    fn from_registry([start, stop, start_control, stop_control]: [u32; 4]) -> Result<Self> {
        ensure!(
            start_control <= 1 && start_control == stop_control,
            "PWRMGRV 开始与停止阈值开关不一致或超出协议范围"
        );
        Self {
            enabled: start_control == 1,
            start,
            stop,
        }
        .validate()
        .context("PWRMGRV 保存的阈值无效")
    }
    fn setters(self, index: u32) -> [(u32, u32); 3] {
        let address = (index & 3) << 8;
        if self.enabled {
            [
                (STOP, address | self.stop),
                (START, address | self.start),
                (MODE, address | 1),
            ]
        } else {
            [(STOP, address), (START, address), (MODE, 0)]
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Status {
    state: Option<Threshold>,
    reason: Option<&'static str>,
}
impl Status {
    fn observed(state: Threshold) -> Self {
        Self {
            state: Some(state),
            reason: None,
        }
    }
    fn unavailable(reason: &'static str) -> Self {
        Self {
            state: None,
            reason: Some(reason),
        }
    }
}

fn missing(code: u32) -> bool {
    matches!(code, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
}
fn win32(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}
impl Key {
    fn open(base: HKEY, name: &str, writable: bool) -> Result<Option<Self>> {
        let name = wide(name)?;
        let mut key = ptr::null_mut();
        let access = KEY_WOW64_64KEY | KEY_QUERY_VALUE | if writable { KEY_SET_VALUE } else { 0 };
        let code = unsafe { RegOpenKeyExW(base, name.as_ptr(), 0, access, &mut key) };
        match code {
            0 => Ok(Some(Self(key))),
            code if missing(code) => Ok(None),
            code => Err(win32(code)).context("无法打开 PWRMGRV 阈值注册表项"),
        }
    }
    fn read<T>(&self, name: &str, kind: u32, output: &mut T) -> Result<Option<usize>> {
        let name = wide(name)?;
        let mut bytes = size_of_val(output) as u32;
        let code = unsafe {
            RegGetValueW(
                self.0,
                ptr::null(),
                name.as_ptr(),
                kind,
                ptr::null_mut(),
                (output as *mut T).cast(),
                &mut bytes,
            )
        };
        match code {
            0 => Ok(Some(bytes as usize)),
            code if missing(code) => Ok(None),
            code => Err(win32(code)).context("无法读取 PWRMGRV 阈值注册表值"),
        }
    }
    fn dword(&self, name: &str) -> Result<Option<u32>> {
        let mut value = 0u32;
        match self.read(name, RRF_RT_REG_DWORD, &mut value)? {
            None => Ok(None),
            Some(4) => Ok(Some(value)),
            Some(_) => bail!("PWRMGRV 阈值 DWORD 长度无效"),
        }
    }
    fn barcode(&self) -> Result<Option<String>> {
        let mut value = [0u16; 256];
        let Some(bytes) = self.read("Barcode Number", RRF_RT_REG_SZ, &mut value)? else {
            return Ok(None);
        };
        ensure!(bytes >= 2 && bytes % 2 == 0, "PWRMGRV 电池标识长度无效");
        let value = &value[..bytes / 2];
        ensure!(value.last() == Some(&0), "PWRMGRV 电池标识缺少终止符");
        let value = String::from_utf16(&value[..value.len() - 1])
            .map_err(|_| anyhow::anyhow!("PWRMGRV 电池标识编码无效"))?;
        if value.trim().is_empty() {
            return Ok(None);
        }
        ensure!(
            value != "."
                && value != ".."
                && !value
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '/' | '\\'))
                && !["Battery1", "Battery2"]
                    .iter()
                    .any(|name| value.eq_ignore_ascii_case(name)),
            "PWRMGRV 电池标识不是有效的配置项名称"
        );
        Ok(Some(value))
    }
    fn set_dword(&self, name: &str, value: u32) -> Result<()> {
        let name = wide(name)?;
        let bytes = value.to_le_bytes();
        let code = unsafe {
            RegSetValueExW(
                self.0,
                name.as_ptr(),
                0,
                REG_DWORD,
                bytes.as_ptr(),
                bytes.len() as u32,
            )
        };
        if code != 0 {
            return Err(win32(code)).context("无法写入 PWRMGRV 阈值注册表值");
        }
        Ok(())
    }
}

struct Driver(HANDLE);
impl Drop for Driver {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}
impl Driver {
    fn open() -> Result<Option<Self>> {
        let path = wide(r"\\.\IBMPmDrv")?;
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_READ_DATA,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(Some(Self(handle)));
        }
        let error = io::Error::last_os_error();
        if error
            .raw_os_error()
            .is_some_and(|code| missing(code as u32))
        {
            return Ok(None);
        }
        Err(error).context("无法打开 IBMPmDrv 阈值设备")
    }
    fn invoke(&self, code: u32, input: u32) -> Result<i32> {
        let mut output = 0i32;
        let mut returned = 0u32;
        let success = unsafe {
            DeviceIoControl(
                self.0,
                code,
                (&input as *const u32).cast(),
                4,
                (&mut output as *mut i32).cast(),
                4,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if success == 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("IBMPmDrv 调用 0x{code:X} 失败"));
        }
        ensure!(
            returned == 4,
            "IBMPmDrv 调用 0x{code:X} 返回无效状态长度 {returned}"
        );
        Ok(output)
    }
}

struct Port {
    driver: Driver,
    batteries: Vec<(u32, Key)>,
}
impl Port {
    fn open(writable: bool) -> Result<std::result::Result<Self, &'static str>> {
        let Some(data) = Key::open(HKEY_LOCAL_MACHINE, DATA, false)? else {
            return Ok(Err("本机没有 Lenovo PWRMGRV 充电阈值配置"));
        };
        let mut batteries = Vec::new();
        let mut barcodes = Vec::new();
        for index in 1..=2 {
            let Some(slot) = Key::open(data.0, &format!("Battery{index}"), false)? else {
                continue;
            };
            let Some(barcode) = slot.barcode()? else {
                continue;
            };
            let Some(key) = Key::open(data.0, &barcode, false)? else {
                continue;
            };
            if key.dword(FIELDS[0])?.is_none() {
                continue;
            }
            let alias = barcode.to_uppercase();
            ensure!(
                !barcodes.contains(&alias),
                "多个 PWRMGRV 电池共用一组阈值配置"
            );
            barcodes.push(alias);
            let key = if writable {
                Key::open(data.0, &barcode, true)?.context("PWRMGRV 电池阈值项已消失")?
            } else {
                key
            };
            batteries.push((index, key));
        }
        if batteries.is_empty() {
            return Ok(Err("PWRMGRV 未发现可读取阈值的电池"));
        }
        let Some(driver) = Driver::open()? else {
            return Ok(Err("本机没有 IBMPmDrv 充电阈值设备"));
        };
        Ok(Ok(Self { driver, batteries }))
    }
}

trait ThresholdPort {
    fn indices(&self) -> Vec<u32>;
    fn read_field(&self, index: u32, field: usize) -> Result<u32>;
    fn write_field(&self, index: u32, field: usize, value: u32) -> Result<()>;
    fn invoke(&self, code: u32, value: u32) -> Result<i32>;
}
impl ThresholdPort for Port {
    fn indices(&self) -> Vec<u32> {
        self.batteries.iter().map(|(index, _)| *index).collect()
    }
    fn read_field(&self, index: u32, field: usize) -> Result<u32> {
        let (_, key) = self
            .batteries
            .iter()
            .find(|(id, _)| *id == index)
            .context("电池已消失")?;
        key.dword(FIELDS[field])?
            .context("PWRMGRV 缺少电池阈值所需的注册表值")
    }
    fn write_field(&self, index: u32, field: usize, value: u32) -> Result<()> {
        let (_, key) = self
            .batteries
            .iter()
            .find(|(id, _)| *id == index)
            .context("电池已消失")?;
        key.set_dword(FIELDS[field], value)
    }
    fn invoke(&self, code: u32, value: u32) -> Result<i32> {
        self.driver.invoke(code, value)
    }
}

struct Observation {
    state: Threshold,
    driver_status: Vec<(i32, i32)>,
}

fn read_configuration(port: &impl ThresholdPort, indices: &[u32]) -> Result<Threshold> {
    let mut observed = None;
    for &index in indices {
        let mut values = [0; 4];
        for (field, value) in values.iter_mut().enumerate() {
            *value = port.read_field(index, field)?;
        }
        let state = Threshold::from_registry(values)?;
        ensure!(
            observed.is_none_or(|prior| prior == state),
            "PWRMGRV 各电池阈值设置不一致"
        );
        observed = Some(state);
    }
    observed.context("未发现支持充电阈值的电池")
}

fn observe(port: &impl ThresholdPort, indices: &[u32]) -> Result<Observation> {
    let state = read_configuration(port, indices)?;
    let driver_status = indices
        .iter()
        .map(|&index| {
            Ok((
                port.invoke(START_STATUS, index)?,
                port.invoke(STOP_STATUS, index)?,
            ))
        })
        .collect::<Result<_>>()?;
    Ok(Observation {
        state,
        driver_status,
    })
}

fn set_driver(port: &impl ThresholdPort, code: u32, input: u32) -> Result<()> {
    let status = port.invoke(code, input)?;
    ensure!(
        status >= 0,
        "IBMPmDrv 拒绝阈值写入 0x{code:X} (状态 0x{status:08X})"
    );
    Ok(())
}

pub(crate) fn read() -> Result<Status> {
    match Port::open(false)? {
        Ok(port) => Ok(Status::observed(observe(&port, &port.indices())?.state)),
        Err(reason) => Ok(Status::unavailable(reason)),
    }
}

pub(crate) fn write(
    _owner: &OwnerToken,
    permit: &ExplicitAction,
    target: Threshold,
) -> Result<Status> {
    let target = target.validate()?;
    let port = Port::open(true)?.map_err(anyhow::Error::msg)?;
    apply(&port, || permit.check_write_boundary(), target).map(Status::observed)
}

fn apply(
    port: &impl ThresholdPort,
    boundary: impl Fn() -> Result<()>,
    target: Threshold,
) -> Result<Threshold> {
    let indices = port.indices();
    let original = observe(port, &indices)?;
    let original_values = original.state.registry();
    let target_values = target.registry();
    let mut touched = Vec::new();
    let mut driver_touched = false;
    let result = (|| {
        for &index in &indices {
            for (field, (&prior, &next)) in original_values.iter().zip(&target_values).enumerate() {
                if prior == next {
                    continue;
                }
                boundary()?;
                ensure!(
                    port.read_field(index, field)? == prior,
                    "PWRMGRV 阈值配置在写入前已变化"
                );
                touched.push((index, field, prior, next));
                port.write_field(index, field, next)?;
            }
        }
        ensure!(
            read_configuration(port, &indices)? == target,
            "PWRMGRV 写入后阈值读回与请求不一致"
        );
        for &index in &indices {
            for (code, input) in target.setters(index) {
                boundary()?;
                driver_touched = true;
                set_driver(port, code, input)?;
            }
        }
        let observed = observe(port, &indices)?;
        // The original adapter provides no encoding for the raw status words.
        // Verify configured percentages via PWRMGRV and require both status IOCTLs to return.
        ensure!(observed.state == target, "PWRMGRV 充电阈值读回与请求不一致");
        boundary()?;
        Ok(observed.state)
    })();
    let Err(error) = result else { return result };
    if touched.is_empty() && !driver_touched {
        return Err(error);
    }
    let mut recovery = Vec::new();
    for (index, field, prior, next) in touched.into_iter().rev() {
        let restore: Result<()> = (|| {
            let current = port.read_field(index, field)?;
            if current != prior {
                // Restore only a value this request may have changed, even if
                // cancelled. Never overwrite a third-party value during cleanup.
                ensure!(current == next, "PWRMGRV 阈值恢复遇到外部更改");
                port.write_field(index, field, prior)?;
            }
            ensure!(
                port.read_field(index, field)? == prior,
                "PWRMGRV 阈值恢复读回不一致"
            );
            Ok(())
        })();
        if let Err(error) = restore {
            recovery.push(format!("{error:#}"));
        }
    }
    if driver_touched && recovery.is_empty() {
        let restore: Result<()> = (|| {
            ensure!(
                read_configuration(port, &indices)? == original.state,
                "PWRMGRV 原阈值配置已被外部更改"
            );
            for &index in &indices {
                for (code, input) in original.state.setters(index) {
                    boundary()?;
                    set_driver(port, code, input)?;
                }
            }
            let observed = observe(port, &indices)?;
            ensure!(
                observed.state == original.state
                    && observed.driver_status == original.driver_status,
                "IBMPmDrv 阈值恢复读回与原状态不一致"
            );
            Ok(())
        })();
        if let Err(error) = restore {
            recovery.push(format!("{error:#}"));
        }
    }
    if recovery.is_empty() {
        Err(error).context("充电阈值写入失败，已核对恢复")
    } else {
        Err(error).context(format!(
            "充电阈值写入失败，恢复未能核对：{}",
            recovery.join("；")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    const ORIGINAL: Threshold = Threshold {
        enabled: false,
        start: 40,
        stop: 60,
    };
    const TARGET: Threshold = Threshold {
        enabled: true,
        start: 50,
        stop: 80,
    };

    struct FakePort {
        registry: RefCell<[[u32; 4]; 2]>,
        driver_status: RefCell<[(i32, i32); 2]>,
        writes: Cell<u32>,
        fail_write: u32,
        fail_ioctl: Option<(u32, u32)>,
        ioctl: RefCell<Vec<(u32, u32)>>,
    }
    impl Default for FakePort {
        fn default() -> Self {
            Self {
                registry: RefCell::new([ORIGINAL.registry(); 2]),
                driver_status: RefCell::new([(0, 0); 2]),
                writes: Cell::new(0),
                fail_write: 0,
                fail_ioctl: None,
                ioctl: RefCell::new(Vec::new()),
            }
        }
    }
    impl ThresholdPort for FakePort {
        fn indices(&self) -> Vec<u32> {
            vec![1, 2]
        }
        fn read_field(&self, index: u32, field: usize) -> Result<u32> {
            Ok(self.registry.borrow()[index as usize - 1][field])
        }
        fn write_field(&self, index: u32, field: usize, value: u32) -> Result<()> {
            self.writes.set(self.writes.get() + 1);
            if self.writes.get() == self.fail_write {
                bail!("fixture registry denial");
            }
            self.registry.borrow_mut()[index as usize - 1][field] = value;
            Ok(())
        }
        fn invoke(&self, code: u32, value: u32) -> Result<i32> {
            self.ioctl.borrow_mut().push((code, value));
            if Some((code, value)) == self.fail_ioctl {
                if matches!(code, START_STATUS | STOP_STATUS) {
                    bail!("fixture driver read denial");
                }
                return Ok(-1);
            }
            let mut status = self.driver_status.borrow_mut();
            match code {
                START_STATUS => return Ok(status[value as usize - 1].0),
                STOP_STATUS => return Ok(status[value as usize - 1].1),
                START => status[(value >> 8) as usize - 1].0 = (value & 255) as i32,
                STOP => status[(value >> 8) as usize - 1].1 = (value & 255) as i32,
                MODE => (),
                _ => bail!("fixture unknown IOCTL"),
            }
            Ok(0)
        }
    }

    #[test]
    fn confirms_all_slots_with_legacy_driver_order_and_bare_status_indices() {
        let port = FakePort::default();
        assert_eq!(apply(&port, || Ok(()), TARGET).unwrap(), TARGET);
        assert_eq!(*port.registry.borrow(), [TARGET.registry(); 2]);
        assert_eq!(
            *port.ioctl.borrow(),
            [
                (0x22262c, 1),
                (0x222634, 1),
                (0x22262c, 2),
                (0x222634, 2),
                (0x222638, 0x150),
                (0x222630, 0x132),
                (0x22261c, 0x101),
                (0x222638, 0x250),
                (0x222630, 0x232),
                (0x22261c, 0x201),
                (0x22262c, 1),
                (0x222634, 1),
                (0x22262c, 2),
                (0x222634, 2),
            ]
        );
    }

    #[test]
    fn registry_failure_restores_prior_values_before_any_driver_setter() {
        let port = FakePort {
            fail_write: 6,
            ..Default::default()
        };
        let error = apply(&port, || Ok(()), TARGET).unwrap_err();
        assert!(format!("{error:#}").contains("已核对恢复"));
        assert_eq!(*port.registry.borrow(), [ORIGINAL.registry(); 2]);
        assert_eq!(port.ioctl.borrow().len(), 4);
    }

    #[test]
    fn partial_driver_failure_restores_registry_and_replays_automatic_mode() {
        let port = FakePort {
            fail_ioctl: Some((START, 0x232)),
            ..Default::default()
        };
        let error = apply(&port, || Ok(()), TARGET).unwrap_err();
        assert!(format!("{error:#}").contains("已核对恢复"));
        assert_eq!(*port.registry.borrow(), [ORIGINAL.registry(); 2]);
        assert_eq!(*port.driver_status.borrow(), [(0, 0); 2]);
        assert!(port.ioctl.borrow().ends_with(&[
            (STOP, 0x100),
            (START, 0x100),
            (MODE, 0),
            (STOP, 0x200),
            (START, 0x200),
            (MODE, 0),
            (START_STATUS, 1),
            (STOP_STATUS, 1),
            (START_STATUS, 2),
            (STOP_STATUS, 2),
        ]));
    }

    #[test]
    fn cancellation_restores_owned_registry_values_and_stops_further_ioctls() {
        let port = FakePort::default();
        let calls = Cell::new(0);
        let boundary = || {
            calls.set(calls.get() + 1);
            ensure!(calls.get() <= 9, "fixture cancellation");
            Ok(())
        };
        let error = apply(&port, boundary, TARGET).unwrap_err();
        assert!(format!("{error:#}").contains("恢复未能核对"));
        assert_eq!(*port.registry.borrow(), [ORIGINAL.registry(); 2]);
        assert_eq!(port.ioctl.borrow().len(), 5);
    }

    #[test]
    fn mixed_battery_state_and_unreadable_driver_preflight_issue_no_writes() {
        let port = FakePort::default();
        port.registry.borrow_mut()[1][0] = 42;
        assert!(apply(&port, || Ok(()), TARGET).is_err());
        assert_eq!(port.writes.get(), 0);
        let port = FakePort {
            fail_ioctl: Some((START_STATUS, 2)),
            ..Default::default()
        };
        assert!(apply(&port, || Ok(()), TARGET).is_err());
        assert_eq!(port.writes.get(), 0);
    }
}
