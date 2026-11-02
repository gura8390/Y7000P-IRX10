//! Original Lenovo dGPU notification contract. This reports actual display
//! arrival availability; it never forces a false status or claims physical sleep.
use crate::{
    domain::ExplicitAction,
    hybrid::{BiosGraphics, HybridSnapshot, IgpuBackend},
    ownership::OwnerToken,
    wmi_native::{MethodInput, WmiFacade},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    mem::{size_of, zeroed},
    ptr,
};
use windows_sys::Win32::Devices::{
    DeviceAndDriverInstallation::*, Display::GUID_DISPLAY_DEVICE_ARRIVAL,
};
pub fn validate_policy(snapshot: &HybridSnapshot, ac: bool) -> Result<()> {
    ensure!(
        snapshot.gsync == Some(false) || snapshot.bios_current == Some(BiosGraphics::Uma),
        "Discrete/unknown mux route cannot request GPU sleep notification"
    );
    ensure!(
        snapshot.igpu_mode == Some(1)
            || snapshot.igpu_mode == Some(2) && !ac
            || snapshot.bios_current == Some(BiosGraphics::Uma),
        "Sleep notification requires IGPU-only, UMA, or Auto on DC"
    );
    Ok(())
}
pub fn parse_hardware_id(raw: &str) -> Result<(u16, u16)> {
    let upper = raw.to_ascii_uppercase();
    fn part(s: &str, key: &str) -> Result<u16> {
        let mut indices = s.match_indices(key);
        let i = indices
            .next()
            .context("Firmware PCI hardware ID token missing")?
            .0;
        ensure!(
            indices.next().is_none(),
            "Duplicate firmware PCI hardware ID token"
        );
        let hex = s
            .get(i + key.len()..i + key.len() + 4)
            .context("Short firmware PCI token")?;
        ensure!(
            hex.bytes().all(|b| b.is_ascii_hexdigit()),
            "Bad firmware PCI hex token"
        );
        Ok(u16::from_str_radix(hex, 16)?)
    }
    let vendor = part(&upper, "VEN_")?;
    let device = part(&upper, "DEV_")?;
    ensure!(vendor == 0x10de, "Firmware dGPU vendor is not NVIDIA");
    Ok((vendor, device))
}
fn parameters(
    values: impl IntoIterator<Item = (&'static str, MethodInput)>,
) -> BTreeMap<String, MethodInput> {
    values.into_iter().map(|(k, v)| (k.into(), v)).collect()
}
fn string<'a>(v: &'a BTreeMap<String, Value>, key: &str) -> Result<&'a str> {
    v.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v.as_str())
        .context("Firmware dGPU string ID missing")
}
fn hardware_id(f: &WmiFacade, backend: IgpuBackend) -> Result<(u16, u16)> {
    match backend {
        IgpuBackend::GameZone => parse_hardware_id(string(
            &f.method_read("LENOVO_GAMEZONE_DATA", "GetDGPUHWId", BTreeMap::new())?,
            "Data",
        )?),
        IgpuBackend::Capability => {
            let output = f.method_read(
                "LENOVO_OTHER_METHOD",
                "GetFeatureValue",
                parameters([("IDs", MethodInput::U32(0x02090000))]),
            )?;
            let v = crate::system_controls::integer(&output, "Value")?;
            ensure!(v as u16 == 0x10de, "Firmware dGPU vendor mismatch");
            Ok((v as u16, (v >> 16) as u16))
        }
    }
}
fn availability(vendor: u16, device: u16) -> Result<bool> {
    let set = unsafe {
        SetupDiGetClassDevsW(
            &GUID_DISPLAY_DEVICE_ARRIVAL,
            ptr::null(),
            ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_PROFILE | DIGCF_DEVICEINTERFACE,
        )
    };
    ensure!(set != -1, "Display arrival metadata enumeration failed");
    struct Set(HDEVINFO);
    impl Drop for Set {
        fn drop(&mut self) {
            unsafe {
                SetupDiDestroyDeviceInfoList(self.0);
            }
        }
    }
    let _set = Set(set);
    for index in 0..256 {
        let mut interface: SP_DEVICE_INTERFACE_DATA = unsafe { zeroed() };
        interface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        if unsafe {
            SetupDiEnumDeviceInterfaces(
                set,
                ptr::null(),
                &GUID_DISPLAY_DEVICE_ARRIVAL,
                index,
                &mut interface,
            )
        } == 0
        {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(259) {
                return Ok(false);
            }
            return Err(e.into());
        }
        let mut required = 0;
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &interface,
                ptr::null_mut(),
                0,
                &mut required,
                ptr::null_mut(),
            );
        }
        ensure!(
            (8..=131072).contains(&required) && required % 2 == 0,
            "Display arrival detail length invalid"
        );
        let mut data = vec![0u64; (required as usize).div_ceil(8)];
        let ptr = data
            .as_mut_ptr()
            .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        unsafe {
            (*ptr).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        }
        let mut dev: SP_DEVINFO_DATA = unsafe { zeroed() };
        dev.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
        ensure!(
            unsafe {
                SetupDiGetDeviceInterfaceDetailW(
                    set,
                    &interface,
                    ptr,
                    required,
                    ptr::null_mut(),
                    &mut dev,
                )
            } != 0,
            "Display arrival detail failed"
        );
        let raw = unsafe {
            std::slice::from_raw_parts(
                data.as_ptr().cast::<u8>().add(4).cast::<u16>(),
                (required as usize - 4) / 2,
            )
        };
        let end = raw
            .iter()
            .position(|v| *v == 0)
            .context("Display arrival path unterminated")?;
        let path = String::from_utf16(&raw[..end])?;
        if parse_hardware_id(&path).ok() != Some((vendor, device)) {
            continue;
        }
        let (mut status, mut problem) = (0, 0);
        let code = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, dev.DevInst, 0) };
        ensure!(
            code == CR_SUCCESS,
            "Display arrival PnP status failed ({code})"
        );
        if status & DN_HAS_PROBLEM == 0 && problem == 0 {
            return Ok(true);
        }
    }
    anyhow::bail!("Display arrival enumeration exceeded its bound")
}
pub(crate) struct SleepRequest {
    backend: IgpuBackend,
    id: (u16, u16),
    available: bool,
}
pub(crate) fn prepare(f: &WmiFacade) -> Result<SleepRequest> {
    let snapshot = crate::hybrid::read(f);
    validate_policy(&snapshot, crate::ac_connected()?)?;
    let backend = snapshot
        .igpu_backend
        .context("No verified IGPU notification backend")?;
    let id = hardware_id(f, backend)?;
    let available = availability(id.0, id.1)?;
    let request = SleepRequest {
        backend,
        id,
        available,
    };
    let (c, m, p) = request.method();
    f.validate_inputs(c, m, &p)?;
    Ok(request)
}
impl SleepRequest {
    fn method(&self) -> (&'static str, &'static str, BTreeMap<String, MethodInput>) {
        match self.backend {
            IgpuBackend::GameZone => (
                "LENOVO_GAMEZONE_DATA",
                "NotifyDGPUStatus",
                parameters([("Status", MethodInput::U32(u32::from(self.available)))]),
            ),
            IgpuBackend::Capability => (
                "LENOVO_OTHER_METHOD",
                "SetFeatureValue",
                parameters([
                    ("IDs", MethodInput::U32(0x02070000)),
                    ("value", MethodInput::U32(u32::from(self.available))),
                ]),
            ),
        }
    }
}
pub(crate) fn execute(
    f: &WmiFacade,
    owner: &OwnerToken,
    action: &ExplicitAction,
    request: SleepRequest,
) -> Result<Value> {
    let (c, m, p) = request.method();
    f.method_write_guarded(owner, action, c, m, p, || {
        let snapshot = crate::hybrid::read(f);
        validate_policy(&snapshot, crate::ac_connected()?)?;
        ensure!(
            snapshot.igpu_backend == Some(request.backend)
                && hardware_id(f, request.backend)? == request.id,
            "dGPU notification identity/backend changed"
        );
        ensure!(
            availability(request.id.0, request.id.1)? == request.available,
            "Display arrival availability changed before firmware notification"
        );
        Ok(())
    })?;
    let observed = availability(request.id.0, request.id.1)?;
    Ok(
        json!({"notification_accepted":true,"reported_available":request.available,"display_arrival_available_after":observed,"physical_sleep_verified":false,"processes_terminated":false}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pci_ids_match_original_both_formats_and_never_guess_missing_id() {
        assert_eq!(
            parse_hardware_id("PCIVEN_10DE&DEV_2D58").unwrap(),
            (0x10de, 0x2d58)
        );
        assert_eq!(
            parse_hardware_id("\\\\?\\pci#ven_10de&dev_2d58#fixture").unwrap(),
            (0x10de, 0x2d58)
        );
        assert!(parse_hardware_id("PCI\\VEN_8086&DEV_1234").is_err());
        assert!(parse_hardware_id("PCI\\VEN_10DE").is_err());
    }
    #[test]
    fn sleep_requires_explicit_sleep_capable_route_and_never_discrete_or_auto_ac() {
        let mut s = HybridSnapshot {
            gsync: Some(false),
            igpu_mode: Some(2),
            ..Default::default()
        };
        assert!(validate_policy(&s, true).is_err());
        assert!(validate_policy(&s, false).is_ok());
        s.gsync = Some(true);
        assert!(validate_policy(&s, false).is_err());
        s.gsync = Some(false);
        s.igpu_mode = Some(1);
        assert!(validate_policy(&s, true).is_ok());
    }
}
