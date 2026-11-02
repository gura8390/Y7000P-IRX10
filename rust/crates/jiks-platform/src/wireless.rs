//! Native WLAN radios and the original LLT IRadioManager ABI. No scan,
//! connection request, key access, hook, or radio mutation during construction.
use crate::{domain::ExplicitAction, ownership::OwnerToken};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, ffi::c_void, mem::size_of, ptr};
use windows_sys::{
    Win32::{
        Foundation::HANDLE,
        NetworkManagement::WiFi::*,
        System::Com::{CLSCTX_ALL, CoCreateInstance},
    },
    core::GUID,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhyRadio {
    pub index: u32,
    pub software_on: Option<bool>,
    pub hardware_on: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WifiInterface {
    pub id: String,
    pub name: String,
    pub connection_state: i32,
    pub connected: Option<bool>,
    pub ssid: Option<String>,
    pub ssid_bytes: Vec<u8>,
    pub radios: Vec<PhyRadio>,
    pub errors: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WifiSnapshot {
    pub interfaces: Vec<WifiInterface>,
    pub any_connected: Option<bool>,
    pub connected_ssids: Vec<String>,
    pub any_software_radio_on: Option<bool>,
    pub all_connections_known: bool,
}
pub fn summarize(interfaces: Vec<WifiInterface>) -> WifiSnapshot {
    let any_connected = if interfaces.iter().any(|i| i.connected == Some(true)) {
        Some(true)
    } else if interfaces.iter().all(|i| i.connected == Some(false)) {
        Some(false)
    } else {
        None
    };
    let radios = interfaces
        .iter()
        .flat_map(|i| &i.radios)
        .collect::<Vec<_>>();
    let any_software_radio_on = if radios.iter().any(|r| r.software_on == Some(true)) {
        Some(true)
    } else if !radios.is_empty() && radios.iter().all(|r| r.software_on == Some(false)) {
        Some(false)
    } else {
        None
    };
    let all_connections_known = interfaces.iter().all(|i| i.connected.is_some());
    let connected_ssids = interfaces
        .iter()
        .filter(|i| i.connected == Some(true))
        .filter_map(|i| i.ssid.clone())
        .collect();
    WifiSnapshot {
        interfaces,
        any_connected,
        connected_ssids,
        any_software_radio_on,
        all_connections_known,
    }
}
fn state(v: i32) -> Option<bool> {
    if v == dot11_radio_state_on {
        Some(true)
    } else if v == dot11_radio_state_off {
        Some(false)
    } else {
        None
    }
}
fn connected(v: i32) -> Option<bool> {
    if v == wlan_interface_state_connected || v == wlan_interface_state_ad_hoc_network_formed {
        Some(true)
    } else if v == wlan_interface_state_disconnected {
        Some(false)
    } else {
        None
    }
}
fn guid_text(g: GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    )
}
struct Allocation(*mut c_void);
impl Drop for Allocation {
    fn drop(&mut self) {
        unsafe { WlanFreeMemory(self.0) }
    }
}
struct Wlan(HANDLE);
impl Drop for Wlan {
    fn drop(&mut self) {
        unsafe {
            WlanCloseHandle(self.0, ptr::null());
        }
    }
}
impl Wlan {
    fn open() -> Result<Self> {
        let mut version = 0;
        let mut handle = ptr::null_mut();
        let code = unsafe { WlanOpenHandle(2, ptr::null(), &mut version, &mut handle) };
        ensure!(
            code == 0 && !handle.is_null() && version >= 2,
            "WlanOpenHandle failed/unsupported ({code})"
        );
        Ok(Self(handle))
    }
    fn interfaces(&self) -> Result<Vec<WLAN_INTERFACE_INFO>> {
        let mut p = ptr::null_mut();
        let code = unsafe { WlanEnumInterfaces(self.0, ptr::null(), &mut p) };
        ensure!(
            code == 0 && !p.is_null(),
            "WLAN interface enumeration failed ({code})"
        );
        let _allocated = Allocation(p.cast());
        let count = unsafe { (*p).dwNumberOfItems } as usize;
        ensure!(count <= 64, "WLAN interface count out of bound");
        Ok(unsafe {
            std::slice::from_raw_parts(
                ptr::addr_of!((*p).InterfaceInfo).cast::<WLAN_INTERFACE_INFO>(),
                count,
            )
        }
        .to_vec())
    }
    fn query<T: Copy>(&self, id: &GUID, opcode: WLAN_INTF_OPCODE) -> Result<T> {
        let mut bytes = 0;
        let mut p = ptr::null_mut();
        let mut value_type = 0;
        let code = unsafe {
            WlanQueryInterface(
                self.0,
                id,
                opcode,
                ptr::null(),
                &mut bytes,
                &mut p,
                &mut value_type,
            )
        };
        ensure!(
            code == 0 && !p.is_null(),
            "WLAN query {opcode} failed ({code})"
        );
        let _allocated = Allocation(p);
        ensure!(
            bytes as usize >= size_of::<T>(),
            "WLAN query {opcode} returned a truncated buffer"
        );
        Ok(unsafe { ptr::read_unaligned(p.cast::<T>()) })
    }
    fn read(&self) -> Result<WifiSnapshot> {
        let mut interfaces = Vec::new();
        for interface in self.interfaces()? {
            let mut item = WifiInterface {
                id: guid_text(interface.InterfaceGuid),
                name: String::from_utf16_lossy(
                    &interface.strInterfaceDescription[..interface
                        .strInterfaceDescription
                        .iter()
                        .position(|v| *v == 0)
                        .unwrap_or(256)],
                ),
                connection_state: interface.isState,
                connected: connected(interface.isState),
                ssid: None,
                ssid_bytes: Vec::new(),
                radios: Vec::new(),
                errors: Vec::new(),
            };
            if item.connected == Some(true) {
                match self.query::<WLAN_CONNECTION_ATTRIBUTES>(
                    &interface.InterfaceGuid,
                    wlan_intf_opcode_current_connection,
                ) {
                    Ok(connection) => {
                        item.connection_state = connection.isState;
                        item.connected = connected(connection.isState);
                        let ssid = connection.wlanAssociationAttributes.dot11Ssid;
                        let len = ssid.uSSIDLength as usize;
                        if len <= 32 {
                            item.ssid_bytes = ssid.ucSSID[..len].to_vec();
                            match String::from_utf8(item.ssid_bytes.clone()){Ok(v)=>item.ssid=Some(v),Err(_)=>item.errors.push("SSID bytes are not UTF-8; retained exact bytes, no guessed trigger string".into())}
                        } else {
                            item.connected = None;
                            item.errors.push("Invalid SSID length".into());
                        }
                    }
                    Err(e) => {
                        item.connected = None;
                        item.errors.push(format!("connection: {e:#}"));
                    }
                }
            }
            match self
                .query::<WLAN_RADIO_STATE>(&interface.InterfaceGuid, wlan_intf_opcode_radio_state)
            {
                Ok(radios) => {
                    if radios.dwNumberOfPhys <= 64 {
                        item.radios = radios.PhyRadioState[..radios.dwNumberOfPhys as usize]
                            .iter()
                            .map(|r| PhyRadio {
                                index: r.dwPhyIndex,
                                software_on: state(r.dot11SoftwareRadioState),
                                hardware_on: state(r.dot11HardwareRadioState),
                            })
                            .collect();
                    } else {
                        item.errors.push("WLAN PHY count exceeds 64".into());
                    }
                }
                Err(e) => item.errors.push(format!("radios: {e:#}")),
            }
            interfaces.push(item);
        }
        Ok(summarize(interfaces))
    }
}
pub fn read_wifi() -> Result<WifiSnapshot> {
    Wlan::open()?.read()
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WifiChange {
    pub interface_id: String,
    pub phy_index: u32,
    pub enabled: bool,
}
pub fn plan_wifi(snapshot: &WifiSnapshot, enabled: bool) -> Result<Vec<WifiChange>> {
    ensure!(
        !snapshot.interfaces.is_empty(),
        "No WLAN interface was enumerated"
    );
    let mut seen = BTreeSet::new();
    let mut changes = Vec::new();
    for interface in &snapshot.interfaces {
        ensure!(
            !interface.radios.is_empty(),
            "Radio capability is unknown for interface {}",
            interface.id
        );
        for phy in &interface.radios {
            ensure!(
                seen.insert((interface.id.clone(), phy.index)),
                "Duplicate WLAN interface/PHY identity"
            );
            let current = phy
                .software_on
                .context("WLAN software-radio state is unknown")?;
            if current != enabled {
                changes.push(WifiChange {
                    interface_id: interface.id.clone(),
                    phy_index: phy.index,
                    enabled,
                });
            }
        }
    }
    Ok(changes)
}
pub trait WifiPort {
    fn read(&mut self) -> Result<WifiSnapshot>;
    fn guard(&mut self) -> Result<()>;
    fn set(&mut self, change: &WifiChange) -> Result<()>;
}
pub fn apply_wifi(port: &mut impl WifiPort, enabled: bool) -> Result<serde_json::Value> {
    let before = port.read()?;
    let changes = plan_wifi(&before, enabled)?;
    let mut attempted = Vec::new();
    let mut completed = Vec::new();
    let work = (|| -> Result<WifiSnapshot> {
        for change in &changes {
            port.guard()?;
            attempted.push(change.clone());
            port.set(change)?;
            let observed = port.read()?;
            let phy = observed
                .interfaces
                .iter()
                .find(|i| i.id == change.interface_id)
                .and_then(|i| i.radios.iter().find(|p| p.index == change.phy_index))
                .context("WLAN selected interface/PHY disappeared")?;
            ensure!(
                phy.software_on == Some(enabled),
                "WLAN software-radio readback mismatch"
            );
            completed.push(change.clone());
        }
        let observed = port.read()?;
        ensure!(
            plan_wifi(&observed, enabled)?.is_empty(),
            "WLAN final software-radio readback changed"
        );
        Ok(observed)
    })();
    match work {
        Ok(observed) => Ok(
            serde_json::json!({"wifi":observed,"attempted":attempted,"completed":completed,"software_radio_verified":true,"network_connection_verified":false}),
        ),
        Err(error) => {
            let observed = port.read().ok();
            anyhow::bail!(
                "WLAN operation partially completed: {}",
                serde_json::json!({"error":format!("{error:#}"),"attempted":attempted,"completed":completed,"observed":observed,"software_radio_verified":false})
            );
        }
    }
}
pub(crate) fn write_wifi(
    _owner: &OwnerToken,
    action: &ExplicitAction,
    enabled: bool,
) -> Result<serde_json::Value> {
    struct Native<'a> {
        wlan: Wlan,
        action: &'a ExplicitAction,
    }
    impl WifiPort for Native<'_> {
        fn read(&mut self) -> Result<WifiSnapshot> {
            self.wlan.read()
        }
        fn guard(&mut self) -> Result<()> {
            self.action.check_write_boundary()
        }
        fn set(&mut self, change: &WifiChange) -> Result<()> {
            let candidates = self
                .wlan
                .interfaces()?
                .into_iter()
                .filter(|i| guid_text(i.InterfaceGuid) == change.interface_id)
                .collect::<Vec<_>>();
            ensure!(candidates.len() == 1, "WLAN interface identity changed");
            let request = WLAN_PHY_RADIO_STATE {
                dwPhyIndex: change.phy_index,
                dot11SoftwareRadioState: if change.enabled {
                    dot11_radio_state_on
                } else {
                    dot11_radio_state_off
                },
                dot11HardwareRadioState: dot11_radio_state_unknown,
            };
            self.action.check_write_boundary()?;
            let code = unsafe {
                WlanSetInterface(
                    self.wlan.0,
                    &candidates[0].InterfaceGuid,
                    wlan_intf_opcode_radio_state,
                    size_of::<WLAN_PHY_RADIO_STATE>() as u32,
                    (&request as *const WLAN_PHY_RADIO_STATE).cast(),
                    ptr::null(),
                )
            };
            ensure!(code == 0, "WLAN radio setter rejected ({code})");
            for _ in 0..20 {
                let states = self.wlan.query::<WLAN_RADIO_STATE>(
                    &candidates[0].InterfaceGuid,
                    wlan_intf_opcode_radio_state,
                )?;
                ensure!(states.dwNumberOfPhys <= 64, "WLAN PHY count changed");
                if states.PhyRadioState[..states.dwNumberOfPhys as usize]
                    .iter()
                    .any(|p| {
                        p.dwPhyIndex == change.phy_index
                            && state(p.dot11SoftwareRadioState) == Some(change.enabled)
                    })
                {
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            anyhow::bail!("WLAN radio setter accepted but software readback did not settle")
        }
    }
    apply_wifi(
        &mut Native {
            wlan: Wlan::open()?,
            action,
        },
        enabled,
    )
}

// Exact original System/AirplaneMode.cs IUnknown slots, not a guessed WinRT API.
#[repr(C)]
struct RadioObject {
    vtable: *const RadioVtable,
}
#[repr(C)]
struct RadioVtable {
    unknown: [usize; 5],
    get: unsafe extern "system" fn(*mut RadioObject, *mut i32, *mut i32, *mut i32) -> i32,
    set: unsafe extern "system" fn(*mut RadioObject, i32) -> i32,
}
struct Radio(*mut RadioObject);
impl Drop for Radio {
    fn drop(&mut self) {
        type Release = unsafe extern "system" fn(*mut RadioObject) -> u32;
        unsafe {
            let release: Release = std::mem::transmute((*(*self.0).vtable).unknown[2]);
            release(self.0);
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AirplaneStatus {
    pub airplane_on: bool,
    pub raw_state: i32,
    pub opaque_arg2: i32,
    pub opaque_arg3: i32,
}
impl Radio {
    fn open() -> Result<Self> {
        let _com = crate::wmi_native::WmiFacade::connect("ROOT\\CIMV2")?;
        let class = GUID::from_u128(0x581333f6_28db_41be_bc7a_ff201f12f3f6);
        let iid = GUID::from_u128(0xdb3afbfb_08e6_46c6_aa70_bf9a34c30ab7);
        let mut p = ptr::null_mut();
        let hr = unsafe { CoCreateInstance(&class, ptr::null_mut(), CLSCTX_ALL, &iid, &mut p) };
        ensure!(
            hr >= 0 && !p.is_null(),
            "IRadioManager creation failed ({hr:#x})"
        );
        Ok(Self(p.cast()))
    }
    fn read(&self) -> Result<AirplaneStatus> {
        let (mut state, mut arg2, mut arg3) = (0, 0, 0);
        let hr = unsafe { ((*(*self.0).vtable).get)(self.0, &mut state, &mut arg2, &mut arg3) };
        ensure!(hr >= 0, "IRadioManager getter failed ({hr:#x})");
        ensure!([0, 1].contains(&state), "Undefined system-radio state");
        Ok(AirplaneStatus {
            airplane_on: state == 0,
            raw_state: state,
            opaque_arg2: arg2,
            opaque_arg3: arg3,
        })
    }
}
pub fn read_airplane() -> Result<AirplaneStatus> {
    Radio::open()?.read()
}
pub(crate) fn write_airplane(
    _owner: &OwnerToken,
    action: &ExplicitAction,
    on: bool,
) -> Result<serde_json::Value> {
    let radio = Radio::open()?;
    let before = radio.read()?;
    if before.airplane_on != on {
        action.check_write_boundary()?;
        let hr = unsafe { ((*(*radio.0).vtable).set)(radio.0, i32::from(!on)) };
        ensure!(hr >= 0, "IRadioManager setter rejected ({hr:#x})");
    }
    for _ in 0..20 {
        let observed = radio.read()?;
        if observed.airplane_on == on {
            return Ok(
                serde_json::json!({"airplane":observed,"system_radio_state_verified":true,"all_physical_radios_verified":false}),
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    anyhow::bail!("System radio request accepted but readback did not settle")
}
pub fn read() -> serde_json::Value {
    fn capture<T: Serialize>(r: Result<T>) -> serde_json::Value {
        match r {
            Ok(v) => serde_json::json!({"available":true,"value":v}),
            Err(e) => serde_json::json!({"available":false,"error":format!("{e:#}")}),
        }
    }
    serde_json::json!({"wifi":capture(read_wifi()),"airplane":capture(read_airplane())})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn interface(software: Option<bool>) -> WifiInterface {
        WifiInterface {
            id: "fake".into(),
            name: "fake Wi-Fi".into(),
            connection_state: 4,
            connected: Some(false),
            ssid: None,
            ssid_bytes: vec![],
            radios: vec![PhyRadio {
                index: 0,
                software_on: software,
                hardware_on: Some(false),
            }],
            errors: vec![],
        }
    }
    #[test]
    fn hardware_off_does_not_invert_software_radio_and_unknown_is_not_disconnected() {
        let mut i = interface(Some(false));
        let plan = plan_wifi(&summarize(vec![i.clone()]), true).unwrap();
        assert!(plan[0].enabled);
        i.connected = None;
        let s = summarize(vec![i]);
        assert_eq!(s.any_connected, None);
        assert!(!s.all_connections_known);
        assert!(plan_wifi(&summarize(vec![interface(None)]), true).is_err());
    }
    #[test]
    fn multi_interface_preflight_and_partial_completion_are_explicit() {
        struct Fake {
            snapshot: WifiSnapshot,
            writes: usize,
        }
        impl WifiPort for Fake {
            fn read(&mut self) -> Result<WifiSnapshot> {
                Ok(self.snapshot.clone())
            }
            fn guard(&mut self) -> Result<()> {
                Ok(())
            }
            fn set(&mut self, c: &WifiChange) -> Result<()> {
                self.writes += 1;
                ensure!(self.writes < 2, "fake second radio failure");
                self.snapshot
                    .interfaces
                    .iter_mut()
                    .find(|i| i.id == c.interface_id)
                    .unwrap()
                    .radios[0]
                    .software_on = Some(c.enabled);
                Ok(())
            }
        }
        let mut a = interface(Some(false));
        a.id = "a".into();
        let mut b = a.clone();
        b.id = "b".into();
        let mut f = Fake {
            snapshot: summarize(vec![a, b]),
            writes: 0,
        };
        let e = apply_wifi(&mut f, true).unwrap_err().to_string();
        assert_eq!(f.writes, 2);
        assert!(e.contains("\"software_radio_verified\":false"));
        assert!(e.contains("\"interface_id\":\"a\""));
        let mut b = interface(None);
        b.id = "unknown".into();
        f.snapshot.interfaces.push(b);
        f.writes = 0;
        assert!(apply_wifi(&mut f, true).is_err());
        assert_eq!(f.writes, 0);
    }
    #[test]
    fn native_packet_and_original_com_slots_are_exact_without_radio_calls() {
        assert_eq!(size_of::<WLAN_PHY_RADIO_STATE>(), 12);
        assert_eq!(size_of::<WLAN_RADIO_STATE>(), 772);
        assert_eq!(
            std::mem::offset_of!(RadioVtable, get),
            5 * size_of::<usize>()
        );
        assert_eq!(
            std::mem::offset_of!(RadioVtable, set),
            6 * size_of::<usize>()
        );
        assert_eq!(connected(wlan_interface_state_associating), None);
        assert_eq!(connected(wlan_interface_state_disconnected), Some(false));
    }
}
