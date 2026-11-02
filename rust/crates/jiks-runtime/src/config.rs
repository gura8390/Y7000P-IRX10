use anyhow::{Context, Result, bail, ensure};
use jiks_core::settings::{JsonNode, SettingsDocument, SettingsKind};
use jiks_store::SettingsStore;
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};

pub const DOCUMENTS: &[(&str, &str)] = &[
    ("application", "settings.json"),
    ("godmode", "godmode.json"),
    ("automation", "automation.json"),
    ("macros", "macro.json"),
    ("hardware_sensors", "hardware_sensors.json"),
    ("sensor_dashboard", "sensor_dashboard.json"),
    ("dashboard", "dashboard.json"),
    ("gpu_oc", "gpu_oc.json"),
    ("balance", "balancemode.json"),
    ("integrations", "integrations.json"),
    ("notifications", "notification_settings.json"),
    ("osd", "osd.json"),
    ("special_keys", "special_key.json"),
    ("rgb_keyboard", "rgb_keyboard.json"),
    ("spectrum_keyboard", "spectrum_keyboard.json"),
    ("lamp_array", "lamp_array.json"),
    ("packages", "package_downloader.json"),
    ("sunrise_sunset", "sunrise_sunset.json"),
    ("update", "update_settings.json"),
    ("amd_oc", "amd_oc.json"),
    ("its", "itsmode_settings.json"),
];

#[derive(Clone)]
pub struct ConfigEntry {
    pub file_name: &'static str,
    pub document: SettingsDocument,
    /// A failed stored document is preserved and cannot be overwritten by defaults.
    pub error: Option<String>,
}

pub struct ConfigRepository {
    store: SettingsStore,
    entries: BTreeMap<&'static str, ConfigEntry>,
    revision: u64,
}

impl ConfigRepository {
    pub fn load(root: impl Into<PathBuf>) -> Result<Self> {
        let store = SettingsStore::new(root)?;
        let mut entries = BTreeMap::new();
        for &(key, file_name) in DOCUMENTS {
            let default = default_document(key)?;
            let loaded = store.read(file_name);
            let (document, error) = match loaded {
                Ok(None) => (default, None),
                Ok(Some(bytes)) => {
                    let parsed = std::str::from_utf8(&bytes)
                        .map_err(anyhow::Error::from)
                        .and_then(|text| parse_document(key, file_name, text));
                    match parsed {
                        Ok(document) => (document, None),
                        Err(error) => {
                            let backup = store.backup_failed_snapshot(file_name, &bytes);
                            let message = match backup {
                                Ok(path) => format!(
                                    "{file_name} 无法读取；原文已备份到 {}。修复后重试：{error:#}",
                                    path.display()
                                ),
                                Err(backup_error) => format!(
                                    "{file_name} 无法读取，原文件保留；备份也未成功：{error:#}; {backup_error}"
                                ),
                            };
                            (default, Some(message))
                        }
                    }
                }
                Err(error) => (
                    default,
                    Some(format!("无法读取 {file_name}，不会覆盖原文件：{error}")),
                ),
            };
            entries.insert(
                key,
                ConfigEntry {
                    file_name,
                    document,
                    error,
                },
            );
        }
        Ok(Self {
            store,
            entries,
            revision: 1,
        })
    }

    pub fn root(&self) -> &std::path::Path {
        self.store.root()
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn entries(&self) -> &BTreeMap<&'static str, ConfigEntry> {
        &self.entries
    }
    pub fn entry(&self, key: &str) -> Result<&ConfigEntry> {
        self.entries
            .get(canonical_key(key))
            .with_context(|| format!("未知配置文档：{key}"))
    }
    pub fn document(&self, key: &str) -> Result<&SettingsDocument> {
        Ok(&self.entry(key)?.document)
    }
    pub fn value(&self, key: &str) -> Result<Value> {
        Ok(self.document(key)?.root().to_value()?)
    }

    pub fn save(&mut self, key: &str, document: SettingsDocument) -> Result<()> {
        let key = canonical_key(key);
        let entry = self.entry(key)?;
        if let Some(error) = &entry.error {
            bail!("{error}");
        }
        let bytes = document.serialize()?;
        let verified = parse_document(key, entry.file_name, &bytes)?;
        crate::config_actions::validate_storage_document(key, &verified)?;
        self.store
            .write(entry.file_name, verified.serialize()?.as_bytes())?;
        let entry = self.entries.get_mut(key).expect("validated document key");
        entry.document = verified;
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    pub fn save_value(&mut self, key: &str, value: &Value) -> Result<()> {
        self.save(
            key,
            SettingsDocument::parse(&serde_json::to_string(value)?)?,
        )
    }
    pub fn validate_changes(&self, changes: &[(String, SettingsDocument)]) -> Result<()> {
        let mut candidate: crate::config_actions::Documents = self
            .entries
            .iter()
            .map(|(key, entry)| ((*key).to_owned(), entry.document.clone()))
            .collect();
        for (key, document) in changes {
            let key = canonical_key(key);
            self.entry(key)?;
            crate::config_actions::validate_storage_document(key, document)?;
            candidate.insert(key.to_owned(), document.clone());
        }
        crate::config_actions::project(&candidate)?;
        Ok(())
    }

    /// Update a known setting without treating differently cased unknown keys as aliases.
    pub fn update_known(&mut self, key: &str, updates: &[(&[&str], JsonNode)]) -> Result<()> {
        let mut root = self.document(key)?.root().clone();
        for (path, value) in updates {
            set_known_path(&mut root, path, value.clone())?;
        }
        self.save(key, SettingsDocument::from_root(root)?)
    }

    pub fn errors(&self) -> Vec<String> {
        self.entries
            .values()
            .filter_map(|entry| entry.error.clone())
            .collect()
    }

    /// Explicit import copies complete original documents into this application's own
    /// directory. It does not write the C# source directory or execute stored actions.
    pub fn import_legacy(&mut self, legacy_root: &std::path::Path) -> Result<usize> {
        ensure!(legacy_root != self.root(), "导入来源不能是当前配置目录");
        let mut pending = Vec::new();
        for &(key, name) in DOCUMENTS {
            let path = legacy_root.join(name);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("无法读取导入文件：{}", path.display()));
                }
            };
            let text = std::str::from_utf8(&bytes)?;
            let mut document = parse_document(key, name, text)?;
            // Importing data is not consent to begin CPU control in another program.
            if key == "application" {
                let mut root = document.root().clone();
                root.set_known("EnableCpuThermalControl", JsonNode::Bool(false))?;
                document = SettingsDocument::from_root(root)?;
            }
            if key == "automation" || key == "macros" {
                let mut root = document.root().clone();
                root.set_known("IsEnabled", JsonNode::Bool(false))?;
                document = SettingsDocument::from_root(root)?;
            }
            pending.push((key, document));
        }
        // All documents parse before the first commit. Multi-file commits are not
        // advertised as one transaction; each original remains in .previous.
        let count = pending.len();
        self.validate_changes(
            &pending
                .iter()
                .map(|(key, doc)| ((*key).to_owned(), doc.clone()))
                .collect::<Vec<_>>(),
        )?;
        for (key, document) in pending {
            self.save(key, document)?;
        }
        Ok(count)
    }
}

fn canonical_key(key: &str) -> &str {
    match key {
        "app" | "settings" => "application",
        "macro" => "macros",
        _ => key,
    }
}

fn parse_document(key: &str, file_name: &str, text: &str) -> Result<SettingsDocument> {
    if let Some(kind) = SettingsKind::from_filename(file_name) {
        return Ok(kind.parse(text)?);
    }
    let document = SettingsDocument::parse(text)?;
    if key == "application" {
        Ok(document.normalized_application()?)
    } else {
        Ok(document)
    }
}

pub(crate) fn default_document(key: &str) -> Result<SettingsDocument> {
    Ok(SettingsDocument::parse(match key {
        "application" => {
            r#"{"MinimizeToTray":true,"MinimizeOnClose":false,"EnableCpuThermalControl":false,"EnableThermalShadow":false,"EnableHardwareSensors":false,"CpuThermalSamplingMilliseconds":250,"CpuThermalFirmwareLimitC":88,"TemperatureUnit":"C"}"#
        }
        "godmode" => r#"{"ActivePresetId":"00000000-0000-0000-0000-000000000000","Presets":{}}"#,
        "automation" => r#"{"IsEnabled":false,"Pipelines":[]}"#,
        "macros" => r#"{"IsEnabled":false,"Sequences":{}}"#,
        _ => "{}",
    })?)
}

fn set_known_path(root: &mut JsonNode, path: &[&str], value: JsonNode) -> Result<()> {
    let Some((key, remaining)) = path.split_first() else {
        bail!("配置字段路径不能为空");
    };
    if remaining.is_empty() {
        root.set_known(key, value)?;
        return Ok(());
    }
    let mut nested = root.get_ci(key).cloned().unwrap_or_else(JsonNode::object);
    ensure!(nested.as_object().is_some(), "配置字段 {key} 不是对象");
    set_known_path(&mut nested, remaining, value)?;
    root.set_known(key, nested)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "JiksConfig-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            assert!(
                self.0.starts_with(std::env::temp_dir())
                    && self
                        .0
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("JiksConfig-")
            );
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn registry_matches_all_legacy_store_file_names() {
        let mut runtime: Vec<_> = DOCUMENTS.iter().map(|(_, name)| *name).collect();
        let mut legacy: Vec<_> = SettingsKind::ALL
            .iter()
            .map(|kind| kind.filename())
            .collect();
        runtime.sort_unstable();
        legacy.sort_unstable();
        assert_eq!(runtime, legacy);
    }
    #[test]
    fn reading_defaults_does_not_write_or_enable_control() {
        let fixture = Fixture::new();
        let repository = ConfigRepository::load(&fixture.0).unwrap();
        assert_eq!(
            repository.value("application").unwrap()["EnableCpuThermalControl"],
            false
        );
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 0);
        assert!(repository.errors().is_empty());
    }
    #[test]
    fn known_updates_retain_unknown_numeric_lexemes_and_keys() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("settings.json"), r#"{"minimizeonclose":true,"Future":1e9999,"future":-0,"More":{"precise":1.234567890123456789}}"#).unwrap();
        let mut repository = ConfigRepository::load(&fixture.0).unwrap();
        repository
            .update_known(
                "application",
                &[(&["MinimizeOnClose"], JsonNode::Bool(false))],
            )
            .unwrap();
        let text = std::fs::read_to_string(fixture.0.join("settings.json")).unwrap();
        assert!(
            text.contains("1e9999") && text.contains("-0") && text.contains("1.234567890123456789")
        );
        assert!(!text.contains("minimizeonclose"));
        let document = SettingsDocument::parse(&text).unwrap();
        assert_eq!(
            document.root().get("MinimizeOnClose"),
            Some(&JsonNode::Bool(false))
        );
        assert!(document.root().get("Future").is_some() && document.root().get("future").is_some());
    }
    #[test]
    fn invalid_original_is_backed_up_and_never_overwritten_by_defaults() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("settings.json"), b"broken original").unwrap();
        let mut repository = ConfigRepository::load(&fixture.0).unwrap();
        assert!(!repository.errors().is_empty());
        assert!(
            repository
                .save_value("application", &serde_json::json!({}))
                .is_err()
        );
        assert_eq!(
            std::fs::read(fixture.0.join("settings.json")).unwrap(),
            b"broken original"
        );
        let backups: Vec<_> = std::fs::read_dir(&fixture.0)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".invalid-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read(backups[0].path()).unwrap(),
            b"broken original"
        );
    }
    #[test]
    fn invalid_known_semantics_do_not_replace_saved_bytes() {
        let fixture = Fixture::new();
        let mut repository = ConfigRepository::load(&fixture.0).unwrap();
        repository
            .save_value(
                "macros",
                &serde_json::json!({"IsEnabled":false,"Sequences":{},"Future":1.2345}),
            )
            .unwrap();
        let old = std::fs::read(fixture.0.join("macro.json")).unwrap();
        assert!(
            repository
                .save_value(
                    "macros",
                    &serde_json::json!({"Sequences":{"Keyboard,96":{"RepeatCount":-1,"Events":[]}}})
                )
                .is_err()
        );
        assert_eq!(old, std::fs::read(fixture.0.join("macro.json")).unwrap());
    }
    #[test]
    fn import_is_inert_and_does_not_modify_the_original_configuration() {
        let source = Fixture::new();
        let destination = Fixture::new();
        let original = r#"{"EnableCpuThermalControl":true,"Future":1e9999}"#;
        std::fs::write(source.0.join("settings.json"), original).unwrap();
        let mut repository = ConfigRepository::load(&destination.0).unwrap();
        assert_eq!(repository.import_legacy(&source.0).unwrap(), 1);
        assert_eq!(
            repository.value("application").unwrap()["EnableCpuThermalControl"],
            false
        );
        assert_eq!(
            std::fs::read_to_string(source.0.join("settings.json")).unwrap(),
            original
        );
    }
}
