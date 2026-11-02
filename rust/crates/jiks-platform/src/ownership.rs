//! The mutex is acquired/released on the hardware thread, including late I/O.
use crate::domain::CpuJournal;
use anyhow::{Context, Result, bail, ensure};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    marker::PhantomData,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    rc::Rc,
};
use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{
        CreateMutexW, GetCurrentProcess, GetProcessTimes, ReleaseMutex, WaitForSingleObject,
    },
};
static JOURNAL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn wide(value: &str) -> Result<Vec<u16>> {
    ensure!(!value.contains('\0'), "Embedded NUL in native string");
    Ok(value.encode_utf16().chain(Some(0)).collect())
}
fn wide_path(value: &Path) -> Result<Vec<u16>> {
    let mut units: Vec<u16> = value.as_os_str().encode_wide().collect();
    ensure!(!units.contains(&0), "Embedded NUL in native file path");
    units.push(0);
    Ok(units)
}

pub(crate) struct OwnerToken {
    handle: HANDLE,
    pub abandoned: bool,
    _thread: PhantomData<Rc<()>>,
}
impl OwnerToken {
    pub(crate) fn acquire() -> Result<Self> {
        Self::acquire_named("Global\\Y7000P_IRX10.FirmwareOwner.v1")
    }
    fn acquire_named(name: &str) -> Result<Self> {
        let name = wide(name)?;
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        ensure!(
            !handle.is_null(),
            "Cannot create machine-wide hardware owner: {}",
            std::io::Error::last_os_error()
        );
        let result = unsafe { WaitForSingleObject(handle, 0) };
        match result {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self {
                handle,
                abandoned: result == WAIT_ABANDONED,
                _thread: PhantomData,
            }),
            _ => {
                unsafe {
                    CloseHandle(handle);
                }
                if result == WAIT_TIMEOUT {
                    bail!("Another process owns the firmware; no write was issued")
                }
                bail!(
                    "Cannot acquire firmware owner: {}",
                    std::io::Error::last_os_error()
                )
            }
        }
    }
}
impl Drop for OwnerToken {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

pub(crate) fn current_creation_time() -> Result<u64> {
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    ensure!(
        unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        } != 0,
        "Cannot read owner creation time"
    );
    Ok((creation.dwHighDateTime as u64) << 32 | creation.dwLowDateTime as u64)
}

#[derive(Clone)]
pub struct JournalStore {
    path: PathBuf,
}
impl JournalStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn read(&self) -> Result<Option<CpuJournal>> {
        ensure!(
            !self.recovery_marker().try_exists()?,
            "An abandoned/unverified hardware owner requires explicit recovery review"
        );
        match fs::read(&self.path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).context(
                "Unparseable prior hardware journal; manual readback required",
            )?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn recovery_marker(&self) -> PathBuf {
        let mut path = self.path.as_os_str().to_os_string();
        path.push(".recovery-required");
        PathBuf::from(path)
    }
    pub(crate) fn mark_recovery_required(&self, reason: &str) -> Result<()> {
        fs::create_dir_all(self.path.parent().context("Journal parent missing")?)?;
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(self.recovery_marker())
        {
            Ok(mut file) => {
                file.write_all(reason.as_bytes())?;
                file.sync_all()?;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    pub(crate) fn write(&self, value: &CpuJournal) -> Result<()> {
        let parent = self.path.parent().context("Journal path needs a parent")?;
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(
            ".cpu-owner-{}-{}-{}.pending",
            std::process::id(),
            crate::domain::unix_ms(),
            JOURNAL_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let result = (|| {
            file.write_all(&serde_json::to_vec_pretty(value)?)?;
            file.sync_all()?;
            drop(file);
            let source = wide_path(&temporary)?;
            let target = wide_path(&self.path)?;
            ensure!(
                unsafe {
                    MoveFileExW(
                        source.as_ptr(),
                        target.as_ptr(),
                        MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                    )
                } != 0,
                "Cannot commit hardware journal: {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        })();
        if temporary.exists() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kernel_owner_rejects_a_second_thread_and_marks_abandonment() {
        let name = format!(
            "Local\\jiks-owner-fixture-{}-{}",
            std::process::id(),
            JOURNAL_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let owner = OwnerToken::acquire_named(&name).unwrap();
        let other_name = name.clone();
        assert!(
            std::thread::spawn(move || OwnerToken::acquire_named(&other_name).is_err())
                .join()
                .unwrap()
        );
        drop(owner);
        let other_name = name.clone();
        let leaked = std::thread::spawn(move || {
            let owner = OwnerToken::acquire_named(&other_name).unwrap();
            let handle = owner.handle as usize;
            std::mem::forget(owner);
            handle
        })
        .join()
        .unwrap();
        let recovered = OwnerToken::acquire_named(&name).unwrap();
        assert!(recovered.abandoned);
        drop(recovered);
        unsafe {
            CloseHandle(leaked as HANDLE);
        }
    }
    #[test]
    fn recovery_marker_survives_new_store_instances() {
        let root = std::env::temp_dir().join(format!(
            "jiks-owner-marker-fixture-{}-{}",
            std::process::id(),
            JOURNAL_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let store = JournalStore::new(root.join("journal.json"));
        store
            .mark_recovery_required("fixture unverified owner")
            .unwrap();
        assert!(store.read().is_err());
        store
            .mark_recovery_required("must not erase first evidence")
            .unwrap();
        assert!(JournalStore::new(store.path()).read().is_err());
        assert_eq!(
            fs::read_to_string(store.recovery_marker()).unwrap(),
            "fixture unverified owner"
        );
        assert!(
            root.starts_with(std::env::temp_dir())
                && root
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("jiks-owner-marker-fixture-")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
