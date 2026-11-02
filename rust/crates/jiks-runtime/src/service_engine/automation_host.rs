//! Single coordinator consumer for owned source events and actual step receipts.
//! It reuses Engine's two long-lived physical workers; no step opens WMI.
use super::*;
use crate::{
    automation_events::{
        EventReducer, GameRules, HardwareObservation, NativeEventSource, RefreshReason,
        SourceMessage, SourceNeeds, SourceOptions,
    },
    automation_executor::{Executor, StateRequirement, StepCompletion, StepEffect},
    automation_process::{NativeProcessTransport, ProcessRequest, ProcessRunner},
    automation_runtime::{self, PlannedAction, StateSource},
};
use jiks_core::automation::{AdapterState, AutomationDocument, AutomationEvent, AutomationState};
use jiks_platform::domain::MappedFeature;
use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicBool, Ordering},
    thread::{self, JoinHandle},
};

trait SourcePort {
    fn start(&mut self, options: SourceOptions) -> Result<()>;
    fn next(&self) -> Option<SourceMessage>;
    fn stop(&self);
    fn finish(&mut self) -> Option<Result<()>>;
    fn active(&self) -> bool;
    fn cleanup_confirmed(&self) -> bool;
    fn suspend_macro(&self, active: bool);
    fn waker(&mut self, wake: Option<Arc<dyn Fn() + Send + Sync>>) -> Result<()>;
}
impl SourcePort for NativeEventSource {
    fn start(&mut self, o: SourceOptions) -> Result<()> {
        self.start(o)
    }
    fn next(&self) -> Option<SourceMessage> {
        self.try_recv()
    }
    fn stop(&self) {
        self.stop()
    }
    fn finish(&mut self) -> Option<Result<()>> {
        self.try_finish()
    }
    fn active(&self) -> bool {
        self.is_running()
    }
    fn cleanup_confirmed(&self) -> bool {
        self.cleanup_confirmed()
    }
    fn suspend_macro(&self, v: bool) {
        self.set_macro_suspended(v)
    }
    fn waker(&mut self, v: Option<Arc<dyn Fn() + Send + Sync>>) -> Result<()> {
        self.set_waker(v)
    }
}
struct Candidate {
    id: u64,
    change: Option<ConfigChange>,
    options: SourceOptions,
    old: SourceOptions,
    failure: Option<String>,
    restoring: bool,
    restore_failed: bool,
    wait_execution: bool,
    wait_macros: bool,
}
struct Barrier {
    required: StateRequirement,
    requests: Vec<u64>,
    failure: Option<String>,
    requested: u64,
    completed: u64,
}
enum PendingKind {
    PresetRead {
        request: Option<u64>,
        target: PresetTarget,
        raw_mode: Option<u64>,
    },
    SpectrumRead {
        request: Option<u64>,
        effects: Value,
        profile: Option<u64>,
    },
    Command {
        request: u64,
        output: Option<String>,
        preset: Option<String>,
    },
    Process,
    Sound,
    Ui {
        request: u64,
        kind: &'static str,
        result: Option<std::result::Result<(), String>>,
        deadline_ms: u64,
    },
}
struct PresetTarget {
    id: String,
    value: Value,
    output: Option<String>,
    saved: bool,
}
struct PendingEffect {
    effect: StepEffect,
    kind: PendingKind,
    noted_late: bool,
}

struct Sound {
    join: JoinHandle<Result<()>>,
    stopped: Arc<AtomicBool>,
}
impl Sound {
    fn start(path: String) -> Result<Self> {
        ensure!(
            !path.trim().is_empty() && !path.contains('\0'),
            "Invalid sound file path"
        );
        let stopped = Arc::new(AtomicBool::new(false));
        let requested_stop = stopped.clone();
        let join = thread::Builder::new()
            .name("jiks-explicit-sound".into())
            .spawn(move || {
                if requested_stop.load(Ordering::Acquire) {
                    bail!("Sound cancelled before playback");
                }
                use std::os::windows::ffi::OsStrExt;
                use windows_sys::Win32::Media::Audio::{
                    PlaySoundW, SND_FILENAME, SND_NODEFAULT, SND_SYNC,
                };
                let path: Vec<_> = std::ffi::OsStr::new(&path)
                    .encode_wide()
                    .chain(Some(0))
                    .collect();
                ensure!(
                    unsafe {
                        PlaySoundW(
                            path.as_ptr(),
                            std::ptr::null_mut(),
                            SND_FILENAME | SND_NODEFAULT | SND_SYNC,
                        )
                    } != 0,
                    "Sound playback failed"
                );
                ensure!(!requested_stop.load(Ordering::Acquire), "Sound cancelled");
                Ok(())
            })?;
        Ok(Self { join, stopped })
    }
    fn cancel(&self) {
        self.stopped.store(true, Ordering::Release); /* Waiting is cancelled, the actual synchronous worker remains owned. */
    }
}

pub(super) struct AutomationHost {
    scheduler: Option<automation_runtime::Engine>,
    executor: Executor,
    source: Box<dyn SourcePort>,
    options: SourceOptions,
    source_ready: bool,
    source_error: Option<String>,
    source_started_at: u64,
    source_attempted: bool,
    candidate: Option<Candidate>,
    revision: Option<u64>,
    automation_text: Option<String>,
    state: AutomationState,
    games: GameRules,
    reducer: EventReducer,
    process_seen: bool,
    clock_seen: bool,
    wifi_seen: bool,
    startup_started: bool,
    startup_error: Option<String>,
    thermal_attempted: bool,
    thermal_request: Option<u64>,
    manual: VecDeque<(u64, String)>,
    manual_ids: BTreeSet<u64>,
    current_manual: Option<u64>,
    pending: Option<PendingEffect>,
    barrier: Option<Barrier>,
    process: ProcessRunner,
    sound: Option<Sound>,
    stopping: bool,
    next_source_read: u64,
}
impl Default for AutomationHost {
    fn default() -> Self {
        Self {
            scheduler: None,
            executor: Executor::new(),
            source: Box::new(NativeEventSource::new()),
            options: SourceOptions::default(),
            source_ready: false,
            source_error: None,
            source_started_at: 0,
            source_attempted: false,
            candidate: None,
            revision: None,
            automation_text: None,
            state: AutomationState::default(),
            games: GameRules::default(),
            reducer: EventReducer::default(),
            process_seen: false,
            clock_seen: false,
            wifi_seen: false,
            startup_started: false,
            startup_error: None,
            thermal_attempted: false,
            thermal_request: None,
            manual: VecDeque::new(),
            manual_ids: BTreeSet::new(),
            current_manual: None,
            pending: None,
            barrier: None,
            process: ProcessRunner::new(NativeProcessTransport::new()),
            sound: None,
            stopping: false,
            next_source_read: 0,
        }
    }
}
fn bool_field(doc: &SettingsDocument, key: &str) -> bool {
    doc.root()
        .get_ci(key)
        .and_then(JsonNode::as_bool)
        .unwrap_or(false)
}
fn parse_automation(document: &SettingsDocument) -> Result<AutomationDocument> {
    let mut error = None;
    let parsed = AutomationDocument::parse(&document.to_json(), || match new_guid() {
        Ok(id) => id,
        Err(e) => {
            error = Some(e);
            "00000000-0000-0000-0000-000000000000".into()
        }
    })?;
    if let Some(error) = error {
        return Err(error);
    }
    Ok(parsed)
}
fn options_from(documents: &Documents) -> Result<(SourceOptions, GameRules)> {
    let automation = parse_automation(
        documents
            .get("automation")
            .context("Automation document absent")?,
    )?;
    let macros = macros::MacroDocument::from_settings(
        documents
            .get("macros")
            .context("Macro document absent")?
            .clone(),
    )?;
    let games = GameRules::from_application(
        documents
            .get("application")
            .context("Application document absent")?
            .root(),
    )?;
    let keys: BTreeSet<u32> = macros
        .sequences
        .iter()
        .filter(|e| {
            e.identifier.source == "Keyboard"
                && !e.sequence.events.is_empty()
                && (0x60..=0x69).contains(&e.identifier.key)
        })
        .map(|e| e.identifier.key as u32)
        .collect();
    let mut needs = SourceNeeds::from_document(&automation);
    let notification = documents.get("notifications").map(SettingsDocument::root);
    let normal_notifications = notification
        .and_then(|root| root.get_ci("DontShowNotifications"))
        .and_then(JsonNode::as_bool)
        != Some(true);
    let category = |name: &str| {
        notification
            .and_then(|root| root.get_ci("Notifications"))
            .and_then(|root| root.get_ci(name))
            .and_then(JsonNode::as_bool)
            .unwrap_or(true)
    };
    let lock_notifications = normal_notifications && (category("CapsLock") || category("NumLock"));
    if normal_notifications {
        needs.power = true;
        needs.display = true;
        needs.devices = true;
    }
    if let Some(app) = documents.get("application") {
        let mapping = app.root().get_ci("PowerModeMappingMode");
        if mapping.is_some_and(|value| {
            value.as_i64().is_some_and(|v| v == 1 || v == 2)
                || value.as_str().is_some_and(|v| {
                    matches!(v, "1" | "2" | "WindowsPowerMode" | "WindowsPowerPlan")
                })
        }) {
            needs.power = true;
            needs.firmware = true;
        }
        if bool_field(app, "SynchronizeBrightnessToAllPowerPlans") {
            needs.display = true;
        }
    }
    if documents
        .get("balance")
        .is_some_and(|doc| bool_field(doc, "AIModeEnabled"))
    {
        needs.processes = true;
        needs.games = true;
        needs.power = true;
        needs.firmware = true;
    }
    if macros.enabled && !keys.is_empty() {
        needs.session = true;
    }
    Ok((
        SourceOptions {
            needs,
            macro_enabled: macros.enabled,
            macro_keys: keys,
            game_detection: games.required.clone(),
            normal_notifications,
            lock_notifications,
        },
        games,
    ))
}
fn actual_values(engine: &Engine) -> BTreeMap<String, Value> {
    let mut values = hardware_projection::project(&engine.cache, engine.read_only, None).values;
    if let Ok(doc) = engine.repository.document("macros") {
        values.insert("macro.enabled".into(), json!(bool_field(doc, "IsEnabled")));
    }
    if let Ok(doc) = engine.repository.document("application") {
        values.insert(
            "sensors.enabled".into(),
            json!(bool_field(doc, "EnableHardwareSensors")),
        );
    }
    if let Ok(doc) = engine.repository.document("osd") {
        values.insert("osd.enabled".into(), json!(bool_field(doc, "ShowOsd")));
        values.insert("osd.locked".into(), json!(bool_field(doc, "IsLocked")));
    }
    if let Ok(doc) = engine.repository.document("balance") {
        values.insert("power.ai".into(), json!(bool_field(doc, "AIModeEnabled")));
    }
    if let Ok(doc) = engine.repository.document("gpu_oc") {
        values.insert("gpu.oc.enabled".into(), json!(bool_field(doc, "Enabled")));
    }
    values
}
fn observation(engine: &Engine) -> HardwareObservation {
    let values = actual_values(engine);
    let mode = values
        .get("power.mode")
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .and_then(|raw| automation_runtime::firmware_power_to_legacy(raw).ok());
    let adapter = engine
        .cache
        .get("system")
        .and_then(|v| v.pointer("/power_status/status"))
        .and_then(Value::as_str)
        .and_then(|value| match value {
            "Connected" => Some(AdapterState::Connected),
            "ConnectedLowWattage" => Some(AdapterState::ConnectedLowWattage),
            "Disconnected" => Some(AdapterState::Disconnected),
            _ => None,
        });
    let external = engine
        .cache
        .get("display_controls")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter(|v| v.get("internal").and_then(Value::as_bool) == Some(false))
                .count()
        });
    let preset = engine
        .repository
        .document("godmode")
        .ok()
        .and_then(|d| d.root().get_ci("ActivePresetId"))
        .and_then(JsonNode::as_str)
        .map(str::to_owned);
    HardwareObservation {
        adapter,
        power_mode: mode,
        hybrid_mode: values
            .get("gpu.hybrid_mode")
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok()),
        hdr_on: values.get("display.hdr").and_then(Value::as_bool),
        external_display_count: external,
        preset_id: preset,
        its_mode: None,
    }
}

impl AutomationHost {
    pub(super) fn set_waker(&mut self, wake: Option<Arc<dyn Fn() + Send + Sync>>) -> Result<()> {
        self.source.waker(wake)
    }
    pub(super) fn is_source_active(&self) -> bool {
        self.source.active() || self.source_error.is_some() || self.candidate.is_some()
    }
    pub(super) fn is_busy(&self) -> bool {
        self.executor.is_busy()
            || self.pending.is_some()
            || self.barrier.is_some()
            || self.process.is_busy()
            || self.sound.is_some()
            || self.candidate.is_some()
            || self.thermal_request.is_some()
            || !self.manual.is_empty()
            || self.scheduler.as_ref().is_some_and(|s| s.is_busy())
    }
    pub(super) fn manual(&mut self, id: u64, pipeline: &str) -> Result<()> {
        ensure!(!self.stopping, "Automation is stopping");
        ensure!(self.manual.len() < 128, "Automation manual queue full");
        let pipeline = jiks_core::automation::normalize_guid(pipeline)?;
        ensure!(self.manual_ids.insert(id), "Duplicate manual request ID");
        self.manual.push_back((id, pipeline));
        Ok(())
    }
    pub(super) fn cancel(&mut self) {
        self.stopping = true;
        self.executor.cancel("Coordinator stopping");
        self.source.stop();
        self.process.cancel();
        if let Some(sound) = &self.sound {
            sound.cancel();
        }
    }
    pub(super) fn ui_complete(&mut self, effect_id: u64, ok: bool, message: String) -> Result<()> {
        let pending = self.pending.as_mut().context("No UI effect pending")?;
        let PendingKind::Ui {
            request, result, ..
        } = &mut pending.kind
        else {
            bail!("No UI effect pending");
        };
        ensure!(
            *request == effect_id && result.is_none(),
            "UI effect acknowledgement mismatch"
        );
        *result = Some(if ok { Ok(()) } else { Err(message) });
        Ok(())
    }
    pub(super) fn request_configuration(
        &mut self,
        engine: &mut Engine,
        id: u64,
        change: ConfigChange,
    ) -> Result<Option<ConfigChange>> {
        ensure!(
            !engine.read_only && !self.stopping,
            "Global listeners cannot be enabled while read-only/stopping"
        );
        ensure!(
            self.candidate.is_none(),
            "Listener configuration transition pending"
        );
        let mut documents = engine.documents();
        for (key, doc) in &change.documents {
            documents.insert(key.clone(), doc.clone());
        }
        let (options, _) = options_from(&documents)?;
        if options == self.options && self.source_error.is_none() {
            return Ok(Some(change));
        }
        let changes_automation = change.documents.iter().any(|(key, _)| key == "automation");
        if changes_automation && self.executor.is_busy() {
            let enabled = documents
                .get("automation")
                .is_some_and(|d| bool_field(d, "IsEnabled"));
            ensure!(
                !enabled,
                "Wait for actual automation completion before editing its running document"
            );
            self.executor.cancel("Automation disabled");
        }
        let wait_macros = change
            .documents
            .iter()
            .any(|(key, doc)| key == "macros" && !bool_field(doc, "IsEnabled"));
        if wait_macros {
            engine.player.stop();
            engine.recorder.stop();
        }
        self.candidate = Some(Candidate {
            id,
            change: Some(change),
            options,
            old: self.options.clone(),
            failure: None,
            restoring: false,
            restore_failed: false,
            wait_execution: changes_automation,
            wait_macros,
        });
        self.source_ready = false;
        self.source.stop();
        self.source_attempted = false;
        Ok(None)
    }
    fn fault_startup(&mut self, engine: &mut Engine, error: impl Into<String>) {
        let error = error.into();
        self.startup_error = Some(error.clone());
        engine.value("automation.startup_error", json!(error));
        engine.value("automation.startup_ready", json!(false));
    }
    fn refresh(&mut self, engine: &mut Engine, reason: RefreshReason) {
        if engine.shutdown.is_some() || self.stopping {
            return;
        }
        match reason {
            RefreshReason::Power { .. } => {
                engine.enqueue_read("system".into(), A::ReadSystem, false);
                engine.enqueue_read("power_modes".into(), A::ReadPowerModes, false);
            }
            RefreshReason::Display => {
                engine.enqueue_read("display_controls".into(), A::ReadDisplayControls, false);
                engine.enqueue_read("system".into(), A::ReadSystem, false);
            }
            RefreshReason::Devices => {
                engine.enqueue_read("devices".into(), A::ReadDevices, false);
            }
            RefreshReason::Firmware => {
                engine.enqueue_read("power_modes".into(), A::ReadPowerModes, false);
                engine.enqueue_read("hybrid".into(), A::ReadHybrid, false);
                engine.enqueue_read("ai".into(), A::ReadAi, false);
            }
            RefreshReason::NotificationState => {
                self.refresh(engine, RefreshReason::Firmware);
                for (key, action) in [
                    ("system", A::ReadSystem),
                    ("energy", A::ReadEnergy),
                    ("devices", A::ReadDevices),
                    ("lighting.0", A::ReadLighting { lighting_id: 0 }),
                    ("lighting.3", A::ReadLighting { lighting_id: 3 }),
                    ("lighting.5", A::ReadLighting { lighting_id: 5 }),
                ] {
                    engine.enqueue_read(key.into(), action, false);
                }
            }
        }
    }
    fn events(&mut self, events: Vec<AutomationEvent>, origin: Option<u64>) -> Result<()> {
        if let Some(scheduler) = &mut self.scheduler {
            for event in events {
                scheduler.on_event(self.state.clone(), event, origin)?;
            }
        }
        Ok(())
    }
    fn poll_source(&mut self, engine: &mut Engine) -> Result<()> {
        self.source.suspend_macro(
            engine.record_preparation.is_some()
                || engine.player.is_active()
                || engine.recorder.is_active(),
        );
        for _ in 0..256 {
            let Some(message) = self.source.next() else {
                break;
            };
            self.games.observe(&message);
            match &message {
                SourceMessage::Started => {
                    engine.notification_source_started();
                    if !self.stopping {
                        self.source_ready = true;
                    }
                }
                SourceMessage::Stopped => self.source_ready = false,
                SourceMessage::Fault { source, detail } => {
                    engine.value(
                        "automation.source_warning",
                        json!(format!("{source}: {detail}")),
                    );
                    if *source == "native-events" {
                        self.source_ready = false;
                        self.source.stop();
                        if let Some(candidate) = &mut self.candidate {
                            candidate.failure = Some(detail.clone());
                            candidate.restore_failed |= candidate.restoring;
                        } else {
                            self.fault_startup(engine, detail.clone());
                        }
                    }
                }
                SourceMessage::Refresh(reason) => self.refresh(engine, *reason),
                SourceMessage::Processes(_) => self.process_seen = true,
                SourceMessage::Clock { .. } => self.clock_seen = true,
                SourceMessage::Wifi { .. } => self.wifi_seen = true,
                SourceMessage::MacroHotkey { source, key } => {
                    if !engine.read_only
                        && !self.stopping
                        && self.source_ready
                        && !engine.player.is_active()
                        && !engine.recorder.is_active()
                    {
                        let document = macros::MacroDocument::from_settings(
                            engine.repository.document("macros")?.clone(),
                        )?;
                        if document.enabled {
                            if let Some(sequence) = document.get(&MacroIdentifier {
                                source: source.clone(),
                                key: *key,
                            }) {
                                engine.player.start(sequence.clone())?;
                                engine.value("macro.running", json!(true));
                            }
                        }
                    }
                }
                SourceMessage::LockState { caps, num } => {
                    engine.notification_lock_state(*caps, *num);
                }
                SourceMessage::SessionLocked(true) => {
                    engine.player.stop();
                    engine.recorder.stop();
                }
                _ => {}
            }
            let events = self
                .reducer
                .reduce(&message, &mut self.state, Some(&self.games));
            self.events(events, None)?;
        }
        if let Some(result) = self.source.finish() {
            self.source_ready = false;
            if let Err(error) = result {
                if !self.source.cleanup_confirmed() {
                    self.source_error = Some(format!("{error:#}"));
                }
                if let Some(candidate) = &mut self.candidate {
                    candidate.failure = Some(format!("{error:#}"));
                    candidate.restore_failed |= candidate.restoring;
                } else {
                    self.fault_startup(engine, format!("Event source cleanup failed: {error:#}"));
                }
            }
        }
        if self.source_error.is_some() && !self.source.active() && self.source.cleanup_confirmed() {
            self.source_error = None;
        }
        if self.stopping {
            if !self.source.active() {
                if let Some(candidate) = self.candidate.take() {
                    engine.finish(
                        candidate.id,
                        false,
                        "Listener transition cancelled after actual cleanup",
                    );
                }
            }
            return Ok(());
        }
        if self.source_error.is_some() {
            if !self.source.active() {
                if let Some(candidate) = self.candidate.take() {
                    engine.finish(
                        candidate.id,
                        false,
                        format!(
                            "Actual listener cleanup failed: {}",
                            self.source_error.as_deref().unwrap()
                        ),
                    );
                }
            }
            return Ok(());
        }
        if self.candidate.as_ref().is_some_and(|c| c.restore_failed)
            && !self.source.active()
            && self.source.cleanup_confirmed()
        {
            let candidate = self.candidate.take().unwrap();
            self.options = candidate.old;
            self.source_attempted = true;
            let message = format!(
                "Listener change and restoration both failed; saved configuration retained, listener inactive: {}",
                candidate.failure.unwrap_or_default()
            );
            self.fault_startup(engine, message.clone());
            engine.finish(candidate.id, false, message);
            return Ok(());
        }
        if let Some(candidate) = &mut self.candidate {
            if !self.source.active()
                && (!candidate.wait_execution || !self.executor.is_busy())
                && (!candidate.wait_macros
                    || !engine.player.is_active()
                        && !engine.recorder.is_active()
                        && !macros::native::has_unreleased_input())
            {
                if candidate.failure.is_some() && !candidate.restoring {
                    candidate.restoring = true;
                    candidate.options = candidate.old.clone();
                    self.source_attempted = false;
                }
                if !self.source_attempted {
                    self.source_attempted = true;
                    self.source_started_at = monotonic_ms();
                    if candidate.options.is_empty() {
                        self.source_ready = true;
                    } else if let Err(error) = self.source.start(candidate.options.clone()) {
                        candidate.failure = Some(format!("{error:#}"));
                        candidate.restore_failed |= candidate.restoring;
                        self.source_attempted = false;
                    }
                }
            }
            if self.source_ready {
                let mut candidate = self.candidate.take().unwrap();
                if let Some(error) = candidate.failure {
                    self.options = candidate.old;
                    engine.finish(
                        candidate.id,
                        false,
                        format!("Listener change failed; previous configuration retained: {error}"),
                    );
                } else {
                    let change = candidate
                        .change
                        .take()
                        .context("Listener candidate already consumed")?;
                    let message = change.message.clone();
                    match engine.commit(change) {
                        Ok(()) => {
                            self.options = candidate.options;
                            engine.finish(candidate.id, true, message);
                            self.revision = None;
                        }
                        Err(error) => {
                            candidate.failure = Some(format!(
                                "Configuration commit failed after registration: {error:#}"
                            ));
                            candidate.restoring = true;
                            candidate.options = candidate.old.clone();
                            self.source.stop();
                            self.source_ready = false;
                            self.source_attempted = false;
                            self.candidate = Some(candidate);
                        }
                    }
                }
            }
        } else if !self.source_attempted && !self.source.active() {
            self.source_attempted = true;
            self.source_started_at = monotonic_ms();
            if self.options.is_empty() {
                self.source_ready = true;
            } else if let Err(error) = self.source.start(self.options.clone()) {
                self.fault_startup(engine, format!("Listener startup failed: {error:#}"));
            }
        }
        if self.source_attempted
            && !self.source_ready
            && monotonic_ms().saturating_sub(self.source_started_at) > 10_000
        {
            self.source.stop();
            if let Some(candidate) = &mut self.candidate {
                candidate.failure = Some("Native listener startup deadline expired".into());
                candidate.restore_failed |= candidate.restoring;
            } else {
                self.fault_startup(engine, "Native listener startup deadline expired");
            }
        }
        Ok(())
    }
    fn sync_configuration(&mut self, engine: &mut Engine) -> Result<()> {
        if self.revision == Some(engine.repository.revision()) {
            return Ok(());
        }
        let documents = engine.documents();
        let (options, games) = options_from(&documents)?;
        let text = engine.repository.document("automation")?.to_json();
        if self.automation_text.as_deref() != Some(&text) {
            let document = parse_automation(engine.repository.document("automation")?)?;
            if let Some(scheduler) = &mut self.scheduler {
                if self.executor.is_busy() {
                    return Ok(());
                }
                scheduler.stop_accepting()?;
                scheduler.set_document(document)?;
                for id in std::mem::take(&mut self.manual_ids) {
                    engine.finish(
                        id,
                        false,
                        "Automation document changed before this queued request executed",
                    );
                }
            } else {
                let mut scheduler = automation_runtime::Engine::new(document);
                scheduler.initialize()?;
                self.scheduler = Some(scheduler);
            }
            self.automation_text = Some(text);
        }
        self.games.included_names = games.included_names;
        self.games.excluded_names = games.excluded_names;
        self.games.required = games.required;
        if self.candidate.is_none() && options != self.options {
            self.source.stop();
            self.options = options;
            self.source_ready = false;
            self.source_attempted = false;
        }
        self.revision = Some(engine.repository.revision());
        Ok(())
    }
    fn snapshot_events(&mut self, engine: &Engine) -> Result<Vec<AutomationEvent>> {
        if let Some(rows) = engine.cache.get("gpu.processes").and_then(Value::as_array) {
            self.games.gpu_available = !engine.cache.contains_key("error.gpu.processes");
            self.games.actual_gpu_pids = rows
                .iter()
                .filter_map(|row| {
                    row.pointer("/identity/pid")
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                })
                .collect();
        }
        let mut events =
            self.reducer
                .reduce_hardware(&observation(engine), &mut self.state, false)?;
        if let Some(wifi) = engine
            .cache
            .get("wireless")
            .and_then(|v| v.pointer("/wifi/value"))
        {
            if let Some(connected) = wifi.get("any_connected").and_then(Value::as_bool) {
                let ssid = if connected {
                    wifi.get("connected_ssids")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| Some(String::new()))
                } else {
                    None
                };
                if self.wifi_seen && self.state.wifi_ssid != ssid {
                    events.push(AutomationEvent::Wifi {
                        connected,
                        ssid: ssid.clone(),
                    });
                }
                self.state.wifi_ssid = ssid;
                self.wifi_seen = true;
            }
        }
        Ok(events)
    }
    fn source_reads(&mut self, engine: &mut Engine) {
        let now = monotonic_ms();
        if now < self.next_source_read || engine.shutdown.is_some() {
            return;
        }
        self.next_source_read = now.saturating_add(1000);
        if self.options.needs.wifi {
            engine.enqueue_read("wireless".into(), A::ReadWireless, false);
        }
        if self.options.needs.games && self.games.required.discrete_gpu {
            let values = actual_values(engine);
            if let (Some(index), Some(name)) = (
                values.get("gpu.index").and_then(Value::as_u64),
                values.get("gpu.name").and_then(Value::as_str),
            ) {
                if let Ok(index) = u32::try_from(index) {
                    engine.enqueue_read(
                        "gpu.processes".into(),
                        A::ReadGpuProcesses {
                            index,
                            expected_name: name.into(),
                        },
                        false,
                    );
                }
            } else if !engine.cache.contains_key("error.gpu") {
                engine.enqueue_read("gpu".into(), A::ReadGpuInventory, false);
            }
        }
    }
    fn initial_ready(&self) -> bool {
        self.source_ready
            && (!self.options.needs.clock || self.clock_seen)
            && (!self.options.needs.games || self.process_seen && self.games.complete())
            && (!self.options.needs.lid || self.state.lid_open.is_some())
            && (!self.options.needs.display || self.state.display_on.is_some())
            && (!self.options.needs.wifi || self.wifi_seen)
    }
    fn request_barrier(&mut self, engine: &mut Engine, required: StateRequirement) -> Result<()> {
        let now = monotonic_ms();
        if required.kind == StateSource::Configuration {
            let values = actual_values(engine);
            self.executor
                .confirm_state(required.token, &values, now, now)?;
            return Ok(());
        }
        let mut barrier = Barrier {
            required,
            requests: Vec::new(),
            failure: None,
            requested: now,
            completed: now,
        };
        let mut groups = BTreeMap::new();
        for key in &barrier.required.keys {
            let action = match key.as_str() {
                "fan.full_speed" => (
                    "mapped.fan_full_speed",
                    A::ReadMapped(MappedFeature::FanFullSpeed),
                ),
                "power.ai" => ("ai", A::ReadAi),
                "network.wifi" | "network.airplane" => ("wireless", A::ReadWireless),
                "gpu.oc.enabled" => {
                    let values = actual_values(engine);
                    let index = values
                        .get("gpu.index")
                        .and_then(Value::as_u64)
                        .context("GPU toggle requires actual inventory")?;
                    let name = values
                        .get("gpu.name")
                        .and_then(Value::as_str)
                        .context("GPU toggle requires actual identity")?
                        .to_owned();
                    (
                        "gpu.oc",
                        A::ReadGpuOverclock {
                            index: u32::try_from(index)?,
                            expected_name: name,
                        },
                    )
                }
                _ => bail!("No actual reader for {key}"),
            };
            groups.insert(action.0, action.1);
        }
        for (key, action) in groups {
            let id = engine.new_internal_request()?;
            barrier.requests.push(id);
            if let Err(error) = engine.request_read(id, key.into(), action) {
                engine.finish(id, false, format!("{error:#}"));
            }
        }
        self.barrier = Some(barrier);
        Ok(())
    }
    fn poll_barrier(&mut self, engine: &mut Engine) -> Result<()> {
        let Some(mut barrier) = self.barrier.take() else {
            return Ok(());
        };
        let mut pending = Vec::new();
        for id in barrier.requests {
            if let Some(result) = engine.take_internal_result(id) {
                if let Err(error) = result {
                    barrier.failure = Some(error);
                }
                if let Some((requested, completed)) = engine.read_receipts.remove(&id) {
                    barrier.requested = barrier.requested.max(requested);
                    barrier.completed = barrier.completed.max(completed);
                }
            } else {
                pending.push(id);
            }
        }
        barrier.requests = pending;
        if !barrier.requests.is_empty() {
            self.barrier = Some(barrier);
            return Ok(());
        }
        if let Some(error) = barrier.failure {
            self.executor.fail_state(barrier.required.token, error)?;
        } else {
            let values = actual_values(engine);
            if let Err(error) = self.executor.confirm_state(
                barrier.required.token,
                &values,
                barrier.requested,
                barrier.completed,
            ) {
                self.executor
                    .fail_state(barrier.required.token, format!("{error:#}"))?;
            }
        }
        Ok(())
    }
    fn ui_effect(
        &mut self,
        engine: &mut Engine,
        effect: StepEffect,
        kind: &'static str,
        payload: Value,
    ) -> Result<()> {
        let request = engine.new_internal_request()?;
        engine.value(
            "automation.ui_effect",
            json!({"effect_id":request,"kind":kind,"payload":payload}),
        );
        self.pending = Some(PendingEffect {
            effect,
            kind: PendingKind::Ui {
                request,
                kind,
                result: None,
                deadline_ms: monotonic_ms().saturating_add(5000),
            },
            noted_late: false,
        });
        Ok(())
    }
    fn begin_preset(
        &mut self,
        engine: &mut Engine,
        effect: StepEffect,
        command: &automation_runtime::SemanticCommand,
    ) -> Result<()> {
        let doc = engine.repository.document("godmode")?;
        let presets = doc
            .root()
            .get_ci("Presets")
            .and_then(JsonNode::as_object)
            .context("Preset dictionary absent")?;
        let cycle = command.command == "preset.cycle";
        if cycle && presets.len() < 2 {
            self.executor
                .complete_step(effect.step_token, StepCompletion::default())?;
            return Ok(());
        }
        let id = if cycle {
            let active = doc
                .root()
                .get_ci("ActivePresetId")
                .and_then(JsonNode::as_str)
                .unwrap_or("");
            let current = presets
                .iter()
                .position(|(id, _)| id.eq_ignore_ascii_case(active))
                .unwrap_or(0);
            presets[(current + 1) % presets.len()].0.clone()
        } else {
            command
                .payload
                .get("selection.presets")
                .and_then(JsonNode::as_str)
                .context("Preset selection absent")?
                .into()
        };
        let Some((id, preset)) = presets
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(&id))
        else {
            self.executor
                .complete_step(effect.step_token, StepCompletion::default())?;
            return Ok(());
        };
        let target = PresetTarget {
            id: id.clone(),
            value: config_actions::preset_to_ui(id, preset)?.to_value()?,
            output: cycle.then(|| {
                preset
                    .get_ci("Name")
                    .and_then(JsonNode::as_str)
                    .unwrap_or("")
                    .to_owned()
            }),
            saved: false,
        };
        let request = engine.new_internal_request()?;
        if let Err(error) = engine.request_read(request, "power_modes".into(), A::ReadPowerModes) {
            engine.finish(request, false, format!("{error:#}"));
        }
        self.pending = Some(PendingEffect {
            effect,
            kind: PendingKind::PresetRead {
                request: Some(request),
                target,
                raw_mode: None,
            },
            noted_late: false,
        });
        Ok(())
    }
    fn dispatch(&mut self, engine: &mut Engine, effect: StepEffect) -> Result<()> {
        match &effect.action {
            PlannedAction::Deferred { .. } => bail!("Deferred preview escaped actual executor"),
            PlannedAction::Delay { .. } => bail!("Delay escaped deadline scheduler"),
            PlannedAction::Run {
                options,
                working_directory,
            } => {
                self.process.start(ProcessRequest {
                    options: options.clone(),
                    working_directory: working_directory.clone(),
                    environment: effect.environment.clone(),
                })?;
                self.pending = Some(PendingEffect {
                    effect,
                    kind: PendingKind::Process,
                    noted_late: false,
                });
            }
            PlannedAction::Notification { text, title, .. } => {
                if text.as_ref().is_none_or(|t| t.trim().is_empty()) {
                    self.executor
                        .complete_step(effect.step_token, StepCompletion::default())?;
                } else {
                    let payload = json!({"text":text,"title":title});
                    self.ui_effect(engine, effect, "notification", payload)?;
                }
            }
            PlannedAction::PlaySound { path } => {
                if let Some(path) = path {
                    self.sound = Some(Sound::start(path.clone())?);
                    self.pending = Some(PendingEffect {
                        effect,
                        kind: PendingKind::Sound,
                        noted_late: false,
                    });
                } else {
                    self.executor
                        .complete_step(effect.step_token, StepCompletion::default())?;
                }
            }
            PlannedAction::ImportSpectrumProfile { path } => {
                if let Some(path) = path {
                    use std::io::Read;
                    let mut bytes = Vec::new();
                    std::fs::File::open(path)?
                        .take(1024 * 1024 + 1)
                        .read_to_end(&mut bytes)?;
                    ensure!(
                        bytes.len() <= 1024 * 1024,
                        "Spectrum import exceeds size budget"
                    );
                    let raw = JsonNode::parse(std::str::from_utf8(&bytes)?)?;
                    let effects: Vec<jiks_platform::spectrum::Effect> =
                        serde_json::from_value(raw.to_value()?)?;
                    // Validate the complete effect packet before requesting the current
                    // profile. The actual profile is read for every import, not inferred
                    // from a previous editor selection or a stale UI projection.
                    jiks_platform::spectrum::Profile {
                        profile: 1,
                        effects: effects.clone(),
                    }
                    .encode_24_zone()?;
                    let mut payload = json!({});
                    engine.context(&mut payload)?;
                    let path = payload
                        .pointer("/_context/keyboard/path")
                        .and_then(Value::as_str)
                        .context("A unique Spectrum interface is required")?
                        .to_owned();
                    let id = engine.new_internal_request()?;
                    if let Err(error) = engine.start_work(
                        id,
                        "keyboard.profile.read",
                        vec![A::Spectrum(
                            jiks_platform::SpectrumAction::ReadActiveProfile { path },
                        )],
                        After::Hardware,
                    ) {
                        engine.finish(id, false, format!("{error:#}"));
                    }
                    self.pending = Some(PendingEffect {
                        effect,
                        kind: PendingKind::SpectrumRead {
                            request: Some(id),
                            effects: serde_json::to_value(effects)?,
                            profile: None,
                        },
                        noted_late: false,
                    });
                } else {
                    self.executor
                        .complete_step(effect.step_token, StepCompletion::default())?;
                }
            }
            PlannedAction::Command(command) => {
                if command.command == "preset.cycle"
                    || command.command == "preset.apply"
                        && command.payload.get("selection.presets").is_some()
                {
                    let command = command.clone();
                    return self.begin_preset(engine, effect, &command);
                }
                if command.command == "preset.apply" && command.payload.get("preset").is_some() {
                    let mut payload = command.payload.to_value()?;
                    engine.context(&mut payload)?;
                    let actions = hardware_routes::plan("preset.apply", &payload)?;
                    let id = engine.new_internal_request()?;
                    if let Err(error) =
                        engine.start_work(id, "preset.apply", actions, After::Hardware)
                    {
                        engine.finish(id, false, format!("{error:#}"));
                    }
                    self.pending = Some(PendingEffect {
                        effect,
                        kind: PendingKind::Command {
                            request: id,
                            output: None,
                            preset: None,
                        },
                        noted_late: false,
                    });
                    return Ok(());
                }
                if command.command == "app.show" {
                    return self.ui_effect(engine, effect, "show", json!({}));
                }
                if command.command == "app.close" {
                    return self.ui_effect(engine, effect, "close", json!({}));
                }
                let id = engine.new_internal_request()?;
                let mut payload = command.payload.to_value()?;
                let result = (|| -> Result<()> {
                    if config_command(&command.command) {
                        let context = config_actions::ActionContext::default();
                        let change = config_actions::apply_action_with_context(
                            &command.command,
                            &command.payload,
                            &engine.documents(),
                            &context,
                        )?;
                        if let Some(change) = self.request_configuration(engine, id, change)? {
                            let message = change.message.clone();
                            engine.commit(change)?;
                            engine.finish(id, true, message);
                        }
                        Ok(())
                    } else {
                        engine.process(Command::Execute {
                            request_id: id,
                            command: command.command.clone(),
                            payload: std::mem::take(&mut payload),
                        })
                    }
                })();
                if let Err(error) = result {
                    if let Some(pending) = engine.pending.as_mut().filter(|p| p.work.id == id) {
                        pending.work.late = true;
                        self.executor
                            .note_inflight_failure(effect.step_token, format!("{error:#}"))?;
                    } else {
                        engine.finish(id, false, format!("{error:#}"));
                    }
                }
                self.pending = Some(PendingEffect {
                    effect,
                    kind: PendingKind::Command {
                        request: id,
                        output: None,
                        preset: None,
                    },
                    noted_late: false,
                });
            }
        }
        Ok(())
    }
    fn poll_effect(&mut self, engine: &mut Engine) -> Result<()> {
        let Some(mut pending) = self.pending.take() else {
            return Ok(());
        };
        let mut completion = None;
        let mut next_kind = None;
        match &mut pending.kind {
            PendingKind::SpectrumRead {
                request,
                effects,
                profile,
            } => {
                let result = (|| -> Result<()> {
                    if let Some(id) = *request {
                        let Some(result) = engine.take_internal_result(id) else {
                            return Ok(());
                        };
                        result.map_err(anyhow::Error::msg)?;
                        *request = None;
                        *profile = engine
                            .cache
                            .get("keyboard.active_profile")
                            .and_then(|v| v.get("profile"))
                            .and_then(Value::as_u64);
                        ensure!(
                            profile.is_some_and(|p| p <= 6),
                            "Actual active Spectrum profile read missing"
                        );
                    }
                    if self.executor.is_cancelled() {
                        completion = Some(StepCompletion {
                            failure: Some("Spectrum import cancelled before apply".into()),
                            ..Default::default()
                        });
                        return Ok(());
                    }
                    ensure!(
                        *profile != Some(0),
                        "Spectrum is switched off; select a supported profile 1..6 before importing effects"
                    );
                    if engine.pending.is_some() {
                        return Ok(());
                    }
                    let mut payload = json!({"profile":profile,"effects":effects});
                    engine.context(&mut payload)?;
                    let actions = hardware_routes::plan("keyboard.effects.apply", &payload)?;
                    let id = engine.new_internal_request()?;
                    if let Err(error) =
                        engine.start_work(id, "keyboard.effects.apply", actions, After::Hardware)
                    {
                        engine.finish(id, false, format!("{error:#}"));
                    }
                    next_kind = Some(PendingKind::Command {
                        request: id,
                        output: None,
                        preset: None,
                    });
                    Ok(())
                })();
                if let Err(error) = result {
                    completion = Some(StepCompletion {
                        failure: Some(format!("{error:#}")),
                        ..Default::default()
                    });
                }
            }
            PendingKind::PresetRead {
                request,
                target,
                raw_mode,
            } => {
                let result = (|| -> Result<()> {
                    if let Some(id) = *request {
                        let Some(result) = engine.take_internal_result(id) else {
                            return Ok(());
                        };
                        engine.read_receipts.remove(&id);
                        result.map_err(anyhow::Error::msg)?;
                        *request = None;
                        *raw_mode = engine
                            .cache
                            .get("power_modes")
                            .and_then(|v| v.get("current_raw"))
                            .and_then(Value::as_u64);
                        ensure!(raw_mode.is_some(), "Actual preset mode read missing");
                    }
                    if self.executor.is_cancelled() {
                        completion = Some(StepCompletion {
                            failure: Some("Preset action cancelled before apply".into()),
                            ..Default::default()
                        });
                        return Ok(());
                    }
                    if engine.pending.is_some() {
                        return Ok(());
                    }
                    if !target.saved {
                        engine.repository.update_known(
                            "godmode",
                            &[(&["ActivePresetId"], JsonNode::String(target.id.clone()))],
                        )?;
                        engine.publish_configuration()?;
                        target.saved = true;
                    }
                    if *raw_mode != Some(255) {
                        completion = Some(StepCompletion {
                            run_output: target.output.take(),
                            emitted: vec![AutomationEvent::PresetChanged(target.id.clone())],
                            ..Default::default()
                        });
                        return Ok(());
                    }
                    let mut payload = json!({"preset":target.value});
                    engine.context(&mut payload)?;
                    let actions = hardware_routes::plan("preset.apply", &payload)?;
                    let id = engine.new_internal_request()?;
                    if let Err(error) =
                        engine.start_work(id, "preset.apply", actions, After::Hardware)
                    {
                        engine.finish(id, false, format!("{error:#}"));
                    }
                    let output = target.output.take();
                    let preset = Some(target.id.clone());
                    next_kind = Some(PendingKind::Command {
                        request: id,
                        output,
                        preset,
                    });
                    Ok(())
                })();
                if let Err(error) = result {
                    completion = Some(StepCompletion {
                        failure: Some(format!("{error:#}")),
                        ..Default::default()
                    });
                }
            }
            PendingKind::Command {
                request,
                output,
                preset,
            } => {
                if !pending.noted_late
                    && engine
                        .pending
                        .as_ref()
                        .is_some_and(|p| p.work.id == *request && p.work.late)
                {
                    self.executor.note_inflight_failure(
                        pending.effect.step_token,
                        "Actual physical operation exceeded deadline",
                    )?;
                    pending.noted_late = true;
                }
                if let Some(result) = engine.take_internal_result(*request) {
                    let events = self.snapshot_events(engine)?;
                    let mut done = StepCompletion {
                        failure: result.err(),
                        run_output: output.take(),
                        emitted: events,
                    };
                    if let Some(preset) = preset.take() {
                        let event = AutomationEvent::PresetChanged(preset);
                        if !done.emitted.contains(&event) {
                            done.emitted.push(event);
                        }
                    }
                    completion = Some(done);
                }
            }
            PendingKind::Process => {
                if let Some(result) = self.process.poll() {
                    completion = Some(StepCompletion {
                        failure: result.failure.clone(),
                        run_output: result.run_output().map(str::to_owned),
                        emitted: Vec::new(),
                    });
                }
            }
            PendingKind::Sound => {
                if self.sound.as_ref().is_some_and(|s| s.join.is_finished()) {
                    let sound = self.sound.take().unwrap();
                    let result = sound
                        .join
                        .join()
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("Sound worker panicked")));
                    completion = Some(StepCompletion {
                        failure: result.err().map(|e| format!("{e:#}")),
                        ..Default::default()
                    });
                }
            }
            PendingKind::Ui {
                request,
                kind,
                result,
                deadline_ms,
            } => {
                if result.is_none()
                    && (self.stopping
                        || self.executor.is_cancelled()
                        || monotonic_ms() >= *deadline_ms)
                {
                    *result = Some(Err(
                        "界面动作没有完成回执；已取消并停止剩余步骤，结果保持未知".into(),
                    ));
                }
                if let Some(result) = result.take() {
                    engine.value("automation.ui_effect", Value::Null);
                    engine.finish(
                        *request,
                        result.is_ok(),
                        result.as_ref().err().cloned().unwrap_or_default(),
                    );
                    engine.take_internal_result(*request);
                    if *kind == "close" && result.is_ok() {
                        self.cancel();
                    }
                    completion = Some(StepCompletion {
                        failure: result.err(),
                        ..Default::default()
                    });
                }
            }
        }
        if let Some(kind) = next_kind {
            pending.kind = kind;
        }
        if let Some(completion) = completion {
            self.executor
                .complete_step(pending.effect.step_token, completion)?;
        } else {
            self.pending = Some(pending);
        }
        Ok(())
    }
    fn finish_pipeline(&mut self, engine: &mut Engine) -> Result<()> {
        if let Some(done) = self.executor.take_completion() {
            let error = if done.failures.is_empty() {
                None
            } else {
                Some(done.failures.join("; "))
            };
            if let Some(scheduler) = &mut self.scheduler {
                if let Err(e) =
                    scheduler.complete_plan(done.plan_token, done.failures, done.emitted)
                {
                    self.fault_startup(engine, format!("{e:#}"));
                }
            }
            if let Some(id) = self.current_manual.take() {
                self.manual_ids.remove(&id);
                engine.finish(
                    id,
                    error.is_none(),
                    error.unwrap_or_else(|| "Automation pipeline actually completed".into()),
                );
            }
        }
        Ok(())
    }
    fn maybe_thermal(&mut self, engine: &mut Engine) -> Result<()> {
        if let Some(id) = self.thermal_request {
            if let Some(result) = engine.take_internal_result(id) {
                self.thermal_request = None;
                if let Err(error) = result {
                    self.fault_startup(engine, format!("Thermal autostart failed: {error}"));
                }
            }
        }
        if self.thermal_attempted
            || self.stopping
            || engine.read_only
            || engine.faulted
            || engine.pending.is_some()
            || self.startup_error.is_some()
            || !engine.policy.allows_thermal_autostart()
        {
            return Ok(());
        }
        if !self
            .scheduler
            .as_ref()
            .is_some_and(|s| s.startup_autostart_allowed())
        {
            return Ok(());
        }
        self.thermal_attempted = true;
        engine.value("automation.startup_ready", json!(true));
        let app =
            ApplicationThermalSettings::from_document(engine.repository.document("application")?)?;
        if app.enabled || app.shadow_enabled {
            let id = engine.new_internal_request()?;
            let payload = json!({"thermal.enabled":app.enabled,"thermal.shadow":app.shadow_enabled,"thermal.sampling_ms":app.sampling_ms,"thermal.firmware_limit_c":app.firmware_limit_c});
            if let Err(error) = engine.request_thermal(id, &payload) {
                engine.finish(id, false, format!("{error:#}"));
            }
            self.thermal_request = Some(id);
        }
        Ok(())
    }
    pub(super) fn poll(&mut self, engine: &mut Engine) -> Result<()> {
        if engine.shutdown.is_some() || engine.faulted {
            self.cancel();
        }
        // Actual acks always drain first, including after cancellation/fault.
        self.poll_effect(engine)?;
        self.poll_barrier(engine)?;
        self.finish_pipeline(engine)?;
        if engine.read_only {
            self.cancel();
            self.poll_source(engine)?;
            while let Some((id, _)) = self.manual.pop_front() {
                self.manual_ids.remove(&id);
                engine.finish(id, false, "Read-only mode does not run automation");
            }
            return Ok(());
        }
        if self.stopping {
            self.poll_source(engine)?;
            if !self.executor.is_busy() {
                if let Some(scheduler) = &mut self.scheduler {
                    scheduler.stop_accepting()?;
                }
            }
            let active = self.current_manual;
            for id in std::mem::take(&mut self.manual_ids) {
                if Some(id) == active {
                    self.manual_ids.insert(id);
                } else {
                    engine.finish(id, false, "Automation request cancelled before execution");
                }
            }
            self.manual.clear();
            return Ok(());
        }
        if engine.initializing {
            return Ok(());
        }
        if let Err(error) = self.sync_configuration(engine) {
            self.fault_startup(engine, format!("Initialization failed: {error:#}"));
            return Ok(());
        }
        self.poll_source(engine)?;
        self.source_reads(engine);
        if self.candidate.is_some() {
            return Ok(());
        }
        if self.pending.is_none() {
            let events = self.snapshot_events(engine)?;
            self.events(events, None)?;
        }
        if let Some(scheduler) = &mut self.scheduler {
            scheduler.update_state(self.state.clone());
        }
        if !self.startup_started && self.startup_error.is_none() && self.initial_ready() {
            self.scheduler
                .as_mut()
                .context("Automation scheduler absent")?
                .start_startup(monotonic_ms().max(1), self.state.clone())?;
            self.startup_started = true;
        }
        if !self.startup_started {
            if self.source_attempted
                && monotonic_ms().saturating_sub(self.source_started_at) > 10000
                && self.startup_error.is_none()
            {
                self.fault_startup(
                    engine,
                    "Initial automation metadata remains unknown; thermal autostart blocked",
                );
            }
            return Ok(());
        }
        while let Some((id, pipeline)) = self.manual.pop_front() {
            if let Err(error) = self
                .scheduler
                .as_mut()
                .unwrap()
                .request_manual_correlated(&pipeline, Some(id))
            {
                self.manual_ids.remove(&id);
                engine.finish(id, false, format!("{error:#}"));
            }
        }
        if !self.executor.is_busy() && self.pending.is_none() {
            let values = actual_values(engine);
            match self.scheduler.as_mut().unwrap().next_plan(&values) {
                Ok(Some(plan)) => {
                    self.current_manual = plan.request_id;
                    self.executor.start(plan)?;
                }
                Ok(None) => {}
                Err(error) => {
                    while let Some((id, message)) =
                        self.scheduler.as_mut().unwrap().take_failed_manual()
                    {
                        if let Some(id) = id {
                            self.manual_ids.remove(&id);
                            engine.finish(id, false, message);
                        }
                    }
                    if !self.scheduler.as_ref().unwrap().startup_autostart_allowed() {
                        self.fault_startup(engine, format!("{error:#}"));
                    }
                }
            }
        }
        if self.pending.is_none()
            && self.barrier.is_none()
            && self.candidate.is_none()
            && engine.pending.is_none()
        {
            let now = monotonic_ms();
            if let Some(required) = self.executor.required_state(now)? {
                let token = required.token;
                if let Err(error) = self.request_barrier(engine, required) {
                    self.executor.fail_state(token, format!("{error:#}"))?;
                }
            }
            if self.barrier.is_none() {
                let values = actual_values(engine);
                if let Some(effect) = self.executor.poll(monotonic_ms(), &values)? {
                    let token = effect.step_token;
                    if let Err(error) = self.dispatch(engine, effect) {
                        self.executor.complete_step(
                            token,
                            StepCompletion {
                                failure: Some(format!("{error:#}")),
                                ..Default::default()
                            },
                        )?;
                    }
                }
            }
        }
        self.finish_pipeline(engine)?;
        if self.startup_error.is_none()
            && self
                .scheduler
                .as_ref()
                .is_some_and(|s| s.startup_autostart_allowed())
            && !self.executor.is_busy()
            && self.pending.is_none()
            && self.barrier.is_none()
        {
            engine.reconcile_power_policies()?;
        }
        self.maybe_thermal(engine)?;
        if self.startup_error.is_none()
            && self
                .scheduler
                .as_ref()
                .is_some_and(|s| s.startup_autostart_allowed())
            && !self.executor.is_busy()
            && self.pending.is_none()
            && self.barrier.is_none()
            && engine.pending.is_none()
        {
            engine.reconcile_ai(&self.state)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn executor_with(steps: Vec<Value>) -> Executor {
        use jiks_core::automation::PipelineDto;
        let steps = steps
            .into_iter()
            .map(|v| {
                automation_runtime::step_from_ui(&JsonNode::parse(&v.to_string()).unwrap()).unwrap()
            })
            .collect();
        let pipeline =
            PipelineDto::new("11111111-1111-1111-1111-111111111111", None, steps).unwrap();
        let mut executor = Executor::new();
        executor
            .start(automation_runtime::ExecutionPlan {
                token: 1,
                pipeline: automation_runtime::plan_pipeline(&pipeline, &[], &BTreeMap::new())
                    .unwrap(),
                origin: None,
                request_id: None,
            })
            .unwrap();
        executor
    }
    #[test]
    fn repeated_ui_effects_use_opaque_ids_and_cancel_cannot_block_eof_shutdown() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        let mut host = AutomationHost::default();
        host.executor = executor_with(vec![json!({"type":"show_app"}), json!({"type":"show_app"})]);
        let first = host.executor.poll(0, &BTreeMap::new()).unwrap().unwrap();
        host.dispatch(&mut f.engine, first).unwrap();
        let id = f.engine.state.lock().unwrap().values["automation.ui_effect"]["effect_id"]
            .as_u64()
            .unwrap();
        host.poll_effect(&mut f.engine).unwrap();
        assert!(host.executor.is_busy());
        host.ui_complete(id, true, String::new()).unwrap();
        host.poll_effect(&mut f.engine).unwrap();
        let second = host.executor.poll(1, &BTreeMap::new()).unwrap().unwrap();
        host.dispatch(&mut f.engine, second).unwrap();
        let next = f.engine.state.lock().unwrap().values["automation.ui_effect"]["effect_id"]
            .as_u64()
            .unwrap();
        assert!(next < id);
        assert!(host.ui_complete(id, true, String::new()).is_err());
        host.cancel();
        host.poll_effect(&mut f.engine).unwrap();
        let done = host.executor.take_completion().unwrap();
        assert!(done.cancelled && !done.failures.is_empty());
        assert!(f.engine.internal_requests.is_empty());
        assert_eq!(
            f.engine.state.lock().unwrap().values["automation.ui_effect"],
            Value::Null
        );
        assert!(f.log.lock().unwrap().is_empty());
    }
    #[test]
    fn cycle_reads_actual_mode_and_only_then_commits_selection_and_contextual_output() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        let first = "11111111-1111-1111-1111-111111111111";
        let second = "22222222-2222-2222-2222-222222222222";
        f.engine.repository.save("godmode",SettingsDocument::parse(&json!({"ActivePresetId":first,"Presets":{first:{"Name":"First"},second:{"Name":"Second"}}}).to_string()).unwrap()).unwrap();
        let answer = f.answer();
        let mut host = AutomationHost::default();
        host.executor = executor_with(vec![
            json!({"type":"cycle_preset"}),
            json!({"type":"notification","parameters":{"message":"$RUN_OUTPUT$"}}),
        ]);
        let effect = host.executor.poll(0, &BTreeMap::new()).unwrap().unwrap();
        host.dispatch(&mut f.engine, effect).unwrap();
        assert!(
            f.log
                .lock()
                .unwrap()
                .iter()
                .all(|a| matches!(a, A::ReadPowerModes))
        );
        host.poll_effect(&mut f.engine).unwrap();
        assert_eq!(
            f.engine
                .repository
                .document("godmode")
                .unwrap()
                .root()
                .get("ActivePresetId")
                .and_then(JsonNode::as_str),
            Some(first)
        );
        f.engine.poll_read().unwrap();
        *answer.lock().unwrap() = Some(Ok(json!({"current_raw":3})));
        f.engine.poll_read().unwrap();
        host.poll_effect(&mut f.engine).unwrap();
        assert_eq!(
            f.engine
                .repository
                .document("godmode")
                .unwrap()
                .root()
                .get("ActivePresetId")
                .and_then(JsonNode::as_str),
            Some(second)
        );
        let next = host.executor.poll(1, &BTreeMap::new()).unwrap().unwrap();
        assert!(
            matches!(next.action,PlannedAction::Notification{text:Some(ref text),..} if text=="Second")
        );
        assert!(f.engine.pending.is_none());
    }
    #[test]
    fn spectrum_import_uses_actual_active_profile_and_waits_for_both_native_receipts() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        let path = f.engine.repository.root().join("synthetic-effects.json");
        let effects = json!([{"Type":"Always","Colors":[{"R":255,"G":0,"B":0}],"Keys":[1]}]);
        std::fs::write(&path, effects.to_string()).unwrap();
        f.engine.cache.insert(
            "spectrum".into(),
            json!({"candidates":[{"path":"\\\\?\\hid#fixture"}]}),
        );
        let read = f.answer();
        let write = f.answer();
        let mut host = AutomationHost::default();
        host.executor = executor_with(vec![
            json!({"type":"spectrum_import","parameters":{"path":path}}),
        ]);
        let effect = host.executor.poll(0, &BTreeMap::new()).unwrap().unwrap();
        host.dispatch(&mut f.engine, effect).unwrap();
        assert!(matches!(
            f.log.lock().unwrap()[0],
            A::Spectrum(jiks_platform::SpectrumAction::ReadActiveProfile { .. })
        ));
        host.poll_effect(&mut f.engine).unwrap();
        assert_eq!(f.log.lock().unwrap().len(), 1);
        *read.lock().unwrap() = Some(Ok(json!({"profile":2})));
        f.engine.poll_write().unwrap();
        host.poll_effect(&mut f.engine).unwrap();
        assert!(
            matches!(&f.log.lock().unwrap()[1],A::Spectrum(jiks_platform::SpectrumAction::Effects{profile,..}) if profile.profile==2)
        );
        assert!(host.executor.is_busy());
        host.poll_effect(&mut f.engine).unwrap();
        assert!(host.executor.is_busy());
        *write.lock().unwrap() = Some(Ok(json!({"profile":2,"effects":effects})));
        f.engine.poll_write().unwrap();
        host.poll_effect(&mut f.engine).unwrap();
        assert!(host.executor.take_completion().unwrap().failures.is_empty());
    }
    struct FailedSource;
    impl SourcePort for FailedSource {
        fn start(&mut self, _: SourceOptions) -> Result<()> {
            bail!("synthetic registration failure")
        }
        fn next(&self) -> Option<SourceMessage> {
            None
        }
        fn stop(&self) {}
        fn finish(&mut self) -> Option<Result<()>> {
            None
        }
        fn active(&self) -> bool {
            false
        }
        fn cleanup_confirmed(&self) -> bool {
            true
        }
        fn suspend_macro(&self, _: bool) {}
        fn waker(&mut self, _: Option<Arc<dyn Fn() + Send + Sync>>) -> Result<()> {
            Ok(())
        }
    }
    #[test]
    fn failed_listener_restoration_acknowledges_failure_after_cleanup_without_retry_loop() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        let mut host = AutomationHost::default();
        host.source = Box::new(FailedSource);
        let mut options = SourceOptions::default();
        options.needs.clock = true;
        host.candidate = Some(Candidate {
            id: 909,
            change: None,
            options: options.clone(),
            old: options,
            failure: Some("candidate registration failed".into()),
            restoring: false,
            restore_failed: false,
            wait_execution: false,
            wait_macros: false,
        });
        host.poll_source(&mut f.engine).unwrap();
        assert!(host.candidate.is_some());
        host.poll_source(&mut f.engine).unwrap();
        assert!(host.candidate.is_none());
        assert!(!host.is_source_active());
        assert!(!host.is_busy());
        assert!(
            f.engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|r| r.request_id == 909 && !r.ok)
        );
    }
    #[test]
    fn default_host_and_empty_documents_do_not_start_any_source_or_action() {
        let host = AutomationHost::default();
        assert!(!host.is_busy());
        assert!(!host.is_source_active());
        assert!(host.scheduler.is_none());
    }
    #[test]
    fn duplicate_or_malformed_manual_requests_fail_without_starting_any_native_work() {
        let mut host = AutomationHost::default();
        assert!(host.manual(1, "invalid").is_err());
        host.manual(1, "11111111-1111-1111-1111-111111111111")
            .unwrap();
        assert!(
            host.manual(1, "11111111-1111-1111-1111-111111111111")
                .is_err()
        );
        assert!(!host.source.active());
        assert!(!host.process.is_busy());
    }
    #[test]
    fn unsolicited_ui_ack_cannot_complete_a_pipeline() {
        let mut host = AutomationHost::default();
        assert!(host.ui_complete(999, true, "fake".into()).is_err());
        assert!(host.executor.take_completion().is_none());
    }
    fn manual_document(engine: &mut Engine, state: i32) {
        use jiks_core::automation::{PipelineDto, StepDto, StepKind};
        let step = StepDto::new(
            StepKind::PowerMode,
            JsonNode::parse(&format!("{{\"State\":{state}}}")).unwrap(),
        )
        .unwrap();
        let pipeline =
            PipelineDto::new("11111111-1111-1111-1111-111111111111", None, vec![step]).unwrap();
        engine.repository.save("automation",SettingsDocument::parse(&json!({"IsEnabled":false,"Pipelines":[pipeline.to_json_node().to_value().unwrap()]}).to_string()).unwrap()).unwrap();
        engine.cache.insert(
            "power_modes".into(),
            serde_json::to_value(jiks_platform::power_modes::decode(
                2, None, None, None, false,
            ))
            .unwrap(),
        );
        engine.cache.insert("cpu".into(), json!({"raw_mode":2}));
        engine.cache.insert(
            "system".into(),
            json!({"ac_connected":{"available":true,"value":true}}),
        );
    }
    #[test]
    fn actual_coordinator_receipt_required_before_manual_success_and_correlation_is_exact() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        manual_document(&mut f.engine, 2);
        let answer = f.answer();
        let mut host = AutomationHost::default();
        host.manual(700, "11111111-1111-1111-1111-111111111111")
            .unwrap();
        host.poll(&mut f.engine).unwrap();
        assert!(f.engine.pending.is_some());
        assert_eq!(f.log.lock().unwrap().len(), 1);
        assert!(
            !f.engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|r| r.request_id == 700)
        );
        host.poll(&mut f.engine).unwrap();
        assert!(
            !f.engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|r| r.request_id == 700)
        );
        *answer.lock().unwrap() = Some(Ok(json!(3)));
        f.engine.poll_write().unwrap();
        host.poll(&mut f.engine).unwrap();
        assert!(
            f.engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|r| r.request_id == 700 && r.ok)
        );
        assert!(!host.executor.is_busy());
    }
    #[test]
    fn cancellation_keeps_real_hardware_ticket_owned_and_does_not_ack_success() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        manual_document(&mut f.engine, 2);
        let answer = f.answer();
        let mut host = AutomationHost::default();
        host.manual(701, "11111111-1111-1111-1111-111111111111")
            .unwrap();
        host.poll(&mut f.engine).unwrap();
        host.cancel();
        host.poll(&mut f.engine).unwrap();
        assert!(host.is_busy());
        assert!(f.engine.pending.is_some());
        assert!(
            !f.engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|r| r.request_id == 701)
        );
        *answer.lock().unwrap() = Some(Ok(json!(3)));
        f.engine.poll_write().unwrap();
        host.poll(&mut f.engine).unwrap();
        assert!(
            f.engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|r| r.request_id == 701 && !r.ok)
        );
        assert!(!host.is_busy());
    }
    #[test]
    fn failed_startup_actual_step_never_sends_thermal_enable() {
        use jiks_core::automation::{PipelineDto, StepDto, StepKind, TriggerDto, TriggerKind};
        let mut f = super::super::lifecycle_tests::Fixture::new();
        let answer = f.answer();
        manual_document(&mut f.engine, 2);
        let step = StepDto::new(
            StepKind::PowerMode,
            JsonNode::parse("{\"State\":2}").unwrap(),
        )
        .unwrap();
        let trigger = TriggerDto::new(TriggerKind::OnStartup, JsonNode::object()).unwrap();
        let pipeline = PipelineDto::new(
            "11111111-1111-1111-1111-111111111111",
            Some(trigger),
            vec![step],
        )
        .unwrap();
        f.engine.repository.save("automation",SettingsDocument::parse(&json!({"IsEnabled":true,"Pipelines":[pipeline.to_json_node().to_value().unwrap()]}).to_string()).unwrap()).unwrap();
        let mut app = f
            .engine
            .repository
            .document("application")
            .unwrap()
            .root()
            .clone();
        app.set_known("EnableCpuThermalControl", JsonNode::Bool(true))
            .unwrap();
        f.engine
            .repository
            .save("application", SettingsDocument::from_root(app).unwrap())
            .unwrap();
        let mut host = AutomationHost::default();
        host.poll(&mut f.engine).unwrap();
        assert!(!host.thermal_attempted);
        *answer.lock().unwrap() = Some(Err("fake actual setter failure".into()));
        f.engine.poll_write().unwrap();
        host.poll(&mut f.engine).unwrap();
        assert!(host.startup_error.is_some());
        assert!(
            f.log
                .lock()
                .unwrap()
                .iter()
                .all(|a| !matches!(a, A::EnableCpu { .. } | A::SetCpuPower { .. }))
        );
        assert!(!host.thermal_attempted);
    }
}
