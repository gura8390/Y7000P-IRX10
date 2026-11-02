//! Durable backup bytes precede intent, which precedes every physical write.
use super::*;
use jiks_store::SettingsStore;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(1);
#[derive(Clone)]
pub(crate) struct BackupStore {
    root: PathBuf,
    binding: Option<Binding>,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Binding {
    data_root_sha256: String,
    machine_sha256: String,
}
#[derive(Serialize, Deserialize)]
struct FileRef {
    name: String,
    length: usize,
    sha256: String,
}
#[derive(Serialize, Deserialize)]
struct StoredSnapshot {
    firmware: Option<Firmware>,
    files: Option<Vec<FileRef>>,
    animation: Option<AnimationState>,
}
#[derive(Serialize, Deserialize)]
enum StoredMutation {
    Info(RawVariable),
    Checksum(RawVariable),
    WriteFile(FileRef),
    DeleteFile(String),
    Animation(AnimationState),
}
#[derive(Serialize, Deserialize)]
struct StoredRecord {
    binding: Binding,
    backup_id: String,
    created_unix_ms: u64,
    baseline: StoredSnapshot,
    target: StoredSnapshot,
    attempted: Vec<StoredMutation>,
    state: TransactionState,
    error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LeaseRecord {
    pub letter: char,
    pub volume: Option<String>,
    pub backup_id: Option<String>,
    pub binding: String,
}
#[derive(Serialize, Deserialize)]
struct Pointer {
    active: bool,
    backup_id: String,
}
impl BackupStore {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            binding: None,
        }
    }
    pub(crate) fn bound(root: impl Into<PathBuf>, machine_sha256: String) -> Result<Self> {
        ensure!(
            machine_sha256.len() == 64 && machine_sha256.bytes().all(|v| v.is_ascii_hexdigit()),
            "Invalid boot machine binding"
        );
        let root = root.into();
        checked_path(&root)?;
        let absolute = if root.try_exists()? {
            root.canonicalize()?
        } else {
            std::path::absolute(&root)?
        };
        let path = absolute
            .to_str()
            .context("Non-Unicode boot data directory")?
            .trim_start_matches("\\\\?\\")
            .to_lowercase();
        Ok(Self {
            root,
            binding: Some(Binding {
                data_root_sha256: digest(path.as_bytes()),
                machine_sha256,
            }),
        })
    }
    pub(crate) fn binding_key(&self) -> Result<String> {
        Ok(digest(&serde_json::to_vec(
            self.binding
                .as_ref()
                .context("Boot store binding is missing")?,
        )?))
    }
    fn directory(&self, id: &str) -> Result<PathBuf> {
        ensure!(
            id.starts_with("boot-logo-")
                && id.len() <= 100
                && id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "Invalid boot backup identifier"
        );
        Ok(self.root.join("backups").join(id))
    }
    pub(crate) fn prepare(&self, record: &mut Record) -> Result<()> {
        ensure!(
            self.pending()?.is_none(),
            "A persisted boot transaction requires recovery before another change"
        );
        let id = format!(
            "boot-logo-{}-{}-{}",
            record.created_unix_ms,
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let dir = self.directory(&id)?;
        checked_path(&dir)?;
        fs::create_dir_all(dir.parent().context("Backup parent missing")?)?;
        fs::create_dir(&dir)?;
        fs::create_dir(dir.join("blobs"))?;
        record.backup_id = id;
        self.persist(record)?;
        let verify = self.load(&record.backup_id)?;
        ensure!(
            verify.baseline == record.baseline && verify.target == record.target,
            "Durable boot backup verification failed"
        );
        Ok(())
    }
    fn blob(&self, id: &str, name: &str, bytes: &[u8]) -> Result<FileRef> {
        let hash = digest(bytes);
        let path = self.directory(id)?.join("blobs").join(&hash);
        checked_path(&path)?;
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(bytes)?;
                f.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        };
        let reference = FileRef {
            name: name.into(),
            length: bytes.len(),
            sha256: hash,
        };
        ensure!(
            self.read_blob(id, &reference)? == bytes,
            "Backup blob write/readback mismatch"
        );
        Ok(reference)
    }
    fn read_blob(&self, id: &str, r: &FileRef) -> Result<Vec<u8>> {
        safe_relative(&r.name)?;
        ensure!(
            r.length <= MAX_BACKUP_BYTES
                && r.sha256.len() == 64
                && r.sha256.bytes().all(|v| v.is_ascii_hexdigit()),
            "Invalid backup blob metadata"
        );
        let path = self.directory(id)?.join("blobs").join(&r.sha256);
        checked_path(&path)?;
        let meta = fs::symlink_metadata(&path)?;
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink() && meta.len() == r.length as u64,
            "Backup blob identity/size mismatch"
        );
        let mut bytes = Vec::with_capacity(r.length);
        OpenOptions::new()
            .read(true)
            .open(path)?
            .take(MAX_BACKUP_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() == r.length && digest(&bytes) == r.sha256,
            "Backup blob hash mismatch"
        );
        Ok(bytes)
    }
    fn store_snapshot(&self, id: &str, s: &Snapshot) -> Result<StoredSnapshot> {
        Ok(StoredSnapshot {
            firmware: s.firmware.clone(),
            files: s
                .files
                .as_ref()
                .map(|files| {
                    files
                        .iter()
                        .map(|(name, bytes)| self.blob(id, name, bytes))
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?,
            animation: s.animation,
        })
    }
    fn read_snapshot(&self, id: &str, s: StoredSnapshot) -> Result<Snapshot> {
        let files = s
            .files
            .map(|files| {
                ensure!(files.len() <= 128, "Backup file count exceeds bound");
                let mut result = BTreeMap::new();
                for reference in files {
                    let bytes = self.read_blob(id, &reference)?;
                    ensure!(
                        result.insert(reference.name, bytes).is_none(),
                        "Duplicate backup name"
                    );
                }
                validate_files(&result)?;
                Ok::<_, anyhow::Error>(result)
            })
            .transpose()?;
        let value = Snapshot {
            firmware: s.firmware,
            files,
            animation: s.animation,
        };
        validate_snapshot(&value)?;
        Ok(value)
    }
    pub(crate) fn persist(&self, r: &Record) -> Result<()> {
        let id = &r.backup_id;
        let stored = StoredRecord {
            binding: self.binding.clone().context(
                "A verified machine/data-directory identity is required before boot journaling",
            )?,
            backup_id: id.clone(),
            created_unix_ms: r.created_unix_ms,
            baseline: self.store_snapshot(id, &r.baseline)?,
            target: self.store_snapshot(id, &r.target)?,
            attempted: r
                .attempted
                .iter()
                .map(|m| {
                    Ok(match m {
                        Mutation::Info(v) => StoredMutation::Info(v.clone()),
                        Mutation::Checksum(v) => StoredMutation::Checksum(v.clone()),
                        Mutation::WriteFile { name, bytes } => {
                            StoredMutation::WriteFile(self.blob(id, name, bytes)?)
                        }
                        Mutation::DeleteFile { name } => StoredMutation::DeleteFile(name.clone()),
                        Mutation::Animation(v) => StoredMutation::Animation(*v),
                    })
                })
                .collect::<Result<_>>()?,
            state: r.state.clone(),
            error: r.error.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&stored)?;
        let store = SettingsStore::new(self.directory(id)?)?;
        store.write("record.json", &bytes)?;
        ensure!(
            store.read("record.json")?.as_deref() == Some(bytes.as_slice()),
            "Boot backup record readback mismatch"
        );
        let active = !matches!(
            r.state,
            TransactionState::Committed | TransactionState::Restored
        );
        let root = SettingsStore::new(&self.root)?;
        let pointer = serde_json::to_vec(&Pointer {
            active,
            backup_id: id.clone(),
        })?;
        root.write("boot-logo-pending.json", &pointer)?;
        ensure!(
            root.read("boot-logo-pending.json")?.as_deref() == Some(pointer.as_slice()),
            "Boot intent journal readback mismatch"
        );
        Ok(())
    }
    pub(crate) fn load(&self, id: &str) -> Result<Record> {
        let path = self.directory(id)?.join("record.json");
        let bytes = bounded_read(&path, 1024 * 1024)?;
        let s: StoredRecord = serde_json::from_slice(&bytes)?;
        ensure!(
            self.binding.as_ref() == Some(&s.binding),
            "Boot backup belongs to another machine or data directory; no restore authorized"
        );
        ensure!(
            s.backup_id == id && s.attempted.len() <= 1024,
            "Invalid boot backup identity/intent count"
        );
        let mut attempted = Vec::new();
        for m in s.attempted {
            attempted.push(match m {
                StoredMutation::Info(v) => Mutation::Info(v),
                StoredMutation::Checksum(v) => Mutation::Checksum(v),
                StoredMutation::WriteFile(r) => Mutation::WriteFile {
                    name: r.name.clone(),
                    bytes: self.read_blob(id, &r)?,
                },
                StoredMutation::DeleteFile(name) => {
                    safe_relative(&name)?;
                    Mutation::DeleteFile { name }
                }
                StoredMutation::Animation(v) => Mutation::Animation(v),
            });
        }
        Ok(Record {
            backup_id: id.into(),
            created_unix_ms: s.created_unix_ms,
            baseline: self.read_snapshot(id, s.baseline)?,
            target: self.read_snapshot(id, s.target)?,
            attempted,
            state: s.state,
            error: s.error,
        })
    }
    pub(crate) fn pending(&self) -> Result<Option<String>> {
        match bounded_read(&self.root.join("boot-logo-pending.json"), 4096) {
            Ok(bytes) => {
                let p: Pointer = serde_json::from_slice(&bytes)?;
                self.directory(&p.backup_id)?;
                Ok(p.active.then_some(p.backup_id))
            }
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
    pub(crate) fn lease(&self) -> Result<Option<LeaseRecord>> {
        match bounded_read(&self.root.join("boot-logo-lease.json"), 4096) {
            Ok(bytes) => {
                let lease: Option<LeaseRecord> = serde_json::from_slice(&bytes)?;
                if let Some(l) = &lease {
                    ensure!(('P'..='Z').contains(&l.letter), "Invalid EFI lease drive");
                    if let Some(id) = &l.backup_id {
                        self.directory(id)?;
                    }
                    if self.binding.is_some() {
                        ensure!(
                            l.binding == self.binding_key()?,
                            "EFI lease belongs to another machine/data directory; manual review required"
                        );
                    }
                }
                Ok(lease)
            }
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
    pub(crate) fn save_lease(&self, lease: Option<&LeaseRecord>) -> Result<()> {
        if let Some(l) = lease {
            ensure!(l.binding == self.binding_key()?, "Wrong-origin EFI lease");
        }
        let store = SettingsStore::new(&self.root)?;
        let bytes = serde_json::to_vec(&lease)?;
        store.write("boot-logo-lease.json", &bytes)?;
        ensure!(
            store.read("boot-logo-lease.json")?.as_deref() == Some(bytes.as_slice()),
            "EFI lease journal readback mismatch"
        );
        Ok(())
    }
    pub(crate) fn summaries(&self) -> Result<Vec<serde_json::Value>> {
        let root = self.root.join("backups");
        let entries = match fs::read_dir(root) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in entries.take(1025) {
            ensure!(out.len() < 1024, "Boot backup count exceeds bound");
            let entry = entry?;
            let id = entry.file_name().to_string_lossy().into_owned();
            if !id.starts_with("boot-logo-") {
                continue;
            }
            let path = self.directory(&id)?.join("record.json");
            let record: StoredRecord = serde_json::from_slice(&bounded_read(&path, 1024 * 1024)?)?;
            out.push(serde_json::json!({"id":id,"created_unix_ms":record.created_unix_ms,"state":record.state,"enabled":record.baseline.firmware.as_ref().map(|f|f.info.bytes.first()==Some(&1))}));
        }
        Ok(out)
    }
}
fn bounded_read(path: &Path, max: usize) -> Result<Vec<u8>> {
    checked_path(path)?;
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .open(path)?
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= max, "Boot journal exceeds bounded size");
    Ok(bytes)
}
fn checked_path(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    for part in std::path::absolute(path)?.ancestors() {
        match fs::symlink_metadata(part) {
            Ok(meta) => ensure!(
                meta.file_attributes() & 0x400 == 0,
                "Reparse point in a boot backup/journal path; no recovery authorized"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn immutable_backup_blobs_and_restart_origin_are_verified_before_recovery() {
        let root = std::env::temp_dir().join(format!(
            "JiksBootBackup-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let moved = root.with_extension("moved");
        assert!(!root.exists());
        let machine = digest(b"fixture physical identity");
        let store = BackupStore::bound(&root, machine.clone()).unwrap();
        assert!(store.pending().unwrap().is_none());
        assert!(store.summaries().unwrap().is_empty());
        assert!(!root.exists());
        let snapshot = Snapshot {
            firmware: None,
            files: None,
            animation: Some(AnimationState {
                present: false,
                disabled: false,
            }),
        };
        let mut r = Record {
            backup_id: String::new(),
            created_unix_ms: 1,
            baseline: snapshot.clone(),
            target: Snapshot {
                animation: Some(AnimationState {
                    present: true,
                    disabled: true,
                }),
                ..snapshot
            },
            attempted: vec![],
            state: TransactionState::Prepared,
            error: None,
        };
        store.prepare(&mut r).unwrap();
        assert_eq!(store.pending().unwrap(), Some(r.backup_id.clone()));
        assert_eq!(
            BackupStore::bound(&root, machine.clone())
                .unwrap()
                .load(&r.backup_id)
                .unwrap()
                .baseline,
            r.baseline
        );
        assert!(
            BackupStore::bound(&root, digest(b"another physical identity"))
                .unwrap()
                .load(&r.backup_id)
                .is_err()
        );
        r.state = TransactionState::Committed;
        store.persist(&r).unwrap();
        assert!(store.pending().unwrap().is_none());
        assert!(store.directory("../escape").is_err());
        let reference = store
            .blob(&r.backup_id, "test.png", b"exact old bytes")
            .unwrap();
        let path = store
            .directory(&r.backup_id)
            .unwrap()
            .join("blobs")
            .join(&reference.sha256);
        fs::write(&path, b"wrong old bytes").unwrap();
        assert!(store.read_blob(&r.backup_id, &reference).is_err());
        for path in [&root, &moved] {
            assert!(
                path.is_absolute()
                    && path.starts_with(std::env::temp_dir())
                    && path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("JiksBootBackup-")
            );
        }
        fs::rename(&root, &moved).unwrap();
        assert!(
            BackupStore::bound(&moved, machine)
                .unwrap()
                .load(&r.backup_id)
                .is_err()
        );
        fs::remove_dir_all(moved).unwrap();
    }
}
