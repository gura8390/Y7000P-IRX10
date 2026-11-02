//! Explicit GPU device/process management. Metadata does not initialize a GPU
//! library. No process, registry, or PnP mutation happens from discovery.
use crate::{
    domain::ExplicitAction,
    ownership::{OwnerToken, wide},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    mem::{size_of, zeroed},
    ptr,
};
use windows_sys::{
    Win32::{
        Devices::DeviceAndDriverInstallation::*,
        Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0},
        System::{Registry::*, Threading::*},
    },
    core::GUID,
};

const DISPLAY_CLASS: GUID = GUID::from_u128(0x4d36e968_e325_11ce_bfc1_08002be10318);
const PREFERENCE_KEY: &str = "Software\\Microsoft\\DirectX\\UserGpuPreferences";
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuDeviceNode {
    pub instance_id: String,
    pub description: String,
    pub hardware_ids: Vec<String>,
    pub started: bool,
    pub problem_code: u32,
}
struct DeviceSet(HDEVINFO);
impl Drop for DeviceSet {
    fn drop(&mut self) {
        unsafe {
            SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}
fn device_set() -> Result<DeviceSet> {
    let value = unsafe {
        SetupDiGetClassDevsW(&DISPLAY_CLASS, ptr::null(), ptr::null_mut(), DIGCF_PRESENT)
    };
    ensure!(
        value != -1,
        "Enumerating present display devices: {}",
        std::io::Error::last_os_error()
    );
    Ok(DeviceSet(value))
}
fn property(set: HDEVINFO, device: &SP_DEVINFO_DATA, id: u32) -> Result<Vec<String>> {
    let mut ty = 0;
    let mut size = 0;
    unsafe {
        SetupDiGetDeviceRegistryPropertyW(set, device, id, &mut ty, ptr::null_mut(), 0, &mut size);
    }
    ensure!(
        size > 0 && size <= 131072 && size % 2 == 0,
        "Display device property missing/too large"
    );
    let mut bytes = vec![0u16; size as usize / 2];
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            set,
            device,
            id,
            &mut ty,
            bytes.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    };
    ensure!(
        ok != 0,
        "Display property read failed: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        ty == REG_SZ || ty == REG_MULTI_SZ,
        "Unexpected display property type"
    );
    Ok(bytes
        .split(|c| *c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16)
        .collect::<std::result::Result<_, _>>()?)
}
fn node(set: HDEVINFO, device: &SP_DEVINFO_DATA) -> Result<GpuDeviceNode> {
    let mut identity = vec![0u16; 4096];
    let mut needed = 0;
    ensure!(
        unsafe {
            SetupDiGetDeviceInstanceIdW(
                set,
                device,
                identity.as_mut_ptr(),
                identity.len() as u32,
                &mut needed,
            )
        } != 0,
        "Display instance identity unavailable"
    );
    ensure!(
        needed > 0 && needed as usize <= identity.len(),
        "Display identity size invalid"
    );
    let hardware_ids = property(set, device, SPDRP_HARDWAREID)?;
    ensure!(
        hardware_ids
            .iter()
            .any(|v| v.to_ascii_uppercase().starts_with("PCI\\VEN_10DE&")),
        "Selected display device is not NVIDIA PCI hardware"
    );
    let description = property(set, device, SPDRP_FRIENDLYNAME)
        .or_else(|_| property(set, device, SPDRP_DEVICEDESC))?
        .into_iter()
        .next()
        .context("Display description missing")?;
    let (mut status, mut problem) = (0, 0);
    let code = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, device.DevInst, 0) };
    ensure!(
        code == CR_SUCCESS,
        "Display PnP status unavailable ({code})"
    );
    Ok(GpuDeviceNode {
        instance_id: String::from_utf16(&identity[..needed as usize - 1])?,
        description,
        hardware_ids,
        started: status & DN_STARTED != 0,
        problem_code: problem,
    })
}
pub fn devices() -> Result<Vec<GpuDeviceNode>> {
    let set = device_set()?;
    let mut result = Vec::new();
    for index in 0..256 {
        let mut data: SP_DEVINFO_DATA = unsafe { zeroed() };
        data.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
        if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut data) } == 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(259) {
                return Ok(result);
            }
            return Err(e.into());
        }
        let ids = property(set.0, &data, SPDRP_HARDWAREID)?;
        if ids
            .iter()
            .any(|id| id.to_ascii_uppercase().starts_with("PCI\\VEN_10DE&"))
        {
            result.push(node(set.0, &data)?);
        }
    }
    anyhow::bail!("Display device enumeration exceeded its bound")
}
pub(crate) fn restart(
    _owner: &OwnerToken,
    action: &ExplicitAction,
    instance_id: &str,
) -> Result<Value> {
    ensure!(
        instance_id
            .to_ascii_uppercase()
            .starts_with("PCI\\VEN_10DE&"),
        "Exact NVIDIA PCI instance is required"
    );
    let set = device_set()?;
    let id = wide(instance_id)?;
    let mut data: SP_DEVINFO_DATA = unsafe { zeroed() };
    data.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
    ensure!(
        unsafe { SetupDiOpenDeviceInfoW(set.0, id.as_ptr(), ptr::null_mut(), 0, &mut data) } != 0,
        "Selected GPU PnP instance is absent"
    );
    let before = node(set.0, &data)?;
    ensure!(
        before.instance_id.eq_ignore_ascii_case(instance_id),
        "GPU instance changed"
    );
    let params = restart_parameters();
    ensure!(
        unsafe {
            SetupDiSetClassInstallParamsW(
                set.0,
                &data,
                &params.ClassInstallHeader,
                size_of::<SP_PROPCHANGE_PARAMS>() as u32,
            )
        } != 0,
        "GPU restart preparation failed: {}",
        std::io::Error::last_os_error()
    );
    action.check_write_boundary()?;
    ensure!(
        unsafe { SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, set.0, &data) } != 0,
        "GPU device restart failed: {}",
        std::io::Error::last_os_error()
    );
    let after = node(set.0, &data)?;
    let mut install: SP_DEVINSTALL_PARAMS_W = unsafe { zeroed() };
    install.cbSize = size_of::<SP_DEVINSTALL_PARAMS_W>() as u32;
    ensure!(
        unsafe { SetupDiGetDeviceInstallParamsW(set.0, &data, &mut install) } != 0,
        "GPU restart was requested but reboot flags could not be read"
    );
    let pending_reboot = install.Flags & (DI_NEEDRESTART | DI_NEEDREBOOT) != 0;
    ensure!(
        pending_reboot || (after.started && after.problem_code == 0),
        "GPU restart returned but device did not verify started/healthy"
    );
    Ok(
        json!({"device":after,"restart_api_accepted":true,"pending_reboot":pending_reboot,"physical_reinitialization_verified":false}),
    )
}
fn restart_parameters() -> SP_PROPCHANGE_PARAMS {
    SP_PROPCHANGE_PARAMS {
        ClassInstallHeader: SP_CLASSINSTALL_HEADER {
            cbSize: size_of::<SP_CLASSINSTALL_HEADER>() as u32,
            InstallFunction: DIF_PROPERTYCHANGE,
        },
        StateChange: DICS_PROPCHANGE,
        Scope: DICS_FLAG_GLOBAL,
        HwProfile: 0,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub creation_filetime: u64,
    pub executable: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuProcess {
    pub identity: ProcessIdentity,
    pub preference: GpuPreference,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuPreference {
    Automatic,
    Integrated,
    Discrete,
}
impl GpuPreference {
    pub fn parse(v: &str) -> Result<Self> {
        Ok(match v.to_ascii_lowercase().as_str() {
            "auto" | "automatic" | "default" => Self::Automatic,
            "integrated" => Self::Integrated,
            "discrete" => Self::Discrete,
            _ => anyhow::bail!("Unknown GPU preference"),
        })
    }
}
pub fn preference_value(existing: Option<&str>, preference: GpuPreference) -> Option<String> {
    let mut flags = existing
        .unwrap_or("")
        .split(';')
        .filter(|v| {
            !v.is_empty()
                && !v
                    .split('=')
                    .next()
                    .unwrap_or("")
                    .eq_ignore_ascii_case("GpuPreference")
        })
        .map(str::to_string)
        .collect::<Vec<_>>();
    match preference {
        GpuPreference::Integrated => flags.push("GpuPreference=1".into()),
        GpuPreference::Discrete => flags.push("GpuPreference=2".into()),
        GpuPreference::Automatic => {}
    }
    (!flags.is_empty()).then(|| format!("{};", flags.join(";")))
}
fn parse_preference(value: Option<&str>) -> GpuPreference {
    let v = value
        .unwrap_or("")
        .split(';')
        .find_map(|flag| flag.strip_prefix("GpuPreference="));
    match v {
        Some("1") => GpuPreference::Integrated,
        Some("2") => GpuPreference::Discrete,
        _ => GpuPreference::Automatic,
    }
}
fn read_preference(path: &str) -> Result<Option<String>> {
    let key = wide(PREFERENCE_KEY)?;
    let name = wide(path)?;
    let mut bytes = 0;
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut bytes,
        )
    };
    if code == 2 {
        return Ok(None);
    }
    ensure!(
        code == 0 && bytes > 0 && bytes <= 131072 && bytes % 2 == 0,
        "GPU preference read failed ({code})"
    );
    let mut value = vec![0u16; bytes as usize / 2];
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            value.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    ensure!(code == 0, "GPU preference read failed ({code})");
    let len = value
        .iter()
        .position(|v| *v == 0)
        .context("GPU preference is unterminated")?;
    Ok(Some(String::from_utf16(&value[..len])?))
}
struct ProcessHandle(HANDLE);
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn identity(handle: HANDLE, pid: u32) -> Result<ProcessIdentity> {
    let (mut created, mut exited, mut kernel, mut user): (_, _, _, _) = unsafe {
        (
            zeroed::<FILETIME>(),
            zeroed::<FILETIME>(),
            zeroed::<FILETIME>(),
            zeroed::<FILETIME>(),
        )
    };
    ensure!(
        unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } != 0,
        "Cannot verify selected process creation time"
    );
    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    ensure!(
        unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut length) } != 0,
        "Cannot verify selected process executable"
    );
    Ok(ProcessIdentity {
        pid,
        creation_filetime: u64::from(created.dwLowDateTime)
            | (u64::from(created.dwHighDateTime) << 32),
        executable: String::from_utf16(&path[..length as usize])?,
    })
}
fn open_process(expected: &ProcessIdentity, terminate: bool) -> Result<ProcessHandle> {
    validate_identities(std::slice::from_ref(expected))?;
    ensure!(
        expected.pid != unsafe { GetCurrentProcessId() },
        "Refusing to target the current coordinator"
    );
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION
                | if terminate {
                    PROCESS_TERMINATE | PROCESS_SYNCHRONIZE
                } else {
                    0
                },
            0,
            expected.pid,
        )
    };
    ensure!(
        !handle.is_null(),
        "Selected process cannot be opened: {}",
        std::io::Error::last_os_error()
    );
    let handle = ProcessHandle(handle);
    let actual = identity(handle.0, expected.pid)?;
    ensure!(
        actual.creation_filetime == expected.creation_filetime
            && actual.executable.eq_ignore_ascii_case(&expected.executable),
        "Process PID was reused or executable changed; no mutation performed"
    );
    Ok(handle)
}
pub fn process_metadata(pid: u32) -> Result<GpuProcess> {
    ensure!(pid > 4, "Invalid system process");
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    ensure!(
        !handle.is_null(),
        "GPU process identity permission denied/unavailable"
    );
    let handle = ProcessHandle(handle);
    let identity = identity(handle.0, pid)?;
    let preference = parse_preference(read_preference(&identity.executable)?.as_deref());
    Ok(GpuProcess {
        identity,
        preference,
    })
}
pub fn validate_identities(targets: &[ProcessIdentity]) -> Result<()> {
    ensure!(
        !targets.is_empty() && targets.len() <= 64,
        "Select 1..64 exact GPU process identities"
    );
    let mut pids = BTreeSet::new();
    for target in targets {
        ensure!(
            target.pid > 4 && target.creation_filetime > 0 && pids.insert(target.pid),
            "Invalid/duplicate process identity"
        );
        ensure!(
            !target.executable.contains('\0')
                && std::path::Path::new(&target.executable).is_absolute(),
            "Process executable identity must be an absolute path"
        );
    }
    Ok(())
}
pub trait ProcessPort {
    type Prepared;
    fn guard(&mut self) -> Result<()>;
    fn prepare(&mut self, target: &ProcessIdentity) -> Result<Self::Prepared>;
    fn terminate_and_wait(&mut self, prepared: &Self::Prepared) -> Result<()>;
}
pub fn kill_selected(port: &mut impl ProcessPort, targets: &[ProcessIdentity]) -> Result<Value> {
    validate_identities(targets)?;
    port.guard()?;
    let prepared = targets
        .iter()
        .map(|target| port.prepare(target))
        .collect::<Result<Vec<_>>>()?;
    let mut completed = Vec::new();
    for (target, handle) in targets.iter().zip(prepared.iter()) {
        let result = port.guard().and_then(|_| port.terminate_and_wait(handle));
        if let Err(error) = result {
            anyhow::bail!(
                "Selected-process operation stopped; completed PIDs {:?}; next PID {}; no unselected child process was targeted: {error:#}",
                completed,
                target.pid
            );
        }
        completed.push(target.pid);
    }
    Ok(
        json!({"completed_pids":completed,"unselected_children_terminated":false,"process_exit_verified":true}),
    )
}
struct NativeProcesses<'a> {
    action: &'a ExplicitAction,
}
impl ProcessPort for NativeProcesses<'_> {
    type Prepared = ProcessHandle;
    fn guard(&mut self) -> Result<()> {
        self.action.check_write_boundary()
    }
    fn prepare(&mut self, target: &ProcessIdentity) -> Result<ProcessHandle> {
        open_process(target, true)
    }
    fn terminate_and_wait(&mut self, process: &ProcessHandle) -> Result<()> {
        self.action.check_write_boundary()?;
        ensure!(
            unsafe { TerminateProcess(process.0, 1) } != 0,
            "TerminateProcess failed: {}",
            std::io::Error::last_os_error()
        );
        // TerminateProcess is asynchronous. Keep the same worker/handles/owner until
        // the actual process has exited; a caller ticket timeout cannot fake this.
        ensure!(
            unsafe { WaitForSingleObject(process.0, INFINITE) } == WAIT_OBJECT_0,
            "Process exit could not be verified after termination request"
        );
        Ok(())
    }
}
pub(crate) fn kill(
    _owner: &OwnerToken,
    action: &ExplicitAction,
    targets: &[ProcessIdentity],
) -> Result<Value> {
    kill_selected(&mut NativeProcesses { action }, targets)
}
pub(crate) fn set_preference(
    _owner: &OwnerToken,
    action: &ExplicitAction,
    target: &ProcessIdentity,
    preference: GpuPreference,
) -> Result<Value> {
    let _process = open_process(target, false)?;
    let before = read_preference(&target.executable)?;
    let after = preference_value(before.as_deref(), preference);
    if before == after {
        return Ok(json!({"identity":target,"preference":preference,"changed":false}));
    }
    let path = wide(PREFERENCE_KEY)?;
    let name = wide(&target.executable)?;
    let mut key = ptr::null_mut();
    let mut disposition = 0;
    action.check_write_boundary()?;
    let code = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            ptr::null(),
            0,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            ptr::null(),
            &mut key,
            &mut disposition,
        )
    };
    ensure!(code == 0, "GPU preference key open failed ({code})");
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
    let _key = Key(key);
    action.check_write_boundary()?;
    ensure!(
        read_preference(&target.executable)? == before,
        "GPU preference flags changed externally before write"
    );
    action.check_deadline()?;
    let code = if let Some(value) = &after {
        let value = wide(value)?;
        unsafe {
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            )
        }
    } else {
        unsafe { RegDeleteValueW(key, name.as_ptr()) }
    };
    ensure!(
        code == 0 || after.is_none() && code == 2,
        "GPU preference write failed ({code})"
    );
    ensure!(
        read_preference(&target.executable)? == after,
        "GPU preference/unknown flags readback mismatch"
    );
    Ok(
        json!({"identity":target,"preference":preference,"changed":true,"existing_process_migration_verified":false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_packet_uses_propertychange_not_disable_enable() {
        let p = restart_parameters();
        assert_eq!(p.ClassInstallHeader.cbSize, 8);
        assert_eq!(p.ClassInstallHeader.InstallFunction, DIF_PROPERTYCHANGE);
        assert_eq!(p.StateChange, DICS_PROPCHANGE);
        assert_eq!(p.Scope, DICS_FLAG_GLOBAL);
        assert_eq!(size_of::<SP_PROPCHANGE_PARAMS>(), 20);
    }
    #[test]
    fn preference_preserves_unknown_flags_even_when_returning_to_automatic() {
        assert_eq!(
            preference_value(
                Some("AutoHDREnable=1;GpuPreference=2;Custom=00;"),
                GpuPreference::Automatic
            ),
            Some("AutoHDREnable=1;Custom=00;".into())
        );
        assert_eq!(
            preference_value(None, GpuPreference::Discrete),
            Some("GpuPreference=2;".into())
        );
        assert_eq!(
            preference_value(Some("GpuPreference=1;"), GpuPreference::Automatic),
            None
        );
    }
    struct Fake {
        prepared: Vec<u32>,
        killed: Vec<u32>,
        fail_pid: u32,
        budget: u32,
    }
    impl ProcessPort for Fake {
        type Prepared = u32;
        fn guard(&mut self) -> Result<()> {
            ensure!(self.budget > 0, "expired");
            Ok(())
        }
        fn prepare(&mut self, p: &ProcessIdentity) -> Result<u32> {
            ensure!(p.pid != self.fail_pid, "PID reused");
            self.prepared.push(p.pid);
            Ok(p.pid)
        }
        fn terminate_and_wait(&mut self, p: &u32) -> Result<()> {
            self.killed.push(*p);
            self.budget -= 1;
            Ok(())
        }
    }
    fn target(pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            creation_filetime: 100,
            executable: format!("C:\\fixture\\{pid}.exe"),
        }
    }
    #[test]
    fn reused_last_pid_rejects_the_entire_batch_before_any_kill() {
        let mut f = Fake {
            prepared: vec![],
            killed: vec![],
            fail_pid: 11,
            budget: 3,
        };
        assert!(kill_selected(&mut f, &[target(10), target(11)]).is_err());
        assert!(f.killed.is_empty());
    }
    #[test]
    fn deadline_after_completed_first_process_never_kills_second() {
        let mut f = Fake {
            prepared: vec![],
            killed: vec![],
            fail_pid: 0,
            budget: 1,
        };
        assert!(kill_selected(&mut f, &[target(10), target(11)]).is_err());
        assert_eq!(f.prepared, vec![10, 11]);
        assert_eq!(f.killed, vec![10]);
    }
    #[test]
    fn zero_system_duplicate_and_missing_identity_are_rejected() {
        assert!(validate_identities(&[target(4)]).is_err());
        assert!(validate_identities(&[target(10), target(10)]).is_err());
        let mut p = target(10);
        p.creation_filetime = 0;
        assert!(validate_identities(&[p]).is_err());
    }
}
