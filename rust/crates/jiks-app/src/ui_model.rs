//! Native UI/runtime contract. The UI never performs hardware or filesystem I/O.
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc::SyncSender};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct UiBridge {
    pub model: Arc<Mutex<UiModel>>,
    pub actions: SyncSender<UiAction>,
    /// Optional bundled font bytes; their storage stays alive for the UI lifetime.
    pub font_data: Option<Arc<[u8]>>,
    pub start_minimized: bool,
    /// Opaque per-profile/user/elevation tag; only the fixed show message uses it.
    pub instance_key: Option<String>,
    /// Normal graphical startup only. Fixtures and readonly runs never prompt.
    pub startup_language_prompt: bool,
    /// Explicit isolated smoke-test option; production stays None.
    pub smoke_close_after: Option<Duration>,
    /// Signals native window/control creation only, never backend readiness.
    pub window_ready: Option<std::sync::mpsc::Sender<()>>,
    /// Isolated fixture only: exercise rebuild + nested modal + close, no Save.
    pub smoke_exercise_reentrancy: bool,
    pub diagnostics: Arc<UiDiagnostics>,
    #[cfg(debug_assertions)]
    pub layout_capture_dir: Option<std::path::PathBuf>,
}
impl UiBridge {
    pub fn new(model: Arc<Mutex<UiModel>>, actions: SyncSender<UiAction>) -> Self {
        Self {
            model,
            actions,
            font_data: None,
            start_minimized: false,
            instance_key: None,
            startup_language_prompt: false,
            smoke_close_after: None,
            window_ready: None,
            smoke_exercise_reentrancy: false,
            diagnostics: Arc::new(UiDiagnostics::default()),
            #[cfg(debug_assertions)]
            layout_capture_dir: None,
        }
    }
    pub fn with_smoke_close_after(mut self, duration: Duration) -> Self {
        self.smoke_close_after = Some(duration);
        self.smoke_exercise_reentrancy = true;
        self
    }
}

#[derive(Debug, Default)]
pub struct UiDiagnostics {
    pub main_rebuilds: AtomicU64,
    pub reentrant_defaults: AtomicU64,
    pub modals_opened: AtomicU64,
    pub modals_closed: AtomicU64,
    pub shutdown_requests: AtomicU64,
    pub overlays_opened: AtomicU64,
    pub overlays_closed: AtomicU64,
    pub ui_effects_completed: AtomicU64,
    pub locale_switches: AtomicU64,
    pub status_opened: AtomicU64,
    pub status_closed: AtomicU64,
    /// Fixed origin codes only; no captions, window titles or user data.
    pub last_exit_origin: AtomicU64,
    pub window_close_messages: AtomicU64,
    pub main_exit_invocations: AtomicU64,
    pub tray_exit_invocations: AtomicU64,
    pub smoke_deadline_exits: AtomicU64,
    pub ui_effect_exits: AtomicU64,
    pub session_end_messages: AtomicU64,
    pub window_destroy_messages: AtomicU64,
}
impl UiDiagnostics {
    pub fn snapshot(&self) -> Value {
        serde_json::json!({"main_rebuilds":self.main_rebuilds.load(Ordering::Relaxed),
            "reentrant_defaults":self.reentrant_defaults.load(Ordering::Relaxed),
            "modals_opened":self.modals_opened.load(Ordering::Relaxed),
            "modals_closed":self.modals_closed.load(Ordering::Relaxed),
            "shutdown_requests":self.shutdown_requests.load(Ordering::Relaxed),
            "overlays_opened":self.overlays_opened.load(Ordering::Relaxed),
            "overlays_closed":self.overlays_closed.load(Ordering::Relaxed),
            "ui_effects_completed":self.ui_effects_completed.load(Ordering::Relaxed),"locale_switches":self.locale_switches.load(Ordering::Relaxed),"status_opened":self.status_opened.load(Ordering::Relaxed),"status_closed":self.status_closed.load(Ordering::Relaxed),
            "last_exit_origin":match self.last_exit_origin.load(Ordering::Relaxed) {1=>"window_close",2=>"main_exit",3=>"tray_exit",4=>"smoke_deadline",5=>"ui_effect",6=>"session_end",_=>"none"},
            "window_close_messages":self.window_close_messages.load(Ordering::Relaxed),
            "main_exit_invocations":self.main_exit_invocations.load(Ordering::Relaxed),
            "tray_exit_invocations":self.tray_exit_invocations.load(Ordering::Relaxed),
            "smoke_deadline_exits":self.smoke_deadline_exits.load(Ordering::Relaxed),
            "ui_effect_exits":self.ui_effect_exits.load(Ordering::Relaxed),
            "session_end_messages":self.session_end_messages.load(Ordering::Relaxed),
            "window_destroy_messages":self.window_destroy_messages.load(Ordering::Relaxed)})
    }
}

#[derive(Clone, Debug)]
pub enum UiAction {
    Execute {
        request_id: u64,
        command: String,
        payload: Value,
    },
    SaveDocument {
        request_id: u64,
        document: String,
        value: Value,
    },
    Shutdown {
        request_id: u64,
    },
}
impl UiAction {
    pub fn request_id(&self) -> u64 {
        match self {
            Self::Execute { request_id, .. }
            | Self::SaveDocument { request_id, .. }
            | Self::Shutdown { request_id } => *request_id,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct UiChoice {
    pub label: String,
    pub value: Value,
}
impl UiChoice {
    pub fn new(label: impl Into<String>, value: impl Into<Value>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct UiCapability {
    pub available: bool,
    pub reason: String,
}
#[derive(Clone, Debug, Default)]
pub struct UiDocument {
    pub title: String,
    pub value: Value,
    pub writable: bool,
    pub reason: String,
}
#[derive(Clone, Debug, Default)]
pub struct UiQuickAction {
    pub id: String,
    pub label: String,
    pub icon: String,
}
pub(crate) fn quick_action_glyph(name: &str) -> &'static str {
    match name {
        "" | "None" => "",
        "Play" => "▶",
        "Power" => "⏻",
        "Settings" => "⚙",
        "Star" => "★",
        "Keyboard" => "⌨",
        "Display" => "▣",
        "Lightning" | "Flash" => "ϟ",
        _ => "◇",
    }
}
#[derive(Clone, Debug, Default)]
pub struct UiResult {
    pub request_id: u64,
    pub ok: bool,
    pub message: String,
}
#[derive(Clone, Debug, Default)]
pub struct UiMetric {
    pub key: String,
    pub label: String,
    pub value: String,
    pub detail: String,
}
#[derive(Clone, Debug, Default)]
pub struct UiItem {
    pub id: String,
    pub label: String,
    pub detail: String,
    pub value: Value,
}
#[derive(Clone, Debug, Default)]
pub struct UiRange {
    pub minimum: i32,
    pub maximum: i32,
    pub step: i32,
    pub unit: String,
}
#[derive(Clone, Debug)]
pub struct UiPreferences {
    pub minimize_to_tray: bool,
    pub close_to_tray: bool,
    pub always_on_top: bool,
    pub compact: bool,
}
impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            minimize_to_tray: true,
            close_to_tray: false,
            always_on_top: false,
            compact: false,
        }
    }
}

/// `values` keys are stable semantic UI keys, never direct WMI IDs.
/// Collections: `presets`, `automation`, `macros`, `gpu.processes`, `special_keys`.
/// Editor payloads are documented beside their builders in ui/editors.rs.
#[derive(Clone, Debug)]
pub struct UiModel {
    pub revision: u64,
    /// Bump only when documents/collections/options/ranges or UI shape change.
    pub metadata_revision: u64,
    pub version: String,
    pub machine: String,
    pub notice: String,
    pub busy: bool,
    pub values: BTreeMap<String, Value>,
    pub options: BTreeMap<String, Vec<UiChoice>>,
    pub capabilities: BTreeMap<String, UiCapability>,
    pub ranges: BTreeMap<String, UiRange>,
    pub documents: BTreeMap<String, UiDocument>,
    pub collections: BTreeMap<String, Vec<UiItem>>,
    pub metrics: Vec<UiMetric>,
    pub quick_actions: Vec<UiQuickAction>,
    pub results: Vec<UiResult>,
    pub preferences: UiPreferences,
    pub shutdown_ready: bool,
}
impl Default for UiModel {
    fn default() -> Self {
        Self {
            revision: 0,
            metadata_revision: 0,
            version: env!("CARGO_PKG_VERSION").into(),
            machine: "Y7000P IRX10 · 83NN".into(),
            notice: "正在连接后台服务…".into(),
            busy: false,
            values: BTreeMap::new(),
            options: BTreeMap::new(),
            capabilities: BTreeMap::new(),
            ranges: BTreeMap::new(),
            documents: BTreeMap::new(),
            collections: BTreeMap::new(),
            metrics: Vec::new(),
            quick_actions: Vec::new(),
            results: Vec::new(),
            preferences: UiPreferences::default(),
            shutdown_ready: false,
        }
    }
}
impl UiModel {
    pub fn touch_metadata(&mut self) {
        self.metadata_revision = self.metadata_revision.wrapping_add(1);
        self.revision = self.revision.wrapping_add(1);
    }
    /// No documents, collections, option arrays or recording/event JSON are cloned.
    pub fn realtime_snapshot(&self) -> Self {
        Self {
            revision: self.revision,
            metadata_revision: self.metadata_revision,
            version: self.version.clone(),
            machine: self.machine.clone(),
            notice: self.notice.clone(),
            busy: self.busy,
            values: self
                .values
                .iter()
                .filter(|(_, value)| !value.is_array() && !value.is_object())
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            capabilities: self.capabilities.clone(),
            metrics: self.metrics.clone(),
            results: self.results.clone(),
            preferences: self.preferences.clone(),
            shutdown_ready: self.shutdown_ready,
            ..Self::default()
        }
    }
    pub fn set_capability(
        &mut self,
        key: impl Into<String>,
        available: bool,
        reason: impl Into<String>,
    ) {
        self.capabilities.insert(
            key.into(),
            UiCapability {
                available,
                reason: reason.into(),
            },
        );
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn can(&self, key: &str) -> bool {
        self.capabilities.get(key).is_some_and(|c| c.available)
    }
    pub fn unavailable_reason(&self, key: &str) -> String {
        self.capabilities
            .get(key)
            .map(|c| c.reason.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "此功能当前不可用，请查看设备支持情况和后台状态。".into())
    }
    pub fn set_value(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        self.values.insert(key.into(), value.into());
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn finish(&mut self, request_id: u64, ok: bool, message: impl Into<String>) {
        let message = message.into();
        self.notice = message.clone();
        self.results.push(UiResult {
            request_id,
            ok,
            message,
        });
        if self.results.len() > 128 {
            self.results.drain(..self.results.len() - 128);
        }
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn result(&self, request_id: u64) -> Option<&UiResult> {
        self.results
            .iter()
            .rev()
            .find(|r| r.request_id == request_id)
    }
    pub fn value(&self, key: &str) -> Value {
        self.values.get(key).cloned().unwrap_or(Value::Null)
    }
}
