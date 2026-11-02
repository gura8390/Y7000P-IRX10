//! Native implementation is only constructed by an explicit owned actor action.
use super::*;
use crate::{
    domain::{ExplicitAction, monotonic_ms},
    ownership::{OwnerToken, wide},
};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    mem::size_of,
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    process::Command,
    ptr,
    rc::Rc,
    sync::{Mutex, MutexGuard},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, SetLastError},
    Globalization::{CP_OEMCP, MultiByteToWideChar},
    Security::*,
    Storage::FileSystem::*,
    System::{
        SystemInformation::GetSystemDirectoryW,
        Threading::{GetCurrentProcess, OpenProcessToken},
        WindowsProgramming::{
            GetFirmwareEnvironmentVariableExW, SetFirmwareEnvironmentVariableExW,
        },
    },
};
const INFO_NAME: &str = "LBLDESP";
const INFO_GUID: &str = "{871455D0-5576-4FB8-9865-AF0824463B9E}";
const CHECK_NAME: &str = "LBLDVC";
const CHECK_GUID: &str = "{871455D1-5576-4FB8-9865-AF0824463C9F}";
static PRIVILEGE: Mutex<()> = Mutex::new(());
static TEMP_FILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
struct Privilege {
    token: HANDLE,
    previous: TOKEN_PRIVILEGES,
    _lock: MutexGuard<'static, ()>,
}
impl Privilege {
    fn enter() -> Result<Self> {
        let lock = PRIVILEGE.lock().unwrap_or_else(|p| p.into_inner());
        let mut token = ptr::null_mut();
        ensure!(
            unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                    &mut token,
                )
            } != 0,
            "Cannot open UEFI privilege token: {}",
            std::io::Error::last_os_error()
        );
        let result = (|| {
            let mut luid = Default::default();
            ensure!(
                unsafe {
                    LookupPrivilegeValueW(
                        ptr::null(),
                        wide("SeSystemEnvironmentPrivilege")?.as_ptr(),
                        &mut luid,
                    )
                } != 0,
                "UEFI privilege lookup failed"
            );
            let mut desired: TOKEN_PRIVILEGES = unsafe { std::mem::zeroed() };
            desired.PrivilegeCount = 1;
            desired.Privileges[0] = LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            };
            let mut previous: TOKEN_PRIVILEGES = unsafe { std::mem::zeroed() };
            let mut length = 0;
            unsafe { SetLastError(0) };
            ensure!(
                unsafe {
                    AdjustTokenPrivileges(
                        token,
                        0,
                        &desired,
                        size_of::<TOKEN_PRIVILEGES>() as u32,
                        &mut previous,
                        &mut length,
                    )
                } != 0
                    && unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED,
                "SeSystemEnvironmentPrivilege is unavailable; capability is unknown at this privilege level"
            );
            Ok(previous)
        })();
        match result {
            Ok(previous) => Ok(Self {
                token,
                previous,
                _lock: lock,
            }),
            Err(e) => {
                unsafe { CloseHandle(token) };
                Err(e)
            }
        }
    }
}
impl Drop for Privilege {
    fn drop(&mut self) {
        unsafe {
            AdjustTokenPrivileges(
                self.token,
                0,
                &self.previous,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            );
            CloseHandle(self.token);
        }
    }
}
fn get_variable(name: &str, guid: &str) -> Result<RawVariable> {
    let _privilege = Privilege::enter()?;
    let mut bytes = vec![0u8; 4096];
    let mut attributes = 0;
    let length = unsafe {
        GetFirmwareEnvironmentVariableExW(
            wide(name)?.as_ptr(),
            wide(guid)?.as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len() as u32,
            &mut attributes,
        )
    };
    ensure!(
        length > 0 && length as usize <= bytes.len(),
        "UEFI {name} read failed: {}",
        std::io::Error::last_os_error()
    );
    bytes.truncate(length as usize);
    Ok(RawVariable { bytes, attributes })
}
pub(crate) fn read_firmware() -> Result<Firmware> {
    let f = Firmware {
        info: get_variable(INFO_NAME, INFO_GUID)?,
        checksum: get_variable(CHECK_NAME, CHECK_GUID)?,
    };
    f.validate()?;
    Ok(f)
}
fn set_variable(
    name: &str,
    guid: &str,
    value: &RawVariable,
    action: &ExplicitAction,
) -> Result<()> {
    let _privilege = Privilege::enter()?;
    action.check_write_boundary()?;
    ensure!(
        unsafe {
            SetFirmwareEnvironmentVariableExW(
                wide(name)?.as_ptr(),
                wide(guid)?.as_ptr(),
                value.bytes.as_ptr().cast(),
                value.bytes.len() as u32,
                value.attributes,
            )
        } != 0,
        "UEFI {name} write failed: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        get_variable_locked(name, guid)? == *value,
        "UEFI {name} exact raw readback mismatch"
    );
    Ok(())
}
// Privilege mutex is already held by a setter; never re-enter it recursively.
fn get_variable_locked(name: &str, guid: &str) -> Result<RawVariable> {
    let mut bytes = vec![0u8; 4096];
    let mut attributes = 0;
    let len = unsafe {
        GetFirmwareEnvironmentVariableExW(
            wide(name)?.as_ptr(),
            wide(guid)?.as_ptr(),
            bytes.as_mut_ptr().cast(),
            4096,
            &mut attributes,
        )
    };
    ensure!(
        len > 0 && len <= 4096,
        "UEFI post-write read failed: {}",
        std::io::Error::last_os_error()
    );
    bytes.truncate(len as usize);
    Ok(RawVariable { bytes, attributes })
}
fn system_tool(name: &str) -> Result<PathBuf> {
    ensure!(
        ["mountvol.exe", "bcdedit.exe"].contains(&name),
        "Unapproved system tool"
    );
    let mut buffer = [0u16; 32768];
    let len = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    ensure!(
        len > 0 && (len as usize) < buffer.len(),
        "System32 path unavailable"
    );
    Ok(PathBuf::from(String::from_utf16(&buffer[..len as usize])?).join(name))
}
fn decode(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(&[0xff, 0xfe]) {
        ensure!(
            bytes.len().is_multiple_of(2),
            "Truncated UTF16 system output"
        );
        return Ok(String::from_utf16(
            &bytes[2..]
                .chunks_exact(2)
                .map(|v| u16::from_le_bytes([v[0], v[1]]))
                .collect::<Vec<_>>(),
        )?);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(text.into());
    }
    let n = unsafe {
        MultiByteToWideChar(
            CP_OEMCP,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            ptr::null_mut(),
            0,
        )
    };
    ensure!(n > 0, "Cannot decode system tool output");
    let mut out = vec![0u16; n as usize];
    ensure!(
        unsafe {
            MultiByteToWideChar(
                CP_OEMCP,
                0,
                bytes.as_ptr(),
                bytes.len() as i32,
                out.as_mut_ptr(),
                n,
            )
        } == n,
        "System output decode changed"
    );
    Ok(String::from_utf16(&out)?)
}
fn tool(name: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(system_tool(name)?)
        .args(args)
        .creation_flags(0x0800_0000)
        .output()?;
    ensure!(
        output.stdout.len() + output.stderr.len() <= 2 * 1024 * 1024,
        "System tool output exceeds bound"
    );
    ensure!(
        output.status.success(),
        "{name} failed code={:?}: {}",
        output.status.code(),
        decode(&output.stderr)?
    );
    decode(&output.stdout)
}
pub fn parse_animation(output: &str) -> Result<AnimationState> {
    let mut matches = output.lines().filter_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()?.eq_ignore_ascii_case("bootuxdisabled")).then(|| words.collect::<Vec<_>>())
    });
    let Some(values) = matches.next() else {
        return Ok(AnimationState {
            present: false,
            disabled: false,
        });
    };
    ensure!(
        matches.next().is_none() && values.len() == 1,
        "Ambiguous BCD bootuxdisabled output"
    );
    let value = values[0].to_lowercase();
    let disabled = match value.as_str() {
        "yes" | "on" | "true" | "1" | "是" => true,
        "no" | "off" | "false" | "0" | "否" => false,
        _ => anyhow::bail!("Unknown BCD bootuxdisabled value"),
    };
    Ok(AnimationState {
        present: true,
        disabled,
    })
}
pub(crate) fn read_animation() -> Result<AnimationState> {
    let output = tool("bcdedit.exe", &["/enum", "{current}"])?;
    ensure!(
        output.contains("{current}"),
        "BCD output did not identify the requested current entry"
    );
    parse_animation(&output)
}
fn write_animation(state: AnimationState, action: &ExplicitAction) -> Result<()> {
    action.check_write_boundary()?;
    if state.present {
        tool(
            "bcdedit.exe",
            &[
                "/set",
                "{current}",
                "bootuxdisabled",
                if state.disabled { "on" } else { "off" },
            ],
        )?;
    } else if read_animation()?.present {
        action.check_write_boundary()?;
        tool(
            "bcdedit.exe",
            &["/deletevalue", "{current}", "bootuxdisabled"],
        )?;
    }
    ensure!(
        read_animation()? == state,
        "BCD animation presence/value readback mismatch"
    );
    Ok(())
}
fn volume(letter: char) -> Result<String> {
    ensure!(('P'..='Z').contains(&letter), "Invalid EFI mount letter");
    let mut out = [0u16; 128];
    ensure!(
        unsafe {
            GetVolumeNameForVolumeMountPointW(
                wide(&format!("{letter}:\\"))?.as_ptr(),
                out.as_mut_ptr(),
                out.len() as u32,
            )
        } != 0,
        "EFI volume identity unavailable: {}",
        std::io::Error::last_os_error()
    );
    let n = out
        .iter()
        .position(|v| *v == 0)
        .context("Unterminated volume identity")?;
    Ok(String::from_utf16(&out[..n])?)
}
fn no_reparse(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "Reparse point in EFI or backup path is forbidden"
    );
    Ok(())
}
pub(super) fn cleanup_requires_unmount(
    expected: Option<&str>,
    occupied: bool,
    actual: Option<&str>,
) -> Result<bool> {
    if !occupied {
        return Ok(false);
    }
    let expected = expected.context(
        "Interrupted EFI mount has no verified volume identity; no unmount is authorized",
    )?;
    let actual =
        actual.context("Occupied mount identity could not be read; no unmount is authorized")?;
    Ok(expected.eq_ignore_ascii_case(actual))
}
pub(crate) struct NativePort {
    store: BackupStore,
    owner: Rc<OwnerToken>,
    action: ExplicitAction,
    cleanup_action: Option<ExplicitAction>,
    lease: Option<store::LeaseRecord>,
}
impl NativePort {
    pub(crate) fn replace_action(&mut self, action: ExplicitAction) {
        self.action = action;
        self.cleanup_action = None;
    }
    pub(crate) fn new(
        root: PathBuf,
        owner: Rc<OwnerToken>,
        action: ExplicitAction,
    ) -> Result<Self> {
        #[derive(Deserialize)]
        struct Product {
            #[serde(rename = "UUID")]
            uuid: String,
        }
        let products: Vec<Product> = crate::wmi_native::WmiFacade::connect("ROOT\\CIMV2")?
            .query("SELECT UUID FROM Win32_ComputerSystemProduct")?;
        ensure!(
            products.len() == 1,
            "Cannot bind the boot journal to one physical system"
        );
        let uuid = products[0].uuid.trim().to_ascii_uppercase();
        ensure!(
            uuid.len() == 36
                && uuid.bytes().all(|v| v.is_ascii_hexdigit() || v == b'-')
                && uuid
                    .bytes()
                    .any(|v| v.is_ascii_hexdigit() && v != b'0' && v != b'F'),
            "Physical system UUID unavailable; no boot write authorized"
        );
        let identity = crate::read_machine_identity()?;
        let machine = digest(
            format!(
                "{uuid}|{}|{}|{}",
                identity.manufacturer, identity.model, identity.bios
            )
            .as_bytes(),
        );
        let store = BackupStore::bound(root, machine)?;
        let lease = store.lease()?;
        Ok(Self {
            store,
            owner,
            action,
            cleanup_action: None,
            lease,
        })
    }
    fn lease_matches(&self) -> Result<()> {
        let lease = self
            .lease
            .as_ref()
            .context("EFI is not mounted by this transaction")?;
        let expected = lease.volume.as_ref().context(
            "An interrupted mount has no verified volume identity; manual recovery is required",
        )?;
        ensure!(
            volume(lease.letter)?.eq_ignore_ascii_case(expected),
            "EFI drive letter no longer refers to the owned volume"
        );
        Ok(())
    }
    fn mount(&mut self) -> Result<()> {
        if self.lease.is_some() {
            return self.lease_matches();
        }
        self.cleanup_action
            .as_ref()
            .unwrap_or(&self.action)
            .check_write_boundary()?;
        let mask = unsafe { GetLogicalDrives() };
        ensure!(mask != 0, "Drive-letter inventory unavailable");
        let letter = ('P'..='Z')
            .rev()
            .find(|c| mask & (1u32 << (*c as u32 - 'A' as u32)) == 0)
            .context("No unoccupied EFI mount letter");
        let letter = letter?;
        let lease = store::LeaseRecord {
            letter,
            volume: None,
            backup_id: None,
            binding: self.store.binding_key()?,
        };
        self.store.save_lease(Some(&lease))?;
        self.lease = Some(lease);
        if let Err(error) = self
            .cleanup_action
            .as_ref()
            .unwrap_or(&self.action)
            .check_write_boundary()
        {
            self.store.save_lease(None)?;
            self.lease = None;
            return Err(error);
        }
        tool("mountvol.exe", &[&format!("{letter}:"), "/s"])?;
        let actual = volume(letter)?;
        self.lease.as_mut().unwrap().volume = Some(actual);
        self.store.save_lease(self.lease.as_ref())?;
        self.lease_matches()
    }
    fn root(&mut self) -> Result<PathBuf> {
        self.mount()?;
        self.lease_matches()?;
        let mut root = PathBuf::from(format!("{}:\\", self.lease.as_ref().unwrap().letter));
        for part in ["EFI", "Lenovo", "Logo"] {
            root.push(part);
            if root.try_exists()? {
                no_reparse(&root)?;
            }
        }
        Ok(root)
    }
    fn file(&mut self, name: &str) -> Result<PathBuf> {
        safe_relative(name)?;
        let root = self.root()?;
        let path = root.join(name);
        let mut current = root;
        for component in Path::new(name).components() {
            let Component::Normal(part) = component else {
                unreachable!()
            };
            current.push(part);
            if current.try_exists()? {
                no_reparse(&current)?;
            }
        }
        Ok(path)
    }
    fn permit(&self, cleanup: bool) -> Result<&ExplicitAction> {
        if cleanup {
            self.cleanup_action
                .as_ref()
                .context("Fresh rollback deadline was not established")
        } else {
            Ok(&self.action)
        }
    }
    pub(crate) fn backup(&self, id: &str) -> Result<Record> {
        self.store.load(id)
    }
    pub(crate) fn pending_record(&self) -> Result<Option<Record>> {
        self.store
            .pending()?
            .or_else(|| self.lease.as_ref().and_then(|l| l.backup_id.clone()))
            .map(|id| self.store.load(&id))
            .transpose()
    }
    pub(crate) fn has_lease(&self) -> bool {
        self.lease.is_some()
    }
}
impl BootPort for NativePort {
    fn begin_recovery(&mut self) -> Result<()> {
        self.cleanup_action = Some(ExplicitAction::with_deadline(
            "Restore the verified original boot-logo transaction",
            monotonic_ms().saturating_add(30_000),
        )?);
        Ok(())
    }
    fn guard(&mut self, cleanup: bool) -> Result<()> {
        let _ = &self.owner;
        self.permit(cleanup)?.check_write_boundary()
    }
    fn firmware(&mut self) -> Result<Firmware> {
        read_firmware()
    }
    fn files(&mut self) -> Result<BTreeMap<String, Vec<u8>>> {
        let root = self.root()?;
        let mut out = BTreeMap::new();
        if !root.try_exists()? {
            return Ok(out);
        }
        fn walk(
            root: &Path,
            dir: &Path,
            out: &mut BTreeMap<String, Vec<u8>>,
            depth: usize,
        ) -> Result<()> {
            ensure!(depth <= 8, "EFI logo tree exceeds depth bound");
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                no_reparse(&path)?;
                let meta = entry.metadata()?;
                ensure!(
                    !meta.permissions().readonly(),
                    "Read-only EFI logo file needs manual attribute review before a reversible transaction"
                );
                if meta.is_dir() {
                    walk(root, &path, out, depth + 1)?;
                } else {
                    ensure!(
                        meta.is_file() && meta.len() <= MAX_BACKUP_BYTES as u64,
                        "EFI contains a nonregular/oversized logo file"
                    );
                    let relative = path
                        .strip_prefix(root)?
                        .to_str()
                        .context("Non-Unicode EFI filename")?
                        .replace('\\', "/");
                    safe_relative(&relative)?;
                    let mut bytes = Vec::new();
                    OpenOptions::new()
                        .read(true)
                        .open(&path)?
                        .take(MAX_BACKUP_BYTES as u64 + 1)
                        .read_to_end(&mut bytes)?;
                    out.insert(relative, bytes);
                    validate_files(out)?;
                }
            }
            Ok(())
        }
        walk(&root, &root, &mut out, 0)?;
        self.lease_matches()?;
        Ok(out)
    }
    fn animation(&mut self) -> Result<AnimationState> {
        read_animation()
    }
    fn validate_target(&mut self, target: &Snapshot) -> Result<()> {
        if let Some(files) = &target.files {
            let _root = self.root()?;
            let letter = self.lease.as_ref().unwrap().letter;
            let mut available = 0;
            ensure!(
                unsafe {
                    GetDiskFreeSpaceExW(
                        wide(&format!("{letter}:\\"))?.as_ptr(),
                        &mut available,
                        ptr::null_mut(),
                        ptr::null_mut(),
                    )
                } != 0,
                "Cannot verify EFI free space"
            );
            let needed = files.values().try_fold(1024 * 1024u64, |sum, v| {
                sum.checked_add(v.len() as u64)
                    .context("Image size overflow")
            })?;
            ensure!(
                available >= needed,
                "Insufficient EFI space for atomic image replacement"
            );
        }
        Ok(())
    }
    fn prepare_backup(&mut self, record: &mut Record) -> Result<()> {
        self.store.prepare(record)?;
        if let Some(lease) = self.lease.as_mut() {
            lease.backup_id = Some(record.backup_id.clone());
            self.store.save_lease(Some(lease))?;
        }
        Ok(())
    }
    fn persist(&mut self, record: &Record) -> Result<()> {
        self.store.persist(record)
    }
    fn mutate(&mut self, change: &Mutation, cleanup: bool) -> Result<()> {
        let permit = self.permit(cleanup)?.clone();
        match change {
            Mutation::Info(value) => {
                Info::decode(&value.bytes)?;
                set_variable(INFO_NAME, INFO_GUID, value, &permit)
            }
            Mutation::Checksum(value) => {
                ensure!(value.bytes.len() == 40, "Checksum write length changed");
                set_variable(CHECK_NAME, CHECK_GUID, value, &permit)
            }
            Mutation::Animation(state) => write_animation(*state, &permit),
            Mutation::DeleteFile { name } => {
                let path = self.file(name)?;
                self.lease_matches()?;
                permit.check_write_boundary()?;
                fs::remove_file(&path)?;
                ensure!(!path.try_exists()?, "EFI file deletion readback mismatch");
                Ok(())
            }
            Mutation::WriteFile { name, bytes } => {
                let path = self.file(name)?;
                self.lease_matches()?;
                permit.check_write_boundary()?;
                fs::create_dir_all(path.parent().context("EFI file parent missing")?)?;
                let temporary = path.with_extension(format!(
                    "jiks-{}-{}.pending",
                    std::process::id(),
                    TEMP_FILE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                struct Temp(PathBuf);
                impl Drop for Temp {
                    fn drop(&mut self) {
                        let _ = fs::remove_file(&self.0);
                    }
                }
                let pending = Temp(temporary);
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .share_mode(0)
                    .open(&pending.0)?;
                file.write_all(bytes)?;
                file.sync_all()?;
                drop(file);
                self.lease_matches()?;
                permit.check_write_boundary()?;
                ensure!(
                    unsafe {
                        MoveFileExW(
                            wide(
                                pending
                                    .0
                                    .to_str()
                                    .context("Non-Unicode EFI temporary path")?,
                            )?
                            .as_ptr(),
                            wide(path.to_str().context("Non-Unicode EFI final path")?)?.as_ptr(),
                            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                        )
                    } != 0,
                    "EFI atomic replacement failed: {}",
                    std::io::Error::last_os_error()
                );
                ensure!(
                    fs::read(&path)? == *bytes,
                    "EFI file content readback mismatch"
                );
                Ok(())
            }
        }
    }
    fn cleanup(&mut self) -> Result<()> {
        let Some(lease) = self.lease.clone() else {
            return Ok(());
        };
        let before = unsafe { GetLogicalDrives() };
        ensure!(before != 0, "Cannot verify EFI cleanup drive inventory");
        let occupied = before & (1u32 << (lease.letter as u32 - 'A' as u32)) != 0;
        let actual = if occupied {
            Some(volume(lease.letter)?)
        } else {
            None
        };
        if !cleanup_requires_unmount(lease.volume.as_deref(), occupied, actual.as_deref())? {
            self.store.save_lease(None)?;
            self.lease = None;
            return Ok(());
        }
        if self.cleanup_action.is_none() {
            self.begin_recovery()?;
        }
        self.permit(true)?.check_write_boundary()?;
        tool("mountvol.exe", &[&format!("{}:", lease.letter), "/d"])?;
        let mask = unsafe { GetLogicalDrives() };
        ensure!(mask != 0, "Cannot verify mount cleanup drive inventory");
        let present = mask & (1u32 << (lease.letter as u32 - 'A' as u32)) != 0;
        if present {
            ensure!(
                !cleanup_requires_unmount(
                    lease.volume.as_deref(),
                    true,
                    Some(&volume(lease.letter)?)
                )?,
                "Owned EFI mount is still present after unmount request"
            );
        }
        self.store.save_lease(None)?;
        self.lease = None;
        Ok(())
    }
}
pub(crate) fn status(root: &Path) -> serde_json::Value {
    fn capture<T: Serialize>(r: Result<T>) -> serde_json::Value {
        match r {
            Ok(v) => serde_json::json!({"available":true,"value":v}),
            Err(e) => serde_json::json!({"available":false,"error":format!("{e:#}")}),
        }
    }
    let firmware = read_firmware();
    let info = firmware
        .as_ref()
        .map(|f| Info::decode(&f.info.bytes))
        .map_err(|e| anyhow::anyhow!("{e:#}"))
        .and_then(|v| v);
    let store = BackupStore::new(root);
    serde_json::json!({"info":capture(info),"checksum_version":capture(firmware.map(|f|i32::from_le_bytes(f.checksum.bytes[..4].try_into().unwrap()))),"animation":capture(read_animation()),"backups":capture(store.summaries()),"pending":capture(store.pending().and_then(|p|Ok(p.is_some()||store.lease()?.is_some()))),"getter_does_not_mount_efi":true})
}
