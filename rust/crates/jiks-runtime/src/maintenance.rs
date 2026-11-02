//! Explicit local maintenance. No constructor scans AppData, starts processes,
//! changes hardware, enables automation or sends a report anywhere. The host
//! supplies user-selected paths and must run capture/export on its IO worker.
use crate::{
    config::{ConfigRepository, DOCUMENTS},
    macros::MacroDocument,
};
use anyhow::{Context, Result, ensure};
use jiks_core::{
    automation::{AutomationDocument, StepDto, StepKind, TriggerDto, TriggerKind, expand_pipeline},
    settings::{JsonNode, SettingsDocument, SettingsKind},
};
use jiks_store::SettingsStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::windows::{ffi::OsStrExt, fs::MetadataExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

const MAGIC: &[u8] = b"JIKS-SETTINGS-1\r\n";
pub const MAX_BUNDLE_BYTES: usize = 64 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 128 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(1);

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
fn known(name: &str) -> Option<(&'static str, &'static str)> {
    DOCUMENTS.iter().copied().find(|(_, file)| *file == name)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupFile {
    pub name: String,
    pub bytes: usize,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupManifest {
    pub format: String,
    pub version: u32,
    pub created_at_unix_ms: u128,
    pub documents: Vec<BackupFile>,
}
#[derive(Debug, Clone)]
pub struct BackupBundle {
    manifest: BackupManifest,
    documents: BTreeMap<String, Vec<u8>>,
}
impl BackupBundle {
    /// Raw bytes are retained even when a current file is malformed. Parsing is
    /// a restore preflight operation; preservation must not destroy bad evidence.
    pub fn from_documents(documents: BTreeMap<String, Vec<u8>>) -> Result<Self> {
        let mut total = 0usize;
        for (name, bytes) in &documents {
            ensure!(
                known(name).is_some(),
                "Unknown settings backup entry: {name}"
            );
            ensure!(
                bytes.len() <= MAX_DOCUMENT_BYTES,
                "Settings document exceeds backup budget: {name}"
            );
            total = total
                .checked_add(bytes.len())
                .context("Backup size overflow")?;
        }
        ensure!(
            total <= MAX_BUNDLE_BYTES - MAX_MANIFEST_BYTES,
            "Settings backup exceeds total budget"
        );
        let manifest = BackupManifest {
            format: "JiksSettingsBackup".into(),
            version: 1,
            created_at_unix_ms: now(),
            documents: documents
                .iter()
                .map(|(name, bytes)| BackupFile {
                    name: name.clone(),
                    bytes: bytes.len(),
                    sha256: hash(bytes),
                })
                .collect(),
        };
        Ok(Self {
            manifest,
            documents,
        })
    }
    pub fn manifest(&self) -> &BackupManifest {
        &self.manifest
    }
    pub fn raw_document(&self, name: &str) -> Option<&[u8]> {
        self.documents.get(name).map(Vec::as_slice)
    }
    pub fn select(&self, names: &BTreeSet<String>) -> Result<Self> {
        ensure!(
            names.iter().all(|name| self.documents.contains_key(name)),
            "Selected document is absent from backup"
        );
        Self::from_documents(
            self.documents
                .iter()
                .filter(|(name, _)| names.contains(*name))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )
    }
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let manifest = serde_json::to_vec(&self.manifest)?;
        ensure!(
            manifest.len() <= MAX_MANIFEST_BYTES,
            "Backup manifest exceeds budget"
        );
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&manifest);
        for file in &self.manifest.documents {
            bytes.extend_from_slice(
                self.documents
                    .get(&file.name)
                    .context("Backup manifest/content mismatch")?,
            );
        }
        ensure!(
            bytes.len() <= MAX_BUNDLE_BYTES,
            "Backup bundle exceeds budget"
        );
        Ok(bytes)
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_BUNDLE_BYTES && bytes.starts_with(MAGIC),
            "Unrecognized/oversized settings backup"
        );
        let length_offset = MAGIC.len();
        let header = bytes
            .get(length_offset..length_offset + 4)
            .context("Truncated backup header")?;
        let manifest_length = u32::from_le_bytes(header.try_into().unwrap()) as usize;
        ensure!(
            manifest_length <= MAX_MANIFEST_BYTES,
            "Backup manifest exceeds budget"
        );
        let mut cursor = length_offset + 4;
        let end = cursor
            .checked_add(manifest_length)
            .context("Manifest size overflow")?;
        let manifest: BackupManifest =
            serde_json::from_slice(bytes.get(cursor..end).context("Truncated manifest")?)?;
        cursor = end;
        ensure!(
            manifest.format == "JiksSettingsBackup" && manifest.version == 1,
            "Unsupported backup version"
        );
        ensure!(
            manifest.documents.len() <= DOCUMENTS.len(),
            "Too many backup entries"
        );
        let mut documents = BTreeMap::new();
        for entry in &manifest.documents {
            ensure!(
                known(&entry.name).is_some() && !documents.contains_key(&entry.name),
                "Unsafe/duplicate backup entry: {}",
                entry.name
            );
            ensure!(
                entry.bytes <= MAX_DOCUMENT_BYTES,
                "Backup document exceeds budget"
            );
            let end = cursor
                .checked_add(entry.bytes)
                .context("Backup document size overflow")?;
            let content = bytes
                .get(cursor..end)
                .context("Truncated backup document")?;
            cursor = end;
            ensure!(
                hash(content) == entry.sha256,
                "Backup SHA256 mismatch: {}",
                entry.name
            );
            documents.insert(entry.name.clone(), content.to_vec());
        }
        ensure!(cursor == bytes.len(), "Unexpected trailing backup payload");
        Ok(Self {
            manifest,
            documents,
        })
    }
}

fn reject_reparse(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_attributes() & 0x400 == 0 && !metadata.file_type().is_symlink(),
        "Maintenance does not follow reparse points"
    );
    Ok(())
}
fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    reject_reparse(path)?;
    let mut bytes = Vec::new();
    File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "Maintenance file exceeds size budget");
    Ok(bytes)
}
/// Compatible with the actual C# SettingsBackupWindow format: a selected
/// directory containing flat JSON files. args.txt/lang are deliberately not
/// imported as executable startup options or a replacement UI language.
pub fn capture_root(root: &Path) -> Result<BackupBundle> {
    reject_reparse(root)?;
    let mut documents = BTreeMap::new();
    let store = SettingsStore::new(root)?;
    for &(_, name) in DOCUMENTS {
        let path = root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                reject_reparse(&path)?;
                ensure!(
                    metadata.len() <= MAX_DOCUMENT_BYTES as u64,
                    "Settings document exceeds backup budget"
                );
                if let Some(bytes) = store.read(name)? {
                    ensure!(
                        bytes.len() <= MAX_DOCUMENT_BYTES,
                        "Settings document exceeds backup budget"
                    );
                    documents.insert(name.into(), bytes);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    BackupBundle::from_documents(documents)
}
pub fn capture(repository: &ConfigRepository) -> Result<BackupBundle> {
    capture_root(repository.root())
}
pub fn read_bundle(path: &Path) -> Result<BackupBundle> {
    BackupBundle::from_bytes(&read_bounded(path, MAX_BUNDLE_BYTES)?)
}

struct Pending(PathBuf);
impl Drop for Pending {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn atomic_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Backup destination has no parent")?;
    reject_reparse(parent)?;
    let name = path
        .file_name()
        .context("Backup name missing")?
        .to_string_lossy();
    let temporary = parent.join(format!(
        ".{name}.{}-{}.pending",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let pending = Pending(temporary);
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending.0)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    drop(output);
    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    ensure!(
        unsafe {
            MoveFileExW(
                wide(&pending.0).as_ptr(),
                wide(path).as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        } != 0,
        "Atomic backup commit failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
pub fn write_backup(bundle: &BackupBundle, directory: &Path) -> Result<PathBuf> {
    fs::create_dir_all(directory)?;
    reject_reparse(directory)?;
    let path = directory.join(format!(
        "jiks-settings-{}-{}-{}.jiksbackup",
        now(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    atomic_new(&path, &bundle.to_bytes()?)?;
    let restored = read_bundle(&path)?;
    ensure!(
        restored.documents == bundle.documents,
        "Written backup verification mismatch"
    );
    Ok(path)
}
/// Export in the old flat-folder form without replacing an existing user file.
/// Returns the already-exported names if an individual later commit fails.
pub fn export_legacy_folder(bundle: &BackupBundle, directory: &Path) -> Result<ExportOutcome> {
    fs::create_dir_all(directory)?;
    reject_reparse(directory)?;
    let mut outcome = ExportOutcome::default();
    for (name, bytes) in &bundle.documents {
        match atomic_new(&directory.join(name), bytes) {
            Ok(()) => outcome.files.push(name.clone()),
            Err(error) => {
                outcome.failure = Some(format!("{name}: {error:#}"));
                break;
            }
        }
    }
    Ok(outcome)
}
#[derive(Debug, Default)]
pub struct ExportOutcome {
    pub files: Vec<String>,
    pub failure: Option<String>,
}

#[derive(Clone)]
pub struct RestorePlan {
    documents: Vec<(&'static str, SettingsDocument)>,
    pub warnings: Vec<String>,
}
impl RestorePlan {
    pub fn document_names(&self) -> Vec<&'static str> {
        self.documents.iter().map(|(key, _)| *key).collect()
    }
}

fn validate_trigger_preserving_unknown(
    trigger: &TriggerDto,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if trigger.kind == TriggerKind::Unknown {
        warnings.push("Unknown automation trigger preserved inert; explicit enable must reject unsupported execution".into());
        return Ok(());
    }
    if matches!(trigger.kind, TriggerKind::And | TriggerKind::Or) {
        for child in &trigger.children {
            validate_trigger_preserving_unknown(child, warnings)?;
        }
    } else {
        trigger.validate()?;
    }
    Ok(())
}
pub fn prepare_restore(bundle: &BackupBundle) -> Result<RestorePlan> {
    ensure!(
        !bundle.documents.is_empty(),
        "Backup contains no known settings documents"
    );
    let mut documents = Vec::new();
    let mut warnings = Vec::new();
    // Every document is parsed/validated before any backup directory or target
    // is modified. Known arrays/dictionaries stay whole replacements.
    for &(key, name) in DOCUMENTS {
        let Some(bytes) = bundle.documents.get(name) else {
            continue;
        };
        let text = std::str::from_utf8(bytes)?.trim_start_matches('\u{feff}');
        let mut document = SettingsKind::from_filename(name)
            .context("Unknown settings kind")?
            .parse(text)?;
        if key == "application" {
            let mut root = document.root().clone();
            root.set_known("EnableCpuThermalControl", JsonNode::Bool(false))?;
            document = SettingsDocument::from_root(root)?.normalized_application()?;
        } else if key == "automation" {
            let mut root = document.root().clone();
            root.set_known("IsEnabled", JsonNode::Bool(false))?;
            document = SettingsDocument::from_root(root)?;
            let parsed = AutomationDocument::parse(&document.to_json(), || {
                "00000000-0000-0000-0000-000000000000".into()
            })?;
            let mut graph = parsed.pipelines.clone();
            for pipeline in &mut graph {
                if let Some(trigger) = &pipeline.trigger {
                    validate_trigger_preserving_unknown(trigger, &mut warnings)?;
                }
                for step in &mut pipeline.steps {
                    if step.kind == StepKind::Unknown {
                        warnings.push("Unknown automation step preserved inert; explicit enable must reject unsupported execution".into());
                        *step = StepDto::new(
                            StepKind::Delay,
                            JsonNode::parse("{\"DelaySeconds\":0}")?,
                        )?;
                    } else {
                        step.validate()?;
                    }
                }
            }
            for pipeline in &graph {
                expand_pipeline(pipeline, &graph)?;
            }
        } else if key == "macros" {
            let mut root = document.root().clone();
            root.set_known("IsEnabled", JsonNode::Bool(false))?;
            document = SettingsDocument::from_root(root)?;
            // Parsing validates key/source/known event shapes without executing
            // or erasing future event metadata; playback validates separately.
            MacroDocument::from_settings(document.clone())?;
        }
        crate::config_actions::validate_storage_document(key,&document)?;
        documents.push((key, document));
    }
    warnings.push("Restore keeps CPU thermal control, automation and global macro listening disabled; explicit re-enable is a separate action".into());
    warnings.push("Only contained known JSON documents are replaced; absent documents and args.txt/lang remain unchanged".into());
    Ok(RestorePlan {
        documents,
        warnings,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RestoreQuiescence {
    pub thermal_stopped: bool,
    pub automation_stopped: bool,
    pub macros_stopped: bool,
    pub native_io_idle: bool,
}
impl RestoreQuiescence {
    fn verified(self) -> bool {
        self.thermal_stopped
            && self.automation_stopped
            && self.macros_stopped
            && self.native_io_idle
    }
}
#[derive(Debug)]
pub struct RestoreOutcome {
    pub restored: Vec<String>,
    pub pre_restore_backup: PathBuf,
    pub failure: Option<String>,
    pub warnings: Vec<String>,
}
pub fn commit_restore(
    repository: &mut ConfigRepository,
    plan: RestorePlan,
    backup_directory: &Path,
    quiescence: RestoreQuiescence,
) -> Result<RestoreOutcome> {
    commit_restore_with_checkpoint(repository, plan, backup_directory, quiescence, |_| Ok(()))
}
/// Testable exception injection; not a power-loss or multi-file transaction test.
pub fn commit_restore_with_checkpoint(
    repository: &mut ConfigRepository,
    plan: RestorePlan,
    backup_directory: &Path,
    quiescence: RestoreQuiescence,
    mut before_document: impl FnMut(&str) -> Result<()>,
) -> Result<RestoreOutcome> {
    ensure!(
        quiescence.verified(),
        "Restore requires verified stopped controllers/listeners/playback/recording and actual native IO completion"
    );
    let root = repository.root().to_path_buf();
    let pre_restore_backup = write_backup(&capture_root(&root)?, backup_directory)?;
    let store = SettingsStore::new(&root)?;
    let mut outcome = RestoreOutcome {
        restored: Vec::new(),
        pre_restore_backup,
        failure: None,
        warnings: plan.warnings,
    };
    for (key, document) in plan.documents {
        let name = DOCUMENTS.iter().find(|(k, _)| *k == key).unwrap().1;
        let result = (|| -> Result<()> {
            before_document(key)?;
            let target = root.join(name);
            if target.exists() {
                reject_reparse(&target)?;
            }
            store.write(name, document.serialize()?.as_bytes())?;
            Ok(())
        })();
        match result {
            Ok(()) => outcome.restored.push(key.into()),
            Err(error) => {
                outcome.failure = Some(format!("{name}: {error:#}"));
                break;
            }
        }
    }
    // Re-read actual committed scope even after a partial failure. Config errors
    // remain visible; the host republishes metadata from the resulting repo.
    match ConfigRepository::load(&root) {
        Ok(reloaded) => *repository = reloaded,
        Err(error) => {
            outcome.failure = Some(format!(
                "{}; repository reload failed: {error:#}",
                outcome.failure.take().unwrap_or_default()
            ));
        }
    }
    outcome.warnings.push("Each document commits atomically; the set is not a multi-file transaction. No hardware restoration or automatic activation is performed by maintenance".into());
    Ok(outcome)
}

#[derive(Debug, Serialize)]
pub enum ReportLifecycle {
    Starting,
    Running,
    Stopping,
    Failed,
    Stopped,
}
#[derive(Debug, Serialize)]
pub struct SoftwareStatus {
    pub application_version: String,
    pub source_revision: Option<String>,
    pub lifecycle: ReportLifecycle,
    pub thermal_active: bool,
    pub automation_enabled: bool,
    pub macros_enabled: bool,
    pub pending_native_io: bool,
    pub read_error_count: usize,
}
#[derive(Debug, Serialize)]
pub struct PortableReport {
    pub format: &'static str,
    pub version: u32,
    pub configuration: Vec<BackupFile>,
    pub config_error_count: usize,
    pub software_status: SoftwareStatus,
    pub hardware_acceptance: &'static str,
}
pub fn portable_report(
    repository: &ConfigRepository,
    software_status: SoftwareStatus,
) -> Result<PortableReport> {
    // No raw configuration, user path, process list, SSID, serial or log payload
    // is included automatically. The supplied software_status must be an owned
    // host DTO suitable for deliberate local export, not a dump of settings.
    let bundle = capture(repository)?;
    Ok(PortableReport {
        format: "JiksLocalSoftwareReport",
        version: 1,
        configuration: bundle.manifest.documents,
        config_error_count: repository.errors().len(),
        software_status,
        hardware_acceptance: "Temperature/full-load acceptance is user-owned; this report does not assert hardware success",
    })
}
pub fn log_directory(data_root: &Path) -> PathBuf {
    data_root.join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "JiksMaintenance-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            assert!(
                self.0.starts_with(std::env::temp_dir())
                    && self
                        .0
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("JiksMaintenance-")
            );
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn bundle(files: &[(&str, &str)]) -> BackupBundle {
        BackupBundle::from_documents(
            files
                .iter()
                .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
                .collect(),
        )
        .unwrap()
    }
    fn stopped() -> RestoreQuiescence {
        RestoreQuiescence {
            thermal_stopped: true,
            automation_stopped: true,
            macros_stopped: true,
            native_io_idle: true,
        }
    }
    #[test]
    fn bundle_hashes_exact_raw_bytes_and_rejects_tampering_trailing_and_unsafe_entries() {
        let original = bundle(&[(
            "settings.json",
            "{\"Future\":1e9999,\"future\":-0,\"Precise\":1.234567890123456789}",
        )]);
        let bytes = original.to_bytes().unwrap();
        let decoded = BackupBundle::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.documents, original.documents);
        let mut changed = bytes.clone();
        *changed.last_mut().unwrap() ^= 1;
        assert!(BackupBundle::from_bytes(&changed).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(BackupBundle::from_bytes(&trailing).is_err());
        assert!(
            BackupBundle::from_documents(BTreeMap::from([("../settings.json".into(), vec![])]))
                .is_err()
        );
    }
    #[test]
    fn legacy_directory_import_is_exact_and_does_not_read_args_or_unrelated_files() {
        let temp = Temp::new();
        fs::write(temp.0.join("settings.json"), b"bad original preserved").unwrap();
        fs::write(temp.0.join("args.txt"), b"synthetic never imported").unwrap();
        fs::write(temp.0.join("private.log"), b"synthetic ignored").unwrap();
        let captured = capture_root(&temp.0).unwrap();
        assert_eq!(captured.documents.len(), 1);
        assert_eq!(
            captured.raw_document("settings.json").unwrap(),
            b"bad original preserved"
        );
        assert!(prepare_restore(&captured).is_err());
    }
    #[test]
    fn preflight_checks_every_document_before_any_write_and_disables_all_automatic_actions() {
        let captured = bundle(&[
            (
                "settings.json",
                "{\"EnableCpuThermalControl\":true,\"Future\":1e9999}",
            ),
            ("automation.json", "{\"IsEnabled\":true,\"Pipelines\":[]}"),
            ("macro.json", "{\"IsEnabled\":true,\"Sequences\":{}}"),
        ]);
        let plan = prepare_restore(&captured).unwrap();
        for (key, doc) in &plan.documents {
            let flag = if *key == "application" {
                "EnableCpuThermalControl"
            } else {
                "IsEnabled"
            };
            assert_eq!(
                doc.root().get_ci(flag).and_then(JsonNode::as_bool),
                Some(false)
            );
        }
        assert!(plan.documents[0].1.to_json().contains("1e9999"));
        let invalid = bundle(&[
            ("settings.json", "{}"),
            ("automation.json", "broken late document"),
        ]);
        assert!(prepare_restore(&invalid).is_err());
    }
    #[test]
    fn restore_requires_real_quiescence_then_backs_up_original_and_can_repair_corrupt_file() {
        let temp = Temp::new();
        let backups = Temp::new();
        fs::write(temp.0.join("settings.json"), b"corrupt source bytes").unwrap();
        let mut repo = ConfigRepository::load(&temp.0).unwrap();
        let plan = prepare_restore(&bundle(&[(
            "settings.json",
            "{\"EnableCpuThermalControl\":true,\"X\":-0}",
        )]))
        .unwrap();
        assert!(
            commit_restore(
                &mut repo,
                plan.clone(),
                &backups.0,
                RestoreQuiescence::default()
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(&backups.0).unwrap().count(), 0);
        let result = commit_restore(&mut repo, plan, &backups.0, stopped()).unwrap();
        assert!(result.failure.is_none());
        assert_eq!(result.restored, vec!["application"]);
        assert_eq!(
            read_bundle(&result.pre_restore_backup)
                .unwrap()
                .raw_document("settings.json")
                .unwrap(),
            b"corrupt source bytes"
        );
        assert_eq!(
            repo.document("application")
                .unwrap()
                .root()
                .get_ci("EnableCpuThermalControl")
                .and_then(JsonNode::as_bool),
            Some(false)
        );
        assert!(
            repo.document("application")
                .unwrap()
                .to_json()
                .contains("-0")
        );
    }
    #[test]
    fn partial_restore_is_reported_and_successful_scope_is_reloaded_not_claimed_atomic_as_set() {
        let temp = Temp::new();
        let backups = Temp::new();
        fs::write(temp.0.join("settings.json"), b"{\"Old\":true}").unwrap();
        fs::write(
            temp.0.join("automation.json"),
            b"{\"IsEnabled\":false,\"Pipelines\":[]}",
        )
        .unwrap();
        let mut repo = ConfigRepository::load(&temp.0).unwrap();
        let plan = prepare_restore(&bundle(&[
            ("settings.json", "{\"New\":true}"),
            ("automation.json", "{\"IsEnabled\":false,\"Pipelines\":[]}"),
        ]))
        .unwrap();
        let result =
            commit_restore_with_checkpoint(&mut repo, plan, &backups.0, stopped(), |key| {
                if key == "automation" {
                    anyhow::bail!("injected later commit failure")
                } else {
                    Ok(())
                }
            })
            .unwrap();
        assert_eq!(result.restored, vec!["application"]);
        assert!(result.failure.is_some());
        assert_eq!(
            repo.document("application").unwrap().root().get("New"),
            Some(&JsonNode::Bool(true))
        );
        assert!(
            read_bundle(&result.pre_restore_backup)
                .unwrap()
                .raw_document("settings.json")
                .unwrap()
                .windows(3)
                .any(|p| p == b"Old")
        );
    }
    #[test]
    fn backups_are_unique_complete_and_legacy_export_never_replaces_existing_file() {
        let temp = Temp::new();
        let original = bundle(&[("settings.json", "{\"Future\":1e9999}")]);
        let first = write_backup(&original, &temp.0).unwrap();
        let second = write_backup(&original, &temp.0).unwrap();
        assert_ne!(first, second);
        assert_eq!(read_bundle(&first).unwrap().documents, original.documents);
        let exported = export_legacy_folder(&original, &temp.0).unwrap();
        assert!(exported.failure.is_none());
        let again = export_legacy_folder(&original, &temp.0).unwrap();
        assert!(again.failure.is_some());
        assert_eq!(
            fs::read(temp.0.join("settings.json")).unwrap(),
            original.raw_document("settings.json").unwrap()
        );
        assert!(fs::read_dir(&temp.0).unwrap().all(|p| {
            !p.unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".pending")
        }));
    }
}
