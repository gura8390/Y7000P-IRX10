//! Native system switches and power schemes. No constructor changes system state.
use crate::{
    domain::ExplicitAction,
    ownership::{OwnerToken, wide},
    wmi_native::{MethodInput, WmiFacade},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    mem::{size_of, zeroed},
    ptr,
};
use windows_sys::{
    Win32::{
        Foundation::{FreeLibrary, LocalFree},
        System::{
            LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
            Power::*,
            Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
        },
        UI::WindowsAndMessaging::{
            HWND_BROADCAST, SC_MONITORPOWER, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SYSCOMMAND,
        },
    },
    core::GUID,
};
mod policies;
pub use policies::{
    PowerOverlay, PowerPolicyPlan, PowerPolicySnapshot, WindowsPowerPolicy, power_policy_snapshot,
};

pub(crate) fn integer(values: &BTreeMap<String, Value>, name: &str) -> Result<u32> {
    values
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
        .with_context(|| format!("Missing/invalid uint32 output {name}"))
}
fn params(
    values: impl IntoIterator<Item = (&'static str, MethodInput)>,
) -> BTreeMap<String, MethodInput> {
    values.into_iter().map(|(k, v)| (k.into(), v)).collect()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum GameZoneSwitch {
    WinKey,
    TouchpadLock,
}
impl GameZoneSwitch {
    fn methods(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::WinKey => (
                "IsSupportDisableWinKey",
                "GetWinKeyStatus",
                "SetWinKeyStatus",
            ),
            Self::TouchpadLock => ("IsSupportDisableTP", "GetTPStatus", "SetTPStatus"),
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct SwitchStatus {
    pub value: u32,
    pub source: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SystemAction {
    Switch { which: GameZoneSwitch, value: u32 },
    Brightness(u32),
    ScreenOff,
    PowerPlan(String),
    PowerMode(WindowsPowerMode),
    PowerPolicy(WindowsPowerPolicy),
    SynchronizeBrightnessToAllPowerPlans(u32),
}
pub(crate) fn write(
    owner: &OwnerToken,
    action: &ExplicitAction,
    request: SystemAction,
) -> Result<Value> {
    match request {
        SystemAction::Switch { which, value } => Ok(serde_json::to_value(write_switch(
            owner, action, which, value,
        )?)?),
        SystemAction::Brightness(value) => {
            Ok(serde_json::to_value(set_brightness(owner, action, value)?)?)
        }
        SystemAction::ScreenOff => {
            screen_off(owner, action)?;
            Ok(serde_json::json!({"request_delivered":true,"physical_panel_state_verified":false}))
        }
        SystemAction::PowerPlan(id) => {
            Ok(serde_json::to_value(set_power_plan(owner, action, &id)?)?)
        }
        SystemAction::PowerMode(mode) => Ok(Value::String(set_power_mode(owner, action, mode)?)),
        SystemAction::PowerPolicy(_) => {
            anyhow::bail!("Windows power policy requires the actor firmware-mode guard")
        }
        SystemAction::SynchronizeBrightnessToAllPowerPlans(value) => Ok(serde_json::to_value(
            crate::brightness_sync::apply(owner, action, value)?,
        )?),
    }
}
pub(crate) fn apply_power_policy_guarded(
    owner: &OwnerToken,
    action: &ExplicitAction,
    policy: WindowsPowerPolicy,
    guard: &mut dyn FnMut() -> Result<()>,
) -> Result<Value> {
    policies::apply(owner, action, policy, guard)
}
fn precision_enabled() -> Result<bool> {
    let path = wide("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\PrecisionTouchPad\\Status")?;
    let value = wide("Enabled")?;
    let mut enabled = 0u32;
    let mut bytes = 4u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            ptr::null_mut(),
            (&mut enabled as *mut u32).cast(),
            &mut bytes,
        )
    };
    ensure!(
        status == 0 && bytes == 4,
        "Precision touchpad status unavailable ({status})"
    );
    Ok(enabled != 0)
}
fn precision_supported(facade: &WmiFacade) -> Result<()> {
    let value = facade.method_read(
        "LENOVO_UTILITY_DATA",
        "GetIfSupportOrVersion",
        params([("datatype", MethodInput::U32(0x12))]),
    )?;
    ensure!(
        integer(&value, "Data")? >= 0x18,
        "Precision touchpad shortcut unsupported"
    );
    Ok(())
}
pub fn read_switch(which: GameZoneSwitch) -> Result<SwitchStatus> {
    let facade = WmiFacade::connect("ROOT\\WMI")?;
    read_switch_with(&facade, which)
}
fn read_switch_with(facade: &WmiFacade, which: GameZoneSwitch) -> Result<SwitchStatus> {
    let (support, get, _) = which.methods();
    let supports = facade
        .method_read("LENOVO_GAMEZONE_DATA", support, BTreeMap::new())
        .and_then(|v| integer(&v, "Data"));
    if supports.is_ok_and(|v| v > 0) {
        let value = integer(
            &facade.method_read("LENOVO_GAMEZONE_DATA", get, BTreeMap::new())?,
            "Data",
        )?;
        ensure!(value <= 1, "Undefined Lenovo system switch enum");
        return Ok(SwitchStatus {
            value,
            source: "Lenovo GameZone WMI".into(),
        });
    }
    ensure!(
        matches!(which, GameZoneSwitch::TouchpadLock),
        "Windows key switch unsupported"
    );
    precision_supported(facade)?;
    Ok(SwitchStatus {
        value: u32::from(!precision_enabled()?),
        source: "PrecisionTouchPad status + Lenovo supported Ctrl+Win+F24 shortcut".into(),
    })
}
pub(crate) fn write_switch(
    owner: &OwnerToken,
    action: &ExplicitAction,
    which: GameZoneSwitch,
    value: u32,
) -> Result<SwitchStatus> {
    ensure!(value <= 1, "System switch must be 0 or 1");
    let facade = WmiFacade::connect("ROOT\\WMI")?;
    let current = read_switch_with(&facade, which)?;
    if current.value == value {
        return Ok(current);
    }
    if current.source == "Lenovo GameZone WMI" {
        facade.method_write(
            owner,
            action,
            "LENOVO_GAMEZONE_DATA",
            which.methods().2,
            params([("Data", MethodInput::U32(value))]),
        )?;
    } else {
        // Match the original six-input chord; never write the user's registry/config.
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        let inputs: Vec<INPUT> = [
            (0x11, false),
            (0x5b, false),
            (0x87, false),
            (0x87, true),
            (0x5b, true),
            (0x11, true),
        ]
        .into_iter()
        .map(|(key, up)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(key),
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        Default::default()
                    },
                    ..Default::default()
                },
            },
        })
        .collect();
        action.check_write_boundary()?;
        let accepted = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
        if accepted != inputs.len() as u32 {
            // A partial chord must not leave a modifier held down.
            let releases: Vec<INPUT> = [0x87, 0x5b, 0x11]
                .into_iter()
                .map(|key| INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(key),
                            dwFlags: KEYEVENTF_KEYUP,
                            ..Default::default()
                        },
                    },
                })
                .collect();
            unsafe { SendInput(&releases, size_of::<INPUT>() as i32) };
        }
        ensure!(
            accepted == inputs.len() as u32,
            "Precision touchpad input chord was partially/rejected"
        );
    }
    for _ in 0..10 {
        let observed = read_switch_with(&facade, which)?;
        if observed.value == value {
            return Ok(observed);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    anyhow::bail!("System switch readback mismatch")
}

#[derive(Debug, Clone, Serialize)]
pub struct BrightnessStatus {
    pub current: u32,
    pub levels: Vec<u32>,
    pub instance: String,
}
pub fn brightness() -> Result<BrightnessStatus> {
    let facade = WmiFacade::connect("ROOT\\WMI")?;
    #[derive(Deserialize)]
    struct Row {
        #[serde(rename = "CurrentBrightness")]
        current: u32,
        #[serde(rename = "Level")]
        levels: Vec<u32>,
        #[serde(rename = "InstanceName")]
        instance: String,
    }
    let mut rows: Vec<Row> = facade.query(
        "SELECT CurrentBrightness,Level,InstanceName FROM WmiMonitorBrightness WHERE Active=TRUE",
    )?;
    ensure!(
        rows.len() == 1,
        "Expected one active internal brightness controller"
    );
    let row = rows.remove(0);
    ensure!(
        row.current <= 100 && row.levels.iter().all(|v| *v <= 100),
        "Invalid brightness capability"
    );
    Ok(BrightnessStatus {
        current: row.current,
        levels: row.levels,
        instance: row.instance,
    })
}
pub(crate) fn set_brightness(
    owner: &OwnerToken,
    action: &ExplicitAction,
    value: u32,
) -> Result<BrightnessStatus> {
    let before = brightness()?;
    ensure!(
        value <= 100 && before.levels.contains(&value),
        "Brightness is not an advertised level"
    );
    let facade = WmiFacade::connect("ROOT\\WMI")?;
    facade.method_write(
        owner,
        action,
        "WmiMonitorBrightnessMethods",
        "WmiSetBrightness",
        params([
            // The documented signature is uint32 Timeout, uint8 Brightness.
            // Zero requests immediate application before independent readback.
            ("Timeout", MethodInput::U32(0)),
            ("Brightness", MethodInput::Byte(value as u8)),
        ]),
    )?;
    let after = brightness()?;
    ensure!(
        after.instance == before.instance && after.current == value,
        "Brightness readback mismatch"
    );
    Ok(after)
}
pub(crate) fn screen_off(_owner: &OwnerToken, _action: &ExplicitAction) -> Result<()> {
    _action.check_write_boundary()?;
    let mut result = 0usize;
    ensure!(
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SYSCOMMAND,
                SC_MONITORPOWER as usize,
                2,
                SMTO_ABORTIFHUNG,
                1000,
                &mut result,
            )
        } != 0,
        "Screen-off broadcast timed out/rejected"
    );
    // Delivery success is not a measurable panel-state readback.
    Ok(())
}

pub fn guid_string(guid: GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        guid.data1,
        guid.data2,
        guid.data3,
        guid.data4[0],
        guid.data4[1],
        guid.data4[2],
        guid.data4[3],
        guid.data4[4],
        guid.data4[5],
        guid.data4[6],
        guid.data4[7]
    )
}
pub fn parse_guid(value: &str) -> Result<GUID> {
    let value = value.trim().trim_start_matches('{').trim_end_matches('}');
    ensure!(
        value.len() == 36
            && [8, 13, 18, 23]
                .into_iter()
                .all(|i| value.as_bytes()[i] == b'-'),
        "Expected a canonical GUID"
    );
    let digits: String = value.chars().filter(|c| *c != '-').collect();
    ensure!(
        digits.len() == 32 && digits.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid GUID"
    );
    Ok(GUID::from_u128(u128::from_str_radix(&digits, 16)?))
}
fn same_guid(a: GUID, b: GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}
#[derive(Debug, Clone, Serialize)]
pub struct PowerPlan {
    pub id: String,
    pub name: String,
    pub active: bool,
}
fn active_plan() -> Result<GUID> {
    let mut value = ptr::null_mut();
    let code = unsafe { PowerGetActiveScheme(ptr::null_mut(), &mut value) };
    ensure!(
        code == 0 && !value.is_null(),
        "PowerGetActiveScheme failed ({code})"
    );
    let result = unsafe { *value };
    unsafe { LocalFree(value.cast()) };
    Ok(result)
}
pub fn power_plans() -> Result<Vec<PowerPlan>> {
    let active = active_plan()?;
    let mut result = vec![];
    for index in 0..4096 {
        let mut guid: GUID = unsafe { zeroed() };
        let mut bytes = size_of::<GUID>() as u32;
        let code = unsafe {
            PowerEnumerate(
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ACCESS_SCHEME,
                index,
                (&mut guid as *mut GUID).cast(),
                &mut bytes,
            )
        };
        if code == 259 {
            return Ok(result);
        }
        ensure!(
            code == 0 && bytes == size_of::<GUID>() as u32,
            "PowerEnumerate failed ({code})"
        );
        let mut name_bytes = 0u32;
        let code = unsafe {
            PowerReadFriendlyName(
                ptr::null_mut(),
                &guid,
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
                &mut name_bytes,
            )
        };
        ensure!(
            (code == 0 || code == 234)
                && name_bytes > 0
                && name_bytes <= 65536
                && name_bytes % 2 == 0,
            "PowerReadFriendlyName size failed ({code})"
        );
        let mut name = vec![0u16; name_bytes as usize / 2];
        let code = unsafe {
            PowerReadFriendlyName(
                ptr::null_mut(),
                &guid,
                ptr::null(),
                ptr::null(),
                name.as_mut_ptr().cast(),
                &mut name_bytes,
            )
        };
        ensure!(code == 0, "PowerReadFriendlyName failed ({code})");
        let end = name
            .iter()
            .position(|v| *v == 0)
            .context("Unterminated power-plan name")?;
        result.push(PowerPlan {
            id: guid_string(guid),
            name: String::from_utf16_lossy(&name[..end]),
            active: same_guid(guid, active),
        });
    }
    anyhow::bail!("Power-plan enumeration exceeded its bound")
}
pub(crate) fn set_power_plan(
    _owner: &OwnerToken,
    _action: &ExplicitAction,
    id: &str,
) -> Result<Vec<PowerPlan>> {
    let guid = parse_guid(id)?;
    ensure!(
        power_plans()?.iter().any(|p| p.id == guid_string(guid)),
        "Power plan no longer exists"
    );
    _action.check_write_boundary()?;
    let code = unsafe { PowerSetActiveScheme(ptr::null_mut(), &guid) };
    ensure!(code == 0, "PowerSetActiveScheme failed ({code})");
    ensure!(
        same_guid(active_plan()?, guid),
        "Power-plan readback mismatch"
    );
    power_plans()
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum WindowsPowerMode {
    Efficiency,
    Balanced,
    Performance,
}
impl WindowsPowerMode {
    fn guid(self) -> GUID {
        GUID::from_u128(match self {
            Self::Efficiency => 0x961cc777_2547_4f9d_8174_7d86181b8a7a,
            Self::Balanced => 0,
            Self::Performance => 0xded574b5_45a0_4f42_8737_46345c09c238,
        })
    }
}
struct OverlayLibrary(windows_sys::Win32::Foundation::HMODULE);
impl OverlayLibrary {
    fn open() -> Result<Self> {
        let name = wide("powrprof.dll")?;
        let handle =
            unsafe { LoadLibraryExW(name.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        ensure!(!handle.is_null(), "System power library unavailable");
        Ok(Self(handle))
    }
    fn symbol(&self, name: &[u8]) -> Result<unsafe extern "system" fn() -> isize> {
        unsafe { GetProcAddress(self.0, name.as_ptr()) }
            .context("Windows power overlay API unavailable")
    }
}
impl Drop for OverlayLibrary {
    fn drop(&mut self) {
        unsafe { FreeLibrary(self.0) };
    }
}
pub fn effective_power_mode() -> Result<String> {
    // Same native ABI as the fixed toolkit fork's PowerGetUserConfigured* getters.
    let library = OverlayLibrary::open()?;
    let get: unsafe extern "system" fn(*mut GUID) -> u32 =
        unsafe { std::mem::transmute(library.symbol(b"PowerGetEffectiveOverlayScheme\0")?) };
    let mut value = unsafe { zeroed() };
    let code = unsafe { get(&mut value) };
    ensure!(code == 0, "PowerGetEffectiveOverlayScheme failed ({code})");
    Ok(guid_string(value))
}
pub(crate) fn set_power_mode(
    _owner: &OwnerToken,
    _action: &ExplicitAction,
    mode: WindowsPowerMode,
) -> Result<String> {
    let mut status: SYSTEM_POWER_STATUS = unsafe { zeroed() };
    ensure!(
        unsafe { GetSystemPowerStatus(&mut status) } != 0,
        "Power status unavailable"
    );
    ensure!(
        status.SystemStatusFlag == 0,
        "Battery saver prevents changing the Windows power overlay"
    );
    let library = OverlayLibrary::open()?;
    let set: unsafe extern "system" fn(GUID) -> u32 =
        unsafe { std::mem::transmute(library.symbol(b"PowerSetActiveOverlayScheme\0")?) };
    _action.check_write_boundary()?;
    let code = unsafe { set(mode.guid()) };
    ensure!(code == 0, "PowerSetActiveOverlayScheme failed ({code})");
    let observed = effective_power_mode()?;
    ensure!(
        observed == guid_string(mode.guid()),
        "Effective Windows power-mode readback mismatch"
    );
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn power_guids_roundtrip_without_native_calls() {
        for mode in [
            WindowsPowerMode::Efficiency,
            WindowsPowerMode::Balanced,
            WindowsPowerMode::Performance,
        ] {
            let value = mode.guid();
            assert!(same_guid(parse_guid(&guid_string(value)).unwrap(), value));
        }
        assert!(parse_guid("bad").is_err());
        assert!(parse_guid("00000000-0000-0000-0000-00000000000g").is_err());
    }
}
