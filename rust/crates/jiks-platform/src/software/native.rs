use super::*;
use anyhow::bail;
use std::{
    mem::{size_of, zeroed},
    path::PathBuf,
    ptr, thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    Win32::{
        Foundation::VARIANT_BOOL,
        System::{
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
            TaskScheduler::{IRegisteredTask, ITaskService, TASK_ENUM_HIDDEN, TaskScheduler},
            Variant::VARIANT,
        },
    },
    core::BSTR,
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, ERROR_NO_MORE_ITEMS,
        ERROR_PATH_NOT_FOUND, ERROR_SERVICE_DOES_NOT_EXIST, FILETIME, HANDLE, WAIT_OBJECT_0,
    },
    Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW},
    System::{
        Diagnostics::ToolHelp::*,
        Environment::ExpandEnvironmentStringsW,
        Registry::*,
        Services::*,
        Threading::{
            GetExitCodeProcess, GetProcessTimes, INFINITE, OpenProcess,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
            QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
        },
    },
};
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn win(ok: bool, name: &str) -> Result<()> {
    ensure!(ok, "{name}: {}", std::io::Error::last_os_error());
    Ok(())
}
struct Service(SC_HANDLE);
impl Drop for Service {
    fn drop(&mut self) {
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}
struct Reg(HKEY);
impl Drop for Reg {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}
struct Process(HANDLE);
impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn hive(name: &str) -> Result<HKEY> {
    match name {
        "HKCU" => Ok(HKEY_CURRENT_USER),
        "HKLM" => Ok(HKEY_LOCAL_MACHINE),
        _ => bail!("Invalid registry hive"),
    }
}
fn open_reg(base: HKEY, path: &str, access: u32) -> Result<Option<Reg>> {
    let mut key = ptr::null_mut();
    let code = unsafe { RegOpenKeyExW(base, wide(path).as_ptr(), 0, access, &mut key) };
    if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND {
        return Ok(None);
    }
    ensure!(code == 0, "Open registry key failed ({code})");
    Ok(Some(Reg(key)))
}
fn query(key: &Reg, name: &str) -> Result<Option<(u32, Vec<u8>)>> {
    let name = wide(name);
    let (mut kind, mut length) = (0, 0);
    let code = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            ptr::null_mut(),
            &mut kind,
            ptr::null_mut(),
            &mut length,
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    ensure!(code == 0, "Read registry value length failed ({code})");
    ensure!(length <= 131072, "Registry value exceeds budget");
    let mut bytes = vec![0; length as usize];
    let code = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            ptr::null_mut(),
            &mut kind,
            bytes.as_mut_ptr(),
            &mut length,
        )
    };
    ensure!(code == 0, "Read registry value failed ({code})");
    bytes.truncate(length as usize);
    Ok(Some((kind, bytes)))
}
fn text_value(kind: u32, bytes: &[u8]) -> Result<String> {
    ensure!(
        (kind == REG_SZ || kind == REG_EXPAND_SZ) && bytes.len() % 2 == 0,
        "Expected registry string"
    );
    let units = bytes
        .chunks_exact(2)
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .take_while(|v| *v != 0)
        .collect::<Vec<_>>();
    Ok(String::from_utf16(&units)?)
}
fn names(key: &Reg, values: bool) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for index in 0..10000 {
        let mut buffer = vec![0u16; 16384];
        let mut length = buffer.len() as u32;
        let code = unsafe {
            if values {
                RegEnumValueW(
                    key.0,
                    index,
                    buffer.as_mut_ptr(),
                    &mut length,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            } else {
                RegEnumKeyExW(
                    key.0,
                    index,
                    buffer.as_mut_ptr(),
                    &mut length,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            }
        };
        if code == ERROR_NO_MORE_ITEMS {
            return Ok(out);
        }
        ensure!(code == 0, "Enumerate registry names failed ({code})");
        out.push(String::from_utf16(&buffer[..length as usize])?);
    }
    bail!("Registry name budget exceeded")
}
const RUN: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const APPROVED: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";
const APPDATA: &str = "Software\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppModel\\SystemAppData";
fn expand(value: &str) -> Result<String> {
    let input = wide(value);
    let required = unsafe { ExpandEnvironmentStringsW(input.as_ptr(), ptr::null_mut(), 0) };
    ensure!(
        required > 0 && required <= 32768,
        "Cannot expand service executable path"
    );
    let mut out = vec![0u16; required as usize];
    let written = unsafe { ExpandEnvironmentStringsW(input.as_ptr(), out.as_mut_ptr(), required) };
    ensure!(
        written > 0 && written <= required,
        "Executable environment changed during read"
    );
    Ok(String::from_utf16(&out[..written as usize - 1])?)
}
fn executable(command: &str) -> Result<String> {
    let value = command.trim();
    let value = if let Some(quoted) = value.strip_prefix('"') {
        quoted
            .split_once('"')
            .context("Unclosed executable quote")?
            .0
    } else {
        let end = value
            .to_ascii_lowercase()
            .find(".exe")
            .context("Executable suffix missing")?;
        &value[..end + 4]
    };
    let value = expand(value)?;
    let value = value.strip_prefix("\\??\\").unwrap_or(&value);
    let value = if value.to_ascii_lowercase().starts_with("\\systemroot\\") {
        format!("{}\\{}", std::env::var("SystemRoot")?, &value[12..])
    } else {
        value.into()
    };
    ensure!(
        PathBuf::from(&value).is_absolute(),
        "Vendor executable path must be absolute"
    );
    Ok(value)
}
fn normalized(path: &str) -> String {
    path.trim_start_matches("\\\\?\\")
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}
struct Roots {
    vendor: Vec<String>,
    windows_apps: Vec<String>,
    driver_store: String,
    zone: Vec<String>,
    vantage: Vec<String>,
}
impl Roots {
    fn read() -> Result<Self> {
        let program = std::env::var("ProgramFiles")?;
        let x86 = std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| program.clone());
        let data = std::env::var("ProgramData")?;
        let windows = std::env::var("SystemRoot")?;
        let vendor = vec![
            format!("{program}\\Lenovo"),
            format!("{x86}\\Lenovo"),
            format!("{data}\\Lenovo"),
            format!("{windows}\\Lenovo"),
        ];
        Ok(Self {
            vendor: vendor.iter().map(|p| normalized(p)).collect(),
            windows_apps: vec![normalized(&format!("{program}\\WindowsApps"))],
            driver_store: normalized(&format!("{windows}\\System32\\DriverStore\\FileRepository")),
            zone: vec![
                normalized(&format!("{program}\\Lenovo\\LegionZone")),
                normalized(&format!("{x86}\\Lenovo\\LegionZone")),
            ],
            vantage: vec![normalized(&format!("{data}\\Lenovo\\Vantage"))],
        })
    }
    fn inside(path: &str, root: &str) -> bool {
        path.strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('\\'))
    }
    fn allowed(&self, path: &str) -> bool {
        let path = normalized(path);
        self.vendor
            .iter()
            .chain(&self.windows_apps)
            .any(|root| Self::inside(&path, root))
            || Self::inside(&path, &self.driver_store)
    }
    fn startup(&self, id: &str, path: &str) -> bool {
        let path = normalized(path);
        match id {
            "zone" => self.zone.iter().any(|root| Self::inside(&path, root)),
            "vantage" => self.vantage.iter().any(|root| Self::inside(&path, root)),
            _ => false,
        }
    }
}
fn version_fields(path: &str) -> Result<(String, String)> {
    let path = wide(path);
    let mut ignored = 0;
    let length = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), &mut ignored) };
    ensure!(
        length > 0 && length <= 4 * 1024 * 1024,
        "Vendor executable version metadata unavailable"
    );
    let mut data = vec![0u8; length as usize];
    win(
        unsafe { GetFileVersionInfoW(path.as_ptr(), 0, length, data.as_mut_ptr().cast()) } != 0,
        "Read executable metadata",
    )?;
    let mut translation = ptr::null_mut();
    let mut translation_size = 0;
    win(
        unsafe {
            VerQueryValueW(
                data.as_ptr().cast(),
                wide("\\VarFileInfo\\Translation").as_ptr(),
                &mut translation,
                &mut translation_size,
            )
        } != 0,
        "Read executable version translation",
    )?;
    ensure!(
        translation_size >= 4 && translation_size <= 4096,
        "Invalid executable translation metadata"
    );
    let translations = unsafe {
        std::slice::from_raw_parts(translation.cast::<u16>(), translation_size as usize / 2)
    };
    for pair in translations.chunks_exact(2) {
        let read = |name: &str| -> Result<String> {
            let key = wide(&format!(
                "\\StringFileInfo\\{:04x}{:04x}\\{name}",
                pair[0], pair[1]
            ));
            let mut value = ptr::null_mut();
            let mut length = 0;
            win(
                unsafe {
                    VerQueryValueW(data.as_ptr().cast(), key.as_ptr(), &mut value, &mut length)
                } != 0,
                "Read vendor identity field",
            )?;
            ensure!(
                length > 0 && length <= 32768,
                "Invalid vendor identity field"
            );
            let units = unsafe { std::slice::from_raw_parts(value.cast::<u16>(), length as usize) };
            Ok(String::from_utf16(
                units.split(|v| *v == 0).next().unwrap_or(&[]),
            )?)
        };
        if let (Ok(company), Ok(description)) = (read("CompanyName"), read("FileDescription")) {
            return Ok((company, description));
        }
    }
    bail!("Executable has no vendor identity metadata")
}
fn vendor_identity(roots: &Roots, spec: &ProductSpec, path: &str, process: bool) -> Result<()> {
    let canonical = std::fs::canonicalize(path)?.to_string_lossy().into_owned();
    ensure!(
        roots.allowed(&canonical),
        "Executable resolves outside protected Lenovo package roots"
    );
    let (company, description) = version_fields(&canonical)?;
    let supplier = spec.id == "zone"
        && roots.startup("zone", &canonical)
        && canonical
            .rsplit(['\\', '/'])
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("NvOcScanner.exe"))
        && company.eq_ignore_ascii_case("NVIDIA Corporation");
    ensure!(
        company.to_ascii_lowercase().starts_with("lenovo") || supplier,
        "Executable vendor metadata is not Lenovo"
    );
    if process {
        ensure!(
            exact_process(spec, &canonical),
            "Executable basename does not match the fixed product list"
        );
        if canonical
            .rsplit(['\\', '/'])
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("utility.exe"))
        {
            ensure!(
                description.eq_ignore_ascii_case("Lenovo Hotkeys"),
                "Generic utility.exe is not Lenovo Hotkeys"
            );
        }
    }
    Ok(())
}
fn process_identity(handle: HANDLE, pid: u32) -> Result<ProcessIdentity> {
    let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
        unsafe { (zeroed(), zeroed(), zeroed(), zeroed()) };
    win(
        unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } != 0,
        "Read vendor process creation",
    )?;
    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    win(
        unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut length) } != 0,
        "Read vendor process image",
    )?;
    Ok(ProcessIdentity {
        pid,
        creation_filetime: (created.dwHighDateTime as u64) << 32 | created.dwLowDateTime as u64,
        executable: String::from_utf16(&path[..length as usize])?,
    })
}
fn process_open(expected: &ProcessIdentity) -> Result<Process> {
    let handle = Process(unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
            0,
            expected.pid,
        )
    });
    ensure!(
        !handle.0.is_null(),
        "Cannot open selected vendor process: {}",
        std::io::Error::last_os_error()
    );
    let actual = process_identity(handle.0, expected.pid)?;
    ensure!(
        actual.creation_filetime == expected.creation_filetime
            && actual.executable.eq_ignore_ascii_case(&expected.executable),
        "Vendor PID was reused or image changed"
    );
    Ok(handle)
}
fn manager() -> Result<Service> {
    let handle = Service(unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) });
    ensure!(
        !handle.0.is_null(),
        "Open SCM: {}",
        std::io::Error::last_os_error()
    );
    Ok(handle)
}
fn service(manager: &Service, name: &str, access: u32) -> Result<Option<Service>> {
    let handle = unsafe { OpenServiceW(manager.0, wide(name).as_ptr(), access) };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) {
            return Ok(None);
        }
        return Err(error.into());
    }
    Ok(Some(Service(handle)))
}
fn service_state(handle: &Service, name: &str) -> Result<ServiceState> {
    let mut status: SERVICE_STATUS_PROCESS = unsafe { zeroed() };
    let mut size = 0;
    win(
        unsafe {
            QueryServiceStatusEx(
                handle.0,
                SC_STATUS_PROCESS_INFO,
                (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
                size_of::<SERVICE_STATUS_PROCESS>() as u32,
                &mut size,
            )
        } != 0,
        "Query vendor service status",
    )?;
    let mut required = 0;
    unsafe {
        QueryServiceConfigW(handle.0, ptr::null_mut(), 0, &mut required);
    };
    ensure!(
        required > 0 && required <= 65536,
        "Invalid vendor service configuration size"
    );
    let mut data = vec![0usize; (required as usize).div_ceil(size_of::<usize>())];
    let config = data.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    win(
        unsafe { QueryServiceConfigW(handle.0, config, required, &mut required) } != 0,
        "Query vendor service configuration",
    )?;
    let config = unsafe { &*config };
    let mut length = 0;
    while length < 32768 && unsafe { *config.lpBinaryPathName.add(length) } != 0 {
        length += 1;
    }
    ensure!(length < 32768, "Vendor service command too long");
    let command =
        String::from_utf16(unsafe { std::slice::from_raw_parts(config.lpBinaryPathName, length) })?;
    Ok(ServiceState {
        name: name.into(),
        running: status.dwCurrentState != SERVICE_STOPPED,
        state: status.dwCurrentState,
        start_type: config.dwStartType,
        can_stop: status.dwControlsAccepted & SERVICE_ACCEPT_STOP != 0,
        executable: executable(&command)?,
    })
}
pub(super) struct NativeSoftware {
    com: Option<wmi::COMLibrary>,
    tasks: Option<ITaskService>,
}
thread_local! {static SOFTWARE_COM:std::cell::RefCell<Option<wmi::COMLibrary>>=const{std::cell::RefCell::new(None)};}
impl NativeSoftware {
    pub fn new() -> Self {
        Self {
            com: None,
            tasks: None,
        }
    }
    fn tasks(&mut self) -> Result<&ITaskService> {
        if self.tasks.is_none() {
            self.com = Some(SOFTWARE_COM.with(|slot| -> Result<wmi::COMLibrary> {
                let mut slot = slot.borrow_mut();
                if slot.is_none() {
                    *slot = Some(wmi::COMLibrary::new()?);
                }
                Ok(slot.unwrap())
            })?);
            let service: ITaskService =
                unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }?;
            unsafe {
                service.Connect(
                    &VARIANT::default(),
                    &VARIANT::default(),
                    &VARIANT::default(),
                    &VARIANT::default(),
                )
            }?;
            self.tasks = Some(service);
        }
        Ok(self.tasks.as_ref().unwrap())
    }
    fn task(&mut self, path: &str) -> Result<IRegisteredTask> {
        let (folder, name) = path.rsplit_once('\\').context("Invalid vendor task path")?;
        let folder = unsafe { self.tasks()?.GetFolder(&BSTR::from(folder)) }?;
        Ok(unsafe { folder.GetTask(&BSTR::from(name)) }?)
    }
}
pub(super) struct Prepared {
    services: BTreeMap<String, Service>,
    tasks: BTreeMap<String, IRegisteredTask>,
    registry: BTreeMap<String, RegistryTarget>,
    processes: BTreeMap<u32, Process>,
}
enum RegistryTarget {
    Existing(Reg),
    Create { parent: Reg, path: &'static str },
}
fn approved_target(base: HKEY) -> Result<RegistryTarget> {
    if let Some(key) = open_reg(base, APPROVED, KEY_QUERY_VALUE | KEY_SET_VALUE)? {
        return Ok(RegistryTarget::Existing(key));
    }
    let parent = open_reg(
        base,
        "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer",
        KEY_CREATE_SUB_KEY,
    )?
    .context("StartupApproved parent absent")?;
    Ok(RegistryTarget::Create {
        parent,
        path: "StartupApproved\\Run",
    })
}
fn materialize<'a>(
    target: &'a mut RegistryTarget,
    guard: &mut dyn FnMut() -> Result<()>,
) -> Result<&'a Reg> {
    if let RegistryTarget::Create { parent, path } = target {
        guard()?;
        let mut key = ptr::null_mut();
        let mut disposition = 0;
        let code = unsafe {
            RegCreateKeyExW(
                parent.0,
                wide(path).as_ptr(),
                0,
                ptr::null_mut(),
                REG_OPTION_NON_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                ptr::null(),
                &mut key,
                &mut disposition,
            )
        };
        ensure!(code == 0, "Create StartupApproved key failed ({code})");
        *target = RegistryTarget::Existing(Reg(key));
    }
    match target {
        RegistryTarget::Existing(key) => Ok(key),
        RegistryTarget::Create { .. } => unreachable!(),
    }
}
impl SoftwarePort for NativeSoftware {
    type Prepared = Prepared;
    fn snapshot(&mut self) -> SoftwareSnapshot {
        let mut snapshot = SoftwareSnapshot {
            products: PRODUCTS
                .iter()
                .map(|spec| (spec.id.into(), ProductState::default()))
                .collect(),
        };
        let roots = Roots::read();
        match manager() {
            Ok(manager) => {
                for spec in PRODUCTS {
                    for name in spec.services {
                        match service(&manager, name, SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG)
                            .and_then(|handle| {
                                handle
                                    .map(|handle| service_state(&handle, name))
                                    .transpose()
                            }) {
                            Ok(Some(state)) => snapshot
                                .products
                                .get_mut(spec.id)
                                .unwrap()
                                .services
                                .push(state),
                            Ok(None) => {}
                            Err(error) => snapshot
                                .products
                                .get_mut(spec.id)
                                .unwrap()
                                .errors
                                .push(format!("Service {name}: {error:#}")),
                        }
                    }
                }
            }
            Err(error) => {
                for state in snapshot.products.values_mut() {
                    state.errors.push(format!("SCM: {error:#}"));
                }
            }
        }
        for spec in PRODUCTS {
            for path in spec.tasks {
                let result = (|| -> Result<Vec<TaskState>> {
                    let folder = match unsafe {
                        self.tasks()?.GetFolder(&BSTR::from(format!("\\{path}")))
                    } {
                        Ok(folder) => folder,
                        Err(error)
                            if [0x80070002, 0x80070003].contains(&(error.code().0 as u32)) =>
                        {
                            return Ok(vec![]);
                        }
                        Err(error) => return Err(error.into()),
                    };
                    let tasks = unsafe { folder.GetTasks(TASK_ENUM_HIDDEN.0) }?;
                    let count = unsafe { tasks.Count() }?;
                    ensure!(
                        (0..=1000).contains(&count),
                        "Too many vendor scheduled tasks"
                    );
                    let mut out = Vec::new();
                    for index in 1..=count {
                        let task = unsafe { tasks.get_Item(&VARIANT::from(index)) }?;
                        out.push(TaskState {
                            path: unsafe { task.Path() }?.to_string(),
                            enabled: unsafe { task.Enabled() }?.as_bool(),
                        });
                    }
                    Ok(out)
                })();
                match result {
                    Ok(tasks) => snapshot
                        .products
                        .get_mut(spec.id)
                        .unwrap()
                        .tasks
                        .extend(tasks),
                    Err(error) => snapshot
                        .products
                        .get_mut(spec.id)
                        .unwrap()
                        .errors
                        .push(format!("Task folder {path}: {error:#}")),
                }
            }
        }
        if let Ok(roots) = &roots {
            for hive_name in ["HKCU", "HKLM"] {
                let result = (|| -> Result<()> {
                    let Some(run) = open_reg(hive(hive_name)?, RUN, KEY_READ)? else {
                        return Ok(());
                    };
                    let approved = open_reg(hive(hive_name)?, APPROVED, KEY_READ)?;
                    for name in names(&run, true)? {
                        let Some((kind, bytes)) = query(&run, &name)? else {
                            continue;
                        };
                        let command = match text_value(kind, &bytes) {
                            Ok(value) => value,
                            Err(_) => continue,
                        };
                        let path = match executable(&command) {
                            Ok(path) => path,
                            Err(_) => continue,
                        };
                        for spec in PRODUCTS
                            .iter()
                            .filter(|s| s.id == "vantage" || s.id == "zone")
                        {
                            let selected = spec
                                .startup_names
                                .iter()
                                .any(|entry| entry.eq_ignore_ascii_case(&name))
                                || roots.startup(spec.id, &path);
                            if !selected {
                                continue;
                            }
                            let result = (|| -> Result<StartupState> {
                                vendor_identity(roots, spec, &path, false)?;
                                let enabled = match approved
                                    .as_ref()
                                    .map(|key| query(key, &name))
                                    .transpose()?
                                    .flatten()
                                {
                                    None => true,
                                    Some((kind, bytes)) => {
                                        ensure!(
                                            kind == REG_BINARY && bytes.len() >= 4,
                                            "Invalid StartupApproved value"
                                        );
                                        match u32::from_le_bytes(bytes[..4].try_into().unwrap()) {
                                            2 => true,
                                            3 => false,
                                            _ => bail!("Unknown StartupApproved state"),
                                        }
                                    }
                                };
                                Ok(StartupState {
                                    hive: hive_name.into(),
                                    name: name.clone(),
                                    executable: path.clone(),
                                    enabled,
                                })
                            })();
                            match result {
                                Ok(entry) => snapshot
                                    .products
                                    .get_mut(spec.id)
                                    .unwrap()
                                    .startup
                                    .push(entry),
                                Err(error) => snapshot
                                    .products
                                    .get_mut(spec.id)
                                    .unwrap()
                                    .errors
                                    .push(format!("Run {hive_name}/{name}: {error:#}")),
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    for id in ["vantage", "zone"] {
                        snapshot
                            .products
                            .get_mut(id)
                            .unwrap()
                            .errors
                            .push(format!("Run inventory: {error:#}"));
                    }
                }
            }
            let processes = (|| -> Result<()> {
                let handle = Process(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) });
                ensure!(handle.0 as isize != -1, "Cannot enumerate vendor processes");
                let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
                entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
                let mut next = unsafe { Process32FirstW(handle.0, &mut entry) } != 0;
                if !next {
                    ensure!(
                        std::io::Error::last_os_error().raw_os_error()
                            == Some(ERROR_NO_MORE_FILES as i32),
                        "Process inventory failed: {}",
                        std::io::Error::last_os_error()
                    );
                }
                while next {
                    let name = String::from_utf16_lossy(
                        entry.szExeFile.split(|c| *c == 0).next().unwrap_or(&[]),
                    );
                    for spec in PRODUCTS.iter().filter(|spec| exact_process(spec, &name)) {
                        let result = (|| -> Result<Option<ProcessIdentity>> {
                            let handle = Process(unsafe {
                                OpenProcess(
                                    PROCESS_QUERY_LIMITED_INFORMATION,
                                    0,
                                    entry.th32ProcessID,
                                )
                            });
                            if handle.0.is_null() {
                                let error = std::io::Error::last_os_error();
                                if error.raw_os_error() == Some(87) {
                                    return Ok(None);
                                }
                                return Err(error.into());
                            }
                            let identity = process_identity(handle.0, entry.th32ProcessID)?;
                            if !roots.allowed(&identity.executable) {
                                return Ok(None);
                            }
                            vendor_identity(roots, spec, &identity.executable, true)?;
                            Ok(Some(identity))
                        })();
                        match result {
                            Ok(Some(identity)) => snapshot
                                .products
                                .get_mut(spec.id)
                                .unwrap()
                                .processes
                                .push(identity),
                            Ok(None) => {}
                            Err(error) => snapshot
                                .products
                                .get_mut(spec.id)
                                .unwrap()
                                .errors
                                .push(format!("Process {name}: {error:#}")),
                        }
                    }
                    next = unsafe { Process32NextW(handle.0, &mut entry) } != 0;
                    if !next {
                        ensure!(
                            std::io::Error::last_os_error().raw_os_error()
                                == Some(ERROR_NO_MORE_FILES as i32),
                            "Process inventory continuation failed: {}",
                            std::io::Error::last_os_error()
                        );
                    }
                }
                Ok(())
            })();
            if let Err(error) = processes {
                for state in snapshot.products.values_mut() {
                    state.errors.push(format!("Process inventory: {error:#}"));
                }
            }
        } else if let Err(error) = roots {
            for state in snapshot.products.values_mut() {
                state
                    .errors
                    .push(format!("Vendor roots unavailable: {error:#}"));
            }
        }
        let uwp = (|| -> Result<Vec<UwpState>> {
            let Some(root) = open_reg(HKEY_CURRENT_USER, APPDATA, KEY_READ)? else {
                return Ok(vec![]);
            };
            let mut out = Vec::new();
            for family in names(&root, false)?
                .into_iter()
                .filter(|family| family.to_ascii_lowercase().contains("lenovoutility"))
            {
                let path = format!("{APPDATA}\\{family}\\LenovoUtilityID");
                let Some(key) = open_reg(HKEY_CURRENT_USER, &path, KEY_READ)? else {
                    continue;
                };
                let (kind, bytes) = query(&key, "State")?.context("UWP startup State absent")?;
                ensure!(
                    kind == REG_DWORD && bytes.len() == 4,
                    "UWP startup State is not DWORD"
                );
                let state = u32::from_le_bytes(bytes.try_into().unwrap());
                ensure!(state == 1 || state == 2, "Unknown UWP startup State");
                out.push(UwpState {
                    family,
                    enabled: state == 2,
                });
            }
            Ok(out)
        })();
        match uwp {
            Ok(uwp) => snapshot.products.get_mut("hotkeys").unwrap().uwp = uwp,
            Err(error) => snapshot
                .products
                .get_mut("hotkeys")
                .unwrap()
                .errors
                .push(format!("UWP startup: {error:#}")),
        }
        for state in snapshot.products.values_mut() {
            state.recompute();
        }
        snapshot
    }
    fn prepare(&mut self, plan: &Plan) -> Result<Prepared> {
        let roots = Roots::read()?;
        let manager = manager()?;
        let mut out = Prepared {
            services: BTreeMap::new(),
            tasks: BTreeMap::new(),
            registry: BTreeMap::new(),
            processes: BTreeMap::new(),
        };
        for operation in &plan.operations {
            match operation {
                Operation::Service {
                    product: id,
                    name,
                    enabled,
                } => {
                    let handle = service(
                        &manager,
                        name,
                        SERVICE_QUERY_STATUS
                            | SERVICE_QUERY_CONFIG
                            | SERVICE_CHANGE_CONFIG
                            | SERVICE_START
                            | SERVICE_STOP,
                    )?
                    .context("Vendor service disappeared during preflight")?;
                    let state = service_state(&handle, name)?;
                    vendor_identity(&roots, product(id)?, &state.executable, false)?;
                    ensure!(
                        *enabled || !state.running || state.can_stop,
                        "Vendor service cannot stop without restart"
                    );
                    out.services.insert(name.clone(), handle);
                }
                Operation::Task { path, .. } => {
                    out.tasks.insert(path.clone(), self.task(path)?);
                }
                Operation::Process {
                    product: id,
                    identity,
                } => {
                    let handle = process_open(identity)?;
                    vendor_identity(&roots, product(id)?, &identity.executable, true)?;
                    out.processes.insert(identity.pid, handle);
                }
                Operation::Startup {
                    product: id,
                    hive: hive_name,
                    name,
                    executable: expected,
                    ..
                } => {
                    vendor_identity(&roots, product(id)?, expected, false)?;
                    let run = open_reg(hive(hive_name)?, RUN, KEY_READ)?
                        .context("Run key disappeared")?;
                    let (kind, bytes) = query(&run, name)?.context("Run entry disappeared")?;
                    ensure!(
                        executable(&text_value(kind, &bytes)?)?.eq_ignore_ascii_case(expected),
                        "Run executable changed during preflight"
                    );
                    out.registry.insert(
                        format!("{hive_name}/{name}"),
                        approved_target(hive(hive_name)?)?,
                    );
                }
                Operation::Uwp { family, .. } => {
                    let key = open_reg(
                        HKEY_CURRENT_USER,
                        &format!("{APPDATA}\\{family}\\LenovoUtilityID"),
                        KEY_QUERY_VALUE | KEY_SET_VALUE,
                    )?
                    .context("UWP startup key disappeared")?;
                    out.registry
                        .insert(family.clone(), RegistryTarget::Existing(key));
                }
            }
        }
        Ok(out)
    }
    fn execute(
        &mut self,
        prepared: &mut Prepared,
        operation: &Operation,
        guard: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        match operation {
            Operation::Task { path, enabled, .. } => {
                let task = &prepared.tasks[path];
                guard()?;
                unsafe { task.SetEnabled(VARIANT_BOOL(if *enabled { -1 } else { 0 })) }?;
                ensure!(
                    unsafe { task.Enabled() }?.as_bool() == *enabled,
                    "Scheduled task readback mismatch"
                );
            }
            Operation::Service {
                product: id,
                name,
                enabled,
            } => {
                let service = &prepared.services[name];
                vendor_identity(
                    &Roots::read()?,
                    product(id)?,
                    &service_state(service, name)?.executable,
                    false,
                )?;
                guard()?;
                win(
                    unsafe {
                        ChangeServiceConfigW(
                            service.0,
                            SERVICE_NO_CHANGE,
                            if *enabled {
                                SERVICE_AUTO_START
                            } else {
                                SERVICE_DISABLED
                            },
                            SERVICE_NO_CHANGE,
                            ptr::null(),
                            ptr::null(),
                            ptr::null_mut(),
                            ptr::null(),
                            ptr::null(),
                            ptr::null(),
                            ptr::null(),
                        )
                    } != 0,
                    "Change vendor service startup",
                )?;
                let state = service_state(service, name)?;
                if state.running != *enabled {
                    vendor_identity(&Roots::read()?, product(id)?, &state.executable, false)?;
                    guard()?;
                    if *enabled {
                        win(
                            unsafe { StartServiceW(service.0, 0, ptr::null()) } != 0,
                            "Start vendor service",
                        )?;
                    } else {
                        let mut status: SERVICE_STATUS = unsafe { zeroed() };
                        win(
                            unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) }
                                != 0,
                            "Stop vendor service",
                        )?;
                    }
                }
                let started = Instant::now();
                loop {
                    let state = service_state(service, name)?;
                    if state.state
                        == if *enabled {
                            SERVICE_RUNNING
                        } else {
                            SERVICE_STOPPED
                        }
                        && state.start_type
                            == if *enabled {
                                SERVICE_AUTO_START
                            } else {
                                SERVICE_DISABLED
                            }
                    {
                        break;
                    }
                    ensure!(
                        started.elapsed() < Duration::from_secs(15),
                        "Vendor service did not reach requested state within 15 seconds; restart may be required"
                    );
                    thread::sleep(Duration::from_millis(100));
                }
            }
            Operation::Process { identity, .. } => {
                let handle = &prepared.processes[&identity.pid];
                let mut code = 0;
                win(
                    unsafe { GetExitCodeProcess(handle.0, &mut code) } != 0,
                    "Read selected vendor process exit",
                )?;
                if code == 259 {
                    guard()?;
                    win(
                        unsafe { TerminateProcess(handle.0, 1) } != 0,
                        "Terminate exact vendor process",
                    )?;
                    ensure!(
                        unsafe { WaitForSingleObject(handle.0, INFINITE) } == WAIT_OBJECT_0,
                        "Vendor process exit could not be confirmed"
                    );
                }
            }
            Operation::Startup {
                hive,
                name,
                enabled,
                executable: expected,
                product: id,
            } => {
                let run = open_reg(self::hive(hive)?, RUN, KEY_READ)?
                    .context("Run key disappeared before mutation")?;
                let (kind, bytes) =
                    query(&run, name)?.context("Run entry disappeared before mutation")?;
                ensure!(
                    executable(&text_value(kind, &bytes)?)?.eq_ignore_ascii_case(expected),
                    "Run executable changed before mutation"
                );
                vendor_identity(&Roots::read()?, product(id)?, expected, false)?;
                let key = materialize(
                    prepared
                        .registry
                        .get_mut(&format!("{hive}/{name}"))
                        .unwrap(),
                    guard,
                )?;
                let ticks = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() / 100
                    + 116444736000000000;
                let value = startup_approved(*enabled, u64::try_from(ticks)?);
                guard()?;
                let code = unsafe {
                    RegSetValueExW(
                        key.0,
                        wide(name).as_ptr(),
                        0,
                        REG_BINARY,
                        value.as_ptr(),
                        12,
                    )
                };
                ensure!(code == 0, "Set StartupApproved failed ({code})");
                let (kind, actual) =
                    query(key, name)?.context("StartupApproved readback absent")?;
                ensure!(
                    kind == REG_BINARY && actual == value,
                    "StartupApproved readback mismatch"
                );
            }
            Operation::Uwp {
                family, enabled, ..
            } => {
                let key = materialize(prepared.registry.get_mut(family).unwrap(), guard)?;
                let value = uwp_startup_state(*enabled).to_le_bytes();
                guard()?;
                let code = unsafe {
                    RegSetValueExW(
                        key.0,
                        wide("State").as_ptr(),
                        0,
                        REG_DWORD,
                        value.as_ptr(),
                        4,
                    )
                };
                ensure!(code == 0, "Set UWP startup failed ({code})");
                let (kind, actual) = query(key, "State")?.context("UWP startup readback absent")?;
                ensure!(
                    kind == REG_DWORD && actual == value,
                    "UWP startup readback mismatch"
                );
            }
        }
        Ok(())
    }
}
