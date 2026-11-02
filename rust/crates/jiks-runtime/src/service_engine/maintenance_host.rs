use super::*;
use crate::maintenance::{self, BackupBundle, ReportLifecycle, RestoreQuiescence, SoftwareStatus};
use std::{
    collections::BTreeSet,
    thread::{self, JoinHandle},
};
pub(super) struct Job {
    id: u64,
    pub(super) restoring: bool,
    worker: JoinHandle<Result<Outcome>>,
}
enum Outcome {
    Backup(PathBuf),
    Restored(ConfigRepository, maintenance::RestoreOutcome),
    Report(PathBuf),
}
fn selected(bundle: BackupBundle, payload: &Value) -> Result<BackupBundle> {
    let names: BTreeSet<_> = bundle
        .manifest()
        .documents
        .iter()
        .filter(|entry| {
            (entry.name != "macro.json"
                || payload
                    .get("include_macros")
                    .and_then(Value::as_bool)
                    .unwrap_or(true))
                && (entry.name != "automation.json"
                    || payload
                        .get("include_automation")
                        .and_then(Value::as_bool)
                        .unwrap_or(true))
        })
        .map(|entry| entry.name.clone())
        .collect();
    bundle.select(&names)
}
impl Engine {
    pub(super) fn request_maintenance(
        &mut self,
        id: u64,
        command: &str,
        payload: &Value,
    ) -> Result<()> {
        ensure!(self.maintenance.is_none(), "已有维护任务尚未完成");
        let path = if command == "settings.reset" {
            self.repository.root().join("backups")
        } else {
            PathBuf::from(
                payload
                    .get("path")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .context("请填写备份目录、还原来源或报告路径")?,
            )
        };
        ensure!(path.is_absolute(), "请选择完整绝对路径");
        let root = self.repository.root().to_path_buf();
        let payload = payload.clone();
        let command = command.to_owned();
        if command == "settings.restore" || command == "settings.reset" {
            self.writable()?;
            ensure!(
                !self.automation.is_busy() && !self.automation.is_source_active(),
                "请等待自动化与全局监听实际停止并收到清理回执"
            );
            ensure!(
                self.controller.is_none() && self.shadow.is_none() && !self.cpu_possible,
                "请先停止CPU控制并核对恢复"
            );
            ensure!(
                self.pending.is_none() && self.reading.is_none() && self.read_queue.is_empty(),
                "请等待实际设备读取和写入完成后再还原"
            );
            ensure!(
                !self.player.is_active()
                    && self.record_preparation.is_none()
                    && !self.recorder.is_active()
                    && !self.script.is_busy()
                    && !self.packages.is_busy()
                    && !self.aux.is_busy()
                    && !self.keys.is_active()
                    && !self.ui_effects.is_busy()
                    && !self.fps.is_busy()
                    && !macros::native::has_unreleased_input(),
                "请先停止宏播放、录制并核对按键释放"
            );
            ensure!(
                self.repository
                    .document("automation")?
                    .root()
                    .get_ci("IsEnabled")
                    .and_then(JsonNode::as_bool)
                    != Some(true)
                    && self
                        .repository
                        .document("macros")?
                        .root()
                        .get_ci("IsEnabled")
                        .and_then(JsonNode::as_bool)
                        != Some(true),
                "请先关闭自动化与宏监听，再还原设置"
            );
        }
        let software = SoftwareStatus {
            application_version: env!("CARGO_PKG_VERSION").into(),
            source_revision: None,
            lifecycle: ReportLifecycle::Running,
            thermal_active: self.controller.is_some(),
            automation_enabled: self
                .repository
                .document("automation")?
                .root()
                .get_ci("IsEnabled")
                .and_then(JsonNode::as_bool)
                .unwrap_or(false),
            macros_enabled: self
                .repository
                .document("macros")?
                .root()
                .get_ci("IsEnabled")
                .and_then(JsonNode::as_bool)
                .unwrap_or(false),
            pending_native_io: self.pending.is_some()
                || self.reading.is_some()
                || self.aux.is_busy(),
            read_error_count: self
                .cache
                .keys()
                .filter(|key| key.starts_with("error."))
                .count(),
        };
        let restoring = command == "settings.restore" || command == "settings.reset";
        let worker = thread::Builder::new()
            .name("Jiks local maintenance".into())
            .spawn(move || -> Result<Outcome> {
                match command.as_str() {
                    "settings.backup" => {
                        let bundle = selected(maintenance::capture_root(&root)?, &payload)?;
                        ensure!(
                            !bundle.manifest().documents.is_empty(),
                            "尚无已保存设置需要备份"
                        );
                        Ok(Outcome::Backup(maintenance::write_backup(&bundle, &path)?))
                    }
                    "settings.restore" | "settings.reset" => {
                        let bundle = if command == "settings.reset" {
                            BackupBundle::from_documents(
                                crate::config::DOCUMENTS
                                    .iter()
                                    .map(|(key, name)| {
                                        Ok((
                                            (*name).to_owned(),
                                            crate::config::default_document(key)?
                                                .serialize()?
                                                .into_bytes(),
                                        ))
                                    })
                                    .collect::<Result<BTreeMap<_, _>>>()?,
                            )?
                        } else if path.is_dir() {
                            maintenance::capture_root(&path)?
                        } else {
                            maintenance::read_bundle(&path)?
                        };
                        let plan = maintenance::prepare_restore(&selected(bundle, &payload)?)?;
                        let mut repository = ConfigRepository::load(&root)?;
                        let outcome = maintenance::commit_restore(
                            &mut repository,
                            plan,
                            &root.join("backups"),
                            RestoreQuiescence {
                                thermal_stopped: true,
                                automation_stopped: true,
                                macros_stopped: true,
                                native_io_idle: true,
                            },
                        )?;
                        Ok(Outcome::Restored(repository, outcome))
                    }
                    "tools.report" => {
                        let repository = ConfigRepository::load(&root)?;
                        let report = maintenance::portable_report(&repository, software)?;
                        let parent = path.parent().context("报告目标缺少目录")?;
                        ensure!(parent.is_dir(), "报告目标目录不存在");
                        ensure!(!path.exists(), "报告文件已存在；请选择新文件名");
                        let name = path
                            .file_name()
                            .context("报告文件名缺失")?
                            .to_str()
                            .context("报告文件名无效")?;
                        jiks_store::SettingsStore::new(parent)?
                            .write(name, &serde_json::to_vec_pretty(&report)?)?;
                        Ok(Outcome::Report(path))
                    }
                    _ => bail!("未知维护动作"),
                }
            })?;
        self.maintenance = Some(Job {
            id,
            restoring,
            worker,
        });
        self.value("maintenance.busy", json!(true));
        Ok(())
    }
    pub(super) fn poll_maintenance(&mut self) -> Result<()> {
        if !self
            .maintenance
            .as_ref()
            .is_some_and(|job| job.worker.is_finished())
        {
            return Ok(());
        }
        let job = self.maintenance.take().expect("maintenance job");
        let result = job
            .worker
            .join()
            .map_err(|_| anyhow::anyhow!("维护工作线程异常"))?;
        self.value("maintenance.busy", json!(false));
        match result {
            Ok(Outcome::Backup(path)) => self.finish(
                job.id,
                true,
                format!("备份已写入并核对SHA256：{}", path.display()),
            ),
            Ok(Outcome::Report(path)) => self.finish(
                job.id,
                true,
                format!("本地软件报告已导出：{}", path.display()),
            ),
            Ok(Outcome::Restored(repository, outcome)) => {
                self.repository = repository;
                self.publish_configuration()?;
                self.finish(
                    job.id,
                    outcome.failure.is_none(),
                    format!(
                        "已还原{}份设置；启用项保持关闭。还原前备份：{}{}",
                        outcome.restored.len(),
                        outcome.pre_restore_backup.display(),
                        outcome
                            .failure
                            .map(|error| format!("；部分提交失败：{error}"))
                            .unwrap_or_default()
                    ),
                );
            }
            Err(error) => self.finish(job.id, false, format!("维护未完成：{error:#}")),
        }
        Ok(())
    }
}
