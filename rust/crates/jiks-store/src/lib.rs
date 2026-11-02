//! Complete-file persistence. No firmware, configuration evaluation or UI runs here.
use std::{collections::hash_map::DefaultHasher, fs::{self, OpenOptions}, hash::{Hash, Hasher},
    io::{self, Read, Write}, path::{Component, Path, PathBuf}, sync::{Mutex, OnceLock, atomic::{AtomicU64, Ordering}}};
use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt};
use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, ReplaceFileW, MOVEFILE_WRITE_THROUGH,
    REPLACEFILE_IGNORE_MERGE_ERRORS, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_SHARE_DELETE};

static PATH_LOCKS: OnceLock<[Mutex<()>; 64]> = OnceLock::new();
static NEXT_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct SettingsStore { root: PathBuf }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitStage { BeforeTemporary, AfterTemporary, BeforeCommit, AfterCommit }

impl SettingsStore {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = std::path::absolute(root.into())?;
        fs::create_dir_all(&root)?;
        // All cooperating stores must derive the same lock for one physical
        // directory, including Windows 8.3 and extended-length spellings.
        // The directory now exists, so resolve it once before joining names.
        let root = canonical_directory_path(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path { &self.root }

    pub fn path(&self, name: &str) -> io::Result<PathBuf> {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some()
            || name.contains([':', '\\', '/', '\0']) || name.is_empty() || name.ends_with(['.', ' '])
            || reserved_device_name(name) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "A settings name must be a single safe file name"));
        }
        Ok(self.root.join(name))
    }

    pub fn read(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        let path = self.path(name)?;
        let _guard = path_lock(&path).lock().unwrap_or_else(|poison| poison.into_inner());
        let mut file = match OpenOptions::new().read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }

    pub fn write(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.write_with_checkpoint(name, bytes, |_| Ok(()))
    }

    pub fn write_json(&self, name: &str, value: &serde_json::Value) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        self.write(name, &bytes)
    }

    /// Checkpoints allow deterministic file-failure tests without touching a user directory.
    /// They are not a claim about abrupt power loss or cross-process transactions.
    pub fn write_with_checkpoint(&self, name: &str, bytes: &[u8], mut checkpoint: impl FnMut(CommitStage) -> io::Result<()>) -> io::Result<()> {
        let target = self.path(name)?;
        let _guard = path_lock(&target).lock().unwrap_or_else(|poison| poison.into_inner());
        checkpoint(CommitStage::BeforeTemporary)?;
        let temporary = self.root.join(format!(".{name}.{}-{}.pending", std::process::id(), NEXT_FILE.fetch_add(1, Ordering::Relaxed)));
        let mut file = OpenOptions::new().write(true).create_new(true).share_mode(0).open(&temporary)?;
        let cleanup = TemporaryFile(temporary);
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        checkpoint(CommitStage::AfterTemporary)?;
        checkpoint(CommitStage::BeforeCommit)?;
        let source = wide(&cleanup.0);
        let destination = wide(&target);
        let previous = wide(&self.root.join(format!("{name}.previous")));
        // Both paths are on the same volume. Cooperating in-process reads share
        // the target lock because ReplaceFile can race a separate filename open.
        let succeeded = unsafe {
            if target.try_exists()? {
                ReplaceFileW(destination.as_ptr(), source.as_ptr(), previous.as_ptr(),
                    REPLACEFILE_IGNORE_MERGE_ERRORS, std::ptr::null(), std::ptr::null())
            } else { MoveFileExW(source.as_ptr(), destination.as_ptr(), MOVEFILE_WRITE_THROUGH) }
        };
        if succeeded == 0 { return Err(io::Error::last_os_error()); }
        checkpoint(CommitStage::AfterCommit)?;
        Ok(())
    }

    /// Back up the exact failed read, not a newer file currently using that name.
    pub fn backup_failed_snapshot(&self, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
        self.path(name)?;
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let backup_name = format!("{name}.invalid-{stamp}-{}-{}.bak", std::process::id(), NEXT_FILE.fetch_add(1, Ordering::Relaxed));
        self.write(&backup_name, bytes)?;
        self.path(&backup_name)
    }
}

/// Stable spelling for an existing or future directory, without creating it.
/// Resolve the nearest existing directory before appending missing components,
/// so its 8.3 alias cannot change identity when the child is later created.
/// This is path identity, not a security/reparse-point admission policy; callers
/// retain their existing validation before and after this operation.
pub fn canonical_directory_path(path: &Path) -> io::Result<PathBuf> {
    let mut existing = std::path::absolute(path)?;
    let mut suffix = Vec::new();
    loop {
        match fs::metadata(&existing) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(io::Error::new(io::ErrorKind::NotADirectory, "Path ancestor is not a directory"));
                }
                let mut resolved = existing.canonicalize()?;
                for component in suffix.into_iter().rev() { resolved.push(component); }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                suffix.push(existing.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Directory has no existing ancestor"))?.to_os_string());
                if !existing.pop() {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "Directory has no existing ancestor"));
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn wide(path: &Path) -> Vec<u16> { path.as_os_str().encode_wide().chain(Some(0)).collect() }
fn reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let bytes = stem.as_bytes();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (bytes.len() == 4 && (&bytes[..3] == b"COM" || &bytes[..3] == b"LPT") && matches!(bytes[3], b'1'..=b'9'))
}
fn path_lock(path: &Path) -> &'static Mutex<()> {
    let mut hasher = DefaultHasher::new();
    // Application file names are stable ASCII, but the directory may be Unicode.
    path.to_string_lossy().to_lowercase().hash(&mut hasher);
    &PATH_LOCKS.get_or_init(|| std::array::from_fn(|_| Mutex::new(())))[hasher.finish() as usize % 64]
}
struct TemporaryFile(PathBuf);
impl Drop for TemporaryFile { fn drop(&mut self) { let _ = fs::remove_file(&self.0); } }

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("JiksStore-{}-{}", std::process::id(), NEXT_FILE.fetch_add(1, Ordering::Relaxed))))
        }
        fn store(&self) -> SettingsStore { SettingsStore::new(&self.0).unwrap() }
        fn pending(&self) -> usize { fs::read_dir(&self.0).unwrap().filter_map(Result::ok).filter(|entry| entry.file_name().to_string_lossy().ends_with(".pending")).count() }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let parent = std::env::temp_dir();
            assert!(self.0.starts_with(parent) && self.0.file_name().unwrap().to_string_lossy().starts_with("JiksStore-"));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn short_path(path: &Path) -> PathBuf {
        use std::os::windows::ffi::OsStringExt;
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
        let wide = wide(path);
        let mut output = vec![0u16; 32768];
        let length = unsafe { GetShortPathNameW(wide.as_ptr(), output.as_mut_ptr(), output.len() as u32) };
        assert!(length > 0 && (length as usize) < output.len());
        // On volumes without 8.3 names Windows returns the long spelling;
        // extended-length versus ordinary roots are still compared below.
        PathBuf::from(std::ffi::OsString::from_wide(&output[..length as usize]))
    }

    #[test]
    fn new_store_resolves_aliases_to_one_root_and_one_file_lock() {
        let fixture = Fixture::new();
        let parent = fixture.0.join("Long Settings Ancestor");
        fs::create_dir_all(&parent).unwrap();
        let short = short_path(&parent).join("NewProfile");
        let long = parent.canonicalize().unwrap().join("NewProfile");
        assert!(!short.exists());
        let first = SettingsStore::new(&short).unwrap();
        let second = SettingsStore::new(&long).unwrap();
        let third = SettingsStore::new(first.root()).unwrap();
        assert_eq!(first.root(), second.root());
        assert_eq!(first.root(), third.root());
        for name in ["settings.json", "args.txt", "未知.json"] {
            let a = first.path(name).unwrap();
            let b = second.path(name).unwrap();
            assert!(std::ptr::eq(path_lock(&a), path_lock(&b)));
        }
    }

    #[test]
    fn canonical_future_directory_is_noncreating_stable_and_rejects_file_ancestors() {
        let fixture = Fixture::new();
        fs::create_dir_all(&fixture.0).unwrap();
        let short = short_path(&fixture.0).join("not-created").join("data");
        let before = canonical_directory_path(&short).unwrap();
        assert!(!short.exists());
        assert_eq!(before, fixture.0.canonicalize().unwrap().join("not-created/data"));
        fs::create_dir_all(&short).unwrap();
        assert_eq!(before, canonical_directory_path(&short).unwrap());
        let file = fixture.0.join("actual-file");
        fs::write(&file, b"not a directory").unwrap();
        assert_eq!(canonical_directory_path(&file).unwrap_err().kind(), io::ErrorKind::NotADirectory);
        assert!(canonical_directory_path(&file.join("child")).is_err());
    }

    #[test]
    fn alias_reader_waits_for_real_replace_and_previous_is_preserved() {
        use std::sync::{mpsc, TryLockError};
        let fixture = Fixture::new();
        let writer = fixture.store();
        let reader = SettingsStore::new(short_path(&fixture.0)).unwrap();
        writer.write("settings.json", b"old complete").unwrap();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        thread::scope(|scope| {
            let task = scope.spawn(move || writer.write_with_checkpoint("settings.json", b"new complete", |stage| {
                if stage == CommitStage::BeforeCommit {
                    ready_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }
                Ok(())
            }));
            ready_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            let target = reader.path("settings.json").unwrap();
            let same_lock_is_held = matches!(path_lock(&target).try_lock(), Err(TryLockError::WouldBlock));
            // Release the worker before asserting, so a regression cannot strand
            // this fixture inside scoped-thread cleanup.
            release_tx.send(()).unwrap();
            let observed = reader.read("settings.json").unwrap().unwrap();
            task.join().unwrap().unwrap();
            assert!(same_lock_is_held);
            assert_eq!(observed, b"new complete");
        });
        assert_eq!(reader.read("settings.json.previous").unwrap().unwrap(), b"old complete");
        assert_eq!(fixture.pending(), 0);
    }

    #[test]
    fn each_failure_boundary_preserves_a_complete_document() {
        for stage in [CommitStage::BeforeTemporary, CommitStage::AfterTemporary, CommitStage::BeforeCommit, CommitStage::AfterCommit] {
            let fixture = Fixture::new(); let store = fixture.store();
            store.write("settings.json", b"{\"revision\":0}").unwrap();
            store.write("settings.json", b"{\"revision\":1}").unwrap();
            assert!(store.write_with_checkpoint("settings.json", b"{\"revision\":2}", |current|
                if current == stage { Err(io::Error::other("injected")) } else { Ok(()) }).is_err());
            let expected = if stage == CommitStage::AfterCommit { b"{\"revision\":2}" } else { b"{\"revision\":1}" };
            assert_eq!(store.read("settings.json").unwrap().unwrap(), expected);
            let old = if stage == CommitStage::AfterCommit { b"{\"revision\":1}" } else { b"{\"revision\":0}" };
            assert_eq!(store.read("settings.json.previous").unwrap().unwrap(), old);
            assert_eq!(fixture.pending(), 0);
        }
    }

    #[test]
    fn real_sharing_failure_preserves_the_original_and_can_retry() {
        let fixture = Fixture::new(); let store = fixture.store();
        store.write("settings.json", b"old").unwrap();
        let held = OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(store.path("settings.json").unwrap()).unwrap();
        assert!(store.write("settings.json", b"new").is_err());
        assert_eq!(store.read("settings.json").unwrap().unwrap(), b"old");
        assert_eq!(fixture.pending(), 0);
        drop(held);
        store.write("settings.json", b"new").unwrap();
        assert_eq!(store.read("settings.json.previous").unwrap().unwrap(), b"old");
    }

    #[test]
    fn readers_and_writers_never_observe_a_missing_or_mixed_file() {
        let fixture = Fixture::new(); let store = Arc::new(fixture.store());
        store.write_json("settings.json", &serde_json::json!({"n":0, "payload":"x".repeat(8192)})).unwrap();
        thread::scope(|scope| {
            let mut tasks = Vec::new();
            for worker in 0..6 {
                let store = Arc::clone(&store);
                tasks.push(scope.spawn(move || {
                    for n in 0..20 {
                        if worker < 3 { store.write_json("settings.json", &serde_json::json!({"n":n, "payload":"x".repeat(8192)})).unwrap(); }
                        else {
                            let bytes = store.read("settings.json").unwrap().expect("target must remain available");
                            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                            assert_eq!(value["payload"].as_str().unwrap().len(), 8192);
                            assert!(value["n"].as_u64().unwrap() < 20);
                        }
                    }
                }));
            }
            for task in tasks { task.join().unwrap(); }
        });
        assert_eq!(fixture.pending(), 0);
    }

    #[test]
    fn failed_snapshot_backups_are_exact_unique_and_paths_are_confined() {
        let fixture = Fixture::new(); let store = fixture.store();
        let first = store.backup_failed_snapshot("settings.json", b"broken original").unwrap();
        store.write("settings.json", b"newer content").unwrap();
        let second = store.backup_failed_snapshot("settings.json", b"broken original").unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(first).unwrap(), b"broken original");
        assert_eq!(fs::read(second).unwrap(), b"broken original");
        store.write("a中.json", b"unicode file").unwrap();
        assert_eq!(store.read("a中.json").unwrap().unwrap(), b"unicode file");
        for name in ["../settings.json", "x/y.json", "C:\\outside", "x:stream", "x.", "x ", "", "CON.json", "nul", "COM1", "lpt9.bak", "x/."] { assert!(store.path(name).is_err(), "{name}"); }
    }
}
