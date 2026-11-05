//! One coordinator, one physical writer and one independent bounded reader.
use crate::physical_port::{PhysicalPort, PhysicalTicket};
use crate::{
    config::ConfigRepository,
    config_actions::{self, ConfigChange, Documents},
    hardware_projection, hardware_routes,
    macros::{self, NativePlayer, NativeRecorder, RecorderSettings},
    service::*,
    thermal_control::{Controller, Prepared},
};
use anyhow::{Context, Result, bail, ensure};
use jiks_core::{
    settings::{ApplicationThermalSettings, JsonNode, MacroIdentifier, SettingsDocument},
    thermal::Epoch,
};
use jiks_platform::{
    Completion, CpuSnapshot, ExplicitAction, HardwareAction as A, PlatformWorker,
    TemperatureSample, monotonic_ms,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, RecvTimeoutError, TryRecvError},
    },
    time::Duration,
};
mod automation_host;
mod aux_host;
mod fps_host;
mod integration_host;
mod keys_actions;
mod keys_host;
#[cfg(test)]
mod macro_roundtrip_tests;
mod maintenance_host;
mod notification_host;
mod package_host;
mod policy_host;
mod preferences_host;
mod script_host;
mod sensor_schedule;
mod spectrum_files;
mod startup_host;
mod ui_effects;

enum After {
    SnapshotRead {
        key: String,
    },
    Preferences {
        change: ConfigChange,
    },
    Hardware,
    Persist {
        change: ConfigChange,
    },
    AiPolicy {
        target: u32,
    },
    Prepare {
        change: ConfigChange,
        settings: ApplicationThermalSettings,
    },
    Enable {
        change: ConfigChange,
        prepared: Prepared,
    },
    Stop {
        change: Option<ConfigChange>,
    },
    Control,
    Shutdown,
}
struct Work {
    id: u64,
    message: String,
    remaining: VecDeque<A>,
    last: Value,
    after: After,
    late: bool,
    deadline_tick_ms: u64,
}
struct Pending {
    cancel: Arc<std::sync::atomic::AtomicBool>,
    action: A,
    ticket: Box<dyn PhysicalTicket>,
    work: Work,
    started: u64,
}
struct Reading {
    ticket: Box<dyn PhysicalTicket>,
    key: String,
    initial: bool,
    request_ids: Vec<u64>,
    requested_at: u64,
}
struct RecordPreparation {
    id: u64,
    settings: RecorderSettings,
    deadline: u64,
}

pub(crate) struct Engine {
    repository: ConfigRepository,
    read_only: bool,
    log: Option<std::fs::File>,
    state: Arc<Mutex<Snapshot>>,
    receiver: Receiver<Command>,
    writer: Box<dyn PhysicalPort>,
    reader: Box<dyn PhysicalPort>,
    pending: Option<Pending>,
    reading: Option<Reading>,
    read_queue: VecDeque<(String, A, bool)>,
    read_requests: BTreeMap<String, Vec<u64>>,
    internal_requests: BTreeMap<u64, Option<std::result::Result<(), String>>>,
    read_receipts: BTreeMap<u64, (u64, u64)>,
    next_internal_request: u64,
    maintenance: Option<maintenance_host::Job>,
    automation: automation_host::AutomationHost,
    packages: package_host::PackageHost,
    script: script_host::ScriptHost,
    integration: integration_host::IntegrationHost,
    keys: keys_host::KeysHost,
    aux: aux_host::AuxHost,
    fps: fps_host::FpsHost,
    policy: policy_host::PolicyState,
    ui_effects: ui_effects::UiEffects,
    spectrum_file: Option<spectrum_files::FileJob>,
    spectrum_import_deadline: u64,
    notifications: notification_host::NotificationHost,
    startup_job: Option<startup_host::Job>,
    cache: BTreeMap<String, Value>,
    initializing: bool,
    identity_supported: bool,
    next_metrics: u64,
    next_presentation: u64,
    next_temperature: u64,
    next_control: u64,
    last_temperature: Option<TemperatureSample>,
    intel_sample: Option<jiks_platform::intel_sensors::IntelSample>,
    gpu_sampling_requested: bool,
    gpu_process_monitoring: bool,
    next_gpu_metadata: u64,
    ai_policy_attempt: Option<String>,
    ai_policy_owned: bool,
    controller: Option<Controller>,
    shadow: Option<Controller>,
    cpu_possible: bool,
    epoch: u64,
    shutdown: Option<u64>,
    writer_closed: bool,
    reader_closed: bool,
    reader_close: Option<Box<dyn PhysicalTicket>>,
    player: NativePlayer,
    recorder: NativeRecorder,
    macro_request: Option<u64>,
    record_stop: Option<u64>,
    record_preparation: Option<RecordPreparation>,
    recording_event_count: u64,
    recording_revision: u64,
    faulted: bool,
    macro_cleanup_requested: bool,
    #[cfg(test)]
    isolated_hardware: bool,
}
impl Engine {
    fn cancel_record_preparation(&mut self, message: &str) {
        if let Some(preparing) = self.record_preparation.take() {
            self.value("macro.preparing", json!(false));
            self.value("macro.prepare_remaining_seconds", json!(0));
            self.finish(preparing.id, false, message);
        }
    }
    fn with_fps<T>(
        &mut self,
        operation: impl FnOnce(&mut fps_host::FpsHost, &mut Self) -> Result<T>,
    ) -> Result<T> {
        let mut fps = std::mem::take(&mut self.fps);
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(&mut fps, self)));
        self.fps = fps;
        match result {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
    pub(crate) fn new(
        root: PathBuf,
        read_only: bool,
        state: Arc<Mutex<Snapshot>>,
        receiver: Receiver<Command>,
    ) -> Result<Self> {
        let repository = ConfigRepository::load(root)?;
        let writer = PlatformWorker::connect(repository.root().join("cpu-owner.json"))?;
        let reader = PlatformWorker::connect(repository.root().join("reader-unused.json"))?;
        let mut engine = Self {
            repository,
            read_only,
            log: None,
            state,
            receiver,
            writer: Box::new(writer),
            reader: Box::new(reader),
            pending: None,
            reading: None,
            read_queue: VecDeque::new(),
            read_requests: BTreeMap::new(),
            internal_requests: BTreeMap::new(),
            read_receipts: BTreeMap::new(),
            next_internal_request: u64::MAX,
            maintenance: None,
            automation: automation_host::AutomationHost::default(),
            packages: package_host::PackageHost::default(),
            script: script_host::ScriptHost::default(),
            integration: integration_host::IntegrationHost::default(),
            keys: keys_host::KeysHost::default(),
            aux: aux_host::AuxHost::default(),
            fps: fps_host::FpsHost::default(),
            policy: policy_host::PolicyState::default(),
            ui_effects: ui_effects::UiEffects::default(),
            spectrum_file: None,
            spectrum_import_deadline: 0,
            notifications: Default::default(),
            startup_job: None,
            cache: BTreeMap::new(),
            initializing: true,
            identity_supported: false,
            next_metrics: 0,
            next_presentation: 0,
            next_temperature: 0,
            next_control: 0,
            last_temperature: None,
            intel_sample: None,
            gpu_sampling_requested: false,
            gpu_process_monitoring: false,
            next_gpu_metadata: 0,
            ai_policy_attempt: None,
            ai_policy_owned: false,
            controller: None,
            shadow: None,
            cpu_possible: false,
            epoch: 0,
            shutdown: None,
            writer_closed: false,
            reader_closed: false,
            reader_close: None,
            player: NativePlayer::new(),
            recorder: NativeRecorder::new(),
            macro_request: None,
            record_stop: None,
            record_preparation: None,
            recording_event_count: 0,
            recording_revision: 0,
            faulted: false,
            macro_cleanup_requested: false,
            #[cfg(test)]
            isolated_hardware: false,
        };
        engine.identity()?;
        engine.automation.set_waker(None)?;
        if !read_only
            && engine
                .repository
                .document("application")?
                .root()
                .get_ci("EnableLogging")
                .and_then(JsonNode::as_bool)
                == Some(true)
        {
            let result = (|| -> Result<std::fs::File> {
                let directory = engine.repository.root().join("logs");
                std::fs::create_dir_all(&directory)?;
                Ok(std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(directory.join(format!(
                        "jiks-{}-{}.log",
                        std::process::id(),
                        monotonic_ms()
                    )))?)
            })();
            match result {
                Ok(log) => engine.log = Some(log),
                Err(error) => {
                    engine.value("backend.log_error", json!(format!("日志未创建：{error:#}")))
                }
            }
        }
        engine.queue_discovery();
        engine.publish_configuration()?;
        Ok(engine)
    }
    fn documents(&self) -> Documents {
        self.repository
            .entries()
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.document.clone()))
            .collect()
    }
    fn identity(&mut self) -> Result<()> {
        let identity = jiks_platform::read_machine_identity()?;
        self.identity_supported = identity.is_supported();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.machine = format!("Y7000P IRX10 · {} · {}", identity.model, identity.cpu);
        state
            .values
            .insert("machine.supported".into(), json!(identity.is_supported()));
        state
            .values
            .insert("backend.read_only".into(), json!(self.read_only));
        state.revision += 1;
        Ok(())
    }
    fn enqueue_read(&mut self, key: String, action: A, initial: bool) {
        if self.shutdown.is_some() {
            return;
        }
        if let Some(queued) = self.read_queue.iter_mut().find(|v| v.0 == key) {
            queued.1 = action;
            queued.2 |= initial;
            return;
        }
        if self.read_queue.len() >= 64 {
            self.value(
                "backend.discovery_warning",
                json!("读取队列已满；本次重复刷新未入队"),
            );
            return;
        }
        self.read_queue.push_back((key, action, initial));
    }
    fn request_read(&mut self, id: u64, key: String, action: A) -> Result<()> {
        ensure!(self.shutdown.is_none(), "后台正在退出");
        ensure!(
            self.read_queue.len() < 64 || self.read_queue.iter().any(|v| v.0 == key),
            "读取队列已满"
        );
        let waiters = self.read_requests.entry(key.clone()).or_default();
        ensure!(waiters.len() < 32, "同一设备读取的待确认请求过多");
        waiters.push(id);
        self.enqueue_read(key, action, false);
        Ok(())
    }
    fn queue_discovery(&mut self) {
        for (key, action, initial) in [
            ("power_modes".into(), A::ReadPowerModes, true),
            ("hybrid".into(), A::ReadHybrid, true),
            ("ai".into(), A::ReadAi, true),
            ("cpu".into(), A::ReadCpu, true),
            ("capabilities".into(), A::ReadCapabilities, true),
            ("godmode".into(), A::ReadGodMode, true),
            ("devices".into(), A::ReadDevices, true),
            ("gpu.nodes".into(), A::ReadGpuDeviceNodes, true),
            ("energy".into(), A::ReadEnergy, true),
            ("battery.threshold".into(), A::ReadChargeThreshold, true),
            ("system".into(), A::ReadSystem, true),
            ("wireless".into(), A::ReadWireless, true),
            ("autorun".into(), A::ReadAutorun, true),
            ("display_controls".into(), A::ReadDisplayControls, true),
        ] {
            self.enqueue_read(key, action, initial);
        }
        for (key, feature) in [
            ("overdrive", jiks_platform::domain::MappedFeature::Overdrive),
            (
                "flip_to_start",
                jiks_platform::domain::MappedFeature::FlipToStart,
            ),
            ("igpu", jiks_platform::domain::MappedFeature::IgpuMode),
            (
                "instant_ac",
                jiks_platform::domain::MappedFeature::InstantBootAc,
            ),
            (
                "instant_usb",
                jiks_platform::domain::MappedFeature::InstantBootUsb,
            ),
            (
                "fan_full_speed",
                jiks_platform::domain::MappedFeature::FanFullSpeed,
            ),
            ("fnq", jiks_platform::domain::MappedFeature::GodModeFnQ),
            (
                "power_led",
                jiks_platform::domain::MappedFeature::PowerLedSync,
            ),
        ] {
            self.enqueue_read(format!("mapped.{key}"), A::ReadMapped(feature), true);
        }
        // No NVAPI/NVML initialization, no Spectrum handshake, no firmware temperature probe at startup.
    }
    fn conflict(&self) -> Option<String> {
        #[cfg(test)]
        if self.isolated_hardware {
            return None;
        }
        jiks_platform::ensure_no_other_controllers()
            .err()
            .map(|e| format!("{e:#}"))
    }
    fn publish_configuration(&mut self) -> Result<()> {
        let docs = self.documents();
        let projection = config_actions::project(&docs)?;
        let mut documents = BTreeMap::new();
        for (key, entry) in self.repository.entries() {
            documents.insert(
                (*key).into(),
                Document {
                    title: (*key).into(),
                    value: ArcValue(Arc::new(entry.document.root().to_value()?)),
                    writable: !self.read_only && entry.error.is_none(),
                    reason: entry.error.clone().unwrap_or_default(),
                },
            );
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.documents = Arc::new(documents);
        state.values.extend(projection.values);
        let mut options = state.options.as_ref().clone();
        options.extend(projection.options);
        state.options = Arc::new(options);
        let mut collections = state.collections.as_ref().clone();
        collections.extend(projection.collections);
        state.collections = Arc::new(collections);
        let mut capabilities = state.capabilities.as_ref().clone();
        for command in [
            "refresh",
            "device.details",
            "tools.open_logs",
            "tools.open_folder",
            "tools.open_data",
            "tools.about",
            "tools.credits",
            "tools.donate",
            "tools.power_settings",
            "packages.open_directory",
            "tools.report",
            "tools.show",
            "update.check",
            "read.boot_logo",
            "read.autorun",
            "battery.refresh",
            "battery.history.refresh",
            "keyboard.candidates",
            "keyboard.effects.import",
            "keyboard.effects.export",
            "startup.arguments.load",
            "startup.arguments.save",
        ] {
            capabilities.insert(
                command.into(),
                Capability {
                    available: true,
                    reason: String::new(),
                },
            );
        }
        for command in [
            "settings.fields.save",
            "app.preferences.save",
            "software.apply",
            "sensors.settings.save",
            "thermal.apply",
            "thermal.stop",
            "automation.settings.save",
            "automation.save",
            "automation.delete",
            "automation.move_up",
            "automation.move_down",
            "automation.run",
            "macro.settings.save",
            "macro.save",
            "macro.delete",
            "macro.run",
            "macro.stop",
            "macro.record.start",
            "macro.record.stop",
            "preset.save",
            "preset.delete",
            "power.ai.settings",
            "gpu.settings.save",
            "gpu.process.preference",
            "osd.apply",
            "osd.position.save",
            "settings.backup",
            "settings.restore",
            "settings.reset",
            "script.run",
            "script.stop",
            "script.console",
            "integration.apply",
            "keys.discover.start",
            "keys.discover.stop",
            "keys.retry_cleanup",
            "sensors.aux.retry_cleanup",
            "fps.retry_cleanup",
            "battery.timer.reset",
        ] {
            capabilities.insert(
                command.into(),
                Capability {
                    available: !self.read_only,
                    reason: if self.read_only {
                        "只读运行".into()
                    } else {
                        String::new()
                    },
                },
            );
        }
        state.capabilities = Arc::new(capabilities);
        state.metadata_revision += 1;
        state.revision += 1;
        drop(state);
        self.publish_native(false)?;
        self.publish_packages()?;
        Ok(())
    }
    fn publish_native(&mut self, live: bool) -> Result<()> {
        if live && monotonic_ms() < self.next_presentation {
            return Ok(());
        }
        self.next_presentation = monotonic_ms().saturating_add(self.display_interval_ms()?);
        let conflict = if live { None } else { self.conflict() };
        let mut projection = hardware_projection::project(
            &self.cache,
            self.read_only || !self.identity_supported,
            conflict.as_deref(),
        );
        let step_capabilities = (!live)
            .then(|| automation_host::step_capabilities(&projection, self.read_only));
        crate::sensor_presentation::apply(
            &mut projection.metrics,
            &self.cache,
            self.repository.document("application")?,
            self.repository.document("hardware_sensors")?,
        );
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.values.extend(projection.values);
        state.values.insert(
            "backend.discovery_errors".into(),
            Value::Object(
                self.cache
                    .iter()
                    .filter_map(|(key, value)| {
                        key.strip_prefix("error.")
                            .map(|name| (name.to_owned(), value.clone()))
                    })
                    .take(64)
                    .collect(),
            ),
        );
        projection
            .metrics
            .retain(|metric| !["Fps", "LowFps", "FrameTime"].contains(&metric.key.as_str()));
        projection.metrics.extend(self.fps.metrics());
        state.metrics = projection.metrics;
        if let Some(step_capabilities) = step_capabilities {
            projection.capabilities.extend(step_capabilities);
            if let Some(information) = self.cache.get("device.information") {
                projection.collections.insert(
                    "device.information".into(),
                    vec![Item {
                        id: "current".into(),
                        label: "设备信息".into(),
                        detail: "仅保留在本机".into(),
                        value: information.clone(),
                    }],
                );
            }
            let capabilities_changed =
                extend_changed(&mut state.capabilities, projection.capabilities);
            let options_changed = extend_changed(&mut state.options, projection.options);
            let collections_changed =
                extend_changed(&mut state.collections, projection.collections);
            let ranges_changed = state.ranges.as_ref() != &projection.ranges;
            if ranges_changed {
                state.ranges = Arc::new(projection.ranges);
            }
            if capabilities_changed || options_changed || collections_changed || ranges_changed {
                state.metadata_revision += 1;
            }
        }
        state.revision += 1;
        Ok(())
    }
    pub(crate) fn contain_fault(&mut self, id: u64, error: anyhow::Error) {
        self.cancel_record_preparation("后台故障，录制准备已取消，未注册录制钩子");
        self.automation.cancel();
        self.packages.cancel();
        self.script.cancel();
        self.keys.cancel();
        self.aux.cancel();
        self.fps.cancel();
        if let Some(pending) = &self.pending {
            pending
                .cancel
                .store(true, std::sync::atomic::Ordering::Release);
        }
        self.faulted = true;
        self.player.stop();
        self.recorder.stop();
        self.value("backend.faulted", json!(true));
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let mut capabilities = state.capabilities.as_ref().clone();
            for (name, capability) in &mut capabilities {
                if ![
                    "refresh",
                    "device.details",
                    "thermal.stop",
                    "tools.open_logs",
                    "keys.retry_cleanup",
                    "sensors.aux.retry_cleanup",
                    "fps.retry_cleanup",
                    "macro.stop",
                    "script.stop",
                ]
                .contains(&name.as_str())
                {
                    capability.available = false;
                    capability.reason = "后台故障尚未恢复；请先停止并核对设备状态".into();
                }
            }
            state.capabilities = Arc::new(capabilities);
            state.metadata_revision = state.metadata_revision.wrapping_add(1);
            state.revision = state.revision.wrapping_add(1);
        }
        self.value(
            "thermal.status",
            json!("后台异常；保留设备会话，仍可停止并核对恢复"),
        );
        self.finish(
            id,
            false,
            format!("后台操作未完成，协调器与所有权已保留：{error:#}"),
        );
    }
    fn new_internal_request(&mut self) -> Result<u64> {
        ensure!(self.internal_requests.len() < 128, "后台内部操作待确认过多");
        let id = self.next_internal_request;
        self.next_internal_request = self
            .next_internal_request
            .checked_sub(1)
            .context("内部请求编号已耗尽")?;
        ensure!(id > (1u64 << 63), "内部请求编号已耗尽");
        self.internal_requests.insert(id, None);
        Ok(id)
    }
    fn with_automation<T>(
        &mut self,
        operation: impl FnOnce(&mut automation_host::AutomationHost, &mut Self) -> Result<T>,
    ) -> Result<T> {
        let mut host = std::mem::take(&mut self.automation);
        // Restore the host even through panic containment: listeners, process
        // waits and actual IO receipts must never disappear during unwinding.
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(&mut host, self)));
        self.automation = host;
        match result {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
    fn take_internal_result(&mut self, id: u64) -> Option<std::result::Result<(), String>> {
        if self.internal_requests.get(&id).is_some_and(Option::is_some) {
            self.internal_requests.remove(&id).flatten()
        } else {
            None
        }
    }
    fn finish(&mut self, id: u64, ok: bool, message: impl Into<String>) {
        let message = message.into();
        if let Some(log) = self.log.as_mut() {
            use std::io::Write;
            let _ = writeln!(
                log,
                "tick={} request={} result={} {}",
                monotonic_ms(),
                id,
                if ok { "ok" } else { "error" },
                message.replace(['\r', '\n'], " ")
            );
        }
        if let Some(slot) = self.internal_requests.get_mut(&id) {
            if slot.is_none() {
                *slot = Some(if ok { Ok(()) } else { Err(message) });
            }
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.notice = message.clone();
        if id != 0 {
            state.results.push_back(CommandResult {
                request_id: id,
                ok,
                message,
            });
        }
        while state.results.len() > 128 {
            state.results.pop_front();
        }
        state.revision += 1;
    }
    fn value(&self, key: &str, value: Value) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.values.get(key) != Some(&value) {
            state.values.insert(key.into(), value);
            state.revision += 1;
        }
    }
    fn commit(&mut self, change: ConfigChange) -> Result<()> {
        self.repository.validate_changes(&change.documents)?;
        // The complete request was validated before the first file commit; each file is atomic.
        let mut error = None;
        for (key, document) in change.documents {
            if let Err(e) = self.repository.save(&key, document) {
                error = Some(e);
                break;
            }
        }
        self.publish_configuration()?;
        let logging = self
            .repository
            .document("application")?
            .root()
            .get_ci("EnableLogging")
            .and_then(JsonNode::as_bool)
            .unwrap_or(false);
        if !logging {
            self.log = None;
        } else if self.log.is_none() && !self.read_only {
            let directory = self.repository.root().join("logs");
            let opened = (|| -> Result<std::fs::File> {
                std::fs::create_dir_all(&directory)?;
                Ok(std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(directory.join(format!(
                        "jiks-{}-{}.log",
                        std::process::id(),
                        monotonic_ms()
                    )))?)
            })();
            match opened {
                Ok(log) => self.log = Some(log),
                Err(e) => {
                    self.value("backend.log_error", json!(format!("日志未能启用：{e:#}")));
                    return Err(e).context("配置已提交，但日志未能创建；没有报告日志已生效");
                }
            }
        }
        self.value("app.logging_active", json!(self.log.is_some()));
        if let Some(error) = error {
            return Err(error)
                .context("保存只完成了部分文档；各旧版仍保留在.previous，当前界面已重读实际结果");
        }
        Ok(())
    }
    fn start_work(&mut self, id: u64, message: &str, actions: Vec<A>, after: After) -> Result<()> {
        ensure!(
            self.pending.is_none(),
            "上一个物理操作尚未返回，不能提交新的设备命令"
        );
        ensure!(
            !self.reading.as_ref().is_some_and(|read| read.key == "battery.threshold")
                || !actions
                    .iter()
                    .any(|action| matches!(action, A::ChargeThreshold(_))),
            "充电阈值读取尚未返回，请稍后再应用"
        );
        let budget = if actions
            .iter()
            .any(|action| matches!(action, A::SetSoftware { .. } | A::BootLogo(_)))
        {
            60000
        } else {
            2000
        };
        let deadline_tick_ms = actions
            .iter()
            .filter_map(|action| match action {
                A::SetCpuPower {
                    deadline_tick_ms, ..
                }
                | A::SetSoftware {
                    deadline_tick_ms, ..
                } => Some(*deadline_tick_ms),
                _ => None,
            })
            .min()
            .map_or(monotonic_ms().saturating_add(budget), |control| {
                control.min(monotonic_ms().saturating_add(budget))
            });
        let mut work = Work {
            id,
            message: message.into(),
            remaining: actions.into(),
            last: Value::Null,
            after,
            late: false,
            deadline_tick_ms,
        };
        self.dispatch_next(&mut work)?;
        Ok(())
    }
    fn dispatch_next(&mut self, work: &mut Work) -> Result<()> {
        let action = work.remaining.pop_front().context("操作计划不能为空")?;
        // Empty background Fn polling is routine listening, not a foreground
        // operation. Pulsing the UI busy flag at every 50ms poll would otherwise
        // republish the whole live snapshot even while the machine is idle.
        // The physical ticket still owns admission and remains visible in the
        // CLI physical diagnostics; a stalled poll is published below.
        let background_key_read = matches!(action, A::ReadDriverKeyEvents)
            && self.internal_requests.contains_key(&work.id);
        if matches!(action, A::EnableCpu { .. }) {
            self.cpu_possible = true;
        }
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ticket = self.writer.dispatch(
            action.clone(),
            ExplicitAction::with_deadline(work.message.clone(), work.deadline_tick_ms)?
                .with_cancellation(cancel.clone()),
        )?;
        let work = std::mem::replace(
            work,
            Work {
                id: 0,
                message: String::new(),
                remaining: VecDeque::new(),
                last: Value::Null,
                after: After::Hardware,
                late: false,
                deadline_tick_ms: 0,
            },
        );
        self.pending = Some(Pending {
            cancel,
            action,
            ticket,
            work,
            started: monotonic_ms(),
        });
        self.value("backend.busy", json!(!background_key_read));
        Ok(())
    }
    fn poll_write(&mut self) -> Result<()> {
        let Some(pending) = self.pending.as_mut() else {
            return Ok(());
        };
        if matches!(pending.work.after, After::Control) {
            if let Some(controller) = self.controller.as_mut() {
                if controller.check_timeout(monotonic_ms())? {
                    pending.work.late = true;
                }
            }
        }
        match pending.ticket.wait(Duration::ZERO) {
            Ok(Completion::Pending { .. }) => {
                let logical_limit =
                    if matches!(pending.action, A::SetSoftware { .. } | A::BootLogo(_)) {
                        60000
                    } else {
                        1500
                    };
                if !pending.work.late
                    && monotonic_ms().saturating_sub(pending.started) >= logical_limit
                {
                    pending.work.late = true;
                    pending
                        .cancel
                        .store(true, std::sync::atomic::Ordering::Release);
                    let id = pending.work.id;
                    // A quiet background read that exceeds its real deadline
                    // must no longer look idle. Keep the original ticket and
                    // cancellation state until actual completion is returned.
                    self.value("backend.busy", json!(true));
                    if !self.internal_requests.contains_key(&id) {
                        self.finish(
                            id,
                            false,
                            "设备调用仍未完成；后台保留物理操作，暂不接受后续写入",
                        );
                    }
                }
            }
            outcome => {
                let mut pending = self.pending.take().expect("pending");
                self.value("backend.busy", json!(false));
                match outcome {
                    Ok(Completion::Completed(value)) => {
                        if let After::SnapshotRead { key } = &pending.work.after {
                            self.cache.insert(key.clone(), value.clone());
                        } else if let Err(error) = crate::hardware_feedback::absorb(
                            &mut self.cache,
                            &pending.action,
                            &value,
                        ) {
                            self.failed(pending.work, error)?;
                            return Ok(());
                        }
                        if !matches!(pending.work.after, After::SnapshotRead { .. }) {
                            self.publish_native(!matches!(pending.action, A::ChargeThreshold(_) | A::KillGpuApps { .. }))?;
                        }
                        pending.work.last = value;
                        if pending.work.late || self.shutdown.is_some() || self.faulted {
                            if !pending.work.remaining.is_empty() {
                                pending.work.late = true;
                            }
                            pending.work.remaining.clear();
                        }
                        if !pending.work.remaining.is_empty() {
                            if let Err(error) = self.dispatch_next(&mut pending.work) {
                                self.failed(pending.work, error)?;
                            }
                        } else {
                            let id = pending.work.id;
                            if let Err(error) = self.completed(pending.work) {
                                self.contain_fault(id, error);
                            }
                        }
                    }
                    Err(error) => self.failed(pending.work, error)?,
                    _ => unreachable!(),
                }
            }
        }
        Ok(())
    }
    fn failed(&mut self, work: Work, error: anyhow::Error) -> Result<()> {
        if work.message == "gpu.kill_apps.automation" {
            self.value("automation.gpu_kill_result", json!({"error":format!("{error:#}")}));
        }
        if work.message == "battery.threshold.apply" {
            self.cache.remove("battery.threshold");
            self.cache.insert(
                "error.battery.threshold".into(),
                json!(format!("{error:#}")),
            );
            self.publish_native(false)?;
            self.enqueue_read("battery.threshold".into(), A::ReadChargeThreshold, false);
        }
        if work.message == "software.apply" {
            self.enqueue_read("software".into(), A::ReadSoftware, false);
        }
        if work.message == "app.autorun.apply" {
            self.enqueue_read("autorun".into(), A::ReadAutorun, false);
        }
        if work.message.starts_with("display.boot_logo.") {
            self.enqueue_read("boot_logo".into(), A::ReadBootLogo, false);
        }
        if matches!(work.after, After::Persist { .. }) {
            self.value("gpu.settings.uncertain", json!(true));
        }
        if matches!(work.after, After::AiPolicy { .. }) {
            self.value(
                "power.ai.status",
                json!(format!("AI子模式未能核对：{error:#}")),
            );
        }
        if matches!(work.after, After::Control) {
            if let Some(controller) = self.controller.as_mut() {
                controller.complete(Err(anyhow::anyhow!("{error:#}")))?;
            }
        }
        if matches!(work.after, After::Shutdown | After::Enable { .. }) {
            if matches!(work.after, After::Shutdown) {
                self.shutdown = None;
            }
            self.faulted = true;
            self.value("backend.faulted", json!(true));
        }
        self.value("thermal.active", json!(self.controller.is_some()));
        self.finish(
            work.id,
            false,
            format!("{}未能确认：{error:#}。未执行剩余计划", work.message),
        );
        Ok(())
    }
    fn completed(&mut self, work: Work) -> Result<()> {
        let id = work.id;
        match work.after {
            After::Preferences { change } => {
                if work.late || self.shutdown.is_some() || self.faulted {
                    self.finish(
                        id,
                        false,
                        "实际系统调用已返回，未保存其余设置；请核对当前状态",
                    );
                } else {
                    let message = change.message.clone();
                    self.route_configuration(id, change, message)?;
                }
            }
            After::SnapshotRead { .. } => self.finish(
                id,
                !work.late,
                if work.late {
                    "读取/监听操作已返回但请求已取消或超时"
                } else {
                    "读取/监听操作已实际返回"
                },
            ),
            After::Persist { change } => {
                if work.late || self.shutdown.is_some() || self.faulted {
                    self.finish(
                        id,
                        false,
                        "物理操作已返回，超时/退出请求未提交设置；请核对实际读回",
                    );
                } else {
                    self.commit(change)?;
                    self.value("gpu.settings.uncertain", json!(false));
                    self.publish_native(false)?;
                    self.finish(id, true, "设备操作已实际核对，设置已保存");
                }
            }
            After::AiPolicy { target } => {
                self.ai_policy_owned = target > 0;
                self.value(
                    "power.ai.status",
                    json!(format!("AI子模式实际读回：{target}")),
                );
                self.finish(id, !work.late, "AI条件策略已实际核对");
            }
            After::Hardware => {
                if work.message == "gpu.kill_apps.automation" {
                    let count = |key: &str| work.last.get(key).and_then(Value::as_object).map_or(0, |v| v.len());
                    let completed = work.last["completed_pids"].as_array().map_or(0, Vec::len);
                    let (skipped, failed) = (count("skipped"), count("failed"));
                    let stopped = work.last["stopped"].as_str();
                    let mut public = work.last.clone();
                    if let Some(report) = public.as_object_mut() {
                        report.remove("completed_identities");
                    }
                    self.value("automation.gpu_kill_result", public);
                    self.finish(
                        id,
                        !work.late && skipped == 0 && failed == 0 && stopped.is_none(),
                        format!(
                            "独显应用已核对退出 {completed} 个，跳过 {skipped} 个，失败 {failed} 个{}",
                            stopped.map_or(String::new(), |reason| format!("；已停止：{reason}"))
                        ),
                    );
                    self.queue_discovery();
                    return Ok(());
                }
                if work.message.starts_with("keyboard.")
                    && work.last.get("profile").and_then(Value::as_u64).is_some()
                    && work.last.get("effects").is_some_and(Value::is_array)
                {
                    self.cache
                        .insert("keyboard.effects".into(), work.last.clone());
                    self.publish_native(false)?;
                }
                let restart_needs_reboot = work.message == "gpu.restart.automation"
                    && work.last.get("pending_reboot").and_then(Value::as_bool) == Some(true);
                self.finish(
                    id,
                    !work.late && !restart_needs_reboot,
                    if work.late {
                        "设备操作已返回；请求超时或已取消，未补发剩余计划"
                    } else if restart_needs_reboot {
                        "系统接收了独显重启请求，但仍需重启 Windows；本步未确认完成"
                    } else {
                        "设备操作已逐项核对"
                    },
                );
                self.queue_discovery();
            }
            After::Prepare { change, settings } => {
                if work.late || self.shutdown.is_some() || self.faulted {
                    self.finish(
                        id,
                        false,
                        "基线读取已返回；请求已超时或正在退出，未启用温控",
                    );
                    return Ok(());
                }
                let prepared = Prepared::from_discovery(settings, &work.last)?;
                if prepared.settings.enabled {
                    let limit = prepared.settings.firmware_limit_c as u32;
                    self.start_work(
                        id,
                        "启用本机CPU温控",
                        vec![A::EnableCpu {
                            firmware_limit: limit,
                        }],
                        After::Enable { change, prepared },
                    )?;
                } else {
                    self.epoch += 1;
                    self.shadow = Some(prepared.observe(
                        Epoch {
                            policy: self.epoch,
                            mode: self.epoch,
                        },
                        monotonic_ms(),
                    )?);
                    self.commit(change)?;
                    self.finish(id, true, "CPU观察已启用；只计算建议，没有写入固件");
                }
            }
            After::Enable { change, prepared } => {
                if work.late || self.shutdown.is_some() || self.faulted {
                    self.finish(
                        id,
                        false,
                        "迟到的温控启用已返回；不启动控制策略，等待停止与恢复核对",
                    );
                    if self.shutdown.is_none() {
                        self.start_work(
                            0,
                            "回滚迟到的温控启用",
                            vec![A::StopCpu],
                            After::Stop { change: None },
                        )?;
                    }
                    return Ok(());
                }
                let snapshot: CpuSnapshot = serde_json::from_value(work.last)?;
                self.epoch += 1;
                self.controller = Some(prepared.activate(
                    snapshot,
                    Epoch {
                        policy: self.epoch,
                        mode: self.epoch,
                    },
                    monotonic_ms(),
                )?);
                self.shadow = None;
                self.value("thermal.active", json!(true));
                self.value(
                    "thermal.status",
                    json!("已确认固件温限；以Lenovo汇总温度调节，不等于核心最高温"),
                );
                self.commit(change)?;
                self.finish(
                    id,
                    !work.late,
                    "CPU温控会话已核对；物理温度效果仍需你的负载验收",
                );
            }
            After::Stop { change } => {
                self.controller = None;
                self.shadow = None;
                self.cpu_possible = false;
                self.faulted = false;
                self.value("backend.faulted", json!(false));
                self.value("thermal.active", json!(false));
                self.value("thermal.status", json!("已停止并核对仍归本程序持有的字段"));
                if let Some(change) = change {
                    self.commit(change)?;
                }
                self.finish(id, true, "CPU控制已停止，恢复结果已确认");
            }
            After::Control => {
                if let Some(controller) = self.controller.as_mut() {
                    let status = controller
                        .complete(serde_json::from_value(work.last).map_err(anyhow::Error::from))?;
                    self.value("thermal.control_status", json!(status));
                }
            }
            After::Shutdown => {
                ensure!(
                    work.last.get("owner_released").and_then(Value::as_bool) == Some(true)
                        && work.last.get("shutdown_confirmed").and_then(Value::as_bool)
                            == Some(true),
                    "设备后台未明确确认所有权释放"
                );
                self.controller = None;
                self.shadow = None;
                self.cpu_possible = false;
                self.writer_closed = true;
            }
        }
        Ok(())
    }
    fn poll_read(&mut self) -> Result<()> {
        if let Some(reading) = self.reading.as_mut() {
            match reading.ticket.wait(Duration::ZERO) {
                Ok(Completion::Pending { .. }) => {}
                result => {
                    let reading = self.reading.take().expect("reading");
                    match result {
                        Ok(Completion::Completed(value)) => {
                            if reading.key == "temperature" {
                                self.last_temperature = serde_json::from_value(value.clone()).ok();
                            }
                            if reading.key == "intel" {
                                self.intel_sample = serde_json::from_value(value.clone()).ok();
                            }
                            self.cache.remove(&format!("error.{}", reading.key));
                            self.cache.insert(reading.key.clone(), value);
                            if !self.initializing {
                                self.publish_native(matches!(
                                    reading.key.as_str(),
                                    "temperature"
                                        | "telemetry"
                                        | "firmware"
                                        | "intel"
                                        | "gpu.sensors"
                                ))?;
                            }
                            for id in &reading.request_ids {
                                if self.internal_requests.contains_key(id) {
                                    self.read_receipts
                                        .insert(*id, (reading.requested_at, monotonic_ms()));
                                }
                                self.finish(*id, true, "设备读取已完成");
                            }
                        }
                        Err(error) => {
                            self.cache.remove(&reading.key);
                            let message = if reading.key == "battery.threshold" {
                                format!("{error:#}")
                            } else {
                                error.to_string()
                            };
                            self.cache.insert(format!("error.{}", reading.key), json!(message));
                            if reading.key == "temperature" {
                                self.last_temperature = None;
                            }
                            if reading.key == "intel" {
                                self.intel_sample = None;
                            }
                            if reading.key == "gpu.processes" {
                                self.gpu_process_monitoring = false;
                            }
                            if reading.initial {
                                self.value(
                                    "backend.discovery_warning",
                                    json!(format!("{}：{error:#}", reading.key)),
                                );
                            }
                            if !self.initializing {
                                self.publish_native(matches!(
                                    reading.key.as_str(),
                                    "temperature"
                                        | "telemetry"
                                        | "firmware"
                                        | "intel"
                                        | "gpu.sensors"
                                ))?;
                            }
                            for id in &reading.request_ids {
                                if self.internal_requests.contains_key(id) {
                                    self.read_receipts
                                        .insert(*id, (reading.requested_at, monotonic_ms()));
                                }
                                self.finish(*id, false, format!("读取未完成：{error:#}"));
                            }
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }
        if self.reading.is_none()
            && self.initializing
            && self.shutdown.is_none()
            && !self.faulted
            && !self.read_queue.iter().any(|v| v.2)
        {
            self.initializing = false;
            self.publish_native(false)?;
            let errors = self.repository.errors();
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.backend_ready = true;
            state.notice = if !errors.is_empty() {
                errors.join("\n")
            } else if self.read_only {
                "本机只读状态已就绪".into()
            } else {
                "本机状态已就绪，设备更改需明确应用".into()
            };
            state.revision += 1;
        }
        if self.reading.is_none() && self.shutdown.is_none() {
            let writing_threshold = self
                .pending
                .as_ref()
                .is_some_and(|work| matches!(work.action, A::ChargeThreshold(_)));
            // Registry fields are individually visible during the legacy
            // driver transaction. Unrelated reads may still use this worker.
            let next = if writing_threshold {
                self.read_queue
                    .iter()
                    .position(|(key, _, _)| key != "battery.threshold")
                    .and_then(|index| self.read_queue.remove(index))
            } else {
                self.read_queue.pop_front()
            };
            if let Some((key, action, initial)) = next {
                match self
                    .reader
                    .dispatch(action, ExplicitAction::new("只读状态采样")?)
                {
                    Ok(ticket) => {
                        self.reading = Some(Reading {
                            ticket,
                            request_ids: self.read_requests.remove(&key).unwrap_or_default(),
                            requested_at: monotonic_ms(),
                            key,
                            initial,
                        })
                    }
                    Err(error) => {
                        for id in self.read_requests.remove(&key).unwrap_or_default() {
                            self.finish(id, false, format!("读取未提交：{error:#}"));
                        }
                        self.finish(0, false, format!("读取未提交：{error:#}"));
                    }
                }
            }
        }
        Ok(())
    }
    fn periodic(&mut self) -> Result<()> {
        if self.shutdown.is_some()
            || self.initializing
            || self.faulted
            || self.maintenance.as_ref().is_some_and(|job| job.restoring)
        {
            return Ok(());
        }
        let now = monotonic_ms();
        let app =
            ApplicationThermalSettings::from_document(self.repository.document("application")?)?;
        let sensor_interval = self.sensor_demand()?.interval_ms;
        let gpu_visible = {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.values.get("ui.main_visible").and_then(Value::as_bool) == Some(true)
                && state.values.get("ui.page").and_then(Value::as_str) == Some("dashboard")
        };
        if self.gpu_process_monitoring
            && !self.read_only
            && gpu_visible
            && now >= self.next_gpu_metadata
        {
            let interval = self
                .repository
                .document("application")?
                .root()
                .get_ci("GPUMonitoringInterval")
                .and_then(JsonNode::as_i64)
                .unwrap_or(5000)
                .clamp(250, 60000) as u64;
            self.next_gpu_metadata = now.saturating_add(interval);
            if let Some(row) = self
                .cache
                .get("gpu")
                .and_then(Value::as_array)
                .filter(|rows| rows.len() == 1)
                .map(|rows| &rows[0])
            {
                if let (Some(index), Some(name)) = (
                    row.get("index")
                        .and_then(Value::as_u64)
                        .and_then(|v| u32::try_from(v).ok()),
                    row.get("name").and_then(Value::as_str),
                ) {
                    self.enqueue_read(
                        "gpu.processes".into(),
                        A::ReadGpuProcesses {
                            index,
                            expected_name: name.into(),
                        },
                        false,
                    );
                }
            }
        }
        if (!app.hardware_sensors_enabled || self.read_only || sensor_interval.is_none())
            && self.gpu_sampling_requested
        {
            self.gpu_sampling_requested = false;
            self.cache.remove("gpu.sensors");
            self.enqueue_read("gpu.sensors".into(), A::StopGpuSensors, false);
        }
        let observing = self.controller.is_some() || self.shadow.is_some();
        if observing
            && now >= self.next_temperature
            && self.reading.is_none()
            && self.read_queue.is_empty()
        {
            self.next_temperature = now.saturating_add(app.sampling_ms.max(100));
            if !self.read_only {
                self.enqueue_read("intel".into(), A::ReadIntelSensors, false);
            }
            self.enqueue_read("temperature".into(), A::ReadCpuTemperature, false);
        }
        if let Some(interval) =
            sensor_interval.filter(|_| app.hardware_sensors_enabled && !self.read_only)
        {
            // A newly visible/faster consumer cannot inherit a previous slow
            // dashboard deadline. Keep an already earlier deadline unchanged.
            self.next_metrics = self.next_metrics.min(now.saturating_add(interval));
            self.next_presentation = self.next_presentation.min(now.saturating_add(interval));
        }
        if let Some(interval) = sensor_interval
            .filter(|_| app.hardware_sensors_enabled && !self.read_only && now >= self.next_metrics)
        {
            self.gpu_sampling_requested = true;
            self.next_metrics = now.saturating_add(interval);
            if !self.read_queue.iter().any(|v| v.0 == "telemetry") {
                self.enqueue_read("telemetry".into(), A::ReadTelemetry, false);
            }
            if !observing {
                self.enqueue_read("intel".into(), A::ReadIntelSensors, false);
            }
            let igpu = self
                .repository
                .document("hardware_sensors")?
                .root()
                .get_ci("SelectedGpuIsIgpu")
                .and_then(JsonNode::as_bool)
                .unwrap_or(false);
            if igpu {
                let inventory = self.cache.get("gpu.integrated").and_then(Value::as_array);
                if let Some(rows) = inventory.filter(|rows| rows.len() == 1) {
                    let row = &rows[0];
                    if let (Some(low), Some(high), Some(name)) = (
                        row.get("luid_low")
                            .and_then(Value::as_u64)
                            .and_then(|v| u32::try_from(v).ok()),
                        row.get("luid_high")
                            .and_then(Value::as_i64)
                            .and_then(|v| i32::try_from(v).ok()),
                        row.get("name").and_then(Value::as_str),
                    ) {
                        self.enqueue_read(
                            "gpu.sensors".into(),
                            A::ReadIntegratedGpuSensors {
                                luid_low: low,
                                luid_high: high,
                                expected_name: name.into(),
                            },
                            false,
                        );
                    }
                } else if inventory.is_none() && !self.cache.contains_key("error.gpu.integrated") {
                    self.enqueue_read(
                        "gpu.integrated".into(),
                        A::ReadIntegratedGpuInventory,
                        false,
                    );
                }
            } else {
                let inventory = self.cache.get("gpu").and_then(Value::as_array);
                if let Some(rows) = inventory.filter(|rows| rows.len() == 1) {
                    if let (Some(index), Some(name)) = (
                        rows[0]
                            .get("index")
                            .and_then(Value::as_u64)
                            .and_then(|v| u32::try_from(v).ok()),
                        rows[0].get("name").and_then(Value::as_str),
                    ) {
                        self.enqueue_read(
                            "gpu.sensors".into(),
                            A::ReadGpuSensors {
                                index,
                                expected_name: name.into(),
                            },
                            false,
                        );
                    }
                } else if inventory.is_none() && !self.cache.contains_key("error.gpu") {
                    self.enqueue_read("gpu".into(), A::ReadNvidia, false);
                }
            }
            if !self.read_queue.iter().any(|v| v.0 == "firmware") {
                self.enqueue_read("firmware".into(), A::ReadFirmwareSensors, false);
            }
        }
        self.poll_control(now, &app)?;
        self.value(
            "osd.visible",
            json!(
                app.hardware_sensors_enabled
                    && self
                        .repository
                        .value("osd")?
                        .get("ShowOsd")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
            ),
        );
        Ok(())
    }
    fn poll_control(&mut self, now: u64, app: &ApplicationThermalSettings) -> Result<()> {
        if self.shutdown.is_some()
            || self.initializing
            || self.faulted
            || self.maintenance.as_ref().is_some_and(|job| job.restoring)
        {
            return Ok(());
        }
        if self.pending.is_none() && now >= self.next_control {
            let freshness = self
                .last_temperature
                .as_ref()
                .map(|t| {
                    t.requested_at_tick_ms
                        .saturating_add(app.options.policy.freshness_ms)
                        .saturating_add(1)
                })
                .filter(|deadline| *deadline > now);
            self.next_control = now.saturating_add(app.sampling_ms.max(100));
            if let Some(deadline) = freshness {
                self.next_control = self.next_control.min(deadline);
            }
            if let Some(controller) = self.controller.as_mut() {
                let (tick, action) = controller.sample_sources(
                    self.intel_sample.as_ref(),
                    self.last_temperature.as_ref(),
                    now,
                )?;
                self.value(
                    "thermal.status",
                    json!(format!("{:?} · {:?}", tick.status, tick.source)),
                );
                self.value("thermal.predicted_rise_c", json!(tick.predicted_rise_c));
                if let Some(action) = action {
                    self.start_work(0, "温控策略功耗约束", vec![action], After::Control)?;
                }
            } else if let Some(shadow) = self.shadow.as_mut() {
                let tick = shadow.shadow_sources(
                    self.intel_sample.as_ref(),
                    self.last_temperature.as_ref(),
                    now,
                )?;
                self.value(
                    "thermal.status",
                    json!(format!("观察 · {:?} · 无固件写入", tick.status)),
                );
            }
        }
        Ok(())
    }
    fn request_thermal(&mut self, id: u64, payload: &Value) -> Result<()> {
        self.writable()?;
        ensure!(self.pending.is_none(), "请等待设备操作完成");
        let change = config_actions::thermal_change(
            &JsonNode::parse(&payload.to_string())?,
            &self.documents(),
        )?;
        let document = change
            .documents
            .iter()
            .find(|v| v.0 == "application")
            .map(|v| &v.1)
            .unwrap_or(self.repository.document("application")?);
        let settings = ApplicationThermalSettings::from_document(document)?;
        if settings.enabled {
            settings.validate_control()?;
        }
        if self.cpu_possible && settings.enabled {
            if let Some(controller) = self.controller.as_ref() {
                let current = &controller.settings;
                if current.enabled == settings.enabled
                    && current.options == settings.options
                    && current.sampling_ms == settings.sampling_ms
                    && current.firmware_limit_c == settings.firmware_limit_c
                    && current.shadow_enabled == settings.shadow_enabled
                {
                    self.commit(change)?;
                    self.finish(id, true, "显示设置已保存；已确认的温控会话继续运行");
                    return Ok(());
                }
            }
        }
        if self.cpu_possible {
            // Explicitly stop first; changing policy cannot silently inherit an uncertain older session.
            ensure!(
                !settings.enabled,
                "请先停止当前温控，再应用新的控制参数；显示设置可单独保存"
            );
            self.start_work(
                id,
                "停止CPU控制",
                vec![A::StopCpu],
                After::Stop {
                    change: Some(change),
                },
            )?;
        } else if settings.enabled || settings.shadow_enabled {
            self.start_work(
                id,
                "核对CPU控制基线",
                vec![A::ReadCpu],
                After::Prepare { change, settings },
            )?;
        } else {
            self.shadow = None;
            self.commit(change)?;
            self.finish(id, true, "传感器设置已保存");
        }
        Ok(())
    }
    fn writable(&self) -> Result<()> {
        ensure!(
            self.maintenance.is_none(),
            "维护操作尚未完成，请等待实际结果"
        );
        ensure!(!self.read_only, "当前为只读运行");
        Ok(())
    }
    fn context(&self, payload: &mut Value) -> Result<()> {
        let context =
            hardware_routes::ActionContext::from_cache(&self.cache, payload)?.into_value();
        let object = payload.as_object_mut().context("操作参数必须是对象")?;
        object.insert(
            "deadline_tick_ms".into(),
            json!(monotonic_ms().saturating_add(2000)),
        );
        object.insert("_context".into(), context);
        Ok(())
    }
    fn process(&mut self, command: Command) -> Result<()> {
        if let Command::Execute {
            command, payload, ..
        } = &command
        {
            if command == "automation.effect.complete" {
                let effect = payload
                    .get("effect_id")
                    .and_then(Value::as_u64)
                    .context("界面动作回执缺少编号")?;
                let ok = payload
                    .get("ok")
                    .and_then(Value::as_bool)
                    .context("界面动作回执缺少结果")?;
                let message = payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                return self.with_automation(|host, _| host.ui_complete(effect, ok, message));
            }
        }
        ensure!(
            !self.maintenance.as_ref().is_some_and(|job| job.restoring)
                || matches!(command, Command::Shutdown { .. }),
            "还原操作正在提交，等待实际完成后再操作"
        );
        if self.shutdown.is_some() && !matches!(command, Command::Shutdown { .. }) {
            bail!("后台正在停止并核对恢复");
        }
        if self.faulted {
            let allowed = matches!(&command, Command::Shutdown { .. })
                || matches!(&command,Command::Execute{command,..}if ["thermal.stop","refresh","device.details","keys.retry_cleanup","sensors.aux.retry_cleanup","fps.retry_cleanup","native.effect.complete","automation.effect.complete","ui.visibility"].contains(&command.as_str()));
            ensure!(allowed, "后台故障尚未恢复；请先停止并核对设备状态");
        }
        match command {
            Command::Shutdown { request_id } => {
                if self.shutdown.is_none() {
                    self.shutdown = Some(request_id);
                    self.automation.cancel();
                    self.packages.cancel();
                    self.script.cancel();
                    self.keys.cancel();
                    self.aux.cancel();
                    self.fps.cancel();
                    if let Some(pending) = &self.pending {
                        pending
                            .cancel
                            .store(true, std::sync::atomic::Ordering::Release);
                    }
                    self.macro_cleanup_requested = false;
                    self.player.stop();
                    self.recorder.stop();
                    self.cancel_record_preparation("退出前取消录制准备，未注册录制钩子");
                    self.read_queue.clear();
                    let queued = std::mem::take(&mut self.read_requests);
                    for (_, ids) in queued {
                        for id in ids {
                            self.finish(id, false, "退出前取消尚未开始的读取");
                        }
                    }
                }
            }
            Command::SaveDocument {
                request_id,
                document,
                value,
            } => {
                self.writable()?;
                let new = SettingsDocument::parse(&value.to_string())?;
                config_actions::validate_transition(
                    &document,
                    self.repository.document(&document)?,
                    &new,
                )?;
                if ["automation", "macros", "macro"].contains(&document.as_str()) {
                    let old = self
                        .repository
                        .document(&document)?
                        .root()
                        .get_ci("IsEnabled")
                        .and_then(JsonNode::as_bool)
                        .unwrap_or(false);
                    let next = new
                        .root()
                        .get_ci("IsEnabled")
                        .and_then(JsonNode::as_bool)
                        .unwrap_or(false);
                    ensure!(
                        old == next,
                        "全局监听开关请通过常用设置更改；必须等待真实注册/清理回执"
                    );
                }
                for (field, default) in match document.as_str() {
                    "integrations" => vec![
                        ("HWiNFO", json!(false)),
                        ("CLI", json!(false)),
                        ("JiksPathAdded", json!(false)),
                    ],
                    "application" | "app" | "settings" => vec![
                        ("SmartFnLockFlags", json!(0)),
                        ("KeepDgpuAwake", json!(false)),
                        ("LockedPStateId", json!(-1)),
                    ],
                    "balance" => vec![("AIModeEnabled", json!(false))],
                    "gpu_oc" => vec![("Enabled", json!(false))],
                    "special_keys" => vec![("KeyModes", json!({}))],
                    _ => vec![],
                } {
                    let old = self
                        .repository
                        .document(&document)?
                        .root()
                        .get_ci(field)
                        .map(JsonNode::to_value)
                        .transpose()?
                        .unwrap_or_else(|| default.clone());
                    let next = new
                        .root()
                        .get_ci(field)
                        .map(JsonNode::to_value)
                        .transpose()?
                        .unwrap_or(default);
                    ensure!(
                        old == next,
                        "{field}请通过常用控制更改，并等待实际操作/监听回执"
                    );
                }
                self.repository
                    .validate_changes(&[(document.clone(), new.clone())])?;
                self.repository.save(&document, new)?;
                self.publish_configuration()?;
                self.finish(request_id, true, "配置已保存");
            }
            Command::Execute {
                request_id: id,
                command,
                mut payload,
            } => match command.as_str() {
                "native.effect.complete" => self.complete_native_ui(&payload)?,
                "startup.arguments.load" | "startup.arguments.save" => {
                    self.request_startup_arguments(id, &command, &payload)?
                }
                "keyboard.effects.import" | "keyboard.effects.export" => {
                    self.request_spectrum_file(id, &command, &payload)?
                }
                "keyboard.effects.imported.ack" => self.acknowledge_spectrum_import(&payload)?,
                "ui.visibility" => {
                    ensure!(
                        payload.get("main_visible").is_some_and(Value::is_boolean)
                            && payload.get("osd_visible").is_some_and(Value::is_boolean),
                        "界面可见状态无效"
                    );
                    ensure!(
                        payload.get("status_visible").is_none_or(Value::is_boolean),
                        "状态窗口可见状态无效"
                    );
                    self.value("ui.main_visible", payload["main_visible"].clone());
                    self.value(
                        "ui.page",
                        payload.get("page").cloned().unwrap_or(Value::Null),
                    );
                    self.value("osd.actual_visible", payload["osd_visible"].clone());
                    self.value(
                        "ui.status_visible",
                        payload
                            .get("status_visible")
                            .cloned()
                            .unwrap_or(json!(false)),
                    );
                }
                "tools.show" => self.request_external_show(id)?,
                "battery.refresh" => {
                    self.with_aux(|aux, engine| aux.battery_information(engine, id))?
                }
                "battery.history.refresh" => {
                    self.with_aux(|aux, engine| aux.battery_history(engine, id))?
                }
                "battery.timer.reset" => {
                    self.writable()?;
                    self.with_aux(|aux, engine| aux.reset_timer(engine))?;
                    self.finish(
                        id,
                        true,
                        "当前会话计时已重置；仅已确认电池供电时建立新起点，Windows事件历史未更改",
                    );
                }
                "sensors.aux.retry_cleanup" => self.aux.retry(id),
                "fps.retry_cleanup" => {
                    self.with_fps(|fps, engine| fps.request_retry(engine, id))?
                }
                "refresh" | "device.details" => {
                    self.identity()?;
                    self.queue_discovery();
                    self.finish(id, true, "已安排只读状态刷新");
                }
                "device.information.refresh" => {
                    self.request_read(id, "device.information".into(), A::ReadDeviceInformation)?
                }
                "settings.backup" | "settings.restore" | "settings.reset" | "tools.report" => {
                    self.request_maintenance(id, &command, &payload)?
                }
                "thermal.apply" => self.request_thermal(id, &payload)?,
                "packages.refresh"
                | "packages.filter"
                | "packages.hide_all"
                | "packages.download"
                | "packages.cancel"
                | "packages.hide"
                | "packages.unhide_all"
                | "update.check" => self.request_packages(id, &command, &payload)?,
                "software.refresh" => self.request_read(id, "software".into(), A::ReadSoftware)?,
                "script.run" | "script.stop" => self.request_script(id, &command, &payload)?,
                "integration.apply" => self.request_integration(id, &payload)?,
                "app.preferences.save" => self.request_app_preferences(id, &payload)?,
                "read.boot_logo" => self.request_read(id, "boot_logo".into(), A::ReadBootLogo)?,
                "read.autorun" => self.request_read(id, "autorun".into(), A::ReadAutorun)?,
                "battery.threshold.read" => {
                    let action = hardware_routes::plan(&command, &payload)?
                        .into_iter()
                        .next()
                        .context("缺少充电阈值读取操作")?;
                    self.request_read(id, "battery.threshold".into(), action)?;
                }
                "keys.discover.start" | "keys.discover.stop" | "keys.retry_cleanup" => {
                    self.request_keys(id, &command)?
                }
                "automation.run" => {
                    self.writable()?;
                    let pipeline = payload
                        .get("selection.automation")
                        .or_else(|| payload.get("selection.quick_action"))
                        .or_else(|| payload.get("id"))
                        .and_then(Value::as_str)
                        .context("请选择自动化规则")?;
                    self.with_automation(|host, _| host.manual(id, pipeline))?;
                }
                "gpu.settings.save" => self.request_gpu_settings(id, &payload)?,
                "gpu.oc.apply" => self.request_gpu_overclock(id, &payload)?,
                "power.apply" if payload.get("power.ai").is_some() => {
                    let change = config_actions::apply_action(
                        "power.ai.settings",
                        &JsonNode::parse(&payload.to_string())?,
                        &self.documents(),
                    )?;
                    if let Some(change) = self.with_automation(|host, engine| {
                        host.request_configuration(engine, id, change)
                    })? {
                        self.commit(change)?;
                        self.finish(id, true, "AI条件监听设置已保存；实际子模式以设备读回为准");
                    }
                }
                "thermal.stop" => {
                    self.writable()?;
                    let fields = json!({"thermal.enabled":false,"thermal.shadow":false});
                    let change = config_actions::thermal_change(
                        &JsonNode::parse(&fields.to_string())?,
                        &self.documents(),
                    )?;
                    self.start_work(
                        id,
                        "停止CPU控制并核对恢复",
                        vec![A::StopCpu],
                        After::Stop {
                            change: Some(change),
                        },
                    )?;
                }
                "keyboard.candidates"
                | "gpu.refresh"
                | "gpu.oc.refresh"
                | "gpu.processes.refresh"
                | "gpu.pstate.refresh" => {
                    self.context(&mut payload)?;
                    let actions = hardware_routes::plan(&command, &payload)?;
                    ensure!(actions.len() == 1, "读取请求必须对应一个受控ticket");
                    let key = match command.as_str() {
                        "gpu.oc.refresh" => "gpu.oc",
                        "gpu.processes.refresh" => "gpu.processes",
                        "gpu.pstate.refresh" => "gpu.pstate",
                        name if name.starts_with("gpu") => "gpu",
                        _ => "spectrum",
                    };
                    self.request_read(id, key.into(), actions.into_iter().next().unwrap())?;
                    if command == "gpu.processes.refresh" && !self.read_only {
                        self.gpu_process_monitoring = true;
                        self.next_gpu_metadata = monotonic_ms().saturating_add(5000);
                    }
                }
                "macro.run" => {
                    self.writable()?;
                    ensure!(
                        self.record_preparation.is_none()
                            && !self.player.is_active()
                            && !self.recorder.is_active(),
                        "已有宏操作尚未完成"
                    );
                    let identifier = MacroIdentifier::parse(
                        payload
                            .get("selection.macros")
                            .and_then(Value::as_str)
                            .context("请选择宏")?,
                    )?;
                    let document = macros::MacroDocument::from_settings(
                        self.repository.document("macros")?.clone(),
                    )?;
                    let sequence = document.get(&identifier).context("宏已不存在")?.clone();
                    sequence.validate_playback()?;
                    self.player.start(sequence)?;
                    self.macro_request = Some(id);
                    self.value("macro.running", json!(true));
                }
                "macro.stop" => {
                    self.cancel_record_preparation("录制准备已取消，未注册录制钩子");
                    self.player.stop();
                    self.recorder.stop();
                    self.record_stop = Some(id);
                }
                "macro.record.start" => {
                    self.writable()?;
                    ensure!(
                        self.record_preparation.is_none()
                            && !self.recorder.is_active()
                            && !self.player.is_active(),
                        "已有宏播放、录制或准备尚未实际结束"
                    );
                    let flags = match payload.get("mode").and_then(Value::as_str) {
                        Some("keyboard") => 1,
                        Some("keyboard_mouse") => 3,
                        Some("keyboard_mouse_movement") => 7,
                        Some(_) => bail!("未知录制模式"),
                        None => payload
                            .get("macro.record_flags")
                            .and_then(Value::as_u64)
                            .unwrap_or(1),
                    };
                    let settings = RecorderSettings::from_bits(u32::try_from(flags)?)?;
                    self.recording_event_count = 0;
                    self.value("macro.recording.count", json!(0));
                    if settings.movement {
                        self.record_preparation = Some(RecordPreparation {
                            id,
                            settings,
                            deadline: monotonic_ms().saturating_add(3000),
                        });
                        self.value("macro.preparing", json!(true));
                        self.value("macro.prepare_remaining_seconds", json!(3));
                    } else {
                        self.recorder.start(settings)?;
                        self.value("macro.recording", json!(true));
                        self.finish(id, true, "录制已开始；ESC结束，完成后可编辑并保存");
                    }
                }
                "macro.record.stop" => {
                    self.cancel_record_preparation("录制准备已取消，未注册录制钩子");
                    self.recorder.stop();
                    self.record_stop = Some(id);
                }
                _ => {
                    if config_command(&command) {
                        self.writable()?;
                        let context = {
                            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                            config_actions::ActionContext {
                                new_id: Some(new_guid()?),
                                ranges: state.ranges.as_ref().clone(),
                                discrete: state
                                    .options
                                    .iter()
                                    .filter_map(|(key, options)| {
                                        let values = options
                                            .iter()
                                            .map(|o| {
                                                o.value.as_i64().and_then(|n| i32::try_from(n).ok())
                                            })
                                            .collect::<Option<Vec<_>>>()?;
                                        (!values.is_empty()).then(|| (key.clone(), values))
                                    })
                                    .collect(),
                            }
                        };
                        let change = config_actions::apply_action_with_context(
                            &command,
                            &JsonNode::parse(&payload.to_string())?,
                            &self.documents(),
                            &context,
                        )?;
                        let message = change.message.clone();
                        self.route_configuration(id, change, message)?;
                    } else {
                        self.writable()?;
                        let mut after = After::Hardware;
                        if command == "preset.apply" {
                            let selected = payload
                                .get("selection.presets")
                                .and_then(Value::as_str)
                                .context("请选择预设")?
                                .to_owned();
                            let projection = config_actions::project(&self.documents())?;
                            let item = projection
                                .collections
                                .get("presets")
                                .and_then(|v| v.iter().find(|v| v.id == selected))
                                .context("预设已不存在")?;
                            payload["preset"] = item.value.clone();
                            let mut root = self.repository.document("godmode")?.root().clone();
                            root.set_known("ActivePresetId", JsonNode::String(selected))?;
                            let change = ConfigChange {
                                documents: vec![(
                                    "godmode".into(),
                                    SettingsDocument::from_root(root)?,
                                )],
                                projection: Default::default(),
                                message: "所选预设已实际应用并保存当前选择".into(),
                            };
                            self.repository.validate_changes(&change.documents)?;
                            after = After::Preferences { change };
                        }
                        self.context(&mut payload)?;
                        if command == "software.apply" {
                            payload["deadline_tick_ms"] =
                                json!(monotonic_ms().saturating_add(60000));
                        }
                        let actions = hardware_routes::plan(&command, &payload)?;
                        let operation = if command == "gpu.restart"
                            && payload.get("gpu.require_started").and_then(Value::as_bool) == Some(true)
                        {
                            "gpu.restart.automation"
                        } else {
                            &command
                        };
                        self.start_work(id, operation, actions, after)?;
                    }
                }
            },
        }
        Ok(())
    }
    fn poll_macros(&mut self) -> Result<()> {
        if let Some(preparing) = self.record_preparation.take() {
            if self.shutdown.is_some() || self.faulted || self.read_only {
                self.record_preparation = Some(preparing);
                self.cancel_record_preparation("录制准备已取消，未注册录制钩子");
            } else if monotonic_ms() < preparing.deadline {
                self.value(
                    "macro.prepare_remaining_seconds",
                    json!(
                        preparing
                            .deadline
                            .saturating_sub(monotonic_ms())
                            .saturating_add(999)
                            / 1000
                    ),
                );
                self.record_preparation = Some(preparing);
            } else {
                self.value("macro.preparing", json!(false));
                self.value("macro.prepare_remaining_seconds", json!(0));
                match self.recorder.start(preparing.settings) {
                    Ok(()) => {
                        self.value("macro.recording", json!(true));
                        self.finish(preparing.id, true, "准备完成，录制钩子已实际注册；ESC结束");
                    }
                    Err(error) => {
                        self.finish(preparing.id, false, format!("录制未开始：{error:#}"))
                    }
                }
            }
        }
        if let Some(result) = self.player.try_finish() {
            self.value("macro.running", json!(false));
            let id = self.macro_request.take().unwrap_or(0);
            match result {
                Ok(report) => self.finish(
                    id,
                    report.errors.is_empty() && report.unreleased.is_empty(),
                    format!(
                        "宏执行结束：{:?}，已发送{}个事件",
                        report.status, report.events_sent
                    ),
                ),
                Err(e) => self.finish(id, false, format!("宏执行异常：{e:#}")),
            }
        }
        if self.recorder.is_active() {
            let events = self.recorder.drain()?;
            self.recording_event_count = self
                .recording_event_count
                .saturating_add(events.len() as u64);
            self.value("macro.recording.count", json!(self.recording_event_count));
        }
        if let Some(result) = self.recorder.try_finish() {
            self.value("macro.recording", json!(false));
            let id = self.record_stop.take().unwrap_or(0);
            match result {
                Ok(report) => {
                    let raw = JsonNode::parse(&report.events_json)?;
                    let events = raw
                        .get_ci("Events")
                        .and_then(JsonNode::as_array)
                        .or_else(|| raw.as_array())
                        .context("录制序列缺少Events数组")?
                        .iter()
                        .map(|event| -> Result<Value> {
                            let event = macros::MacroEvent::from_json(event)?;
                            Ok(config_actions::macro_event_to_ui(&event)?.to_value()?)
                        })
                        .collect::<Result<Vec<Value>>>()?;
                    self.recording_event_count = events.len() as u64;
                    self.value("macro.recording.count", json!(self.recording_event_count));
                    self.recording_revision = self
                        .recording_revision
                        .checked_add(1)
                        .context("录制版本标识已耗尽")?;
                    self.value("macro.recording.revision", json!(self.recording_revision));
                    let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    let mut collections = state.collections.as_ref().clone();
                    collections.insert(
                        "macro.recording".into(),
                        vec![Item {
                            id: format!("recording-{}", self.recording_revision),
                            label: "刚录制的事件".into(),
                            detail: report.reason.clone(),
                            value: json!(events),
                        }],
                    );
                    state.collections = Arc::new(collections);
                    state.metadata_revision += 1;
                    state.revision += 1;
                    drop(state);
                    self.finish(
                        id,
                        !report.interrupted,
                        format!("录制结束：{}；尚未保存", report.reason),
                    );
                }
                Err(e) => self.finish(id, false, format!("录制异常：{e:#}")),
            }
        }
        if !self.player.is_active() && !self.recorder.is_active() {
            if let Some(id) = self.record_stop.take() {
                self.finish(id, true, "宏输入会话已停止");
            }
        }
        Ok(())
    }
    fn poll_shutdown(&mut self) -> Result<bool> {
        let Some(id) = self.shutdown else {
            return Ok(false);
        };
        if let Some(error) = self.fps.cleanup_failure() {
            self.shutdown = None;
            self.finish(
                id,
                false,
                format!("FPS清理未确认，原会话保留，可显式重试后退出：{error}"),
            );
            return Ok(false);
        }
        if self.pending.is_some()
            || self.maintenance.is_some()
            || self.automation.is_busy()
            || self.automation.is_source_active()
            || self.packages.is_busy()
            || self.script.is_busy()
            || self.integration.is_busy()
            || self.integration.needs_cleanup()
            || self.keys.is_busy()
            || self.aux.is_busy()
            || self.fps.is_busy()
            || self.ui_effects.is_busy()
            || self.spectrum_file.is_some()
            || self.startup_job.is_some()
            || self.reading.is_some()
            || self.player.is_active()
            || self.recorder.is_active()
        {
            return Ok(false);
        }
        if macros::native::has_unreleased_input() {
            if !self.macro_cleanup_requested {
                self.macro_cleanup_requested = true;
                let remaining = macros::native::retry_unreleased_input()?;
                if !remaining.is_empty() {
                    self.shutdown = None;
                    self.finish(
                        id,
                        false,
                        "仍有宏按键释放未核对，后台保留；可再次退出重试清理",
                    );
                    return Ok(false);
                }
            }
            if macros::native::has_unreleased_input() {
                return Ok(false);
            }
        }
        if !self.writer_closed {
            let ticket = self.writer.shutdown_ticket()?;
            self.pending = Some(Pending {
                cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                action: A::ShutdownWorker,
                ticket,
                work: Work {
                    id,
                    message: "停止并释放固件所有权".into(),
                    remaining: VecDeque::new(),
                    last: Value::Null,
                    after: After::Shutdown,
                    late: false,
                    deadline_tick_ms: 0,
                },
                started: monotonic_ms(),
            });
            return Ok(false);
        }
        if !self.reader_closed {
            if self.reader_close.is_none() {
                self.reader_close = Some(self.reader.shutdown_ticket()?);
            }
            if let Some(ticket) = self.reader_close.as_mut() {
                match ticket.wait(Duration::ZERO) {
                    Ok(Completion::Pending { .. }) => return Ok(false),
                    Ok(Completion::Completed(value)) => {
                        ensure!(
                            value.get("owner_released").and_then(Value::as_bool) == Some(true)
                                && value.get("shutdown_confirmed").and_then(Value::as_bool)
                                    == Some(true),
                            "读取后台未明确确认所有权释放"
                        );
                        self.reader_closed = true;
                        self.reader_close = None;
                    }
                    Err(e) => {
                        self.shutdown = None;
                        self.reader_close = None;
                        self.finish(id, false, format!("读取后台未能正常停止：{e:#}"));
                        return Ok(false);
                    }
                }
            }
        }
        self.finish(id, true, "后台与宏输入已停止，固件所有权已释放");
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.shutdown_ready = true;
        state.revision += 1;
        Ok(true)
    }
    fn wait_duration(&self, now: u64) -> Result<Duration> {
        Ok(
            if self.pending.is_some()
                || self.maintenance.is_some()
                || self.reader_close.is_some()
                || self.shutdown.is_some()
                || self.automation.is_busy()
                || self.packages.is_busy()
                || self.script.is_busy()
                || self.integration.is_busy()
                || self.aux.polling()
                || self.fps.is_busy() && !self.fps.is_active()
                || self.ui_effects.is_busy()
                || self.spectrum_file.is_some()
                || self.startup_job.is_some()
                || self.reading.is_some()
                || self.player.is_active()
                || self.recorder.is_active()
                || self.record_preparation.is_some()
            {
                Duration::from_millis(25)
            } else if !self.read_queue.is_empty() {
                Duration::ZERO
            } else if self.automation.is_source_active()
                || self.integration.is_active()
                || self.keys.is_active()
                || self.fps.is_active()
            {
                Duration::from_millis(50)
            } else {
                let app = ApplicationThermalSettings::from_document(
                    self.repository.document("application")?,
                )?;
                let mut next = None;
                if self.controller.is_some() || self.shadow.is_some() {
                    next = Some(self.next_temperature.min(self.next_control));
                }
                if app.hardware_sensors_enabled && !self.read_only && self.sensor_consumers()? {
                    next = Some(next.map_or(self.next_metrics, |n: u64| n.min(self.next_metrics)));
                }
                let gpu_visible = {
                    let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    state.values.get("ui.main_visible").and_then(Value::as_bool) == Some(true)
                        && state.values.get("ui.page").and_then(Value::as_str) == Some("dashboard")
                };
                if self.gpu_process_monitoring && !self.read_only && gpu_visible {
                    next = Some(next.map_or(self.next_gpu_metadata, |n: u64| {
                        n.min(self.next_gpu_metadata)
                    }));
                }
                let wait = next
                    .map(|deadline| {
                        Duration::from_millis(deadline.saturating_sub(now).clamp(1, 250))
                    })
                    .unwrap_or(Duration::from_secs(30));
                self.aux.next_deadline().map_or(wait, |deadline| {
                    // Preserve the existing fast-source caps, but an otherwise idle
                    // 2s battery metadata consumer needs neither a 30s stall nor a
                    // permanent 25/250ms polling loop just because its port is alive.
                    wait.min(Duration::from_millis(deadline.saturating_sub(now).max(1)))
                })
            },
        )
    }
    /// Reap actual completion before admission. GUI and CLI requests get the
    /// available slot before periodic Fn reads; an unfinished ticket stays owned.
    fn poll_foreground(&mut self, received: Option<Command>) {
        let mut command = received.or_else(|| match self.receiver.try_recv() {
            Ok(command) => Some(command),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Command::Shutdown { request_id: 0 }),
        });
        // Mark exit before reaping: a completed step must not dispatch the
        // remainder of its plan or start a queued reader after exit was received.
        if matches!(command, Some(Command::Shutdown { .. })) {
            let command = command.take().unwrap();
            let id = command.request_id();
            if let Err(error) = self.process(command) {
                self.finish(id, false, format!("{error:#}"));
            }
        }
        if let Err(error) = self.poll_write() {
            self.contain_fault(0, error);
        }
        // Consume already-returned sensor readings before evaluating a due
        // thermal budget. Completion does not replace the original sample time.
        if let Err(error) = self.poll_read() {
            self.contain_fault(0, error);
        }
        if let Some(command) = command {
            let id = command.request_id();
            if let Err(error) = self.process_foreground(command) {
                self.finish(id, false, format!("{error:#}"));
            }
        }
        if let Err(error) = self.poll_integration() {
            self.value("integration.error", json!(format!("集成未完成：{error:#}")));
        }
        self.poll_foreground_control();
    }
    fn poll_foreground_control(&mut self) {
        // Preserve the controller's due priority over ordinary foreground
        // writers and background reads. Shutdown/stop are admitted first.
        if self.controller.is_some() && monotonic_ms() >= self.next_control {
            let result = (|| -> Result<()> {
                let app = ApplicationThermalSettings::from_document(
                    self.repository.document("application")?,
                )?;
                self.poll_control(monotonic_ms(), &app)
            })();
            if let Err(error) = result {
                self.contain_fault(0, error);
            }
        }
    }
    fn process_foreground(&mut self, command: Command) -> Result<()> {
        if !matches!(&command, Command::Shutdown { .. })
            && !matches!(&command, Command::Execute { command, .. } if command == "thermal.stop")
        {
            self.poll_foreground_control();
        }
        self.process(command)
    }
    pub(crate) fn run(&mut self) -> Result<()> {
        let mut received = None;
        loop {
            self.poll_foreground(received.take());
            if let Err(error) = self.poll_macros() {
                self.contain_fault(0, error);
            }
            if let Err(error) = self.with_automation(|host, engine| host.poll(engine)) {
                self.contain_fault(0, error);
            }
            if let Err(error) = self.poll_maintenance() {
                self.contain_fault(0, error);
            }
            if let Err(error) = self.poll_packages() {
                self.contain_fault(0, error);
            }
            self.poll_script();
            self.poll_ui_effects();
            self.poll_spectrum_file();
            self.poll_startup_arguments();
            if let Err(error) = self.with_keys(|keys, engine| keys.poll(engine)) {
                self.contain_fault(0, error);
            }
            if let Err(error) = self.reconcile_notifications() {
                self.value(
                    "notifications.error",
                    json!(format!("通知未完成：{error:#}")),
                );
            }
            if let Err(error) = self.with_aux(|aux, engine| aux.poll(engine)) {
                self.contain_fault(0, error);
            }
            if let Err(error) = self.with_fps(|fps, engine| fps.poll(engine)) {
                self.contain_fault(0, error);
            }
            match self.poll_shutdown() {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => {
                    let id = self.shutdown.take().unwrap_or(0);
                    self.contain_fault(id, error);
                }
            }
            if let Err(error) = self.periodic() {
                self.contain_fault(0, error);
            }
            let wait = self.wait_duration(monotonic_ms())?;
            match self.receiver.recv_timeout(wait) {
                Ok(command) => received = Some(command),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    if self.shutdown.is_none() {
                        received = Some(Command::Shutdown { request_id: 0 });
                    }
                }
            }
        }
    }
}
// Non-telemetry reads may finish repeatedly with identical presentation
// metadata. Retain the shared maps and the metadata generation in that case.
fn extend_changed<T: Clone + Eq>(
    current: &mut Arc<BTreeMap<String, T>>,
    additions: BTreeMap<String, T>,
) -> bool {
    if additions.iter().all(|(key, value)| current.get(key) == Some(value)) {
        return false;
    }
    Arc::make_mut(current).extend(additions);
    true
}

fn config_command(command: &str) -> bool {
    matches!(
        command,
        "settings.fields.save"
            | "app.preferences.save"
            | "sensors.settings.save"
            | "automation.settings.save"
            | "automation.save"
            | "automation.delete"
            | "automation.move_up"
            | "automation.move_down"
            | "macro.settings.save"
            | "macro.save"
            | "macro.delete"
            | "preset.save"
            | "preset.delete"
            | "power.ai.settings"
            | "gpu.settings.save"
            | "osd.apply"
            | "osd.position.save"
    )
}
fn new_guid() -> Result<String> {
    let mut guid = windows_sys::core::GUID::from_u128(0);
    let result = unsafe { windows_sys::Win32::System::Com::CoCreateGuid(&mut guid) };
    ensure!(result >= 0, "无法创建配置标识符");
    Ok(format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        guid.data1,
        guid.data2,
        guid.data3,
        guid.data4[0],
        guid.data4[1],
        guid.data4[2],
        guid.data4[3],
        guid.data4[4],
        guid.data4[5],
        guid.data4[6],
        guid.data4[7]
    ))
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    pub(super) type Answer = Arc<Mutex<Option<std::result::Result<Value, String>>>>;
    struct FakeTicket(Answer);
    impl PhysicalTicket for FakeTicket {
        fn wait(&mut self, _: Duration) -> Result<Completion> {
            match self.0.lock().unwrap().take() {
                None => Ok(Completion::Pending { operation_id: 1 }),
                Some(Ok(value)) => Ok(Completion::Completed(value)),
                Some(Err(error)) => Err(anyhow::anyhow!(error)),
            }
        }
    }
    struct FakePort {
        log: Arc<Mutex<Vec<A>>>,
        answers: Arc<Mutex<VecDeque<Answer>>>,
    }
    impl PhysicalPort for FakePort {
        fn dispatch(&self, action: A, _: ExplicitAction) -> Result<Box<dyn PhysicalTicket>> {
            self.log.lock().unwrap().push(action);
            Ok(Box::new(FakeTicket(
                self.answers
                    .lock()
                    .unwrap()
                    .pop_front()
                    .context("No fake answer")?,
            )))
        }
        fn shutdown_ticket(&self) -> Result<Box<dyn PhysicalTicket>> {
            self.dispatch(A::ShutdownWorker, ExplicitAction::new("fake shutdown")?)
        }
    }
    static NEXT: AtomicU64 = AtomicU64::new(1);
    pub(super) struct Fixture {
        pub(super) engine: Engine,
        pub(super) sender: std::sync::mpsc::SyncSender<Command>,
        root: PathBuf,
        pub(super) log: Arc<Mutex<Vec<A>>>,
        pub(super) answers: Arc<Mutex<VecDeque<Answer>>>,
    }
    impl Fixture {
        pub(super) fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "JiksEngineFake-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut repository = ConfigRepository::load(&root).unwrap();
            repository
                .save(
                    "notifications",
                    SettingsDocument::parse(r#"{"DontShowNotifications":true}"#).unwrap(),
                )
                .unwrap();
            let state = Arc::new(Mutex::new(Snapshot::default()));
            let (sender, receiver) = std::sync::mpsc::sync_channel(16);
            let log = Arc::new(Mutex::new(vec![]));
            let answers = Arc::new(Mutex::new(VecDeque::new()));
            let port = || {
                Box::new(FakePort {
                    log: log.clone(),
                    answers: answers.clone(),
                }) as Box<dyn PhysicalPort>
            };
            let engine = Engine {
                repository,
                read_only: false,
                log: None,
                state,
                receiver,
                writer: port(),
                reader: port(),
                pending: None,
                reading: None,
                read_queue: VecDeque::new(),
                read_requests: BTreeMap::new(),
                internal_requests: BTreeMap::new(),
                read_receipts: BTreeMap::new(),
                next_internal_request: u64::MAX,
                maintenance: None,
                automation: automation_host::AutomationHost::default(),
                packages: package_host::PackageHost::default(),
                script: script_host::ScriptHost::default(),
                integration: integration_host::IntegrationHost::default(),
                keys: keys_host::KeysHost::default(),
                aux: aux_host::AuxHost::default(),
                fps: fps_host::FpsHost::default(),
                policy: policy_host::PolicyState::default(),
                ui_effects: ui_effects::UiEffects::default(),
                spectrum_file: None,
                spectrum_import_deadline: 0,
                notifications: Default::default(),
                startup_job: None,
                cache: BTreeMap::new(),
                initializing: false,
                identity_supported: true,
                next_metrics: 0,
                next_presentation: 0,
                next_temperature: 0,
                next_control: 0,
                last_temperature: None,
                intel_sample: None,
                gpu_sampling_requested: false,
                gpu_process_monitoring: false,
                next_gpu_metadata: 0,
                ai_policy_attempt: None,
                ai_policy_owned: false,
                controller: None,
                shadow: None,
                cpu_possible: false,
                epoch: 0,
                shutdown: None,
                writer_closed: false,
                reader_closed: false,
                reader_close: None,
                player: NativePlayer::new(),
                recorder: NativeRecorder::new(),
                macro_request: None,
                record_stop: None,
                record_preparation: None,
                recording_event_count: 0,
                recording_revision: 0,
                faulted: false,
                macro_cleanup_requested: false,
                isolated_hardware: true,
            };
            Self {
                engine,
                sender,
                root,
                log,
                answers,
            }
        }
        pub(super) fn answer(&self) -> Answer {
            let a = Arc::new(Mutex::new(None));
            self.answers.lock().unwrap().push_back(a.clone());
            a
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            assert!(
                self.root.starts_with(std::env::temp_dir())
                    && self
                        .root
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("JiksEngineFake-")
            );
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn repeated_native_reads_only_advance_metadata_when_controls_change() {
        let mut f = Fixture::new();
        f.engine.repository.save(
            "application",
            SettingsDocument::parse(r#"{"EnableHardwareSensors":true}"#).unwrap(),
        ).unwrap();
        f.engine.cache.insert("telemetry".into(), json!({"cpu_utilization_percent":12.5}));
        f.engine.publish_native(false).unwrap();
        let initial = f.engine.state.lock().unwrap().clone();
        f.engine.cache.insert("telemetry".into(), json!({"cpu_utilization_percent":15.5}));
        f.engine.publish_native(false).unwrap();
        let next = f.engine.state.lock().unwrap().clone();
        assert!(next.revision > initial.revision);
        assert_eq!(next.metadata_revision, initial.metadata_revision);
        assert!(Arc::ptr_eq(&initial.capabilities, &next.capabilities));
        assert!(Arc::ptr_eq(&initial.options, &next.options));
        assert!(Arc::ptr_eq(&initial.collections, &next.collections));
        assert!(Arc::ptr_eq(&initial.ranges, &next.ranges));
        let metric = |state: &Snapshot| state.metrics.iter()
            .find(|metric| metric.key == "CpuUtilization")
            .unwrap().value.clone();
        assert_ne!(metric(&next), metric(&initial));

        f.engine.cache.insert("gpu.processes".into(), json!({"processes":[]}));
        f.engine.publish_native(false).unwrap();
        let changed = f.engine.state.lock().unwrap().clone();
        assert!(changed.collections.contains_key("gpu.processes"));
        assert_eq!(changed.metadata_revision, next.metadata_revision + 1);
        f.engine.publish_native(false).unwrap();
        assert_eq!(f.engine.state.lock().unwrap().metadata_revision, changed.metadata_revision);
    }
    #[test]
    fn late_completion_never_dispatches_the_remaining_hardware_plan() {
        let mut f = Fixture::new();
        let answer = f.answer();
        f.engine
            .start_work(
                42,
                "fake",
                vec![A::ReadCpu, A::SetRawMode(3)],
                After::Hardware,
            )
            .unwrap();
        f.engine.pending.as_mut().unwrap().work.late = true;
        *answer.lock().unwrap() = Some(Ok(json!({})));
        f.engine.poll_write().unwrap();
        assert_eq!(f.log.lock().unwrap().len(), 1);
        assert!(f.engine.pending.is_none());
        assert!(!f.engine.state.lock().unwrap().results.back().unwrap().ok);
    }
    #[test]
    fn shutdown_during_io_absorbs_completion_without_new_writes() {
        let mut f = Fixture::new();
        let answer = f.answer();
        f.engine
            .start_work(
                42,
                "fake",
                vec![A::ReadCpu, A::SetRawMode(3)],
                After::Hardware,
            )
            .unwrap();
        f.engine.shutdown = Some(9);
        *answer.lock().unwrap() = Some(Ok(json!({})));
        f.engine.poll_write().unwrap();
        assert_eq!(f.log.lock().unwrap().len(), 1);
        assert!(f.engine.pending.is_none());
        assert!(!f.engine.state.lock().unwrap().results.back().unwrap().ok);
    }
    fn send_shutdown(f: &Fixture, already_received: bool) -> Option<Command> {
        let command = Command::Shutdown { request_id: 516 };
        if already_received {
            Some(command)
        } else {
            f.sender.send(command).unwrap();
            None
        }
    }
    #[test]
    fn routine_internal_key_reads_keep_ui_idle_but_retain_physical_admission() {
        let mut f = Fixture::new();
        f.engine.value("backend.busy", json!(false));
        let revision = f.engine.state.lock().unwrap().revision;
        for _ in 0..4 {
            let answer = f.answer();
            let id = f.engine.new_internal_request().unwrap();
            f.engine
                .start_work(
                    id,
                    "keys.driver.read",
                    vec![A::ReadDriverKeyEvents],
                    After::SnapshotRead {
                        key: "keys.driver".into(),
                    },
                )
                .unwrap();
            assert_eq!(f.engine.state.lock().unwrap().revision, revision);
            assert!(f.engine.pending.is_some());
            // A retained physical read still prevents an overlapping writer.
            assert!(
                f.engine
                    .start_work(42, "foreground", vec![A::ReadCpu], After::Hardware)
                    .is_err()
            );
            *answer.lock().unwrap() = Some(Ok(json!({"raw_keys":[]})));
            f.engine.poll_write().unwrap();
            assert!(f.engine.take_internal_result(id).unwrap().is_ok());
            assert_eq!(f.engine.state.lock().unwrap().revision, revision);
        }
        let _answer = f.answer();
        f.engine
            .start_work(42, "foreground", vec![A::ReadCpu], After::Hardware)
            .unwrap();
        assert_eq!(
            f.engine.state.lock().unwrap().values["backend.busy"],
            json!(true)
        );
    }
    #[test]
    fn stalled_internal_key_read_becomes_busy_without_replacing_its_ticket() {
        let mut f = Fixture::new();
        let answer = f.answer();
        let id = f.engine.new_internal_request().unwrap();
        f.engine
            .start_work(
                id,
                "keys.driver.read",
                vec![A::ReadDriverKeyEvents],
                After::SnapshotRead {
                    key: "keys.driver".into(),
                },
            )
            .unwrap();
        f.engine.pending.as_mut().unwrap().started = monotonic_ms().saturating_sub(2000);
        f.engine.poll_write().unwrap();
        let pending = f.engine.pending.as_ref().unwrap();
        assert_eq!(pending.work.id, id);
        assert!(pending.work.late && pending.cancel.load(Ordering::Acquire));
        assert_eq!(
            f.engine.state.lock().unwrap().values["backend.busy"],
            json!(true)
        );
        assert_eq!(f.log.lock().unwrap().len(), 1);
        *answer.lock().unwrap() = Some(Ok(json!({"raw_keys":[]})));
        f.engine.poll_write().unwrap();
        assert!(f.engine.pending.is_none());
        assert_eq!(
            f.engine.state.lock().unwrap().values["backend.busy"],
            json!(false)
        );
        assert!(f.engine.take_internal_result(id).unwrap().is_err());
    }
    #[test]
    fn foreground_shutdown_cancels_remaining_plan_and_queued_read_before_reaping() {
        for already_received in [false, true] {
            let mut f = Fixture::new();
            let first = f.answer();
            f.engine
                .start_work(
                    42,
                    "fake plan",
                    vec![A::ReadCpu, A::SetRawMode(3)],
                    After::Hardware,
                )
                .unwrap();
            f.engine
                .enqueue_read("queued".into(), A::ReadFirmwareSensors, false);
            // Answers are available so a wrong extra dispatch is observable in
            // the log, rather than hidden by a missing-fixture-answer failure.
            let _unexpected_next = f.answer();
            let _unexpected_reader = f.answer();
            *first.lock().unwrap() = Some(Ok(json!({})));
            let shutdown = send_shutdown(&f, already_received);
            f.engine.poll_foreground(shutdown);
            assert_eq!(f.engine.shutdown, Some(516));
            assert!(
                matches!(f.log.lock().unwrap().as_slice(), [A::ReadCpu]),
                "received shutdown allowed the next write or queued reader to start"
            );
            assert!(f.engine.pending.is_none() && f.engine.reading.is_none());
            assert!(f.engine.read_queue.is_empty());
            assert!(!f.engine.state.lock().unwrap().shutdown_ready);
            assert!(
                f.engine
                    .state
                    .lock()
                    .unwrap()
                    .results
                    .iter()
                    .any(|r| r.request_id == 42 && !r.ok)
            );
        }
    }
    #[test]
    fn foreground_shutdown_keeps_unfinished_ticket_and_requires_both_cleanup_barriers() {
        for already_received in [false, true] {
            let mut f = Fixture::new();
            let first = f.answer();
            f.engine
                .start_work(
                    42,
                    "fake plan",
                    vec![A::ReadCpu, A::SetRawMode(3)],
                    After::Hardware,
                )
                .unwrap();
            let original = f.engine.pending.as_ref().unwrap().work.id;
            let writer_barrier = f.answer();
            let shutdown = send_shutdown(&f, already_received);
            f.engine.poll_foreground(shutdown);
            assert_eq!(f.engine.pending.as_ref().unwrap().work.id, original);
            assert!(
                f.engine
                    .pending
                    .as_ref()
                    .unwrap()
                    .cancel
                    .load(std::sync::atomic::Ordering::Acquire)
            );
            assert!(!f.engine.poll_shutdown().unwrap());
            assert!(matches!(f.log.lock().unwrap().as_slice(), [A::ReadCpu]));
            *first.lock().unwrap() = Some(Ok(json!({})));
            f.engine.poll_foreground(None);
            assert!(f.engine.pending.is_none());
            assert!(!f.engine.poll_shutdown().unwrap());
            assert!(matches!(
                f.engine.pending.as_ref().unwrap().action,
                A::ShutdownWorker
            ));
            assert!(!f.engine.writer_closed);
            f.engine.poll_foreground(None);
            assert!(!f.engine.state.lock().unwrap().shutdown_ready);
            *writer_barrier.lock().unwrap() =
                Some(Ok(json!({"owner_released":true,"shutdown_confirmed":true})));
            f.engine.poll_foreground(None);
            assert!(f.engine.writer_closed);
            let reader_barrier = f.answer();
            assert!(!f.engine.poll_shutdown().unwrap());
            assert!(f.engine.reader_close.is_some() && !f.engine.reader_closed);
            assert!(!f.engine.state.lock().unwrap().shutdown_ready);
            *reader_barrier.lock().unwrap() =
                Some(Ok(json!({"owner_released":true,"shutdown_confirmed":true})));
            assert!(f.engine.poll_shutdown().unwrap());
            assert!(f.engine.state.lock().unwrap().shutdown_ready);
            assert!(matches!(
                f.log.lock().unwrap().as_slice(),
                [A::ReadCpu, A::ShutdownWorker, A::ShutdownWorker]
            ));
        }
    }
    #[test]
    fn late_cpu_baseline_cannot_enable_a_controller() {
        let mut f = Fixture::new();
        let answer = f.answer();
        let settings = ApplicationThermalSettings::from_document(
            &SettingsDocument::parse("{\"EnableCpuThermalControl\":true}").unwrap(),
        )
        .unwrap();
        let change = ConfigChange {
            documents: vec![],
            projection: Default::default(),
            message: String::new(),
        };
        f.engine
            .start_work(
                42,
                "fake",
                vec![A::ReadCpu],
                After::Prepare { change, settings },
            )
            .unwrap();
        f.engine.pending.as_mut().unwrap().work.late = true;
        *answer.lock().unwrap() = Some(Ok(Value::Null));
        f.engine.poll_write().unwrap();
        assert_eq!(f.log.lock().unwrap().len(), 1);
        assert!(f.engine.controller.is_none() && !f.engine.cpu_possible);
    }
    #[test]
    fn failed_activation_keeps_coordinator_and_stop_path_available() {
        let mut f = Fixture::new();
        f.engine.cpu_possible = true;
        f.engine
            .contain_fault(42, anyhow::anyhow!("disk full after enable"));
        assert!(f.engine.cpu_possible && f.engine.faulted);
        assert!(
            f.engine
                .process(Command::Execute {
                    request_id: 43,
                    command: "power.apply".into(),
                    payload: json!({"power.mode":3})
                })
                .is_err()
        );
        let _ = f.answer();
        assert!(
            f.engine
                .process(Command::Execute {
                    request_id: 44,
                    command: "thermal.stop".into(),
                    payload: json!({})
                })
                .is_ok()
        );
        assert!(matches!(f.log.lock().unwrap().last(), Some(A::StopCpu)));
    }
    #[test]
    fn shutdown_ready_waits_for_both_actual_worker_barriers() {
        let mut f = Fixture::new();
        f.engine.shutdown = Some(5);
        let writer = f.answer();
        assert!(!f.engine.poll_shutdown().unwrap());
        assert!(!f.engine.poll_shutdown().unwrap());
        assert!(!f.engine.state.lock().unwrap().shutdown_ready);
        *writer.lock().unwrap() =
            Some(Ok(json!({"owner_released":true,"shutdown_confirmed":true})));
        f.engine.poll_write().unwrap();
        let reader = f.answer();
        assert!(!f.engine.poll_shutdown().unwrap());
        *reader.lock().unwrap() =
            Some(Ok(json!({"owner_released":true,"shutdown_confirmed":true})));
        assert!(f.engine.poll_shutdown().unwrap());
        assert!(f.engine.state.lock().unwrap().shutdown_ready);
    }
    #[test]
    fn refresh_queue_coalesces_and_is_bounded() {
        let mut f = Fixture::new();
        for _ in 0..1000 {
            f.engine.queue_discovery();
        }
        let keys = f
            .engine
            .read_queue
            .iter()
            .map(|(key, _, _)| key.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(keys.len(), f.engine.read_queue.len());
        for key in [
            "power_modes",
            "cpu",
            "godmode",
            "devices",
            "system",
            "wireless",
            "display_controls",
            "autorun",
        ] {
            assert!(keys.contains(key));
        }
        assert!(f.engine.read_queue.iter().all(|(_, action, _)| matches!(
            action,
            A::ReadPowerModes
                | A::ReadHybrid
                | A::ReadAi
                | A::ReadCpu
                | A::ReadCapabilities
                | A::ReadGodMode
                | A::ReadDevices
                | A::ReadGpuDeviceNodes
                | A::ReadEnergy
                | A::ReadChargeThreshold
                | A::ReadSystem
                | A::ReadWireless
                | A::ReadAutorun
                | A::ReadDisplayControls
                | A::ReadMapped(_)
        )));
        for i in 0..1000 {
            f.engine
                .enqueue_read(format!("fake-{i}"), A::ReadCpu, false);
        }
        assert_eq!(f.engine.read_queue.len(), 64);
        assert!(f.log.lock().unwrap().is_empty());
    }
    pub(super) fn prepared_fixture() -> Prepared {
        let settings = ApplicationThermalSettings::from_document(
            &SettingsDocument::parse("{\"EnableCpuThermalControl\":true}").unwrap(),
        )
        .unwrap();
        Prepared::from_discovery(settings,&json!({"raw_mode":255,"limits":{"long_watts":70,"short_watts":125,"temperature_limit":100},"ranges":[{"IDs":0x0102ff00u32,"MinValue":50,"MaxValue":135,"Step":1,"DefaultValue":70},{"IDs":0x0101ff00u32,"MinValue":60,"MaxValue":195,"Step":1,"DefaultValue":125},{"IDs":0x0104ff00u32,"MinValue":85,"MaxValue":100,"Step":1,"DefaultValue":100}]})).unwrap()
    }
    fn intel_temperature(celsius: f64, requested: u64, completed: u64) -> Value {
        json!({"requested_at_tick_ms":requested,"completed_at_tick_ms":completed,
            "cores":[],"package_temperature_celsius":celsius,
            "maximum_core_temperature_celsius":celsius,"conservative_temperature_celsius":celsius,
            "source":"fake DTS","errors":[]})
    }
    fn foreground_temperature_case(old_expired: bool, new_temperature: Option<f64>) {
        let mut f = Fixture::new();
        let prepared = prepared_fixture();
        let now = monotonic_ms();
        let freshness = prepared.settings.options.policy.freshness_ms;
        assert!(now > freshness + 1);
        let old_tick = if old_expired {
            now - freshness - 1
        } else {
            now
        };
        f.engine.intel_sample =
            Some(serde_json::from_value(intel_temperature(60.0, old_tick, old_tick)).unwrap());
        let read = f.answer();
        f.engine
            .enqueue_read("intel".into(), A::ReadIntelSensors, false);
        f.engine.poll_read().unwrap();
        if let Some(temperature) = new_temperature {
            *read.lock().unwrap() = Some(Ok(intel_temperature(temperature, now, now)));
        }
        let mut baseline = prepared.baseline.clone();
        baseline.temperature_limit = 88;
        f.engine.controller = Some(
            prepared
                .activate(baseline, Epoch { policy: 1, mode: 1 }, now)
                .unwrap(),
        );
        f.engine.next_control = 0;
        let _control = f.answer();
        // This is the production admission helper, including the zero-wait
        // reader reap and due thermal evaluation in their actual order.
        f.engine.poll_foreground(None);
        let status = f.engine.state.lock().unwrap().values["thermal.status"]
            .as_str()
            .unwrap()
            .to_owned();
        match new_temperature {
            Some(temperature) => {
                assert!(!status.contains("SensorFault"), "{status}");
                assert_eq!(
                    f.engine.intel_sample.as_ref().unwrap().requested_at_tick_ms,
                    now
                );
                assert_eq!(
                    f.engine
                        .intel_sample
                        .as_ref()
                        .unwrap()
                        .maximum_core_temperature_celsius,
                    Some(temperature)
                );
                if temperature >= 90.0 {
                    assert!(matches!(f.engine.pending.as_ref().map(|p| &p.action),
                        Some(A::SetCpuPower { power, .. }) if power.long_watts < 70));
                } else {
                    assert!(
                        !matches!(f.engine.pending.as_ref().map(|p| &p.action),
                        Some(A::SetCpuPower { power, .. }) if power.long_watts < 70),
                        "fresh cool sample incorrectly inherited the expired-cache minimum budget"
                    );
                }
                assert!(f.engine.reading.is_none());
            }
            None => {
                assert!(status.contains("SensorFault"), "{status}");
                assert_eq!(
                    f.engine.intel_sample.as_ref().unwrap().requested_at_tick_ms,
                    old_tick
                );
                assert!(
                    f.engine.reading.is_some(),
                    "uncompleted reader was treated as a fresh sample"
                );
                assert!(matches!(f.engine.pending.as_ref().map(|p| &p.action),
                    Some(A::SetCpuPower { power, .. }) if power.long_watts == 50));
            }
        }
    }
    #[test]
    fn foreground_reaps_completed_fresh_temperature_before_expired_cache_evaluation() {
        foreground_temperature_case(true, Some(60.0));
    }
    #[test]
    fn foreground_uses_completed_hotter_sample_instead_of_still_fresh_cool_cache() {
        foreground_temperature_case(false, Some(95.0));
    }
    #[test]
    fn foreground_uncompleted_temperature_reader_cannot_refresh_expired_cache() {
        foreground_temperature_case(true, None);
    }
    #[test]
    fn actual_enable_completion_with_commit_failure_keeps_stop_available() {
        let mut f = Fixture::new();
        let answer = f.answer();
        let prepared = prepared_fixture();
        let change = ConfigChange {
            documents: vec![(
                "does-not-exist".into(),
                SettingsDocument::parse("{}").unwrap(),
            )],
            projection: Default::default(),
            message: String::new(),
        };
        f.engine
            .start_work(
                42,
                "fake enable",
                vec![A::EnableCpu { firmware_limit: 88 }],
                After::Enable { change, prepared },
            )
            .unwrap();
        *answer.lock().unwrap() = Some(Ok(
            json!({"raw_mode":255,"power":{"long_watts":70,"short_watts":125},"temperature_limit":88}),
        ));
        f.engine.poll_write().unwrap();
        assert!(f.engine.faulted && f.engine.cpu_possible && f.engine.controller.is_some());
        assert!(f.engine.pending.is_none());
        let _ = f.answer();
        assert!(
            f.engine
                .process(Command::Execute {
                    request_id: 43,
                    command: "thermal.stop".into(),
                    payload: json!({})
                })
                .is_ok()
        );
        assert!(matches!(f.log.lock().unwrap().last(), Some(A::StopCpu)));
    }
    #[test]
    fn controller_timeout_is_checked_before_the_general_display_deadline() {
        let mut f = Fixture::new();
        let prepared = prepared_fixture();
        let mut verified = prepared.baseline.clone();
        verified.temperature_limit = 88;
        let mut controller = prepared
            .activate(verified, Epoch { policy: 1, mode: 1 }, 0)
            .unwrap();
        let (_, action) = controller.sample(None, 0).unwrap();
        f.engine.controller = Some(controller);
        let _ = f.answer();
        f.engine
            .start_work(0, "fake power", vec![action.unwrap()], After::Control)
            .unwrap();
        f.engine.poll_write().unwrap();
        assert!(f.engine.pending.as_ref().unwrap().work.late);
        assert!(
            f.engine
                .controller
                .as_mut()
                .unwrap()
                .sample(None, monotonic_ms())
                .is_err()
        );
    }
    #[test]
    fn invalid_raw_typed_documents_and_source_enable_never_reach_disk() {
        let mut f = Fixture::new();
        for (key, file, invalid) in [
            (
                "automation",
                "automation.json",
                json!({"IsEnabled":false,"Pipelines":123}),
            ),
            (
                "macros",
                "macro.json",
                json!({"IsEnabled":false,"Sequences":{"Keyboard,96":{"RepeatCount":-1,"Events":[]}}}),
            ),
            ("osd", "osd.json", json!({"ShowOsd":"yes","FontSize":-5})),
        ] {
            let old = f.engine.repository.document(key).unwrap().clone();
            f.engine.repository.save(key, old).unwrap();
            let original = std::fs::read(f.root.join(file)).unwrap();
            assert!(
                f.engine
                    .process(Command::SaveDocument {
                        request_id: 70,
                        document: key.into(),
                        value: invalid
                    })
                    .is_err()
            );
            assert_eq!(std::fs::read(f.root.join(file)).unwrap(), original);
        }
        for (key, file) in [("automation", "automation.json"), ("macros", "macro.json")] {
            let mut enabled = f.engine.repository.value(key).unwrap();
            enabled["IsEnabled"] = json!(true);
            let old = std::fs::read(f.root.join(file)).unwrap();
            assert!(
                f.engine
                    .process(Command::SaveDocument {
                        request_id: 71,
                        document: key.into(),
                        value: enabled
                    })
                    .is_err()
            );
            assert_eq!(std::fs::read(f.root.join(file)).unwrap(), old);
        }
        assert!(f.log.lock().unwrap().is_empty());
    }
    #[test]
    fn verified_mode_ack_precedes_next_preset_even_when_metadata_reader_is_delayed() {
        let mut f = Fixture::new();
        let field = jiks_platform::god_mode::GodModeField::CpuLongPower;
        let spec = jiks_platform::god_mode::CapabilitySpec {
            field,
            min: 50,
            max: 135,
            step: Some(1),
            discrete: vec![],
            default_value: Some(70),
        };
        f.engine.cache.insert("cpu".into(),json!({"raw_mode":3,"limits":{"long_watts":70,"short_watts":125,"temperature_limit":100}}));
        f.engine.cache.insert("godmode".into(),json!({"raw_mode":3,"fields":[{"key":"cpu.pl1","capability":{"available":true,"value":spec},"current":{"available":true,"value":70}}]}));
        let answer = f.answer();
        let id = f.engine.new_internal_request().unwrap();
        f.engine
            .start_work(id, "power.apply", vec![A::SetRawMode(255)], After::Hardware)
            .unwrap();
        assert!(f.engine.take_internal_result(id).is_none());
        *answer.lock().unwrap() = Some(Ok(json!(255)));
        f.engine.poll_write().unwrap();
        assert!(f.engine.take_internal_result(id).unwrap().is_ok());
        // All queued metadata events/readers are intentionally still pending.
        assert!(!f.engine.read_queue.is_empty());
        assert!(f.engine.cache["cpu"].get("limits").is_none());
        let mut payload = json!({"preset":{"values":{"cpu.pl1":70}}});
        f.engine.context(&mut payload).unwrap();
        assert_eq!(payload["_context"]["raw_mode"], 255);
        assert!(matches!(
            hardware_routes::plan("preset.apply", &payload)
                .unwrap()
                .first(),
            Some(A::ApplyGodMode(_))
        ));
    }
    #[test]
    fn logical_timeout_does_not_acknowledge_an_internal_physical_step() {
        let mut f = Fixture::new();
        let answer = f.answer();
        let id = f.engine.new_internal_request().unwrap();
        f.engine
            .start_work(id, "fake", vec![A::ReadCpu], After::Hardware)
            .unwrap();
        f.engine.pending.as_mut().unwrap().started = monotonic_ms().saturating_sub(1501);
        f.engine.poll_write().unwrap();
        assert!(f.engine.pending.is_some());
        assert!(f.engine.take_internal_result(id).is_none());
        *answer.lock().unwrap() = Some(Ok(json!({})));
        f.engine.poll_write().unwrap();
        assert!(f.engine.take_internal_result(id).unwrap().is_err());
    }
    #[test]
    fn reader_barrier_is_not_satisfied_by_an_older_inflight_read() {
        let mut f = Fixture::new();
        let old = f.answer();
        f.engine.enqueue_read("cpu".into(), A::ReadCpu, false);
        f.engine.poll_read().unwrap();
        let id = f.engine.new_internal_request().unwrap();
        f.engine.request_read(id, "cpu".into(), A::ReadCpu).unwrap();
        *old.lock().unwrap() = Some(Ok(json!({"raw_mode":3})));
        let fresh = f.answer();
        f.engine.poll_read().unwrap();
        assert!(f.engine.take_internal_result(id).is_none());
        *fresh.lock().unwrap() = Some(Ok(json!({"raw_mode":255})));
        f.engine.poll_read().unwrap();
        assert!(f.engine.take_internal_result(id).unwrap().is_ok());
        assert!(f.engine.read_receipts.remove(&id).is_some());
        assert_eq!(f.engine.cache["cpu"]["raw_mode"], 255);
    }
    #[test]
    fn battery_threshold_uses_native_readback_and_invalidates_a_mismatched_receipt() {
        let mut f = Fixture::new();
        let target = json!({
            "battery.threshold_enabled": true,
            "battery.start_threshold": 50,
            "battery.stop_threshold": 80
        });
        let read = f.answer();
        f.engine
            .process(Command::Execute {
                request_id: 41,
                command: "battery.threshold.read".into(),
                payload: json!({}),
            })
            .unwrap();
        f.engine.poll_read().unwrap();
        assert!(matches!(
            f.log.lock().unwrap().last(),
            Some(A::ReadChargeThreshold)
        ));
        assert!(
            f.engine
                .process(Command::Execute {
                    request_id: 44,
                    command: "battery.threshold.apply".into(),
                    payload: target.clone(),
                })
                .is_err()
        );
        let original = json!({"state":{"enabled":false,"start":40,"stop":60},"reason":null});
        *read.lock().unwrap() = Some(Ok(original.clone()));
        f.engine.poll_read().unwrap();
        let initial = f.engine.state.lock().unwrap();
        assert_eq!(initial.values["battery.start_threshold"], json!(40));
        assert!(initial.capabilities["battery.threshold.apply"].available);
        drop(initial);

        let applied = f.answer();
        f.engine
            .process(Command::Execute {
                request_id: 42,
                command: "battery.threshold.apply".into(),
                payload: target,
            })
            .unwrap();
        assert!(matches!(
            f.log.lock().unwrap().last(),
            Some(A::ChargeThreshold(target)) if target.start == 50
        ));
        let reread = f.answer();
        f.engine
            .process(Command::Execute {
                request_id: 43,
                command: "battery.threshold.read".into(),
                payload: json!({}),
            })
            .unwrap();
        f.engine.poll_read().unwrap();
        assert!(f.engine.reading.is_none());
        *applied.lock().unwrap() = Some(Ok(original));
        f.engine.poll_write().unwrap();
        let rejected = f.engine.state.lock().unwrap();
        assert!(!rejected.results.back().unwrap().ok);
        assert!(rejected.values["battery.start_threshold"].is_null());
        assert!(!rejected.capabilities["battery.threshold.apply"].available);
        drop(rejected);

        f.engine.poll_read().unwrap();
        *reread.lock().unwrap() = Some(Ok(json!({"state":null,"reason":"本机没有 IBMPmDrv"})));
        f.engine.poll_read().unwrap();
        let unsupported = f.engine.state.lock().unwrap();
        assert!(unsupported.values["battery.start_threshold"].is_null());
        assert_eq!(
            unsupported.capabilities["battery.threshold.apply"].reason,
            "本机没有 IBMPmDrv"
        );
        assert!(unsupported.capabilities["battery.threshold.read"].available);
    }
    #[test]
    fn malformed_shutdown_acknowledgement_cannot_release_exit_gate() {
        let mut f = Fixture::new();
        f.engine.shutdown = Some(8);
        let answer = f.answer();
        assert!(!f.engine.poll_shutdown().unwrap());
        *answer.lock().unwrap() = Some(Ok(json!({"done":true})));
        f.engine.poll_write().unwrap();
        assert!(f.engine.faulted);
        assert!(!f.engine.writer_closed);
        assert!(!f.engine.state.lock().unwrap().shutdown_ready);
    }
    fn editor_baseline(enabled: bool) -> Value {
        json!({"thermal.enabled":enabled,"thermal.shadow":false,"thermal.sampling_ms":250,"thermal.firmware_limit_c":88})
    }
    fn editor_discovery() -> Value {
        json!({"raw_mode":255,"limits":{"long_watts":70,"short_watts":125,"temperature_limit":100},"ranges":[{"IDs":0x0102ff00u32,"MinValue":50,"MaxValue":135,"Step":1,"DefaultValue":70},{"IDs":0x0101ff00u32,"MinValue":60,"MaxValue":195,"Step":1,"DefaultValue":125},{"IDs":0x0104ff00u32,"MinValue":85,"MaxValue":100,"Step":1,"DefaultValue":100}]})
    }
    #[test]
    fn real_sensor_editor_contract_replays_enable_display_policy_and_disable_actual_ack() {
        let mut fixture = Fixture::new();
        let engine = &mut fixture.engine;
        engine
            .repository
            .save(
                "application",
                SettingsDocument::parse(r#"{"EnableCpuThermalControl":false}"#).unwrap(),
            )
            .unwrap();
        let baseline = editor_baseline(false);
        let mut payload = baseline.clone();
        payload["thermal.enabled"] = json!(true);
        let (command, payload) =
            config_actions::sensor_editor_command(&baseline, &baseline, payload, true).unwrap();
        assert_eq!(command, "thermal.apply");
        let baseline_ack = fixture.answer();
        fixture
            .engine
            .process(Command::Execute {
                request_id: 901,
                command: command.into(),
                payload,
            })
            .unwrap();
        assert_eq!(
            fixture.engine.repository.value("application").unwrap()["EnableCpuThermalControl"],
            false
        );
        let enabled_ack = fixture.answer();
        *baseline_ack.lock().unwrap() = Some(Ok(editor_discovery()));
        fixture.engine.poll_write().unwrap();
        assert!(matches!(
            fixture.log.lock().unwrap().last(),
            Some(A::EnableCpu { .. })
        ));
        assert_eq!(
            fixture.engine.repository.value("application").unwrap()["EnableCpuThermalControl"],
            false
        );
        *enabled_ack.lock().unwrap() = Some(Ok(
            json!({"raw_mode":255,"power":{"long_watts":70,"short_watts":125},"temperature_limit":88}),
        ));
        fixture.engine.poll_write().unwrap();
        assert!(fixture.engine.controller.is_some());
        assert_eq!(
            fixture.engine.repository.value("application").unwrap()["EnableCpuThermalControl"],
            true
        );
        let baseline = editor_baseline(true);
        let mut payload = baseline.clone();
        payload["sensors.temperature_unit"] = json!("F");
        let (command, payload) =
            config_actions::sensor_editor_command(&baseline, &baseline, payload, true).unwrap();
        let count = fixture.log.lock().unwrap().len();
        fixture
            .engine
            .process(Command::Execute {
                request_id: 902,
                command: command.into(),
                payload,
            })
            .unwrap();
        assert_eq!(fixture.log.lock().unwrap().len(), count);
        assert!(fixture.engine.controller.is_some());
        let before = std::fs::read(fixture.root.join("settings.json")).unwrap();
        let mut policy = baseline.clone();
        policy["thermal.sampling_ms"] = json!(500);
        let (command, payload) =
            config_actions::sensor_editor_command(&baseline, &baseline, policy, true).unwrap();
        assert_eq!(command, "thermal.apply");
        assert!(
            fixture
                .engine
                .process(Command::Execute {
                    request_id: 903,
                    command: command.into(),
                    payload
                })
                .is_err()
        );
        assert_eq!(
            std::fs::read(fixture.root.join("settings.json")).unwrap(),
            before
        );
        let mut disabled = baseline.clone();
        disabled["thermal.enabled"] = json!(false);
        let (command, payload) =
            config_actions::sensor_editor_command(&baseline, &baseline, disabled, true).unwrap();
        let stopped = fixture.answer();
        fixture
            .engine
            .process(Command::Execute {
                request_id: 904,
                command: command.into(),
                payload,
            })
            .unwrap();
        assert_eq!(
            fixture.engine.repository.value("application").unwrap()["EnableCpuThermalControl"],
            true
        );
        assert!(fixture.engine.controller.is_some());
        *stopped.lock().unwrap() = Some(Ok(
            json!({"raw_mode":255,"power":{"long_watts":70,"short_watts":125},"temperature_limit":100}),
        ));
        fixture.engine.poll_write().unwrap();
        assert!(fixture.engine.controller.is_none());
        assert_eq!(
            fixture.engine.repository.value("application").unwrap()["EnableCpuThermalControl"],
            false
        );
    }
    #[test]
    fn real_sensor_editor_rejected_enable_ack_never_persists() {
        let mut fixture = Fixture::new();
        fixture
            .engine
            .repository
            .save(
                "application",
                SettingsDocument::parse(r#"{"EnableCpuThermalControl":false}"#).unwrap(),
            )
            .unwrap();
        let before = std::fs::read(fixture.root.join("settings.json")).unwrap();
        let baseline = editor_baseline(false);
        let mut payload = baseline.clone();
        payload["thermal.enabled"] = json!(true);
        let (command, payload) =
            config_actions::sensor_editor_command(&baseline, &baseline, payload, true).unwrap();
        let read = fixture.answer();
        fixture
            .engine
            .process(Command::Execute {
                request_id: 911,
                command: command.into(),
                payload,
            })
            .unwrap();
        let apply = fixture.answer();
        *read.lock().unwrap() = Some(Ok(editor_discovery()));
        fixture.engine.poll_write().unwrap();
        *apply.lock().unwrap() = Some(Err("fake physical setter refused".into()));
        fixture.engine.poll_write().unwrap();
        assert_eq!(
            std::fs::read(fixture.root.join("settings.json")).unwrap(),
            before
        );
        assert!(fixture.engine.cpu_possible);
        assert!(
            fixture
                .engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|result| result.request_id == 911 && !result.ok)
        );
    }
}
