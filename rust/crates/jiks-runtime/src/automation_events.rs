//! Owned event-source boundary. Native callbacks only enqueue metadata. The
//! coordinator owns state reduction, reader requests and automation execution.
//! `new` and document inspection are inert; only explicit `start` registers OS
//! listeners. No test starts the native listener or captures real user input.
use anyhow::{Result, ensure};
use jiks_core::automation::{
    AdapterState, AutomationDocument, AutomationEvent, AutomationState, NativeEvent, TimeOfDay,
    TriggerDto, TriggerKind,
};
use std::collections::{BTreeMap, BTreeSet};

pub mod native;
pub use native::NativeEventSource;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceNeeds {
    pub session: bool,
    pub power: bool,
    pub lid: bool,
    pub display: bool,
    pub devices: bool,
    pub clock: bool,
    pub idle: bool,
    pub processes: bool,
    pub games: bool,
    pub wifi: bool,
    pub firmware: bool,
}
impl SourceNeeds {
    pub fn from_document(document: &AutomationDocument) -> Self {
        let mut needs = Self::default();
        if document.enabled {
            for pipeline in &document.pipelines {
                if pipeline
                    .to_json_node()
                    .get_ci("JiksEnabled")
                    .and_then(|v| v.as_bool())
                    == Some(false)
                {
                    continue;
                }
                if let Some(trigger) = &pipeline.trigger {
                    needs.add(trigger);
                }
            }
        }
        needs
    }
    fn add(&mut self, trigger: &TriggerDto) {
        match trigger.kind {
            TriggerKind::SessionLock | TriggerKind::SessionUnlock => self.session = true,
            TriggerKind::AcConnected
            | TriggerKind::AcDisconnected
            | TriggerKind::LowWattageAcConnected
            | TriggerKind::BatteryPercentage
            | TriggerKind::OnResume => self.power = true,
            TriggerKind::LidOpened | TriggerKind::LidClosed => self.lid = true,
            TriggerKind::DisplayOn
            | TriggerKind::DisplayOff
            | TriggerKind::HdrOn
            | TriggerKind::HdrOff
            | TriggerKind::ExternalDisplayConnected
            | TriggerKind::ExternalDisplayDisconnected => self.display = true,
            TriggerKind::DeviceConnected | TriggerKind::DeviceDisconnected => self.devices = true,
            TriggerKind::Time | TriggerKind::TimeRange | TriggerKind::Periodic => self.clock = true,
            TriggerKind::UserInactivity => self.idle = true,
            TriggerKind::ProcessesAreRunning | TriggerKind::ProcessesStopRunning => {
                self.processes = true
            }
            TriggerKind::GamesAreRunning | TriggerKind::GamesStop => {
                self.processes = true;
                self.games = true;
            }
            TriggerKind::WifiConnected | TriggerKind::WifiDisconnected => self.wifi = true,
            TriggerKind::PowerMode
            | TriggerKind::HybridMode
            | TriggerKind::GodModePresetChanged
            | TriggerKind::ItsMode => self.firmware = true,
            _ => {}
        }
        for child in &trigger.children {
            self.add(child);
        }
    }
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceOptions {
    pub needs: SourceNeeds,
    pub macro_enabled: bool,
    /// Configured, nonempty Keyboard sequences only. NumPad0..9, as in C#.
    pub macro_keys: BTreeSet<u32>,
    pub game_detection: GameDetectionOptions,
    pub normal_notifications: bool,
    pub lock_notifications: bool,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GameDetectionOptions {
    pub config_store: bool,
    pub effective_power_mode: bool,
    pub discrete_gpu: bool,
}
impl SourceOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.macro_keys.iter().all(|k| (0x60..=0x69).contains(k)),
            "Macro shortcuts must be NumPad0..9"
        );
        Ok(())
    }
    pub fn has_hook(&self) -> bool {
        self.macro_enabled && !self.macro_keys.is_empty() || self.lock_notifications
    }
    pub fn is_empty(&self) -> bool {
        self.needs.is_empty() && !self.has_hook() && !self.normal_notifications
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshReason {
    Power {
        resume: bool,
    },
    Display,
    Devices,
    Firmware,
    /// An actual firmware notification, rather than the automation fallback
    /// polling tick. Additional normal-notification getters run only here.
    NotificationState,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessMetadata {
    pub pid: u32,
    pub name: String,
    pub executable_path: Option<String>,
}
#[derive(Debug, Clone, PartialEq)]
pub enum SourceMessage {
    Started,
    Stopped,
    Refresh(RefreshReason),
    Lid(bool),
    Display(bool),
    SessionLocked(bool),
    BatteryPercentage(i32),
    Wifi {
        connected: bool,
        ssid: Option<String>,
    },
    Device {
        connected: bool,
        instance_id: String,
    },
    Clock {
        utc_time: TimeOfDay,
        utc_day: u8,
        local_day_seconds: f64,
    },
    Inactivity {
        milliseconds: u64,
    },
    Processes(Vec<ProcessMetadata>),
    GameConfigPaths(BTreeSet<String>),
    /// Foreground PID is sampled only alongside a real Windows effective Game
    /// Mode notification, never used as evidence by itself.
    EffectiveGameMode {
        active: bool,
        foreground_pid: Option<u32>,
    },
    MacroHotkey {
        source: String,
        key: u64,
    },
    LockState {
        caps: bool,
        num: bool,
    },
    Fault {
        source: &'static str,
        detail: String,
    },
}

/// Game evidence is supplied from completed metadata reads. The absence of an
/// unavailable detector is not evidence that no game is running.
#[derive(Debug, Clone, Default)]
pub struct GameRules {
    pub included_names: BTreeSet<String>,
    pub excluded_names: BTreeSet<String>,
    pub known_game_paths: BTreeSet<String>,
    pub actual_gpu_pids: BTreeSet<u32>,
    pub actual_effective_game_pid: Option<u32>,
    pub effective_pinned_pids: BTreeSet<u32>,
    pub required: GameDetectionOptions,
    pub config_store_available: bool,
    pub gpu_available: bool,
    pub effective_mode_available: bool,
}
impl GameRules {
    pub fn from_application(root: &jiks_core::settings::JsonNode) -> Result<Self> {
        let names = |key: &str| -> Result<BTreeSet<String>> {
            let Some(node) = root.get_ci(key) else {
                return Ok(BTreeSet::new());
            };
            let values = node
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("{key} must be array"))?;
            values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow::anyhow!("{key} must contain strings"))
                })
                .collect()
        };
        let detection = root.get_ci("GameDetection");
        let enabled = |key: &str| -> Result<bool> {
            detection.and_then(|n| n.get_ci(key)).map_or(Ok(true), |n| {
                n.as_bool()
                    .ok_or_else(|| anyhow::anyhow!("{key} must be bool"))
            })
        };
        Ok(Self {
            included_names: names("IncludedProcesses")?,
            excluded_names: names("ExcludedProcesses")?,
            required: GameDetectionOptions {
                config_store: enabled("UseGameConfigStore")?,
                effective_power_mode: enabled("UseEffectiveGameMode")?,
                discrete_gpu: enabled("UseDiscreteGPU")?,
            },
            ..Self::default()
        })
    }
    pub fn complete(&self) -> bool {
        (!self.required.config_store || self.config_store_available)
            && (!self.required.discrete_gpu || self.gpu_available)
            && (!self.required.effective_power_mode || self.effective_mode_available)
    }
    pub fn observe(&mut self, message: &SourceMessage) {
        match message {
            SourceMessage::GameConfigPaths(paths) => {
                self.known_game_paths = paths.clone();
                self.config_store_available = true;
            }
            SourceMessage::EffectiveGameMode {
                active,
                foreground_pid,
            } => {
                self.effective_mode_available = true;
                if *active {
                    if let Some(pid) = foreground_pid {
                        self.effective_pinned_pids.insert(*pid);
                    }
                }
                // As in C#, Game Mode ending does not evict a still-running
                // pinned game. Actual process disappearance releases it.
            }
            SourceMessage::Processes(processes) => {
                self.effective_pinned_pids
                    .retain(|pid| processes.iter().any(|p| p.pid == *pid));
                if self
                    .actual_effective_game_pid
                    .is_some_and(|pid| !processes.iter().any(|p| p.pid == pid))
                {
                    self.actual_effective_game_pid = None;
                }
            }
            SourceMessage::Fault {
                source: "game-config-store",
                ..
            } => self.config_store_available = false,
            SourceMessage::Fault {
                source: "gpu-processes",
                ..
            } => self.gpu_available = false,
            _ => {}
        }
    }
    pub fn is_game(&self, process: &ProcessMetadata) -> bool {
        let name = &process.name;
        let included = self
            .included_names
            .iter()
            .any(|n| jiks_core::automation::ordinal_ignore_case(n, name));
        if included {
            return true;
        }
        if self
            .excluded_names
            .iter()
            .any(|n| jiks_core::automation::ordinal_ignore_case(n, name))
        {
            return false;
        }
        if DEFAULT_GAME_EXCLUSIONS
            .iter()
            .any(|n| jiks_core::automation::ordinal_ignore_case(n, name))
        {
            return false;
        }
        (self.required.discrete_gpu && self.actual_gpu_pids.contains(&process.pid))
            || (self.required.effective_power_mode
                && (self.effective_pinned_pids.contains(&process.pid)
                    || self.actual_effective_game_pid == Some(process.pid)))
            || self.required.config_store
                && process.executable_path.as_ref().is_some_and(|path| {
                    self.known_game_paths
                        .iter()
                        .any(|p| jiks_core::automation::ordinal_ignore_case(p, path))
                })
    }
}
const DEFAULT_GAME_EXCLUSIONS: &[&str] = &[
    "explorer",
    "Lenovo Legion Toolkit",
    "Jiks",
    "SearchUI",
    "LockApp",
    "TextInputHost",
    "ShellExperienceHost",
    "StartMenuExperienceHost",
    "SearchHost",
    "dwm",
    "csrss",
    "WmiApSrv",
    "HWiNFO64",
    "HWiNFO32",
    "nvidia-smi",
    "steamwebhelper",
    "EpicWebHelper",
    "GalaxyCommunicationService",
    "GalaxyClientHelper",
    "EABackgroundService",
    "Link2EA",
    "RiotClientCrashHandler",
    "CrashReportClient",
    "UnityCrashHandler64",
    "UnityCrashHandler32",
    "UE4-CrashTracker",
    "crs-handler",
    "cefsharp.browsersubprocess",
    "conhost",
    "nvngx_update",
    "GameGuard",
    "GameGuard.des",
    "GameMon",
    "GameMon64",
    "EasyAntiCheat",
    "EasyAntiCheat_EOS",
    "BEService",
    "BEService_x64",
    "vgc",
    "vgtray",
    "EAAntiCheat.GameService",
    "EAAntiCheat.GameServiceLauncher",
    "ACE-Base",
    "UbisoftConnect",
    "UbisoftGameLauncher",
    "Battle.net Helper",
    "Agent",
];

#[derive(Default)]
pub struct EventReducer {
    processes: Option<BTreeMap<u32, ProcessMetadata>>,
    last_idle_bucket: Option<u64>,
    wifi_seen: bool,
}
/// Only completed, successful reader results belong here. Firmware modes are
/// legacy domain values (Quiet=0, Extreme=223, GodMode=254), not raw WMI bytes.
#[derive(Debug, Clone, Default)]
pub struct HardwareObservation {
    pub adapter: Option<AdapterState>,
    pub power_mode: Option<i32>,
    pub hybrid_mode: Option<i32>,
    pub its_mode: Option<i32>,
    pub hdr_on: Option<bool>,
    pub external_display_count: Option<usize>,
    pub preset_id: Option<String>,
}
impl EventReducer {
    pub fn reduce_hardware(
        &mut self,
        observed: &HardwareObservation,
        state: &mut AutomationState,
        resume: bool,
    ) -> Result<Vec<AutomationEvent>> {
        ensure!(
            observed
                .power_mode
                .is_none_or(|v| [0, 1, 2, 223, 254].contains(&v)),
            "Firmware power mode must be converted to legacy enum"
        );
        ensure!(
            observed.hybrid_mode.is_none_or(|v| (0..=4).contains(&v)),
            "Invalid hybrid mode"
        );
        ensure!(
            observed.its_mode.is_none_or(|v| (0..=4).contains(&v)),
            "Invalid ITS mode"
        );
        if let Some(id) = &observed.preset_id {
            jiks_core::automation::normalize_guid(id)?;
        }
        let mut events = Vec::new();
        let changed = observed
            .adapter
            .is_some_and(|new| state.adapter.replace(new).is_some_and(|old| old != new));
        if changed || resume {
            events.push(AutomationEvent::PowerState {
                status_changed: changed,
                adapter_changed: changed,
                resume,
            });
        }
        for (new, old, make) in [
            (
                observed.power_mode,
                &mut state.power_mode,
                AutomationEvent::PowerMode as fn(i32) -> AutomationEvent,
            ),
            (
                observed.hybrid_mode,
                &mut state.hybrid_mode,
                AutomationEvent::HybridMode,
            ),
            (
                observed.its_mode,
                &mut state.its_mode,
                AutomationEvent::ItsMode,
            ),
        ] {
            if let Some(new) = new {
                if old.replace(new).is_some_and(|old| old != new) {
                    events.push(make(new));
                }
            }
        }
        if let Some(new) = observed.hdr_on {
            if state.hdr_on.replace(new).is_some_and(|old| old != new) {
                events.push(AutomationEvent::Hdr(Some(new)));
            }
        }
        if let Some(new) = observed.external_display_count {
            if let Some(old) = state.external_display_count.replace(new) {
                if old != new {
                    events.push(AutomationEvent::Native {
                        kind: if new > old {
                            NativeEvent::ExternalMonitorConnected
                        } else {
                            NativeEvent::ExternalMonitorDisconnected
                        },
                        instance_id: None,
                    });
                }
            }
        }
        if let Some(new) = &observed.preset_id {
            if state
                .preset_id
                .replace(new.clone())
                .is_some_and(|old| old != *new)
            {
                events.push(AutomationEvent::PresetChanged(new.clone()));
            }
        }
        Ok(events)
    }
    /// Metadata snapshots initialize unknown state without inventing transitions.
    /// Native WLAN connection notifications are actual transitions, not snapshots.
    pub fn reduce(
        &mut self,
        message: &SourceMessage,
        state: &mut AutomationState,
        games: Option<&GameRules>,
    ) -> Vec<AutomationEvent> {
        let mut events = Vec::new();
        match message {
            SourceMessage::Lid(open) => {
                if state
                    .lid_open
                    .replace(*open)
                    .is_some_and(|old| old != *open)
                {
                    events.push(AutomationEvent::Native {
                        kind: if *open {
                            NativeEvent::LidOpened
                        } else {
                            NativeEvent::LidClosed
                        },
                        instance_id: None,
                    });
                }
            }
            SourceMessage::Display(on) => {
                if state.display_on.replace(*on).is_some_and(|old| old != *on) {
                    events.push(AutomationEvent::Native {
                        kind: if *on {
                            NativeEvent::MonitorOn
                        } else {
                            NativeEvent::MonitorOff
                        },
                        instance_id: None,
                    });
                }
            }
            SourceMessage::SessionLocked(locked) => {
                // WTS sends explicit transitions, so an unknown initial state
                // must not suppress an actual Lock/Unlock event.
                if state.session_locked != Some(*locked) {
                    state.session_locked = Some(*locked);
                    events.push(AutomationEvent::SessionLocked(*locked));
                }
            }
            SourceMessage::BatteryPercentage(value) if (0..=100).contains(value) => {
                if state
                    .battery_percentage
                    .replace(*value)
                    .is_some_and(|old| old != *value)
                {
                    events.push(AutomationEvent::BatteryPercentage(*value));
                }
            }
            SourceMessage::Wifi { connected, ssid } => {
                if !self.wifi_seen || state.wifi_ssid != *ssid || !*connected {
                    self.wifi_seen = true;
                    state.wifi_ssid = if *connected { ssid.clone() } else { None };
                    events.push(AutomationEvent::Wifi {
                        connected: *connected,
                        ssid: ssid.clone(),
                    });
                }
            }
            SourceMessage::Device {
                connected,
                instance_id,
            } => {
                if *connected {
                    if !state.connected_device_ids.contains(instance_id) {
                        state.connected_device_ids.push(instance_id.clone());
                    }
                } else {
                    state.connected_device_ids.retain(|id| id != instance_id);
                }
                events.push(AutomationEvent::Native {
                    kind: if *connected {
                        NativeEvent::DeviceConnected
                    } else {
                        NativeEvent::DeviceDisconnected
                    },
                    instance_id: Some(instance_id.clone()),
                });
            }
            SourceMessage::Clock {
                utc_time,
                utc_day,
                local_day_seconds,
            } => {
                state.utc_time = *utc_time;
                state.utc_day = *utc_day;
                state.local_day_seconds = *local_day_seconds;
                events.push(AutomationEvent::Time {
                    time: *utc_time,
                    day: *utc_day,
                });
            }
            SourceMessage::Inactivity { milliseconds } => {
                let bucket = milliseconds / 10_000;
                state.inactivity_ticks =
                    i64::try_from(bucket.saturating_mul(100_000_000)).unwrap_or(i64::MAX);
                if self.last_idle_bucket.is_some_and(|old| old != bucket) {
                    events.push(AutomationEvent::UserInactivity {
                        ticks: state.inactivity_ticks,
                    });
                }
                self.last_idle_bucket = Some(bucket);
            }
            SourceMessage::Processes(processes) => {
                let mut current: BTreeMap<_, _> =
                    processes.iter().map(|p| (p.pid, p.clone())).collect();
                if let Some(previous) = &self.processes {
                    for (pid, new) in &mut current {
                        if let Some(old) = previous.get(pid) {
                            if old.name == new.name && new.executable_path.is_none() {
                                new.executable_path = old.executable_path.clone();
                            }
                        }
                    }
                }
                state.process_names = current.values().map(|p| p.name.clone()).collect();
                if let Some(previous) = &self.processes {
                    for (id, old) in previous {
                        if current.get(id) != Some(old) {
                            events.push(AutomationEvent::Process {
                                started: false,
                                name: old.name.clone(),
                                executable_path: old.executable_path.clone(),
                            });
                        }
                    }
                    for (id, new) in &current {
                        if previous.get(id) != Some(new) {
                            events.push(AutomationEvent::Process {
                                started: true,
                                name: new.name.clone(),
                                executable_path: new.executable_path.clone(),
                            });
                        }
                    }
                }
                if let Some(rules) = games {
                    let running = current.values().any(|p| rules.is_game(p));
                    if self.processes.is_some()
                        && state.games_running != running
                        && (running || rules.complete())
                    {
                        events.push(AutomationEvent::Game(running));
                    }
                    if running || rules.complete() {
                        state.games_running = running;
                    }
                }
                self.processes = Some(current);
            }
            _ => {}
        }
        events
    }
}

/// Pure shortcut filter: never stores general typing. Only configured NumPad
/// keys are tracked. Down autorepeat triggers once; a swallowed down's matching
/// up stays swallowed even if the global enabled/recording state changes.
#[derive(Default)]
pub struct ShortcutFilter {
    swallowed: BTreeSet<u32>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutDecision {
    Pass,
    Swallow,
    Fire,
}
impl ShortcutFilter {
    pub fn inspect(
        &self,
        key: u32,
        down: bool,
        flags: u32,
        enabled: bool,
        keys: &BTreeSet<u32>,
        suspended: bool,
    ) -> ShortcutDecision {
        if flags & 0x12 != 0 {
            return ShortcutDecision::Pass;
        }
        if self.swallowed.contains(&key) {
            return ShortcutDecision::Swallow;
        }
        if !down || flags != 0 || !enabled || suspended || !keys.contains(&key) {
            return ShortcutDecision::Pass;
        }
        ShortcutDecision::Fire
    }
    pub fn accepted(&mut self, key: u32, down: bool) {
        if down {
            self.swallowed.insert(key);
        } else {
            self.swallowed.remove(&key);
        }
    }
}

pub fn device_instance_id(path: &str) -> Option<String> {
    let parts: Vec<_> = path.split('#').collect();
    if parts.len() < 3 {
        return None;
    }
    Some(
        format!(
            "{}\\{}\\{}",
            parts[0].trim_start_matches(['\\', '?']),
            parts[1],
            parts[2]
        )
        .to_uppercase(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiks_core::{
        automation::{PipelineDto, StepDto, StepKind},
        settings::JsonNode,
    };
    fn process(pid: u32, name: &str) -> ProcessMetadata {
        ProcessMetadata {
            pid,
            name: name.into(),
            executable_path: Some(format!("D:\\fixtures\\{name}.exe")),
        }
    }
    #[test]
    fn default_construction_and_disabled_document_are_inert() {
        let mut source = NativeEventSource::new();
        assert!(!source.is_running());
        source.start(SourceOptions::default()).unwrap();
        assert!(!source.is_running());
        assert!(source.try_recv().is_none());
        assert_eq!(
            SourceNeeds::from_document(
                &AutomationDocument::parse("{\"IsEnabled\":false}", || "unused".into()).unwrap()
            ),
            SourceNeeds::default()
        );
    }
    #[test]
    fn recursive_subscriptions_enable_only_relevant_sources() {
        let child = TriggerDto::new(TriggerKind::UserInactivity, JsonNode::object()).unwrap();
        let trigger = TriggerDto::new(
            TriggerKind::And,
            JsonNode::parse(&format!("{{\"Triggers\":[{}]}}", child.raw().to_json())).unwrap(),
        )
        .unwrap();
        let pipeline = PipelineDto::new(
            "11111111-1111-1111-1111-111111111111",
            Some(trigger),
            vec![StepDto::new(StepKind::ShowApp, JsonNode::object()).unwrap()],
        )
        .unwrap();
        let document = AutomationDocument::parse(
            &format!(
                "{{\"IsEnabled\":true,\"Pipelines\":[{}]}}",
                pipeline.to_json_node().to_json()
            ),
            || "unused".into(),
        )
        .unwrap();
        let needs = SourceNeeds::from_document(&document);
        assert!(needs.idle);
        assert!(!needs.processes && !needs.wifi && !needs.power);
    }
    #[test]
    fn numpad_filter_ignores_typing_injection_repeat_and_recording_without_io() {
        let keys = BTreeSet::from([0x60]);
        let mut f = ShortcutFilter::default();
        assert_eq!(
            f.inspect(65, true, 0, true, &keys, false),
            ShortcutDecision::Pass
        );
        assert_eq!(
            f.inspect(0x60, true, 0x10, true, &keys, false),
            ShortcutDecision::Pass
        );
        assert_eq!(
            f.inspect(0x60, true, 0, false, &keys, false),
            ShortcutDecision::Pass
        );
        assert_eq!(
            f.inspect(0x60, true, 0, true, &keys, true),
            ShortcutDecision::Pass
        );
        assert_eq!(
            f.inspect(0x60, true, 0, true, &keys, false),
            ShortcutDecision::Fire
        );
        f.accepted(0x60, true);
        assert_eq!(
            f.inspect(0x60, true, 0, true, &keys, false),
            ShortcutDecision::Swallow
        );
        assert_eq!(
            f.inspect(0x60, false, 0x80, false, &keys, true),
            ShortcutDecision::Swallow
        );
        f.accepted(0x60, false);
        assert_eq!(
            f.inspect(0x60, false, 0x80, true, &keys, false),
            ShortcutDecision::Pass
        );
    }
    #[test]
    fn process_snapshots_are_baseline_then_actual_diff_and_failed_reads_do_not_clear() {
        let mut r = EventReducer::default();
        let mut state = AutomationState::default();
        assert!(
            r.reduce(
                &SourceMessage::Processes(vec![process(1, "game")]),
                &mut state,
                None
            )
            .is_empty()
        );
        assert!(
            r.reduce(
                &SourceMessage::Fault {
                    source: "process",
                    detail: "denied".into()
                },
                &mut state,
                None
            )
            .is_empty()
        );
        assert_eq!(state.process_names, vec!["game"]);
        let events = r.reduce(
            &SourceMessage::Processes(vec![process(2, "other")]),
            &mut state,
            None,
        );
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0],AutomationEvent::Process{started:false,name,..} if name=="game")
        );
        assert!(
            matches!(&events[1],AutomationEvent::Process{started:true,name,..} if name=="other")
        );
    }
    #[test]
    fn initial_power_metadata_is_not_a_transition_and_unknown_is_not_false() {
        let mut r = EventReducer::default();
        let mut state = AutomationState::default();
        assert!(
            r.reduce(&SourceMessage::Lid(false), &mut state, None)
                .is_empty()
        );
        assert_eq!(state.lid_open, Some(false));
        let events = r.reduce(&SourceMessage::Lid(true), &mut state, None);
        assert!(matches!(
            events[0],
            AutomationEvent::Native {
                kind: NativeEvent::LidOpened,
                ..
            }
        ));
        assert!(
            r.reduce(&SourceMessage::BatteryPercentage(255), &mut state, None)
                .is_empty()
        );
        assert_eq!(state.battery_percentage, None);
    }
    #[test]
    fn game_rules_preserve_explicit_inclusion_and_do_not_guess_gpu_usage() {
        let mut rules = GameRules::default();
        rules.included_names.insert("game".into());
        rules.excluded_names.insert("game".into());
        assert!(rules.is_game(&process(1, "game")));
        assert!(!rules.is_game(&process(2, "editor")));
        rules.actual_gpu_pids.insert(2);
        rules.required.discrete_gpu = true;
        rules.gpu_available = true;
        assert!(rules.is_game(&process(2, "editor")));
        rules.excluded_names.insert("editor".into());
        assert!(!rules.is_game(&process(2, "editor")));
        let mut state = AutomationState::default();
        let mut reducer = EventReducer::default();
        reducer.reduce(&SourceMessage::Processes(vec![]), &mut state, Some(&rules));
        let events = reducer.reduce(
            &SourceMessage::Processes(vec![process(1, "game")]),
            &mut state,
            Some(&rules),
        );
        assert!(events.contains(&AutomationEvent::Game(true)));
    }
    #[test]
    fn idle_uses_legacy_ten_second_buckets_and_reset_and_device_ids_are_canonical() {
        let mut r = EventReducer::default();
        let mut state = AutomationState::default();
        assert!(
            r.reduce(
                &SourceMessage::Inactivity { milliseconds: 0 },
                &mut state,
                None
            )
            .is_empty()
        );
        assert!(
            r.reduce(
                &SourceMessage::Inactivity { milliseconds: 9999 },
                &mut state,
                None
            )
            .is_empty()
        );
        assert_eq!(
            r.reduce(
                &SourceMessage::Inactivity {
                    milliseconds: 10000
                },
                &mut state,
                None
            ),
            vec![AutomationEvent::UserInactivity { ticks: 100_000_000 }]
        );
        assert_eq!(
            r.reduce(
                &SourceMessage::Inactivity { milliseconds: 0 },
                &mut state,
                None
            ),
            vec![AutomationEvent::UserInactivity { ticks: 0 }]
        );
        assert_eq!(
            device_instance_id("\\\\?\\HID#VID_FAKE#SYNTHETIC#{0000}").as_deref(),
            Some("HID\\VID_FAKE\\SYNTHETIC")
        );
    }
    #[test]
    fn hardware_diffs_require_successful_reads_and_validate_whole_snapshot_first() {
        let mut reducer = EventReducer::default();
        let mut state = AutomationState::default();
        let initial = HardwareObservation {
            adapter: Some(AdapterState::Connected),
            power_mode: Some(2),
            hdr_on: Some(false),
            external_display_count: Some(0),
            ..Default::default()
        };
        assert!(
            reducer
                .reduce_hardware(&initial, &mut state, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            reducer
                .reduce_hardware(&HardwareObservation::default(), &mut state, false)
                .unwrap()
                .is_empty()
        );
        assert_eq!(state.power_mode, Some(2));
        let invalid = HardwareObservation {
            power_mode: Some(224),
            hdr_on: Some(true),
            ..Default::default()
        };
        assert!(
            reducer
                .reduce_hardware(&invalid, &mut state, false)
                .is_err()
        );
        assert_eq!(state.hdr_on, Some(false));
        let next = HardwareObservation {
            adapter: Some(AdapterState::Disconnected),
            power_mode: Some(223),
            hdr_on: Some(true),
            external_display_count: Some(1),
            ..Default::default()
        };
        let events = reducer.reduce_hardware(&next, &mut state, false).unwrap();
        assert_eq!(events.len(), 4);
        assert!(events.contains(&AutomationEvent::PowerMode(223)));
        assert!(events.contains(&AutomationEvent::Native {
            kind: NativeEvent::ExternalMonitorConnected,
            instance_id: None
        }));
    }
    #[test]
    fn unavailable_game_detector_never_manufactures_stop_and_effective_pin_lives_until_exit() {
        let root=jiks_core::settings::JsonNode::parse("{\"IncludedProcesses\":[\"game\"],\"GameDetection\":{\"UseDiscreteGPU\":false,\"UseGameConfigStore\":false,\"UseEffectiveGameMode\":true}}").unwrap();
        let mut rules = GameRules::from_application(&root).unwrap();
        let mut state = AutomationState::default();
        let mut reducer = EventReducer::default();
        reducer.reduce(&SourceMessage::Processes(vec![]), &mut state, Some(&rules));
        let game = SourceMessage::Processes(vec![process(1, "game")]);
        rules.observe(&game);
        reducer.reduce(&game, &mut state, Some(&rules));
        assert!(state.games_running);
        let empty = SourceMessage::Processes(vec![]);
        rules.observe(&empty);
        assert!(
            reducer
                .reduce(&empty, &mut state, Some(&rules))
                .iter()
                .all(|e| !matches!(e, AutomationEvent::Game(false)))
        );
        assert!(state.games_running);
        rules.observe(&SourceMessage::EffectiveGameMode {
            active: true,
            foreground_pid: Some(2),
        });
        rules.observe(&SourceMessage::EffectiveGameMode {
            active: false,
            foreground_pid: None,
        });
        let game = SourceMessage::Processes(vec![process(2, "actual-game-mode-target")]);
        rules.observe(&game);
        reducer.reduce(&game, &mut state, Some(&rules));
        assert!(state.games_running);
        rules.observe(&empty);
        let events = reducer.reduce(&empty, &mut state, Some(&rules));
        assert!(events.contains(&AutomationEvent::Game(false)));
    }
    #[test]
    fn transient_process_path_failure_is_not_an_exit_or_restart() {
        let mut reducer = EventReducer::default();
        let mut state = AutomationState::default();
        reducer.reduce(
            &SourceMessage::Processes(vec![process(1, "game")]),
            &mut state,
            None,
        );
        let mut still_running = process(1, "game");
        still_running.executable_path = None;
        assert!(
            reducer
                .reduce(
                    &SourceMessage::Processes(vec![still_running]),
                    &mut state,
                    None
                )
                .is_empty()
        );
    }
}
