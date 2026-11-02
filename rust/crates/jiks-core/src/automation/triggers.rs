use super::dto::{
    TriggerDto, TriggerKind, integer, normalize_guid, optional_bool, optional_string, string_list,
};
use crate::settings::{JsonNode, parse_timespan_ticks};
use crate::{CoreError, CoreResult};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeOfDay {
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}
impl TimeOfDay {
    pub fn new(hour: u8, minute: u8, second: u8) -> CoreResult<Self> {
        if hour < 24 && minute < 60 && second < 60 {
            Ok(Self {
                hour,
                minute,
                second,
            })
        } else {
            Err(CoreError::invalid("time of day"))
        }
    }
    pub const fn seconds(self) -> u32 {
        self.hour as u32 * 3600 + self.minute as u32 * 60 + self.second as u32
    }
    pub fn between(self, start: Self, end: Self) -> bool {
        if start.seconds() <= end.seconds() {
            self.seconds() >= start.seconds() && self.seconds() <= end.seconds()
        } else {
            self.seconds() >= start.seconds() || self.seconds() <= end.seconds()
        }
    }
}
fn time(node: Option<&JsonNode>) -> CoreResult<Option<TimeOfDay>> {
    let Some(node) = node.filter(|v| **v != JsonNode::Null) else {
        return Ok(None);
    };
    let part =
        |key| u8::try_from(integer(node, key)?).map_err(|_| CoreError::invalid("time component"));
    Ok(Some(TimeOfDay::new(
        part("Hour")?,
        part("Minute")?,
        part("Second")?,
    )?))
}
pub fn weekday_number(node: &JsonNode) -> CoreResult<u8> {
    let value = match node {
        JsonNode::Number(_) => node.as_i64(),
        JsonNode::String(name) => [
            "Sunday",
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
        ]
        .iter()
        .position(|s| s.eq_ignore_ascii_case(name))
        .map(|v| v as i64),
        _ => None,
    };
    value
        .and_then(|v| u8::try_from(v).ok())
        .filter(|v| *v <= 6)
        .ok_or_else(|| CoreError::invalid(".NET weekday Sunday=0..Saturday=6"))
}
fn days(node: &JsonNode) -> CoreResult<Vec<u8>> {
    match node.get_ci("Days") {
        None | Some(JsonNode::Null) => Ok(Vec::new()),
        Some(JsonNode::Array(items)) => items.iter().map(weekday_number).collect(),
        _ => Err(CoreError::invalid("Days array")),
    }
}
fn timespan(node: &JsonNode, key: &str) -> CoreResult<i64> {
    match node.get_ci(key) {
        None => Ok(0),
        Some(JsonNode::String(value)) => parse_timespan_ticks(value),
        _ => Err(CoreError::invalid(format!("{key} TimeSpan string"))),
    }
}
fn enumeration(node: &JsonNode, key: &str, names: &[(&str, i32)]) -> CoreResult<i32> {
    match node.get_ci(key) {
        None => Ok(0),
        Some(JsonNode::Number(value)) => value
            .parse()
            .map_err(|_| CoreError::invalid(format!("{key} enum integer"))),
        Some(JsonNode::String(value)) => names
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(value))
            .map(|(_, v)| *v)
            .ok_or_else(|| CoreError::Unsupported(format!("unknown {key} enum {value}"))),
        _ => Err(CoreError::invalid(format!("{key} enum"))),
    }
}
const POWER: &[(&str, i32)] = &[
    ("Quiet", 0),
    ("Balance", 1),
    ("Performance", 2),
    ("Extreme", 223),
    ("GodMode", 254),
];
const ITS: &[(&str, i32)] = &[
    ("None", 0),
    ("ItsAuto", 1),
    ("MmcCool", 2),
    ("MmcPerformance", 3),
    ("MmcGeek", 4),
];
const HYBRID: &[(&str, i32)] = &[
    ("On", 0),
    ("OnIGPUOnly", 1),
    ("OnAuto", 2),
    ("UMA", 3),
    ("Off", 4),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterState {
    Disconnected,
    Connected,
    ConnectedLowWattage,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeEvent {
    LidOpened,
    LidClosed,
    MonitorOn,
    MonitorOff,
    ExternalMonitorConnected,
    ExternalMonitorDisconnected,
    DeviceConnected,
    DeviceDisconnected,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutomationEvent {
    Startup,
    Hdr(Option<bool>),
    Native {
        kind: NativeEvent,
        instance_id: Option<String>,
    },
    PowerState {
        status_changed: bool,
        adapter_changed: bool,
        resume: bool,
    },
    PowerMode(i32),
    ItsMode(i32),
    HybridMode(i32),
    PresetChanged(String),
    Game(bool),
    Process {
        started: bool,
        name: String,
        executable_path: Option<String>,
    },
    SessionLocked(bool),
    Time {
        time: TimeOfDay,
        day: u8,
    },
    UserInactivity {
        ticks: i64,
    },
    Wifi {
        connected: bool,
        ssid: Option<String>,
    },
    BatteryPercentage(i32),
}
#[derive(Debug, Clone)]
pub struct AutomationState {
    pub adapter: Option<AdapterState>,
    pub lid_open: Option<bool>,
    pub display_on: Option<bool>,
    pub external_display_count: Option<usize>,
    pub hdr_on: Option<bool>,
    pub session_locked: Option<bool>,
    pub games_running: bool,
    pub process_names: Vec<String>,
    pub connected_device_ids: Vec<String>,
    pub wifi_ssid: Option<String>,
    pub inactivity_ticks: i64,
    pub battery_percentage: Option<i32>,
    pub power_mode: Option<i32>,
    pub its_mode: Option<i32>,
    pub hybrid_mode: Option<i32>,
    pub preset_id: Option<String>,
    pub utc_time: TimeOfDay,
    pub utc_day: u8,
    pub local_day_seconds: f64,
    pub sunrise: Option<TimeOfDay>,
    pub sunset: Option<TimeOfDay>,
}
impl Default for AutomationState {
    fn default() -> Self {
        Self {
            adapter: None,
            lid_open: None,
            display_on: None,
            external_display_count: None,
            hdr_on: None,
            session_locked: None,
            games_running: false,
            process_names: Vec::new(),
            connected_device_ids: Vec::new(),
            wifi_ssid: None,
            inactivity_ticks: 0,
            battery_percentage: None,
            power_mode: None,
            its_mode: None,
            hybrid_mode: None,
            preset_id: None,
            utc_time: TimeOfDay {
                hour: 0,
                minute: 0,
                second: 0,
            },
            utc_day: 0,
            local_day_seconds: 0.,
            sunrise: None,
            sunset: None,
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutomationEnvironment {
    pub startup: bool,
    pub variables: BTreeMap<String, Option<String>>,
}
impl AutomationEnvironment {
    pub fn set_bool(&mut self, key: &str, value: bool) {
        self.variables.insert(
            key.into(),
            Some(if value { "TRUE" } else { "FALSE" }.into()),
        );
    }
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        self.variables.insert(key.into(), Some(value.into()));
    }
}

impl TriggerDto {
    pub fn validate(&self) -> CoreResult<()> {
        self.validate_known()?;
        for child in &self.children {
            child.validate()?;
        }
        match self.kind {
            TriggerKind::PowerMode => {
                enumeration(self.raw(), "PowerModeState", POWER)?;
            }
            TriggerKind::ItsMode => {
                enumeration(self.raw(), "ITSModeState", ITS)?;
            }
            TriggerKind::HybridMode => {
                enumeration(self.raw(), "HybridModeState", HYBRID)?;
            }
            TriggerKind::GodModePresetChanged => {
                normalize_guid(
                    &optional_string(self.raw(), "PresetId")?
                        .ok_or_else(|| CoreError::invalid("PresetId required"))?,
                )?;
            }
            TriggerKind::WifiConnected => {
                if !matches!(self.field("Ssids"), Some(JsonNode::Array(_))) {
                    return Err(CoreError::invalid("Ssids non-null array required"));
                }
                string_list(self.raw(), "Ssids")?;
            }
            TriggerKind::DeviceConnected | TriggerKind::DeviceDisconnected => {
                string_list(self.raw(), "InstanceIds")?;
            }
            TriggerKind::ProcessesAreRunning | TriggerKind::ProcessesStopRunning => {
                self.process_names()?;
            }
            TriggerKind::UserInactivity => {
                timespan(self.raw(), "InactivityTimeSpan")?;
            }
            TriggerKind::Periodic => {
                timespan(self.raw(), "Period")?;
            }
            TriggerKind::Time => {
                time(self.field("Time"))?;
                days(self.raw())?;
                optional_bool(self.raw(), "IsSunrise", false)?;
                optional_bool(self.raw(), "IsSunset", false)?;
            }
            TriggerKind::TimeRange => {
                time(self.field("StartTime"))?;
                time(self.field("EndTime"))?;
                days(self.raw())?;
                optional_bool(self.raw(), "IsSunriseToSunset", false)?;
                optional_bool(self.raw(), "IsSunsetToSunrise", false)?;
            }
            TriggerKind::BatteryPercentage => {
                let percent = integer(self.raw(), "Percentage")?;
                if !(0..=100).contains(&percent) {
                    return Err(CoreError::invalid("battery percentage 0..100"));
                }
                optional_bool(self.raw(), "IsBelow", false)?;
            }
            _ => {}
        }
        Ok(())
    }
    fn process_names(&self) -> CoreResult<Vec<String>> {
        match self.field("Processes") {
            None | Some(JsonNode::Null) => Ok(Vec::new()),
            Some(JsonNode::Array(items)) => items
                .iter()
                .map(|item| {
                    optional_string(item, "Name")?.ok_or_else(|| CoreError::invalid("process Name"))
                })
                .collect(),
            _ => Err(CoreError::invalid("Processes array")),
        }
    }
    fn day_matches(
        &self,
        start: TimeOfDay,
        day: u8,
        end: TimeOfDay,
        current: Option<TimeOfDay>,
    ) -> CoreResult<bool> {
        let selected = days(self.raw())?;
        if selected.is_empty() {
            return Ok(true);
        }
        let active_day = if start.seconds() > end.seconds()
            && current.is_some_and(|v| v.seconds() <= end.seconds())
        {
            (day + 6) % 7
        } else {
            day
        };
        Ok(selected.contains(&active_day))
    }
    fn matches_time(&self, now: TimeOfDay, day: u8, state: &AutomationState) -> CoreResult<bool> {
        let selected = days(self.raw())?;
        if time(self.field("Time"))? == Some(now)
            && (selected.is_empty() || selected.contains(&day))
        {
            return Ok(true);
        }
        if optional_bool(self.raw(), "IsSunrise", false)? && state.sunrise == Some(now) {
            return Ok(true);
        }
        if optional_bool(self.raw(), "IsSunset", false)? && state.sunset == Some(now) {
            return Ok(true);
        }
        Ok(false)
    }
    fn range(&self, state: &AutomationState) -> CoreResult<Option<(TimeOfDay, TimeOfDay)>> {
        if optional_bool(self.raw(), "IsSunriseToSunset", false)? {
            return Ok(state.sunrise.zip(state.sunset));
        }
        if optional_bool(self.raw(), "IsSunsetToSunrise", false)? {
            return Ok(state.sunset.zip(state.sunrise));
        }
        Ok(time(self.field("StartTime"))?.zip(time(self.field("EndTime"))?))
    }
    fn event_range(&self, state: &AutomationState) -> CoreResult<Option<(TimeOfDay, TimeOfDay)>> {
        if optional_bool(self.raw(), "IsSunriseToSunset", false)?
            && let Some(pair) = state.sunrise.zip(state.sunset)
        {
            return Ok(Some(pair));
        }
        if optional_bool(self.raw(), "IsSunsetToSunrise", false)?
            && let Some(pair) = state.sunset.zip(state.sunrise)
        {
            return Ok(Some(pair));
        }
        Ok(time(self.field("StartTime"))?.zip(time(self.field("EndTime"))?))
    }
    pub fn matches_state(&self, state: &AutomationState) -> CoreResult<bool> {
        self.validate()?;
        Ok(match self.kind {
            TriggerKind::And | TriggerKind::Or | TriggerKind::OnStartup | TriggerKind::OnResume => {
                false
            }
            TriggerKind::AcConnected => state.adapter == Some(AdapterState::Connected),
            TriggerKind::AcDisconnected => state.adapter == Some(AdapterState::Disconnected),
            TriggerKind::LowWattageAcConnected => {
                state.adapter == Some(AdapterState::ConnectedLowWattage)
            }
            TriggerKind::LidOpened => state.lid_open == Some(true),
            TriggerKind::LidClosed => state.lid_open == Some(false),
            TriggerKind::DisplayOn => state.display_on == Some(true),
            TriggerKind::DisplayOff => state.display_on == Some(false),
            TriggerKind::ExternalDisplayConnected => {
                state.external_display_count.is_some_and(|n| n > 0)
            }
            TriggerKind::ExternalDisplayDisconnected => state.external_display_count == Some(0),
            TriggerKind::HdrOn => state.hdr_on == Some(true),
            TriggerKind::HdrOff => state.hdr_on == Some(false),
            TriggerKind::SessionLock => state.session_locked == Some(true),
            TriggerKind::SessionUnlock => state.session_locked == Some(false),
            TriggerKind::GamesAreRunning => state.games_running,
            TriggerKind::GamesStop => !state.games_running,
            TriggerKind::PowerMode => {
                state.power_mode == Some(enumeration(self.raw(), "PowerModeState", POWER)?)
            }
            TriggerKind::ItsMode => {
                state.its_mode == Some(enumeration(self.raw(), "ITSModeState", ITS)?)
            }
            TriggerKind::HybridMode => {
                state.hybrid_mode == Some(enumeration(self.raw(), "HybridModeState", HYBRID)?)
            }
            TriggerKind::GodModePresetChanged => {
                state.preset_id.as_deref().map(normalize_guid).transpose()?
                    == optional_string(self.raw(), "PresetId")?
                        .map(|s| normalize_guid(&s))
                        .transpose()?
            }
            TriggerKind::ProcessesAreRunning | TriggerKind::ProcessesStopRunning => {
                let configured = self.process_names()?;
                let running = configured.iter().any(|name| {
                    state
                        .process_names
                        .iter()
                        .any(|running| super::ordinal::equal(name, running))
                });
                if self.kind == TriggerKind::ProcessesAreRunning {
                    running
                } else {
                    !running
                }
            }
            TriggerKind::DeviceConnected | TriggerKind::DeviceDisconnected => {
                let ids = string_list(self.raw(), "InstanceIds")?;
                let connected = ids.iter().any(|id| state.connected_device_ids.contains(id));
                if self.kind == TriggerKind::DeviceConnected {
                    connected
                } else {
                    !connected
                }
            }
            TriggerKind::WifiDisconnected => state.wifi_ssid.is_none(),
            TriggerKind::WifiConnected => {
                let ssids = string_list(self.raw(), "Ssids")?;
                state
                    .wifi_ssid
                    .as_ref()
                    .is_some_and(|ssid| ssids.is_empty() || ssids.contains(ssid))
            }
            TriggerKind::UserInactivity => {
                state.inactivity_ticks == timespan(self.raw(), "InactivityTimeSpan")?
            }
            TriggerKind::BatteryPercentage => {
                let boundary = integer(self.raw(), "Percentage")?;
                let below = optional_bool(self.raw(), "IsBelow", false)?;
                state.battery_percentage.is_some_and(|p| {
                    if below {
                        i64::from(p) < boundary
                    } else {
                        i64::from(p) > boundary
                    }
                })
            }
            TriggerKind::Time => self.matches_time(state.utc_time, state.utc_day, state)?,
            TriggerKind::TimeRange => {
                if let Some((start, end)) = self.range(state)? {
                    state.utc_time.between(start, end)
                        && self.day_matches(start, state.utc_day, end, Some(state.utc_time))?
                } else {
                    false
                }
            }
            TriggerKind::Periodic => {
                let period =
                    (timespan(self.raw(), "Period")? as f64 / 10_000_000.).round_ties_even();
                state.local_day_seconds.is_finite()
                    && period > 0.
                    && period <= i64::MAX as f64
                    && (state.local_day_seconds.round_ties_even() as i64) % period as i64 == 0
            }
            TriggerKind::Unknown => unreachable!(),
        })
    }
    pub fn matches_event(
        &self,
        event: &AutomationEvent,
        state: &AutomationState,
    ) -> CoreResult<bool> {
        self.validate()?;
        match self.kind {
            TriggerKind::Or => {
                for child in &self.children {
                    if child.matches_event(event, state)? {
                        return Ok(true);
                    }
                }
                return Ok(false);
            }
            TriggerKind::And => {
                for (index, child) in self.children.iter().enumerate() {
                    if !child.matches_event(event, state)? {
                        continue;
                    }
                    for (other, trigger) in self.children.iter().enumerate() {
                        if other != index && !trigger.matches_state(state)? {
                            return Ok(false);
                        }
                    }
                    return Ok(true);
                }
                return Ok(false);
            }
            _ => {}
        }
        if matches!(event, AutomationEvent::Startup) {
            return match self.kind {
                TriggerKind::OnStartup => Ok(true),
                TriggerKind::OnResume
                | TriggerKind::SessionLock
                | TriggerKind::GamesAreRunning
                | TriggerKind::GamesStop
                | TriggerKind::Time
                | TriggerKind::Periodic
                | TriggerKind::ProcessesStopRunning => Ok(false),
                _ => self.matches_state(state),
            };
        }
        Ok(match (self.kind, event) {
            (TriggerKind::HdrOn, AutomationEvent::Hdr(Some(true)))
            | (TriggerKind::HdrOff, AutomationEvent::Hdr(Some(false))) => true,
            (
                TriggerKind::LidOpened,
                AutomationEvent::Native {
                    kind: NativeEvent::LidOpened,
                    ..
                },
            )
            | (
                TriggerKind::LidClosed,
                AutomationEvent::Native {
                    kind: NativeEvent::LidClosed,
                    ..
                },
            )
            | (
                TriggerKind::DisplayOn,
                AutomationEvent::Native {
                    kind: NativeEvent::MonitorOn,
                    ..
                },
            )
            | (
                TriggerKind::DisplayOff,
                AutomationEvent::Native {
                    kind: NativeEvent::MonitorOff,
                    ..
                },
            )
            | (
                TriggerKind::ExternalDisplayConnected,
                AutomationEvent::Native {
                    kind: NativeEvent::ExternalMonitorConnected,
                    ..
                },
            )
            | (
                TriggerKind::ExternalDisplayDisconnected,
                AutomationEvent::Native {
                    kind: NativeEvent::ExternalMonitorDisconnected,
                    ..
                },
            ) => true,
            (
                TriggerKind::DeviceConnected,
                AutomationEvent::Native {
                    kind: NativeEvent::DeviceConnected,
                    instance_id: Some(id),
                },
            )
            | (
                TriggerKind::DeviceDisconnected,
                AutomationEvent::Native {
                    kind: NativeEvent::DeviceDisconnected,
                    instance_id: Some(id),
                },
            ) => string_list(self.raw(), "InstanceIds")?.contains(id),
            (
                TriggerKind::AcConnected | TriggerKind::AcDisconnected,
                AutomationEvent::PowerState {
                    status_changed: true,
                    adapter_changed: true,
                    ..
                },
            ) => self.matches_state(state)?,
            (TriggerKind::LowWattageAcConnected, AutomationEvent::PowerState { .. }) => {
                self.matches_state(state)?
            }
            (
                TriggerKind::OnResume,
                AutomationEvent::PowerState {
                    resume: true,
                    adapter_changed: false,
                    ..
                },
            ) => true,
            (TriggerKind::PowerMode, AutomationEvent::PowerMode(mode)) => {
                *mode == enumeration(self.raw(), "PowerModeState", POWER)?
            }
            (TriggerKind::ItsMode, AutomationEvent::ItsMode(mode)) => {
                *mode == enumeration(self.raw(), "ITSModeState", ITS)?
            }
            (TriggerKind::HybridMode, AutomationEvent::HybridMode(mode)) => {
                *mode == enumeration(self.raw(), "HybridModeState", HYBRID)?
            }
            (TriggerKind::GodModePresetChanged, AutomationEvent::PresetChanged(id)) => {
                Some(normalize_guid(id)?)
                    == optional_string(self.raw(), "PresetId")?
                        .map(|s| normalize_guid(&s))
                        .transpose()?
            }
            (TriggerKind::GamesAreRunning, AutomationEvent::Game(true))
            | (TriggerKind::GamesStop, AutomationEvent::Game(false))
            | (TriggerKind::SessionLock, AutomationEvent::SessionLocked(true))
            | (TriggerKind::SessionUnlock, AutomationEvent::SessionLocked(false)) => true,
            (
                TriggerKind::ProcessesAreRunning,
                AutomationEvent::Process {
                    started: true,
                    name,
                    ..
                },
            )
            | (
                TriggerKind::ProcessesStopRunning,
                AutomationEvent::Process {
                    started: false,
                    name,
                    ..
                },
            ) => {
                self.process_names()?
                    .iter()
                    .any(|configured| super::ordinal::equal(configured, name))
                    && self.matches_state(state)?
            }
            (
                TriggerKind::WifiConnected,
                AutomationEvent::Wifi {
                    connected: true,
                    ssid,
                },
            ) => {
                let wanted = string_list(self.raw(), "Ssids")?;
                wanted.is_empty() || ssid.as_ref().is_some_and(|s| wanted.contains(s))
            }
            (
                TriggerKind::WifiDisconnected,
                AutomationEvent::Wifi {
                    connected: false, ..
                },
            ) => true,
            (TriggerKind::UserInactivity, AutomationEvent::UserInactivity { ticks }) => {
                *ticks == timespan(self.raw(), "InactivityTimeSpan")?
            }
            (TriggerKind::BatteryPercentage, AutomationEvent::BatteryPercentage(_))
            | (
                TriggerKind::BatteryPercentage,
                AutomationEvent::PowerState {
                    status_changed: true,
                    ..
                },
            ) => self.matches_state(state)?,
            (TriggerKind::Time, AutomationEvent::Time { time, day }) => {
                self.matches_time(*time, *day, state)?
            }
            (TriggerKind::TimeRange, AutomationEvent::Time { time, day }) => {
                if let Some((start, end)) = self.event_range(state)? {
                    *time == start && self.day_matches(start, *day, end, None)?
                } else {
                    false
                }
            }
            (TriggerKind::Periodic, AutomationEvent::Time { .. }) => self.matches_state(state)?,
            _ => false,
        })
    }
    pub fn update_environment(&self, environment: &mut AutomationEnvironment) -> CoreResult<()> {
        self.validate()?;
        let e = environment;
        match self.kind {
            TriggerKind::And | TriggerKind::Or => {
                for child in &self.children {
                    child.update_environment(e)?;
                }
            }
            TriggerKind::OnStartup => {
                e.startup = true;
                e.set_bool("LLT_STARTUP", true);
            }
            TriggerKind::OnResume => e.set_bool("LLT_RESUME", true),
            TriggerKind::AcConnected
            | TriggerKind::AcDisconnected
            | TriggerKind::LowWattageAcConnected => {
                e.set_bool(
                    "LLT_IS_AC_ADAPTER_CONNECTED",
                    self.kind != TriggerKind::AcDisconnected,
                );
                if self.kind == TriggerKind::LowWattageAcConnected {
                    e.set_bool("LLT_IS_AC_ADAPTER_LOW_POWER", true);
                }
            }
            TriggerKind::LidOpened | TriggerKind::LidClosed => {
                e.set_bool("LLT_IS_LID_OPEN", self.kind == TriggerKind::LidOpened)
            }
            TriggerKind::DisplayOn | TriggerKind::DisplayOff => {
                e.set_bool("LLT_IS_DISPLAY_ON", self.kind == TriggerKind::DisplayOn)
            }
            TriggerKind::ExternalDisplayConnected | TriggerKind::ExternalDisplayDisconnected => e
                .set_bool(
                    "LLT_IS_EXTERNAL_DISPLAY_CONNECTED",
                    self.kind == TriggerKind::ExternalDisplayConnected,
                ),
            TriggerKind::HdrOn | TriggerKind::HdrOff => {
                e.set_bool("LLT_IS_HDR_ON", self.kind == TriggerKind::HdrOn)
            }
            TriggerKind::GamesAreRunning | TriggerKind::GamesStop => e.set_bool(
                "LLT_IS_GAME_RUNNING",
                self.kind == TriggerKind::GamesAreRunning,
            ),
            TriggerKind::SessionLock | TriggerKind::SessionUnlock => {
                e.set_bool("LLT_SESSION_LOCKED", self.kind == TriggerKind::SessionLock)
            }
            TriggerKind::WifiConnected => {
                e.set_bool("LLT_WIFI_CONNECTED", true);
                e.set("LLT_WIFI_SSID", string_list(self.raw(), "Ssids")?.join(","));
            }
            TriggerKind::WifiDisconnected => e.set_bool("LLT_WIFI_CONNECTED", false),
            TriggerKind::DeviceConnected | TriggerKind::DeviceDisconnected => {
                e.set_bool(
                    "LLT_DEVICE_CONNECTED",
                    self.kind == TriggerKind::DeviceConnected,
                );
                e.set(
                    "LLT_DEVICE_INSTANCE_IDS",
                    string_list(self.raw(), "InstanceIds")?.join(","),
                );
            }
            TriggerKind::ProcessesAreRunning | TriggerKind::ProcessesStopRunning => {
                e.set_bool(
                    "LLT_PROCESSES_STARTED",
                    self.kind == TriggerKind::ProcessesAreRunning,
                );
                e.set("LLT_PROCESSES", self.process_names()?.join(","));
            }
            TriggerKind::PowerMode => {
                let mode = enumeration(self.raw(), "PowerModeState", POWER)?;
                let (code, name) = match mode {
                    0 => ("1", "QUIET"),
                    1 => ("2", "BALANCE"),
                    2 => ("3", "PERFORMANCE"),
                    223 => ("223", "EXTREME"),
                    254 => ("255", "CUSTOM"),
                    _ => ("", ""),
                };
                e.set("LLT_POWER_MODE", code);
                e.set("LLT_POWER_MODE_NAME", name);
            }
            TriggerKind::ItsMode => {
                let mode = enumeration(self.raw(), "ITSModeState", ITS)?;
                let (code, name) = match mode {
                    1 => ("1", "INTELLIGENT COOLING"),
                    2 => ("2", "BATTERY SAVING"),
                    3 => ("3", "EXTREME PERFORMANCE"),
                    4 => ("4", "GEEK"),
                    _ => ("", ""),
                };
                e.set("LLT_POWER_MODE", code);
                e.set("LLT_POWER_MODE_NAME", name);
            }
            TriggerKind::HybridMode => {
                let mode = enumeration(self.raw(), "HybridModeState", HYBRID)?;
                let (code, name) = match mode {
                    4 => ("1", "DGPU Only"),
                    0 => ("2", "Hybrid"),
                    2 => ("3", "Hybrid Auto"),
                    1 => ("4", "Hybrid IGPU"),
                    3 => ("5", "UMA"),
                    _ => ("", ""),
                };
                e.set("LLT_HYBRID_MODE", code);
                e.set("LLT_HYBRID_MODE_NAME", name);
            }
            TriggerKind::UserInactivity => e.set_bool(
                "LLT_IS_USER_ACTIVE",
                timespan(self.raw(), "InactivityTimeSpan")? == 0,
            ),
            TriggerKind::Periodic => e.set(
                "LLT_PERIOD",
                (timespan(self.raw(), "Period")? / 10_000_000).to_string(),
            ),
            TriggerKind::Time | TriggerKind::TimeRange => {
                let names = [
                    "SUNDAY",
                    "MONDAY",
                    "TUESDAY",
                    "WEDNESDAY",
                    "THURSDAY",
                    "FRIDAY",
                    "SATURDAY",
                ];
                let selected = days(self.raw())?;
                e.variables.insert(
                    "LLT_DAYS".into(),
                    (!selected.is_empty()).then(|| {
                        selected
                            .iter()
                            .map(|d| names[*d as usize])
                            .collect::<Vec<_>>()
                            .join(",")
                    }),
                );
                if self.kind == TriggerKind::Time {
                    e.set_bool(
                        "LLT_IS_SUNRISE",
                        optional_bool(self.raw(), "IsSunrise", false)?,
                    );
                    e.set_bool(
                        "LLT_IS_SUNSET",
                        optional_bool(self.raw(), "IsSunset", false)?,
                    );
                    e.variables.insert(
                        "LLT_TIME".into(),
                        time(self.field("Time"))?
                            .map(|t| format!("{}:{}.{}", t.hour, t.minute, t.second)),
                    );
                } else {
                    e.set_bool(
                        "LLT_IS_SUNRISE_TO_SUNSET",
                        optional_bool(self.raw(), "IsSunriseToSunset", false)?,
                    );
                    e.set_bool(
                        "LLT_IS_SUNSET_TO_SUNRISE",
                        optional_bool(self.raw(), "IsSunsetToSunrise", false)?,
                    );
                    for (field, key) in
                        [("StartTime", "LLT_START_TIME"), ("EndTime", "LLT_END_TIME")]
                    {
                        e.variables.insert(
                            key.into(),
                            time(self.field(field))?
                                .map(|t| format!("{}:{}:{}", t.hour, t.minute, t.second)),
                        );
                    }
                }
            }
            TriggerKind::BatteryPercentage | TriggerKind::GodModePresetChanged => {}
            TriggerKind::Unknown => unreachable!(),
        }
        Ok(())
    }
}
