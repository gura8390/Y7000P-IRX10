//! Current-user Task Scheduler autorun. No task is touched by constructors.
use crate::{domain::ExplicitAction, ownership::OwnerToken};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{path::Path, ptr};
use windows::{
    Win32::{
        Foundation::VARIANT_BOOL,
        System::{
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
            TaskScheduler::*,
            Variant::VARIANT,
        },
    },
    core::{BSTR, Interface},
};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    Security::{GetTokenInformation, TOKEN_QUERY, TokenUser},
    System::{
        Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW},
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};
const MARKER: &str = "Jiks native autorun v1";
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Off,
    Normal,
    Delayed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub mode: Mode,
    pub task_present: bool,
    pub owned: bool,
    pub action_matches: bool,
    pub settings_verified: bool,
    pub run_level_highest: bool,
}
#[derive(Debug, Clone)]
pub struct Spec {
    pub sid: String,
    pub executable: String,
    pub data_root: String,
    pub working_directory: String,
    pub highest: bool,
}
#[derive(Debug, Clone)]
pub struct TaskView {
    pub marker: String,
    pub sid: String,
    pub logon_sid: String,
    pub executable: String,
    pub arguments: String,
    pub working_directory: String,
    pub delay: String,
    pub highest: bool,
    pub enabled: bool,
    pub trigger_enabled: bool,
    pub battery_allowed: bool,
    pub unlimited: bool,
    pub single_action_logon: bool,
    pub safe_lifecycle: bool,
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
pub fn quote_argument(value: &str) -> Result<String> {
    ensure!(
        !value.contains(['\0', '\r', '\n']),
        "Invalid autorun argument"
    );
    let mut result = String::from("\"");
    let mut slash = 0;
    for c in value.chars() {
        if c == '\\' {
            slash += 1;
            continue;
        }
        if c == '"' {
            result.extend(std::iter::repeat_n('\\', slash * 2 + 1));
            result.push('"');
        } else {
            result.extend(std::iter::repeat_n('\\', slash));
            result.push(c);
        }
        slash = 0;
    }
    result.extend(std::iter::repeat_n('\\', slash * 2));
    result.push('"');
    Ok(result)
}
impl Spec {
    pub fn task_name(&self) -> String {
        format!(
            "Jiks_Autorun_{}",
            &crate::boot_logo::digest(self.sid.as_bytes())[..16]
        )
    }
    pub fn arguments(&self) -> Result<String> {
        Ok(format!(
            "--data-dir {} --minimized",
            quote_argument(&self.data_root)?
        ))
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.sid.starts_with("S-1-")
                && self
                    .sid
                    .bytes()
                    .all(|c| c.is_ascii_digit() || c == b'S' || c == b'-'),
            "Invalid current-user SID"
        );
        for p in [&self.executable, &self.data_root, &self.working_directory] {
            ensure!(
                Path::new(p).is_absolute() && !p.contains(['\0', '\r', '\n']),
                "Autorun paths must be absolute normal paths"
            );
        }
        ensure!(
            Path::new(&self.executable).parent() == Some(Path::new(&self.working_directory)),
            "Autorun working directory must match the current executable"
        );
        Ok(())
    }
    pub fn document(&self, mode: Mode) -> Result<String> {
        self.validate()?;
        ensure!(mode != Mode::Off, "Disabled autorun has no task document");
        let sid = xml(&self.sid);
        let exe = xml(&self.executable);
        let args = xml(&self.arguments()?);
        let cwd = xml(&self.working_directory);
        let level = if self.highest {
            "HighestAvailable"
        } else {
            "LeastPrivilege"
        };
        let delay = if mode == Mode::Delayed {
            "PT30S"
        } else {
            "PT0S"
        };
        Ok(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-16\"?><Task version=\"1.3\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\"><RegistrationInfo><Description>{MARKER}</Description></RegistrationInfo><Triggers><LogonTrigger><Enabled>true</Enabled><Delay>{delay}</Delay><UserId>{sid}</UserId></LogonTrigger></Triggers><Principals><Principal id=\"CurrentUser\"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>{level}</RunLevel></Principal></Principals><Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><AllowHardTerminate>false</AllowHardTerminate><StartWhenAvailable>true</StartWhenAvailable><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><Enabled>true</Enabled></Settings><Data>{MARKER}</Data><Actions Context=\"CurrentUser\"><Exec><Command>{exe}</Command><Arguments>{args}</Arguments><WorkingDirectory>{cwd}</WorkingDirectory></Exec></Actions></Task>"
        ))
    }
}
fn owned(view: &TaskView, spec: &Spec) -> bool {
    view.marker == MARKER
        && view.sid.eq_ignore_ascii_case(&spec.sid)
        && view.logon_sid.eq_ignore_ascii_case(&spec.sid)
}
pub fn status(view: Option<&TaskView>, spec: &Spec) -> Result<Status> {
    let Some(v) = view else {
        return Ok(Status {
            mode: Mode::Off,
            task_present: false,
            owned: true,
            action_matches: true,
            settings_verified: true,
            run_level_highest: spec.highest,
        });
    };
    ensure!(
        owned(v, spec),
        "The current user's reserved Jiks autorun task is not owned by this application; no change was authorized"
    );
    let mode = if !v.enabled || !v.trigger_enabled {
        Mode::Off
    } else if v.delay == "PT30S" {
        Mode::Delayed
    } else if v.delay.is_empty() || v.delay == "PT0S" {
        Mode::Normal
    } else {
        anyhow::bail!("Unexpected Jiks logon delay; native task state is unknown")
    };
    Ok(Status {
        mode,
        task_present: true,
        owned: true,
        action_matches: v.executable.eq_ignore_ascii_case(&spec.executable)
            && v.arguments == spec.arguments()?
            && v.working_directory
                .eq_ignore_ascii_case(&spec.working_directory),
        settings_verified: v.battery_allowed
            && v.unlimited
            && v.single_action_logon
            && v.safe_lifecycle
            && v.highest == spec.highest,
        run_level_highest: v.highest,
    })
}
pub trait TaskPort {
    fn read(&mut self) -> Result<Option<TaskView>>;
    fn guard(&mut self) -> Result<()>;
    fn register(&mut self, document: &str) -> Result<()>;
    fn delete(&mut self) -> Result<()>;
}
pub fn apply(port: &mut impl TaskPort, spec: &Spec, mode: Mode) -> Result<Status> {
    spec.validate()?;
    let before = port.read()?;
    let before_status = status(before.as_ref(), spec)?;
    ensure!(
        before_status.action_matches,
        "The reserved task points to a different executable/data directory or has different arguments; refusing to overwrite or delete it"
    );
    if before_status.mode == mode
        && before_status.action_matches
        && before_status.settings_verified
        && (mode != Mode::Off || !before_status.task_present)
    {
        return Ok(before_status);
    }
    let document = if mode != Mode::Off {
        Some(spec.document(mode)?)
    } else {
        None
    };
    if mode == Mode::Off {
        if before.is_some() {
            port.guard()?;
            port.delete()?;
        }
    } else {
        port.guard()?;
        port.register(document.as_deref().unwrap())?;
    }
    let observed = port.read()?;
    let after = status(observed.as_ref(), spec)?;
    ensure!(
        after.mode == mode
            && after.action_matches
            && after.settings_verified
            && (mode != Mode::Off || !after.task_present),
        "Autorun registration/deletion actual readback does not match the request"
    );
    Ok(after)
}
pub fn sid_text(bytes: &[u8]) -> Result<String> {
    ensure!(
        bytes.len() >= 8
            && bytes[0] == 1
            && bytes[1] <= 15
            && bytes.len() == 8 + bytes[1] as usize * 4,
        "Invalid native SID buffer"
    );
    let mut authority = 0u64;
    for b in &bytes[2..8] {
        authority = (authority << 8) | *b as u64;
    }
    let mut result = format!("S-1-{authority}");
    for v in bytes[8..].chunks_exact(4) {
        result.push_str(&format!("-{}", u32::from_le_bytes(v.try_into()?)));
    }
    Ok(result)
}
fn current_sid() -> Result<String> {
    let mut handle = ptr::null_mut();
    ensure!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) } != 0,
        "Current-user token unavailable"
    );
    struct Token(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let token = Token(handle);
    let mut length = 0;
    unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut length) };
    ensure!(
        length >= 16 && length <= 65536,
        "Current-user SID size unavailable/out of bound"
    );
    let mut buffer = vec![0u8; length as usize];
    ensure!(
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } != 0,
        "Current-user SID read failed"
    );
    let user = unsafe {
        ptr::read_unaligned(
            buffer
                .as_ptr()
                .cast::<windows_sys::Win32::Security::TOKEN_USER>(),
        )
    };
    let address = user.User.Sid as usize;
    let begin = buffer.as_ptr() as usize;
    ensure!(
        address >= begin
            && address
                .checked_add(8)
                .is_some_and(|v| v <= begin + buffer.len()),
        "SID pointer is outside token buffer"
    );
    let offset = address - begin;
    let size = 8 + buffer[offset + 1] as usize * 4;
    ensure!(
        offset + size <= buffer.len(),
        "SID subauthority data truncated"
    );
    sid_text(&buffer[offset..offset + size])
}
fn highest(sid: &str) -> Result<bool> {
    let mut enabled = 1u32;
    let mut length = 4;
    let path =
        crate::ownership::wide("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\System")?;
    let name = crate::ownership::wide("EnableLUA")?;
    let code = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            path.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            ptr::null_mut(),
            (&mut enabled as *mut u32).cast(),
            &mut length,
        )
    };
    ensure!(
        code == 0 && length == 4,
        "Cannot establish the system UAC policy for autorun registration"
    );
    Ok(!sid.ends_with("-500") && enabled != 0)
}
fn native_spec(root: &Path) -> Result<Spec> {
    let sid = current_sid()?;
    let executable = std::env::current_exe()?.canonicalize()?;
    let working = executable
        .parent()
        .context("Current executable parent missing")?;
    let highest = highest(&sid)?;
    let spec = Spec {
        sid,
        executable: display_path(&executable)?,
        data_root: display_path(&jiks_store::canonical_directory_path(root)?)?,
        working_directory: display_path(working)?,
        highest,
    };
    spec.validate()?;
    Ok(spec)
}
fn display_path(path: &Path) -> Result<String> {
    let value = path.to_str().context("Non-Unicode autorun path")?;
    Ok(if let Some(v) = value.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{v}")
    } else {
        value.strip_prefix("\\\\?\\").unwrap_or(value).to_string()
    })
}
fn normalized_sid(value: String) -> Result<String> {
    if value.starts_with("S-1-") {
        return Ok(value);
    }
    use windows_sys::Win32::Security::{LookupAccountNameW, SID_NAME_USE};
    let name = crate::ownership::wide(&value)?;
    let (mut sid_len, mut domain_len) = (0, 0);
    let mut kind: SID_NAME_USE = 0;
    unsafe {
        LookupAccountNameW(
            ptr::null(),
            name.as_ptr(),
            ptr::null_mut(),
            &mut sid_len,
            ptr::null_mut(),
            &mut domain_len,
            &mut kind,
        )
    };
    ensure!(
        sid_len >= 8 && sid_len <= 1024 && domain_len <= 32768,
        "Task account SID could not be resolved"
    );
    let mut sid = vec![0u8; sid_len as usize];
    let mut domain = vec![0u16; domain_len as usize];
    ensure!(
        unsafe {
            LookupAccountNameW(
                ptr::null(),
                name.as_ptr(),
                sid.as_mut_ptr().cast(),
                &mut sid_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        } != 0,
        "Task principal name did not resolve to a SID"
    );
    sid.truncate(sid_len as usize);
    sid_text(&sid)
}
fn bstr(call: impl FnOnce(*mut BSTR) -> windows::core::Result<()>) -> Result<String> {
    let mut value = BSTR::new();
    call(&mut value)?;
    Ok(value.to_string())
}
struct Native<'a> {
    folder: ITaskFolder,
    spec: Spec,
    action: Option<&'a ExplicitAction>,
}
impl<'a> Native<'a> {
    fn open(root: &Path, action: Option<&'a ExplicitAction>) -> Result<Self> {
        let _com = crate::wmi_native::WmiFacade::connect("ROOT\\CIMV2")?;
        let service: ITaskService =
            unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)? };
        unsafe {
            service.Connect(
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
            )?
        };
        let folder = unsafe { service.GetFolder(&BSTR::from("\\"))? };
        Ok(Self {
            folder,
            spec: native_spec(root)?,
            action,
        })
    }
}
impl TaskPort for Native<'_> {
    fn read(&mut self) -> Result<Option<TaskView>> {
        let task = match unsafe { self.folder.GetTask(&BSTR::from(self.spec.task_name())) } {
            Ok(t) => t,
            Err(e) if e.code().0 == 0x80070002u32 as i32 => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let definition = unsafe { task.Definition()? };
        let principal = unsafe { definition.Principal()? };
        let triggers = unsafe { definition.Triggers()? };
        let actions = unsafe { definition.Actions()? };
        let settings = unsafe { definition.Settings()? };
        let mut action_count = 0;
        let mut trigger_count = 0;
        unsafe {
            actions.Count(&mut action_count)?;
            triggers.Count(&mut trigger_count)?
        };
        ensure!(
            action_count == 1 && trigger_count == 1,
            "Reserved autorun task has extra actions/triggers"
        );
        let action = unsafe { actions.get_Item(1)? };
        let trigger = unsafe { triggers.get_Item(1)? };
        let mut action_type = TASK_ACTION_TYPE::default();
        let mut trigger_type = TASK_TRIGGER_TYPE2::default();
        unsafe {
            action.Type(&mut action_type)?;
            trigger.Type(&mut trigger_type)?
        };
        ensure!(
            action_type == TASK_ACTION_EXEC && trigger_type == TASK_TRIGGER_LOGON,
            "Reserved autorun task is not one logon executable"
        );
        let exec = action.cast::<IExecAction>()?;
        let logon = trigger.cast::<ILogonTrigger>()?;
        let mut run_level = TASK_RUNLEVEL_TYPE::default();
        unsafe { principal.RunLevel(&mut run_level)? };
        let mut logon_type = TASK_LOGON_TYPE::default();
        unsafe { principal.LogonType(&mut logon_type)? };
        let mut trigger_enabled = VARIANT_BOOL::default();
        unsafe { trigger.Enabled(&mut trigger_enabled)? };
        let (mut disallow, mut stop) = (VARIANT_BOOL::default(), VARIANT_BOOL::default());
        let mut hard_terminate = VARIANT_BOOL::default();
        let mut instances = TASK_INSTANCES_POLICY::default();
        let mut start_when_available = VARIANT_BOOL::default();
        unsafe {
            settings.DisallowStartIfOnBatteries(&mut disallow)?;
            settings.StopIfGoingOnBatteries(&mut stop)?;
            settings.AllowHardTerminate(&mut hard_terminate)?;
            settings.MultipleInstances(&mut instances)?;
            settings.StartWhenAvailable(&mut start_when_available)?;
        };
        Ok(Some(TaskView {
            marker: bstr(|p| unsafe { definition.Data(p) })?,
            sid: normalized_sid(bstr(|p| unsafe { principal.UserId(p) })?)?,
            logon_sid: normalized_sid(bstr(|p| unsafe { logon.UserId(p) })?)?,
            executable: bstr(|p| unsafe { exec.Path(p) })?,
            arguments: bstr(|p| unsafe { exec.Arguments(p) })?,
            working_directory: bstr(|p| unsafe { exec.WorkingDirectory(p) })?,
            delay: bstr(|p| unsafe { logon.Delay(p) })?,
            highest: run_level == TASK_RUNLEVEL_HIGHEST,
            enabled: unsafe { task.Enabled()? }.as_bool(),
            trigger_enabled: trigger_enabled.as_bool(),
            battery_allowed: !disallow.as_bool() && !stop.as_bool(),
            unlimited: bstr(|p| unsafe { settings.ExecutionTimeLimit(p) })? == "PT0S",
            single_action_logon: logon_type == TASK_LOGON_INTERACTIVE_TOKEN,
            safe_lifecycle: !hard_terminate.as_bool()
                && instances == TASK_INSTANCES_IGNORE_NEW
                && start_when_available.as_bool(),
        }))
    }
    fn guard(&mut self) -> Result<()> {
        self.action
            .context("Autorun write requires explicit action")?
            .check_write_boundary()
    }
    fn register(&mut self, document: &str) -> Result<()> {
        self.guard()?;
        unsafe {
            self.folder.RegisterTask(
                &BSTR::from(self.spec.task_name()),
                &BSTR::from(document),
                TASK_CREATE_OR_UPDATE.0,
                &VARIANT::from(self.spec.sid.as_str()),
                &VARIANT::default(),
                TASK_LOGON_INTERACTIVE_TOKEN,
                &VARIANT::default(),
            )?
        };
        Ok(())
    }
    fn delete(&mut self) -> Result<()> {
        self.guard()?;
        unsafe {
            self.folder
                .DeleteTask(&BSTR::from(self.spec.task_name()), 0)?
        };
        Ok(())
    }
}
pub fn read(root: &Path) -> Result<Status> {
    let mut native = Native::open(root, None)?;
    let spec = native.spec.clone();
    status(native.read()?.as_ref(), &spec)
}
pub(crate) fn write(
    _owner: &OwnerToken,
    action: &ExplicitAction,
    root: &Path,
    mode: Mode,
) -> Result<Status> {
    let mut native = Native::open(root, Some(action))?;
    let spec = native.spec.clone();
    let result = apply(&mut native, &spec, mode);
    if let Err(error) = result {
        let observed = native
            .read()
            .ok()
            .flatten()
            .and_then(|v| status(Some(&v), &spec).ok());
        anyhow::bail!("Autorun request failed; actual status={observed:?}: {error:#}");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> Spec {
        Spec {
            sid: "S-1-5-21-1234".into(),
            executable: "D:\\应用\\Jiks.exe".into(),
            data_root: "D:\\数据 & more\\".into(),
            working_directory: "D:\\应用".into(),
            highest: true,
        }
    }
    #[test]
    fn data_directory_alias_does_not_make_an_owned_task_action_stale() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
        let parent = std::env::temp_dir().join(format!(
            "JiksAutorun-LongAncestor-{}-{}",
            std::process::id(),
            crate::monotonic_ms()
        ));
        std::fs::create_dir(&parent).unwrap();
        let wide: Vec<u16> = parent.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut buffer = vec![0u16; 32768];
        let length =
            unsafe { GetShortPathNameW(wide.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) };
        assert!(length > 0 && (length as usize) < buffer.len());
        let alias =
            std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length as usize]));
        let root = alias.join("new-data");
        let mut before = spec();
        before.data_root =
            display_path(&jiks_store::canonical_directory_path(&root).unwrap()).unwrap();
        let task = view(&before, Mode::Normal);
        assert!(!root.exists());
        std::fs::create_dir(&root).unwrap();
        let mut after = before.clone();
        after.data_root = display_path(
            &jiks_store::canonical_directory_path(&root.canonicalize().unwrap()).unwrap(),
        )
        .unwrap();
        assert!(status(Some(&task), &after).unwrap().action_matches);
        let mut other = after.clone();
        other.data_root.push_str("-different");
        assert!(!status(Some(&task), &other).unwrap().action_matches);
        assert!(parent.is_absolute() && parent.starts_with(std::env::temp_dir()));
        std::fs::remove_dir_all(parent).unwrap();
    }
    fn view(s: &Spec, mode: Mode) -> TaskView {
        TaskView {
            marker: MARKER.into(),
            sid: s.sid.clone(),
            logon_sid: s.sid.clone(),
            executable: s.executable.clone(),
            arguments: s.arguments().unwrap(),
            working_directory: s.working_directory.clone(),
            delay: if mode == Mode::Delayed {
                "PT30S"
            } else {
                "PT0S"
            }
            .into(),
            highest: s.highest,
            enabled: true,
            trigger_enabled: true,
            battery_allowed: true,
            unlimited: true,
            single_action_logon: true,
            safe_lifecycle: true,
        }
    }
    #[test]
    fn task_xml_is_current_user_delayed_and_quotes_paths_without_shell() {
        let s = spec();
        let xml = s.document(Mode::Delayed).unwrap();
        assert!(xml.contains("<Delay>PT30S</Delay>"));
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("&amp; more\\\\&quot;"));
        assert!(xml.contains("--minimized"));
        assert!(!xml.contains("rundll32"));
        assert!(s.task_name().starts_with("Jiks_Autorun_"));
        assert_eq!(quote_argument("x\\").unwrap(), "\"x\\\\\"");
        assert!(quote_argument("x\n").is_err());
    }
    #[test]
    fn absent_other_owner_and_stale_actions_are_distinct() {
        let s = spec();
        assert_eq!(status(None, &s).unwrap().mode, Mode::Off);
        let mut v = view(&s, Mode::Normal);
        v.sid = "S-1-5-18".into();
        assert!(status(Some(&v), &s).is_err());
        let mut v = view(&s, Mode::Delayed);
        v.executable = "D:\\old\\Jiks.exe".into();
        let state = status(Some(&v), &s).unwrap();
        assert_eq!(state.mode, Mode::Delayed);
        assert!(!state.action_matches);
        for alter in [0, 1, 2] {
            let mut stale = view(&s, Mode::Normal);
            match alter {
                0 => stale.arguments = "--data-dir D:\\other --minimized".into(),
                1 => stale.working_directory = "D:\\other".into(),
                _ => stale.executable = "D:\\other\\Jiks.exe".into(),
            };
            assert!(!status(Some(&stale), &s).unwrap().action_matches);
        }
        let mut disabled_trigger = view(&s, Mode::Normal);
        disabled_trigger.trigger_enabled = false;
        assert_eq!(status(Some(&disabled_trigger), &s).unwrap().mode, Mode::Off);
        assert_eq!(
            sid_text(&[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]).unwrap(),
            "S-1-5-18"
        );
    }
    #[test]
    fn deadline_and_foreign_task_never_register_and_success_requires_readback() {
        struct Fake {
            state: Option<TaskView>,
            spec: Spec,
            writes: usize,
            expired: bool,
            bad_readback: bool,
        }
        impl TaskPort for Fake {
            fn read(&mut self) -> Result<Option<TaskView>> {
                Ok(self.state.clone())
            }
            fn guard(&mut self) -> Result<()> {
                ensure!(!self.expired, "expired");
                Ok(())
            }
            fn register(&mut self, _: &str) -> Result<()> {
                self.writes += 1;
                if !self.bad_readback {
                    self.state = Some(view(&self.spec, Mode::Normal));
                }
                Ok(())
            }
            fn delete(&mut self) -> Result<()> {
                self.writes += 1;
                if !self.bad_readback {
                    self.state = None;
                }
                Ok(())
            }
        }
        let s = spec();
        let mut f = Fake {
            state: None,
            spec: s.clone(),
            writes: 0,
            expired: true,
            bad_readback: false,
        };
        assert!(apply(&mut f, &s, Mode::Normal).is_err());
        assert_eq!(f.writes, 0);
        f.expired = false;
        let mut foreign = view(&s, Mode::Normal);
        foreign.sid = "S-1-5-18".into();
        f.state = Some(foreign);
        assert!(apply(&mut f, &s, Mode::Normal).is_err());
        assert_eq!(f.writes, 0);
        f.state = None;
        f.bad_readback = true;
        assert!(apply(&mut f, &s, Mode::Normal).is_err());
        assert_eq!(f.writes, 1);
        f.bad_readback = false;
        assert_eq!(apply(&mut f, &s, Mode::Normal).unwrap().mode, Mode::Normal);
        let registered = f.writes;
        assert_eq!(apply(&mut f, &s, Mode::Normal).unwrap().mode, Mode::Normal);
        assert_eq!(f.writes, registered);
        f.bad_readback = true;
        assert!(apply(&mut f, &s, Mode::Off).is_err());
        assert!(f.state.is_some());
        f.bad_readback = false;
        assert_eq!(apply(&mut f, &s, Mode::Off).unwrap().mode, Mode::Off);
        for alter in [0, 1, 2] {
            let mut stale = view(&s, Mode::Normal);
            match alter {
                0 => stale.arguments = "--data-dir D:\\other --minimized".into(),
                1 => stale.executable = "D:\\other\\Jiks.exe".into(),
                _ => stale.logon_sid = "S-1-5-18".into(),
            };
            f.state = Some(stale);
            let writes = f.writes;
            assert!(apply(&mut f, &s, Mode::Off).is_err());
            assert!(apply(&mut f, &s, Mode::Normal).is_err());
            assert_eq!(f.writes, writes);
        }
    }
}
