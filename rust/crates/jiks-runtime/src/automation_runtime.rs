//! Pure UI/legacy mappings and owned plans. No event callback performs native IO.
use anyhow::{Context, Result, bail, ensure};
use jiks_core::{automation::*, settings::JsonNode};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};

pub const STEP_TYPES: &[(&str, StepKind)] = &[
    ("power_mode", StepKind::PowerMode),
    ("godmode_preset", StepKind::GodModePreset),
    ("cycle_preset", StepKind::CycleGodModePreset),
    ("ai_engine", StepKind::AiEngine),
    ("battery", StepKind::Battery),
    ("battery_night", StepKind::BatteryNightCharge),
    ("always_on_usb", StepKind::AlwaysOnUsb),
    ("deactivate_gpu", StepKind::DeactivateGpu),
    ("delay", StepKind::Delay),
    ("display_brightness", StepKind::DisplayBrightness),
    ("dpi", StepKind::DpiScale),
    ("flip_to_start", StepKind::FlipToStart),
    ("fn_lock", StepKind::FnLock),
    ("sensors", StepKind::HardwareSensors),
    ("hdr", StepKind::Hdr),
    ("acm", StepKind::AutoColorManagement),
    ("its_mode", StepKind::ItsMode),
    ("hybrid", StepKind::HybridMode),
    ("instant_boot", StepKind::InstantBoot),
    ("macro", StepKind::Macro),
    ("microphone", StepKind::Microphone),
    ("speaker", StepKind::Speaker),
    ("volume", StepKind::SpeakerVolume),
    ("notification", StepKind::Notification),
    ("white_single", StepKind::OneLevelWhiteKeyboardBacklight),
    ("gpu_oc", StepKind::OverclockDiscreteGpu),
    ("overdrive", StepKind::OverDrive),
    ("panel_logo", StepKind::PanelLogoBacklight),
    ("play_sound", StepKind::PlaySound),
    ("ports_light", StepKind::PortsBacklight),
    ("quick_action", StepKind::QuickAction),
    ("refresh_rate", StepKind::RefreshRate),
    ("resolution", StepKind::Resolution),
    ("rgb_keyboard", StepKind::RgbKeyboardBacklight),
    ("run", StepKind::Run),
    ("spectrum_brightness", StepKind::SpectrumKeyboardBrightness),
    ("spectrum_profile", StepKind::SpectrumKeyboardProfile),
    ("spectrum_import", StepKind::SpectrumKeyboardImportProfile),
    ("touchpad", StepKind::TouchpadLock),
    ("monitors_off", StepKind::TurnOffMonitors),
    ("wifi", StepKind::Wifi),
    ("airplane", StepKind::AirplaneMode),
    ("white_keyboard", StepKind::WhiteKeyboardBacklight),
    ("win_key", StepKind::WinKey),
    ("close_app", StepKind::CloseApp),
    ("show_app", StepKind::ShowApp),
    ("osd", StepKind::Osd),
    ("osd_lock", StepKind::OsdLockPosition),
    ("fan_max", StepKind::FanMaxSpeed),
];
pub const TRIGGER_TYPES: &[(&str, TriggerKind)] = &[
    ("ac_connected", TriggerKind::AcConnected),
    ("low_watt_ac", TriggerKind::LowWattageAcConnected),
    ("ac_disconnected", TriggerKind::AcDisconnected),
    ("battery_percentage", TriggerKind::BatteryPercentage),
    ("power_mode", TriggerKind::PowerMode),
    ("godmode_preset", TriggerKind::GodModePresetChanged),
    ("games_running", TriggerKind::GamesAreRunning),
    ("games_stopped", TriggerKind::GamesStop),
    ("process_running", TriggerKind::ProcessesAreRunning),
    ("process_stopped", TriggerKind::ProcessesStopRunning),
    ("user_idle", TriggerKind::UserInactivity),
    ("session_lock", TriggerKind::SessionLock),
    ("session_unlock", TriggerKind::SessionUnlock),
    ("lid_open", TriggerKind::LidOpened),
    ("lid_closed", TriggerKind::LidClosed),
    ("display_on", TriggerKind::DisplayOn),
    ("display_off", TriggerKind::DisplayOff),
    ("hdr_on", TriggerKind::HdrOn),
    ("hdr_off", TriggerKind::HdrOff),
    ("hybrid", TriggerKind::HybridMode),
    ("device_connected", TriggerKind::DeviceConnected),
    ("device_disconnected", TriggerKind::DeviceDisconnected),
    (
        "external_display_connected",
        TriggerKind::ExternalDisplayConnected,
    ),
    (
        "external_display_disconnected",
        TriggerKind::ExternalDisplayDisconnected,
    ),
    ("wifi_connected", TriggerKind::WifiConnected),
    ("wifi_disconnected", TriggerKind::WifiDisconnected),
    ("time", TriggerKind::Time),
    ("time_range", TriggerKind::TimeRange),
    ("periodic", TriggerKind::Periodic),
    ("startup", TriggerKind::OnStartup),
    ("resume", TriggerKind::OnResume),
    ("its_mode", TriggerKind::ItsMode),
    ("all", TriggerKind::And),
    ("any", TriggerKind::Or),
];
fn node(value: Value) -> Result<JsonNode> {
    Ok(JsonNode::parse(&serde_json::to_string(&value)?)?)
}
fn put(raw: &mut JsonNode, name: &str, value: JsonNode) -> Result<()> {
    raw.set_known(name, value)?;
    Ok(())
}
fn n(value: impl ToString) -> JsonNode {
    JsonNode::Number(value.to_string())
}
fn s(value: impl Into<String>) -> JsonNode {
    JsonNode::String(value.into())
}
fn text<'a>(raw: &'a JsonNode, name: &str) -> Result<&'a str> {
    raw.get_ci(name)
        .and_then(JsonNode::as_str)
        .with_context(|| format!("{name} must be string"))
}
fn int(raw: &JsonNode, name: &str) -> Result<i64> {
    raw.get_ci(name)
        .and_then(JsonNode::as_i64)
        .with_context(|| format!("{name} must be integer"))
}
fn flag(raw: &JsonNode, name: &str, default: bool) -> Result<bool> {
    raw.get_ci(name).map_or(Ok(default), |v| {
        v.as_bool()
            .with_context(|| format!("{name} must be boolean"))
    })
}
fn legacy(ui: &JsonNode, kind: StepKind) -> Result<JsonNode> {
    if let Some(raw) = ui.get("legacy") {
        let old = StepDto::parse(raw.clone())?;
        ensure!(old.kind == kind, "Mismatched legacy step kind");
        return Ok(raw.clone());
    }
    Ok(JsonNode::object())
}
fn state_enum(value: &JsonNode) -> Result<i32> {
    if let Some(value) = value.as_bool() {
        return Ok(i32::from(value));
    }
    if let Some(value) = value.as_i64() {
        return Ok(i32::try_from(value)?);
    }
    match value.as_str().unwrap_or("") {
        "off" => Ok(0),
        "on" => Ok(1),
        "toggle" => Ok(2),
        value => value.parse().context("Automation state enum"),
    }
}
fn power_to_legacy(value: i32) -> Result<i32> {
    Ok(match value {
        1 => 0,
        2 => 1,
        3 => 2,
        224 => 223,
        255 => 254,
        _ => bail!("Unsupported firmware power mode"),
    })
}
pub fn firmware_power_to_legacy(value:i32)->Result<i32> {power_to_legacy(value)}
fn power_to_ui(value: i32) -> Result<i32> {
    Ok(match value {
        0 => 1,
        1 => 2,
        2 => 3,
        223 => 224,
        254 => 255,
        _ => bail!("Unsupported legacy power mode"),
    })
}
pub fn step_from_ui(ui: &JsonNode) -> Result<StepDto> {
    if ui.get("$type").is_some() {
        let step = StepDto::parse(ui.clone())?;
        step.validate()?;
        return Ok(step);
    }
    let id = text(ui, "type")?;
    let kind = STEP_TYPES
        .iter()
        .find(|(id2, _)| *id2 == id)
        .map(|(_, kind)| *kind)
        .context("Unknown automation UI action")?;
    let empty = JsonNode::object();
    let p = ui.get_ci("parameters").unwrap_or(&empty);
    ensure!(p.as_object().is_some(), "Step parameters must be object");
    let mut raw = legacy(ui, kind)?;
    match kind {
        StepKind::Delay => {
            let ms = int(p, "milliseconds")?;
            ensure!(
                ms >= 0 && ms <= i64::from(i32::MAX) * 1000,
                "Delay milliseconds out of range"
            );
            put(&mut raw, "State", node(json!({"DelaySeconds":ms/1000}))?)?;
            put(&mut raw, "JiksDelayMilliseconds", n(ms))?;
        }
        StepKind::Run => {
            put(&mut raw, "ScriptPath", s(text(p, "path")?))?;
            put(
                &mut raw,
                "ScriptArguments",
                s(p.get_ci("arguments")
                    .and_then(JsonNode::as_str)
                    .unwrap_or("")),
            )?;
            put(
                &mut raw,
                "WaitUntilFinished",
                JsonNode::Bool(flag(p, "wait", false)?),
            )?;
            put(
                &mut raw,
                "JiksWorkingDirectory",
                s(p.get_ci("working_directory")
                    .and_then(JsonNode::as_str)
                    .unwrap_or("")),
            )?;
        }
        StepKind::Notification => {
            put(&mut raw, "Text", s(text(p, "message")?))?;
            put(
                &mut raw,
                "JiksTitle",
                s(p.get_ci("title").and_then(JsonNode::as_str).unwrap_or("")),
            )?;
        }
        StepKind::PlaySound | StepKind::SpectrumKeyboardImportProfile => {
            put(&mut raw, "Path", s(text(p, "path")?))?
        }
        StepKind::GodModePreset => put(&mut raw, "PresetId", s(normalize_guid(text(p, "id")?)?))?,
        StepKind::QuickAction => put(&mut raw, "PipelineId", s(normalize_guid(text(p, "id")?)?))?,
        StepKind::DisplayBrightness | StepKind::SpeakerVolume => put(
            &mut raw,
            if kind == StepKind::DisplayBrightness {
                "Brightness"
            } else {
                "Volume"
            },
            n(int(p, "value")?),
        )?,
        StepKind::DpiScale => put(&mut raw, "State", node(json!({"Scale":int(p,"value")?}))?)?,
        StepKind::Resolution => {
            let value = p.get_ci("value").context("Resolution value required")?;
            let (width, height) = if let Some(value) = value.as_str() {
                let (w, h) = value
                    .split_once('x')
                    .or_else(|| value.split_once('×'))
                    .context("Resolution WIDTHxHEIGHT")?;
                (w.trim().parse::<i32>()?, h.trim().parse::<i32>()?)
            } else {
                (int(value, "Width")? as i32, int(value, "Height")? as i32)
            };
            put(
                &mut raw,
                "State",
                node(json!({"Width":width,"Height":height}))?,
            )?;
        }
        StepKind::RefreshRate => {
            let value = p.get_ci("value").context("Refresh value required")?;
            let state = if value.as_object().is_some() {
                value.clone()
            } else {
                node(
                    json!({"Frequency":value.as_i64().context("Refresh frequency integer")?,"IsDynamic":false,"BaseFrequency":0}),
                )?
            };
            put(&mut raw, "State", state)?;
        }
        StepKind::SpectrumKeyboardBrightness | StepKind::SpectrumKeyboardProfile => {
            put(&mut raw, "State", n(int(p, "value")?))?
        }
        StepKind::CycleGodModePreset
        | StepKind::CloseApp
        | StepKind::ShowApp
        | StepKind::TurnOffMonitors => {}
        _ => {
            let value = p.get_ci("value").context("Feature state value required")?;
            let mut state = state_enum(value)?;
            if kind == StepKind::PowerMode {
                state = power_to_legacy(state)?;
            }
            if kind == StepKind::BatteryNightCharge && value.as_bool().is_some() {
                state = 1 - state;
            }
            put(&mut raw, "State", n(state))?;
        }
    }
    let step = StepDto::new(kind, raw)?;
    step.validate()?;
    Ok(step)
}
pub fn step_to_ui(step: &StepDto) -> Result<JsonNode> {
    let id = STEP_TYPES
        .iter()
        .find(|(_, kind)| *kind == step.kind)
        .map(|(id, _)| *id)
        .unwrap_or("unsupported");
    if step.kind == StepKind::Unknown {
        return node(json!({"type":id,"parameters":{},"legacy":step.raw().to_value()?}));
    }
    let request = step.action_request()?;
    let p = match request {
        ActionRequest::Delay { seconds } => {
            json!({"milliseconds":step.field("JiksDelayMilliseconds").and_then(JsonNode::as_i64).unwrap_or(i64::from(seconds)*1000)})
        }
        ActionRequest::Run(run) => {
            json!({"path":run.script_path,"arguments":run.script_arguments,"wait":run.wait_until_finished,"working_directory":step.field("JiksWorkingDirectory").and_then(JsonNode::as_str).unwrap_or("")})
        }
        ActionRequest::Notification { text, .. } => {
            json!({"message":text,"title":step.field("JiksTitle").and_then(JsonNode::as_str).unwrap_or("")})
        }
        ActionRequest::PlaySound { path } | ActionRequest::ImportSpectrumProfile { path } => {
            json!({"path":path})
        }
        ActionRequest::GodModePreset { id } => json!({"id":id}),
        ActionRequest::QuickAction { pipeline_id } => json!({"id":pipeline_id}),
        ActionRequest::DisplayBrightness(value)
        | ActionRequest::SpeakerVolume(value)
        | ActionRequest::SpectrumBrightness(value)
        | ActionRequest::SpectrumProfile(value) => json!({"value":value}),
        ActionRequest::Feature { kind, state } => match state {
            FeatureState::Enum(value) => {
                json!({"value":if kind==StepKind::PowerMode{Value::from(power_to_ui(value)?)}else if kind==StepKind::Macro{Value::from(match value{0=>"off",1=>"on",_=>"toggle"})}else{Value::from(value)}})
            }
            FeatureState::DpiScale(value) => json!({"value":value}),
            FeatureState::Resolution { width, height } => {
                json!({"value":format!("{width}x{height}")})
            }
            FeatureState::RefreshRate {
                frequency,
                is_dynamic,
                base_frequency,
            } => {
                json!({"value":{"Frequency":frequency,"IsDynamic":is_dynamic,"BaseFrequency":base_frequency}})
            }
        },
        _ => json!({}),
    };
    let mut ui = node(json!({"type":id,"parameters":p}))?;
    ui.set("legacy", step.raw().clone())?;
    Ok(ui)
}
fn split_list(p: &JsonNode, key: &str) -> Result<Vec<String>> {
    let Some(raw) = p.get_ci(key) else {
        return Ok(vec![]);
    };
    if let Some(items) = raw.as_array() {
        return items
            .iter()
            .map(|n| {
                n.as_str()
                    .map(str::to_owned)
                    .context("String list expected")
            })
            .collect();
    }
    Ok(raw
        .as_str()
        .context("Comma-separated string expected")?
        .split([',', '\n', '\r'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect())
}
fn time_node(value: &str) -> Result<JsonNode> {
    let parts = value
        .split(':')
        .map(str::parse::<u8>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!((2..=3).contains(&parts.len()), "Time HH:MM[:SS]");
    let time = TimeOfDay::new(parts[0], parts[1], *parts.get(2).unwrap_or(&0))?;
    node(json!({"Hour":time.hour,"Minute":time.minute,"Second":time.second}))
}
fn duration_seconds(value: i64) -> Result<String> {
    ensure!(
        value >= 0 && value <= i64::MAX / 10_000_000,
        "Nonnegative bounded duration required"
    );
    let days = value / 86400;
    Ok(format!(
        "{}{:02}:{:02}:{:02}",
        if days > 0 {
            format!("{days}.")
        } else {
            String::new()
        },
        value % 86400 / 3600,
        value % 3600 / 60,
        value % 60
    ))
}
pub fn trigger_from_ui(ui: &JsonNode) -> Result<Option<TriggerDto>> {
    trigger_from_ui_depth(ui, 0)
}
fn trigger_from_ui_depth(ui: &JsonNode, depth: usize) -> Result<Option<TriggerDto>> {
    ensure!(depth <= 32, "UI trigger tree exceeds depth budget");
    if matches!(ui, JsonNode::Null) {
        return Ok(None);
    }
    if ui.get("$type").is_some() {
        let trigger = TriggerDto::parse(ui.clone())?;
        trigger.validate()?;
        return Ok(Some(trigger));
    }
    let id = text(ui, "type")?;
    if id == "manual" {
        return Ok(None);
    }
    let kind = TRIGGER_TYPES
        .iter()
        .find(|(name, _)| *name == id)
        .map(|(_, kind)| *kind)
        .context("Unknown UI trigger")?;
    if matches!(kind, TriggerKind::And | TriggerKind::Or) {
        let children = ui
            .get_ci("conditions")
            .and_then(JsonNode::as_array)
            .context("Composite conditions array")?;
        ensure!(
            !children.is_empty() && children.len() <= 128,
            "Composite needs 1..128 conditions"
        );
        let children = children
            .iter()
            .map(|n| {
                trigger_from_ui_depth(n, depth + 1)?.context("Manual condition cannot be nested")
            })
            .collect::<Result<Vec<_>>>()?;
        let mut raw = ui.get("legacy").cloned().unwrap_or_else(JsonNode::object);
        if ui.get("legacy").is_some() {
            ensure!(
                TriggerDto::parse(raw.clone())?.kind == kind,
                "Mismatched composite legacy kind"
            );
        }
        raw.set_known(
            "Triggers",
            JsonNode::Array(children.iter().map(|t| t.raw().clone()).collect()),
        )?;
        return Ok(Some(TriggerDto::new(kind, raw)?));
    }
    let empty = JsonNode::object();
    let p = ui.get_ci("parameters").unwrap_or(&empty);
    ensure!(p.as_object().is_some(), "Trigger parameters object");
    let mut raw = if let Some(old) = ui.get("legacy") {
        ensure!(
            TriggerDto::parse(old.clone())?.kind == kind,
            "Mismatched legacy trigger kind"
        );
        old.clone()
    } else {
        JsonNode::object()
    };
    match kind {
        TriggerKind::PowerMode => put(
            &mut raw,
            "PowerModeState",
            n(power_to_legacy(state_enum(
                p.get_ci("value").context("Power mode value")?,
            )?)?),
        )?,
        TriggerKind::HybridMode => put(
            &mut raw,
            "HybridModeState",
            n(state_enum(p.get_ci("value").context("Hybrid mode value")?)?),
        )?,
        TriggerKind::ItsMode => put(
            &mut raw,
            "ITSModeState",
            n(state_enum(p.get_ci("value").context("ITS mode value")?)?),
        )?,
        TriggerKind::GodModePresetChanged => {
            put(&mut raw, "PresetId", s(normalize_guid(text(p, "id")?)?))?
        }
        TriggerKind::BatteryPercentage => {
            let cmp = text(p, "comparison")?;
            ensure!(["le", "ge"].contains(&cmp), "Battery comparison le/ge");
            put(&mut raw, "Percentage", n(int(p, "percentage")?))?;
            put(&mut raw, "IsBelow", JsonNode::Bool(cmp == "le"))?;
        }
        TriggerKind::ProcessesAreRunning | TriggerKind::ProcessesStopRunning => put(
            &mut raw,
            "Processes",
            node(json!(
                split_list(p, "processes")?
                    .iter()
                    .map(|name| json!({"Name":name}))
                    .collect::<Vec<_>>()
            ))?,
        )?,
        TriggerKind::DeviceConnected | TriggerKind::DeviceDisconnected => put(
            &mut raw,
            "InstanceIds",
            node(json!(split_list(p, "devices")?))?,
        )?,
        TriggerKind::WifiConnected => {
            put(&mut raw, "Ssids", node(json!(split_list(p, "ssids")?))?)?
        }
        TriggerKind::Periodic | TriggerKind::UserInactivity => {
            let key = if kind == TriggerKind::Periodic {
                "Period"
            } else {
                "InactivityTimeSpan"
            };
            let seconds = int(p, "seconds")?;
            let unchanged = raw
                .get_ci(key)
                .and_then(JsonNode::as_str)
                .and_then(|s| jiks_core::settings::parse_timespan_ticks(s).ok())
                .is_some_and(|ticks| ticks / 10_000_000 == seconds);
            if !unchanged {
                put(&mut raw, key, s(duration_seconds(seconds)?))?;
            }
        }
        TriggerKind::Time => {
            let value = text(p, "time")?;
            put(
                &mut raw,
                "Time",
                if ["sunrise", "sunset"].contains(&value) {
                    JsonNode::Null
                } else {
                    time_node(value)?
                },
            )?;
            put(&mut raw, "IsSunrise", JsonNode::Bool(value == "sunrise"))?;
            put(&mut raw, "IsSunset", JsonNode::Bool(value == "sunset"))?;
        }
        TriggerKind::TimeRange => {
            let start = text(p, "start")?;
            let end = text(p, "end")?;
            let sunpair = (start, end) == ("sunrise", "sunset");
            let reverse = (start, end) == ("sunset", "sunrise");
            put(
                &mut raw,
                "StartTime",
                if sunpair || reverse {
                    JsonNode::Null
                } else {
                    time_node(start)?
                },
            )?;
            put(
                &mut raw,
                "EndTime",
                if sunpair || reverse {
                    JsonNode::Null
                } else {
                    time_node(end)?
                },
            )?;
            put(&mut raw, "IsSunriseToSunset", JsonNode::Bool(sunpair))?;
            put(&mut raw, "IsSunsetToSunrise", JsonNode::Bool(reverse))?;
        }
        _ => {}
    }
    if matches!(kind, TriggerKind::Time | TriggerKind::TimeRange) {
        let days = split_list(p, "days")?
            .iter()
            .map(|d| {
                let n = d.parse::<u8>()?;
                ensure!(n <= 6, "Weekday must be Sunday0..Saturday6");
                Ok(n)
            })
            .collect::<Result<Vec<_>>>()?;
        put(&mut raw, "Days", node(json!(days))?)?;
    }
    let trigger = TriggerDto::new(kind, raw)?;
    trigger.validate()?;
    Ok(Some(trigger))
}
fn legacy_time(node: Option<&JsonNode>) -> String {
    node.map(|n| {
        format!(
            "{:02}:{:02}:{:02}",
            n.get_ci("Hour").and_then(JsonNode::as_i64).unwrap_or(0),
            n.get_ci("Minute").and_then(JsonNode::as_i64).unwrap_or(0),
            n.get_ci("Second").and_then(JsonNode::as_i64).unwrap_or(0)
        )
    })
    .unwrap_or_default()
}
pub fn trigger_to_ui(trigger: Option<&TriggerDto>) -> Result<JsonNode> {
    let Some(t) = trigger else {
        return Ok(JsonNode::Null);
    };
    let id = TRIGGER_TYPES
        .iter()
        .find(|(_, kind)| *kind == t.kind)
        .map(|(id, _)| *id)
        .unwrap_or("unsupported");
    if matches!(t.kind, TriggerKind::And | TriggerKind::Or) {
        let mut ui = node(json!({"type":id}))?;
        ui.set(
            "conditions",
            JsonNode::Array(
                t.children
                    .iter()
                    .map(|t| trigger_to_ui(Some(t)))
                    .collect::<Result<_>>()?,
            ),
        )?;
        ui.set("legacy", t.raw().clone())?;
        return Ok(ui);
    }
    let r = t.raw();
    let val = |key: &str| r.get_ci(key).cloned().unwrap_or(JsonNode::Null);
    let join = |key: &str| -> String {
        r.get_ci(key)
            .and_then(JsonNode::as_array)
            .map(|v| {
                v.iter()
                    .map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_json()))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let mut p = JsonNode::object();
    match t.kind {
        TriggerKind::PowerMode => {
            let v = val("PowerModeState");
            let legacy = if let Some(n) = v.as_i64() {
                n as i32
            } else {
                match v.as_str().unwrap_or("") {
                    "Quiet" => 0,
                    "Balance" => 1,
                    "Performance" => 2,
                    "Extreme" => 223,
                    "GodMode" => 254,
                    _ => bail!("Unsupported legacy mode"),
                }
            };
            p.set("value", n(power_to_ui(legacy)?))?;
        }
        TriggerKind::HybridMode => {
            p.set("value", val("HybridModeState"))?;
        }
        TriggerKind::ItsMode => {
            p.set("value", val("ITSModeState"))?;
        }
        TriggerKind::GodModePresetChanged => {
            p.set("id", val("PresetId"))?;
        }
        TriggerKind::BatteryPercentage => {
            p.set("percentage", val("Percentage"))?;
            p.set(
                "comparison",
                s(
                    if r.get_ci("IsBelow")
                        .and_then(JsonNode::as_bool)
                        .unwrap_or(false)
                    {
                        "le"
                    } else {
                        "ge"
                    },
                ),
            )?;
        }
        TriggerKind::ProcessesAreRunning | TriggerKind::ProcessesStopRunning => {
            let names = r
                .get_ci("Processes")
                .and_then(JsonNode::as_array)
                .unwrap_or(&[])
                .iter()
                .filter_map(|n| n.get_ci("Name").and_then(JsonNode::as_str))
                .collect::<Vec<_>>()
                .join(", ");
            p.set("processes", s(names))?;
        }
        TriggerKind::DeviceConnected | TriggerKind::DeviceDisconnected => {
            p.set("devices", s(join("InstanceIds")))?;
        }
        TriggerKind::WifiConnected => {
            p.set("ssids", s(join("Ssids")))?;
        }
        TriggerKind::Periodic | TriggerKind::UserInactivity => {
            let key = if t.kind == TriggerKind::Periodic {
                "Period"
            } else {
                "InactivityTimeSpan"
            };
            let ticks = jiks_core::settings::parse_timespan_ticks(
                r.get_ci(key)
                    .and_then(JsonNode::as_str)
                    .unwrap_or("00:00:00"),
            )?;
            p.set("seconds", n(ticks / 10_000_000))?;
        }
        TriggerKind::Time => {
            p.set(
                "time",
                s(
                    if r.get_ci("IsSunrise").and_then(JsonNode::as_bool) == Some(true) {
                        "sunrise".into()
                    } else if r.get_ci("IsSunset").and_then(JsonNode::as_bool) == Some(true) {
                        "sunset".into()
                    } else {
                        legacy_time(r.get_ci("Time"))
                    },
                ),
            )?;
        }
        TriggerKind::TimeRange => {
            let sun = r.get_ci("IsSunriseToSunset").and_then(JsonNode::as_bool) == Some(true);
            let reverse = r.get_ci("IsSunsetToSunrise").and_then(JsonNode::as_bool) == Some(true);
            p.set(
                "start",
                s(if sun {
                    "sunrise".into()
                } else if reverse {
                    "sunset".into()
                } else {
                    legacy_time(r.get_ci("StartTime"))
                }),
            )?;
            p.set(
                "end",
                s(if sun {
                    "sunset".into()
                } else if reverse {
                    "sunrise".into()
                } else {
                    legacy_time(r.get_ci("EndTime"))
                }),
            )?;
        }
        _ => {}
    }
    if matches!(t.kind, TriggerKind::Time | TriggerKind::TimeRange) {
        let days = r
            .get_ci("Days")
            .and_then(JsonNode::as_array)
            .unwrap_or(&[])
            .iter()
            .map(|day| weekday_number(day).map(|d| d.to_string()))
            .collect::<jiks_core::CoreResult<Vec<_>>>()?
            .join(", ");
        p.set("days", s(days))?;
    }
    let mut ui = node(json!({"type":id}))?;
    ui.set("parameters", p)?;
    ui.set("legacy", t.raw().clone())?;
    Ok(ui)
}

#[derive(Debug, Clone)]
pub struct SemanticCommand {
    pub command: String,
    pub payload: JsonNode,
}
#[derive(Debug, Clone)]
pub enum PlannedAction {
    /// Preview only. The executor requires a step-scoped verified state read
    /// before materializing a Toggle, even if an initial cache has a value.
    Deferred { step:StepDto },
    Command(SemanticCommand),
    Delay {
        milliseconds: u64,
    },
    Run {
        options: RunOptions,
        working_directory: Option<String>,
    },
    Notification {
        text: Option<String>,
        title: Option<String>,
        overrides: JsonNode,
    },
    PlaySound {
        path: Option<String>,
    },
    ImportSpectrumProfile {
        path: Option<String>,
    },
}
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub enum StateSource {Configuration,Hardware}
pub fn toggle_state_key(step:&StepDto)->Result<Option<(StateSource,&'static str)>> {
    let (source,key)=match step.kind {
        StepKind::Macro=>(StateSource::Configuration,"macro.enabled"),
        StepKind::Osd=>(StateSource::Configuration,"osd.enabled"),
        StepKind::OsdLockPosition=>(StateSource::Configuration,"osd.locked"),
        StepKind::FanMaxSpeed=>(StateSource::Hardware,"fan.full_speed"),
        StepKind::AiEngine=>(StateSource::Configuration,"power.ai"),
        StepKind::AirplaneMode=>(StateSource::Hardware,"network.airplane"),
        StepKind::Wifi=>(StateSource::Hardware,"network.wifi"),
        StepKind::OverclockDiscreteGpu=>(StateSource::Configuration,"gpu.oc.enabled"),
        _=>return Ok(None),
    };
    Ok(matches!(step.feature_state()?,FeatureState::Enum(2)).then_some((source,key)))
}
fn command(name: &str, key: &str, value: Value) -> Result<PlannedAction> {
    Ok(PlannedAction::Command(SemanticCommand {
        command: name.into(),
        payload: node(if key.is_empty() {
            json!({})
        } else {
            json!({key:value})
        })?,
    }))
}
fn target_bool(state: i32, key: &str, values: &BTreeMap<String, Value>) -> Result<bool> {
    Ok(match state {
        0 => false,
        1 => true,
        2 => !values
            .get(key)
            .and_then(Value::as_bool)
            .with_context(|| format!("Toggle requires current {key}"))?,
        _ => bail!("Unsupported toggle"),
    })
}
pub fn translate_step(step: &StepDto, values: &BTreeMap<String, Value>) -> Result<PlannedAction> {
    match step.action_request()? {
        ActionRequest::Delay { seconds } => {
            let ms =
                step.field("JiksDelayMilliseconds")
                    .map_or(Ok(u64::from(seconds) * 1000), |n| {
                        n.as_i64()
                            .and_then(|n| u64::try_from(n).ok())
                            .context("Jiks delay milliseconds")
                    })?;
            Ok(PlannedAction::Delay { milliseconds: ms })
        }
        ActionRequest::Run(options) => Ok(PlannedAction::Run {
            options,
            working_directory: step
                .field("JiksWorkingDirectory")
                .and_then(JsonNode::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        }),
        ActionRequest::Notification { text, overrides } => Ok(PlannedAction::Notification {
            text,
            title: step
                .field("JiksTitle")
                .and_then(JsonNode::as_str)
                .map(str::to_owned),
            overrides,
        }),
        ActionRequest::PlaySound { path } => Ok(PlannedAction::PlaySound { path }),
        ActionRequest::ImportSpectrumProfile { path } => {
            Ok(PlannedAction::ImportSpectrumProfile { path })
        }
        ActionRequest::GodModePreset { id } => {
            command("preset.apply", "selection.presets", json!(id))
        }
        ActionRequest::CycleGodModePreset => command("preset.cycle", "", Value::Null),
        ActionRequest::DisplayBrightness(v) => {
            command("display.apply", "display.brightness", json!(v))
        }
        ActionRequest::SpeakerVolume(v) => command("keys.apply", "speaker.volume", json!(v)),
        ActionRequest::SpectrumBrightness(v) => {
            command("keyboard.apply", "keyboard.brightness", json!(v))
        }
        ActionRequest::SpectrumProfile(v) => {
            command("keyboard.apply", "keyboard.profile", json!(v))
        }
        ActionRequest::CloseApp => command("app.close", "", Value::Null),
        ActionRequest::ShowApp => command("app.show", "", Value::Null),
        ActionRequest::TurnOffMonitors => command("display.turn_off", "", Value::Null),
        ActionRequest::QuickAction { .. } => {
            bail!("QuickAction must be graph-expanded before translation")
        }
        ActionRequest::Feature { kind, state } => {
            if let FeatureState::DpiScale(scale) = state {
                return command("display.apply", "display.dpi", json!(scale));
            }
            if let FeatureState::Resolution { width, height } = state {
                return command(
                    "display.apply",
                    "display.resolution",
                    json!({"width":width,"height":height}),
                );
            }
            if let FeatureState::RefreshRate {
                frequency,
                is_dynamic,
                base_frequency,
            } = state
            {
                return command("display.apply", "display.refresh_rate", if is_dynamic {
                    json!({"Frequency":frequency,"IsDynamic":true,"BaseFrequency":base_frequency})
                }else{json!(frequency)});
            }
            let FeatureState::Enum(state) = state else {
                bail!("Feature enum expected")
            };
            let (route, key, is_bool) = match kind {
                StepKind::PowerMode => {
                    return command("power.apply", "power.mode", json!(power_to_ui(state)?));
                }
                StepKind::ItsMode => bail!("ITS is unavailable on target 83NN"),
                StepKind::Battery => ("battery.apply", "battery.mode", false),
                StepKind::BatteryNightCharge => {
                    return command("battery.apply", "battery.night_charge", json!(state == 0));
                }
                StepKind::AlwaysOnUsb => ("power.accessories.apply", "power.always_on_usb", false),
                StepKind::InstantBoot => ("power.accessories.apply", "power.instant_boot", false),
                StepKind::FlipToStart => ("power.accessories.apply", "power.flip_to_start", true),
                StepKind::HybridMode => ("gpu.hybrid.apply", "gpu.hybrid_mode", false),
                StepKind::FnLock => ("keys.apply", "keys.fn_lock", true),
                StepKind::WinKey => ("keys.apply", "keys.win_lock", true),
                StepKind::TouchpadLock => ("keys.apply", "keys.touchpad_lock", true),
                StepKind::Microphone => {
                    // Legacy Off means the microphone is muted; the UI key
                    // explicitly describes mute, rather than enabled state.
                    return command("keys.apply","keys.microphone_muted",json!(state==0));
                }
                StepKind::Speaker => {
                    return command("keys.apply", "speaker.muted", json!(state == 0));
                }
                StepKind::HardwareSensors => ("app.preferences.save", "sensors.enabled", true),
                StepKind::Macro => ("macro.settings.save", "macro.enabled", true),
                StepKind::Osd => ("osd.apply", "osd.enabled", true),
                StepKind::OsdLockPosition => ("osd.apply", "osd.locked", true),
                StepKind::Wifi => ("network.apply", "network.wifi", true),
                StepKind::AirplaneMode => ("network.apply", "network.airplane", true),
                StepKind::Hdr => ("display.apply", "display.hdr", true),
                StepKind::AutoColorManagement => ("display.apply", "display.acm", true),
                StepKind::OverDrive => ("display.apply", "display.overdrive", true),
                StepKind::OverclockDiscreteGpu => ("gpu.oc.apply", "gpu.oc.enabled", true),
                StepKind::AiEngine => ("power.apply", "power.ai", true),
                StepKind::FanMaxSpeed => {
                    return Ok(PlannedAction::Command(SemanticCommand {
                        command: "preset.apply".into(),
                        payload: node(
                            json!({"preset":{"values":{},"fan":{"full_speed":target_bool(state,"fan.full_speed",values)?}}}),
                        )?,
                    }));
                }
                StepKind::OneLevelWhiteKeyboardBacklight => ("keys.apply","keyboard.white_single",true),
                StepKind::WhiteKeyboardBacklight => {
                    ("keys.apply", "keyboard.white_level", false)
                }
                StepKind::RgbKeyboardBacklight => ("keyboard.apply", "keyboard.profile", false),
                StepKind::PortsBacklight => ("keys.apply", "keyboard.ports", true),
                StepKind::PanelLogoBacklight => ("keys.apply", "keyboard.logo", true),
                StepKind::DeactivateGpu => {
                    return match state {
                        1 => command("gpu.restart.automation", "", Value::Null),
                        0 => command("gpu.kill_apps.automation", "", Value::Null),
                        _ => bail!("未知独显自动化动作"),
                    };
                }
                _ => bail!("No semantic route for {kind:?}"),
            };
            command(
                route,
                key,
                if is_bool {
                    json!(target_bool(state, key, values)?)
                } else {
                    json!(state)
                },
            )
        }
    }
}
#[derive(Debug, Clone)]
pub struct PipelinePlan {
    pub id: String,
    pub environment: AutomationEnvironment,
    /// Owned graph-expanded sources, in action order. The executor translates
    /// again at actual dispatch so a Toggle sees the preceding readback.
    pub source_steps: Vec<StepDto>,
    pub actions: Vec<PlannedAction>,
}
pub fn plan_pipeline(
    pipeline: &PipelineDto,
    pipelines: &[PipelineDto],
    values: &BTreeMap<String, Value>,
) -> Result<PipelinePlan> {
    let steps = expand_pipeline(pipeline, pipelines)?;
    let environment = pipeline_environment(pipeline)?;
    let mut actions = Vec::new();
    let mut source_steps = Vec::new();
    for step in steps {
        if step.kind == StepKind::QuickAction
            || environment.startup && step.is_dangerous_on_startup()
        {
            continue;
        }
        actions.push(if toggle_state_key(&step)?.is_some() {
            PlannedAction::Deferred{step:step.clone()}
        }else{translate_step(&step, values)?});
        source_steps.push(step);
    }
    Ok(PipelinePlan {
        id: pipeline.id.clone(),
        environment,
        source_steps,
        actions,
    })
}
/// Native callbacks only enqueue owned events. The single runtime consumer plans
/// and executes them later. Overflow is explicit, never an unbounded backlog.
#[derive(Default)]
pub struct EventQueue {
    queue: VecDeque<(Option<u64>, AutomationEvent)>,
}
impl EventQueue {
    pub fn enqueue(&mut self, origin: Option<u64>, event: AutomationEvent) -> Result<()> {
        ensure!(
            self.queue.len() < 128,
            "Automation event queue capacity exceeded"
        );
        self.queue.push_back((origin, event));
        Ok(())
    }
    pub fn next_event(&mut self) -> Option<(Option<u64>, AutomationEvent)> {
        self.queue.pop_front()
    }
    pub fn len(&self) -> usize {
        self.queue.len()
    }
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueDisposition {
    Accepted,
    LateIgnored,
}
#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub token: u64,
    pub pipeline: PipelinePlan,
    pub origin: Option<u64>,
    pub request_id:Option<u64>,
}
enum Job {
    Event(AutomationEvent),
    Manual {pipeline_id:String,request_id:Option<u64>},
}
struct Cursor {
    event: AutomationEvent,
    index: usize,
    origin: Option<u64>,
}
struct Active {
    token: u64,
    origin: Option<u64>,
}
/// Async-safe coordination without owning async IO: the runtime owns an issued
/// plan until complete_plan confirms ALL actual step completions/readbacks.
pub struct Engine {
    document: AutomationDocument,
    state: AutomationState,
    queue: VecDeque<Job>,
    cursor: Option<Cursor>,
    active: Option<Active>,
    startup: Option<StartupBatch<AutomationEvent>>,
    closed_origin: u64,
    initialized: bool,
    startup_complete: bool,
    startup_failed: bool,
    next_token: u64,
    failures: VecDeque<String>,
    failed_manuals:VecDeque<(Option<u64>,String)>,
}
impl Engine {
    pub fn new(document: AutomationDocument) -> Self {
        Self {
            document,
            state: AutomationState::default(),
            queue: VecDeque::new(),
            cursor: None,
            active: None,
            startup: None,
            closed_origin: 0,
            initialized: false,
            startup_complete: false,
            startup_failed: false,
            next_token: 1,
            failures: VecDeque::new(),
            failed_manuals:VecDeque::new(),
        }
    }
    pub fn initialize(&mut self) -> Result<()> {
        ensure!(
            !self.is_busy(),
            "Cannot initialize while an actual automation plan/event is pending"
        );
        if self.document.enabled {
            self.document.validate_execution()?;
        }
        self.initialized = true;
        self.startup_complete = false;
        self.startup_failed = false;
        Ok(())
    }
    pub fn set_document(&mut self, document: AutomationDocument) -> Result<()> {
        ensure!(
            self.active.is_none() && self.startup.is_none() && self.cursor.is_none(),
            "Finish the active automation batch before replacing its document"
        );
        if document.enabled {
            document.validate_execution()?;
        }
        self.document = document;
        Ok(())
    }
    pub fn update_state(&mut self, state: AutomationState) {
        self.state = state;
    }
    pub fn start_startup(&mut self, batch_id: u64, state: AutomationState) -> Result<()> {
        ensure!(
            self.initialized && !self.startup_failed,
            "Successful initialization required before startup"
        );
        ensure!(
            !self.is_busy() && batch_id > self.closed_origin && batch_id > 0,
            "Startup requires a fresh origin and no pending IO/events"
        );
        self.state = state;
        self.startup_complete = false;
        let mut batch = StartupBatch::new(batch_id);
        if self.document.enabled {
            batch.enqueue_from(Some(batch_id), AutomationEvent::Startup);
        }
        self.startup = Some(batch);
        Ok(())
    }
    pub fn on_event(
        &mut self,
        state: AutomationState,
        event: AutomationEvent,
        origin: Option<u64>,
    ) -> Result<EnqueueDisposition> {
        if origin.is_some_and(|origin| origin > 0 && origin <= self.closed_origin) {
            return Ok(EnqueueDisposition::LateIgnored);
        }
        ensure!(
            self.initialized,
            "Initialize automation before admitting events"
        );
        self.state = state;
        if let Some(batch) = &mut self.startup {
            match batch.enqueue_from(origin, event) {
                EnqueueResult::Accepted => return Ok(EnqueueDisposition::Accepted),
                EnqueueResult::LateIgnored => return Ok(EnqueueDisposition::LateIgnored),
                EnqueueResult::Foreign(event) => return self.enqueue_job(Job::Event(event)),
            }
        }
        // Unknown origins must not be silently reclassified as independent
        // runtime callers; the owning startup batch may already have closed.
        ensure!(
            origin.is_none(),
            "Automation event has no matching startup origin"
        );
        self.enqueue_job(Job::Event(event))
    }
    fn enqueue_job(&mut self, job: Job) -> Result<EnqueueDisposition> {
        ensure!(
            self.queue.len() < 128,
            "Automation runtime queue capacity exceeded"
        );
        self.queue.push_back(job);
        Ok(EnqueueDisposition::Accepted)
    }
    pub fn request_manual(&mut self, id: &str) -> Result<()> {
        self.request_manual_correlated(id,None)
    }
    pub fn request_manual_correlated(&mut self,id:&str,request_id:Option<u64>)->Result<()> {
        ensure!(self.failed_manuals.len()<128,"Consume failed manual results before admitting more jobs");
        let id = normalize_guid(id)?;
        ensure!(
            self.document.pipelines.iter().any(|p| p.id == id),
            "Manual pipeline ID not found"
        );
        self.enqueue_job(Job::Manual {pipeline_id:id,request_id})?;
        Ok(())
    }
    pub fn take_failed_manual(&mut self)->Option<(Option<u64>,String)> {self.failed_manuals.pop_front()}
    pub fn startup_autostart_allowed(&self) -> bool {
        self.initialized && self.startup_complete && !self.startup_failed && self.startup.is_none()
    }
    pub fn is_busy(&self) -> bool {
        self.active.is_some()
            || self.cursor.is_some()
            || self.startup.is_some()
            || !self.queue.is_empty()
    }
    pub fn failures(&self) -> &VecDeque<String> {
        &self.failures
    }
    fn remember(&mut self, message: String) {
        self.failures.push_back(message);
        while self.failures.len() > 128 {
            self.failures.pop_front();
        }
    }
    fn fail_startup(&mut self, error: &anyhow::Error) {
        if let Some(mut batch) = self.startup.take() {
            self.closed_origin = self.closed_origin.max(batch.id());
            batch.close();
        }
        self.cursor = None;
        self.startup_failed = true;
        self.startup_complete = false;
        self.remember(format!("Startup failed: {error:#}"));
    }
    fn issue(
        &mut self,
        pipeline: &PipelineDto,
        origin: Option<u64>,
        values: &BTreeMap<String, Value>,
    ) -> Result<ExecutionPlan> {
        let plan = plan_pipeline(pipeline, &self.document.pipelines, values)?;
        let token = self.next_token;
        self.next_token = self
            .next_token
            .checked_add(1)
            .context("Automation token overflow")?;
        self.active = Some(Active { token, origin });
        Ok(ExecutionPlan {
            token,
            pipeline: plan,
            origin,
            request_id:None,
        })
    }
    pub fn next_plan(&mut self, values: &BTreeMap<String, Value>) -> Result<Option<ExecutionPlan>> {
        if self.active.is_some() {
            return Ok(None);
        }
        loop {
            if let Some(mut cursor) = self.cursor.take() {
                while cursor.index < self.document.pipelines.len() {
                    let pipeline = self.document.pipelines[cursor.index].clone();
                    cursor.index += 1;
                    if pipeline
                        .to_json_node()
                        .get_ci("JiksEnabled")
                        .and_then(JsonNode::as_bool)
                        == Some(false)
                    {
                        continue;
                    }
                    if matches!(cursor.event, AutomationEvent::Startup)
                        && !pipeline.run_on_startup
                        && !pipeline
                            .trigger
                            .as_ref()
                            .is_some_and(|t| t.kind == TriggerKind::OnStartup)
                    {
                        continue;
                    }
                    let Some(trigger) = &pipeline.trigger else {
                        continue;
                    };
                    let matched = match trigger.matches_event(&cursor.event, &self.state) {
                        Ok(matched) => matched,
                        Err(error) => {
                            let error: anyhow::Error = error.into();
                            if cursor.origin.is_some() {
                                self.fail_startup(&error);
                                return Err(error);
                            }
                            self.remember(error.to_string());
                            continue;
                        }
                    };
                    if !matched {
                        continue;
                    }
                    let origin = cursor.origin;
                    if !pipeline.is_exclusive {
                        self.cursor = Some(cursor);
                    }
                    match self.issue(&pipeline, origin, values) {
                        Ok(plan) => return Ok(Some(plan)),
                        Err(error) => {
                            if origin.is_some() {
                                self.fail_startup(&error);
                                return Err(error);
                            }
                            self.remember(error.to_string());
                            if pipeline.is_exclusive {
                                return Ok(None);
                            }
                            break;
                        }
                    }
                }
                // If a nonexclusive failed planner restored the cursor, resume it.
                if self.cursor.is_some() {
                    continue;
                }
            }
            let (event, origin) = if let Some(batch) = &mut self.startup {
                let id = batch.id();
                match batch.next_event() {
                    Ok(Some(event)) => (Some(event), Some(id)),
                    Ok(None) => {
                        self.closed_origin = self.closed_origin.max(id);
                        self.startup = None;
                        self.startup_complete = true;
                        (None, None)
                    }
                    Err(error) => {
                        let error: anyhow::Error = error.into();
                        self.fail_startup(&error);
                        return Err(error);
                    }
                }
            } else {
                match self.queue.pop_front() {
                    Some(Job::Event(event)) => (Some(event), None),
                    Some(Job::Manual{pipeline_id:id,request_id}) => {
                        let pipeline = self
                            .document
                            .pipelines
                            .iter()
                            .find(|p| p.id == id)
                            .cloned();
                        let Some(pipeline)=pipeline else {
                            self.failed_manuals.push_back((request_id,"Queued manual pipeline was removed".into()));
                            bail!("Queued manual pipeline was removed");
                        };
                        return match self.issue(&pipeline,None,values) {
                            Ok(mut plan)=>{plan.request_id=request_id;Ok(Some(plan))},
                            Err(error)=>{self.failed_manuals.push_back((request_id,format!("{error:#}")));Err(error)},
                        };
                    }
                    None => return Ok(None),
                }
            };
            if let Some(event) = event {
                if self.document.enabled {
                    self.cursor = Some(Cursor {
                        event,
                        index: 0,
                        origin,
                    });
                }
            } else if self.queue.is_empty() {
                return Ok(None);
            }
        }
    }
    pub fn complete_plan(
        &mut self,
        token: u64,
        failures: Vec<String>,
        emitted: Vec<AutomationEvent>,
    ) -> Result<()> {
        ensure!(
            self.active
                .as_ref()
                .is_some_and(|active| active.token == token),
            "Completion belongs to a different physical automation plan"
        );
        let active = self.active.take().unwrap();
        if active.origin.is_some() && !failures.is_empty() {
            let error = anyhow::anyhow!(failures.join("; "));
            self.fail_startup(&error);
            return Err(error);
        }
        for failure in failures {
            self.remember(failure);
        }
        for event in emitted {
            self.on_event(self.state.clone(), event, active.origin)?;
        }
        Ok(())
    }
    /// Shutdown stops admitting events but never discards an issued physical
    /// plan. Its owner must stop/finish actual steps and acknowledge first.
    pub fn stop_accepting(&mut self) -> Result<()> {
        ensure!(
            self.active.is_none(),
            "An automation plan still owns actual IO"
        );
        self.queue.clear();
        self.cursor = None;
        if let Some(mut batch) = self.startup.take() {
            self.closed_origin = self.closed_origin.max(batch.id());
            batch.close();
            self.startup_failed = true;
        }
        self.startup_complete = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ui(v: Value) -> JsonNode {
        node(v).unwrap()
    }
    fn id(n: u8) -> String {
        format!("00000000-0000-4000-8000-{n:012x}")
    }
    fn delay() -> StepDto {
        step_from_ui(&ui(json!({"type":"delay","parameters":{"milliseconds":0}}))).unwrap()
    }
    fn trigger(kind: TriggerKind) -> TriggerDto {
        TriggerDto::new(kind, JsonNode::object()).unwrap()
    }
    fn pipeline(n: u8, kind: TriggerKind) -> PipelineDto {
        let mut p = PipelineDto::new(&id(n), Some(trigger(kind)), vec![delay()]).unwrap();
        p.is_exclusive = false;
        p
    }
    fn document(enabled: bool, pipelines: Vec<PipelineDto>) -> AutomationDocument {
        let raw = ui(
            json!({"IsEnabled":enabled,"Pipelines":pipelines.iter().map(|p|p.to_json_node().to_value().unwrap()).collect::<Vec<_>>()}),
        );
        AutomationDocument::parse(&raw.to_json(), || id(99)).unwrap()
    }
    #[test]
    fn catalog_covers_every_core_step_and_trigger_once() {
        assert_eq!(STEP_TYPES.len(), 49);
        assert_eq!(TRIGGER_TYPES.len(), 34);
        for kind in StepKind::ALL {
            assert_eq!(STEP_TYPES.iter().filter(|(_, k)| k == kind).count(), 1);
        }
        for kind in TriggerKind::ALL {
            assert_eq!(TRIGGER_TYPES.iter().filter(|(_, k)| k == kind).count(), 1);
        }
    }
    #[test]
    fn exact_millisecond_delay_and_run_extensions_roundtrip() {
        let step = step_from_ui(&ui(
            json!({"type":"delay","parameters":{"milliseconds":175}}),
        ))
        .unwrap();
        assert!(matches!(
            translate_step(&step, &BTreeMap::new()).unwrap(),
            PlannedAction::Delay { milliseconds: 175 }
        ));
        let step2 = step_from_ui(&step_to_ui(&step).unwrap()).unwrap();
        assert_eq!(
            step.action_request().unwrap(),
            step2.action_request().unwrap()
        );
        assert_eq!(
            step.field("JiksDelayMilliseconds"),
            step2.field("JiksDelayMilliseconds")
        );
        let run=step_from_ui(&ui(json!({"type":"run","parameters":{"path":"fixture.exe","arguments":"--fixture","working_directory":"D:\\fixture","wait":true}}))).unwrap();
        assert!(
            matches!(translate_step(&run,&BTreeMap::new()).unwrap(),PlannedAction::Run{options,working_directory:Some(cwd)} if options.wait_until_finished&&cwd=="D:\\fixture")
        );
    }
    #[test]
    fn known_ui_edits_preserve_step_and_composite_extensions() {
        let mut step = step_to_ui(&delay()).unwrap();
        let mut raw = step.get("legacy").unwrap().clone();
        raw.set("Future", JsonNode::Number("1e9999".into()))
            .unwrap();
        step.set("legacy", raw).unwrap();
        assert!(
            step_from_ui(&step)
                .unwrap()
                .raw()
                .to_json()
                .contains("1e9999")
        );
        let t=trigger_from_ui(&ui(json!({"type":"all","conditions":[{"type":"ac_connected","parameters":{}},{"type":"wifi_connected","parameters":{"ssids":"a,b"}}]}))).unwrap().unwrap();
        let mut projected = trigger_to_ui(Some(&t)).unwrap();
        let mut raw = projected.get("legacy").unwrap().clone();
        raw.set("Future", JsonNode::Number("-0".into())).unwrap();
        projected.set("legacy", raw).unwrap();
        assert!(
            trigger_from_ui(&projected)
                .unwrap()
                .unwrap()
                .raw()
                .get("Future")
                .is_some()
        );
    }
    #[test]
    fn weekday_names_and_fractional_period_survive_unchanged_ui_fields() {
        let t = TriggerDto::new(
            TriggerKind::Time,
            ui(json!({"Time":{"Hour":1,"Minute":2,"Second":3},"Days":["Monday",6]})),
        )
        .unwrap();
        let projected = trigger_to_ui(Some(&t)).unwrap();
        assert_eq!(
            projected
                .get_path(&["parameters", "days"])
                .and_then(JsonNode::as_str),
            Some("1, 6")
        );
        trigger_from_ui(&projected).unwrap();
        let t = TriggerDto::new(
            TriggerKind::Periodic,
            ui(json!({"Period":"00:00:00.5000001"})),
        )
        .unwrap();
        let back = trigger_from_ui(&trigger_to_ui(Some(&t)).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(back.field("Period"), t.field("Period"));
    }
    #[test]
    fn common_semantic_routes_pass_actual_pure_hardware_planner() {
        for (kind, params) in [
            ("speaker", json!({"value":true})),
            ("volume", json!({"value":50})),
            ("fn_lock", json!({"value":true})),
            ("ports_light", json!({"value":true})),
            ("panel_logo", json!({"value":false})),
            ("white_keyboard", json!({"value":2})),
            ("display_brightness", json!({"value":50})),
            ("fan_max", json!({"value":true})),
            ("refresh_rate", json!({"value":165})),
        ] {
            let step = step_from_ui(&ui(json!({"type":kind,"parameters":params}))).unwrap();
            let PlannedAction::Command(command) = translate_step(&step, &BTreeMap::new()).unwrap()
            else {
                panic!()
            };
            let mut payload = command.payload.to_value().unwrap();
            payload["_context"] = json!({"keyboard":{"white_backend":"energy"},"display":{"device":"\\\\.\\DISPLAY1","monitor_path":"\\\\?\\DISPLAY#fixture","width":2560,"height":1600,"internal":true}});
            assert!(
                crate::hardware_routes::plan(&command.command, &payload).is_ok(),
                "{} {payload}",
                command.command
            );
        }
    }
    #[test]
    fn power_ui_firmware_enum_is_converted_without_sending_legacy_custom254() {
        for (raw, legacy) in [(1, 0), (2, 1), (3, 2), (255, 254)] {
            let step =
                step_from_ui(&ui(json!({"type":"power_mode","parameters":{"value":raw}}))).unwrap();
            assert_eq!(step.field("State").unwrap().as_i64(), Some(legacy));
            let PlannedAction::Command(c) = translate_step(&step, &BTreeMap::new()).unwrap() else {
                panic!()
            };
            assert_eq!(c.payload.get("power.mode").unwrap().as_i64(), Some(raw));
        }
    }
    #[test]
    fn legacy_boolean_feature_truth_tables_match_original_get_set_and_ui_labels() {
        // C# Microphone/Speaker setters set Mute for Off/Mute(0); WinKey's
        // dashboard is titled 'disable Win key', On(1) means locked. Touchpad
        // On is also the lock, FnLock On is logical on (platform owns inversion).
        for (kind,key,off,on) in [
            ("microphone","keys.microphone_muted",true,false),
            ("speaker","speaker.muted",true,false),
            ("win_key","keys.win_lock",false,true),
            ("touchpad","keys.touchpad_lock",false,true),
            ("fn_lock","keys.fn_lock",false,true),
            // BatteryNightCharge enum is declared On=0, Off=1.
            ("battery_night","battery.night_charge",true,false),
        ] {
            for (state,expected) in [(0,off),(1,on)] {
                let step=step_from_ui(&ui(json!({"type":kind,"parameters":{"value":state}}))).unwrap();
                let PlannedAction::Command(action)=translate_step(&step,&BTreeMap::new()).unwrap() else{panic!()};
                assert_eq!(action.payload.get(key).and_then(JsonNode::as_bool),Some(expected),"{kind} state={state}");
            }
        }
        for state in [0,1] {
            let step=step_from_ui(&ui(json!({"type":"white_single","parameters":{"value":state}}))).unwrap();
            let PlannedAction::Command(action)=translate_step(&step,&BTreeMap::new()).unwrap() else{panic!()};
            assert_eq!(action.payload.get("keyboard.white_single").and_then(JsonNode::as_bool),Some(state==1));
        }
    }
    #[test]
    fn startup_success_waits_for_actual_ack_and_all_descendants() {
        let p1 = pipeline(1, TriggerKind::OnStartup);
        let p2 = pipeline(2, TriggerKind::HdrOff);
        let mut engine = Engine::new(document(true, vec![p1, p2]));
        engine.initialize().unwrap();
        engine.start_startup(1, AutomationState::default()).unwrap();
        let first = engine.next_plan(&BTreeMap::new()).unwrap().unwrap();
        assert!(!engine.startup_autostart_allowed());
        assert!(engine.next_plan(&BTreeMap::new()).unwrap().is_none());
        assert!(!engine.startup_autostart_allowed());
        engine
            .complete_plan(first.token, vec![], vec![AutomationEvent::Hdr(Some(false))])
            .unwrap();
        let child = engine.next_plan(&BTreeMap::new()).unwrap().unwrap();
        assert_eq!(child.origin, Some(1));
        assert!(!engine.startup_autostart_allowed());
        engine.complete_plan(child.token, vec![], vec![]).unwrap();
        assert!(engine.next_plan(&BTreeMap::new()).unwrap().is_none());
        assert!(engine.startup_autostart_allowed());
    }
    #[test]
    fn failed_startup_discards_pending_descendants_and_ignores_closed_late_events() {
        let mut engine = Engine::new(document(
            true,
            vec![
                pipeline(1, TriggerKind::OnStartup),
                pipeline(2, TriggerKind::HdrOff),
            ],
        ));
        engine.initialize().unwrap();
        engine.start_startup(7, AutomationState::default()).unwrap();
        let active = engine.next_plan(&BTreeMap::new()).unwrap().unwrap();
        assert!(
            engine
                .complete_plan(
                    active.token,
                    vec!["fake physical failure".into()],
                    vec![AutomationEvent::Hdr(Some(false))]
                )
                .is_err()
        );
        assert!(!engine.startup_autostart_allowed());
        assert!(engine.next_plan(&BTreeMap::new()).unwrap().is_none());
        assert_eq!(
            engine
                .on_event(
                    AutomationState::default(),
                    AutomationEvent::Hdr(Some(false)),
                    Some(7)
                )
                .unwrap(),
            EnqueueDisposition::LateIgnored
        );
    }
    #[test]
    fn startup_cumulative_limit_is_not_current_queue_length() {
        let mut engine = Engine::new(document(true, vec![pipeline(1, TriggerKind::OnStartup)]));
        engine.initialize().unwrap();
        engine.start_startup(1, AutomationState::default()).unwrap();
        let p = engine.next_plan(&BTreeMap::new()).unwrap().unwrap();
        engine
            .complete_plan(
                p.token,
                vec![],
                vec![AutomationEvent::Hdr(Some(false)); 128],
            )
            .unwrap();
        assert!(engine.next_plan(&BTreeMap::new()).is_err());
        assert!(!engine.startup_autostart_allowed());
    }
    #[test]
    fn subsequent_runtime_pipeline_matches_after_prior_real_ack_state_change() {
        let first = pipeline(1, TriggerKind::HdrOn);
        let mut second = pipeline(2, TriggerKind::HdrOn);
        let mode =
            TriggerDto::new(TriggerKind::PowerMode, ui(json!({"PowerModeState":1}))).unwrap();
        second.trigger = Some(
            TriggerDto::composite(TriggerKind::And, vec![trigger(TriggerKind::HdrOn), mode])
                .unwrap(),
        );
        let mut engine = Engine::new(document(true, vec![first, second]));
        engine.initialize().unwrap();
        let mut state = AutomationState {
            power_mode: Some(0),
            ..Default::default()
        };
        engine
            .on_event(state.clone(), AutomationEvent::Hdr(Some(true)), None)
            .unwrap();
        let p = engine.next_plan(&BTreeMap::new()).unwrap().unwrap();
        state.power_mode = Some(1);
        engine.update_state(state);
        engine
            .complete_plan(p.token, vec!["runtime step failed".into()], vec![])
            .unwrap();
        let p = engine.next_plan(&BTreeMap::new()).unwrap().unwrap();
        assert_eq!(p.pipeline.id, id(2));
        engine.complete_plan(p.token, vec![], vec![]).unwrap();
        assert!(!engine.failures().is_empty());
    }
    #[test]
    fn wrong_ack_and_shutdown_never_release_an_issued_plan() {
        let mut engine = Engine::new(document(false, vec![pipeline(1, TriggerKind::HdrOn)]));
        engine.initialize().unwrap();
        engine.request_manual(&id(1)).unwrap();
        let p = engine.next_plan(&BTreeMap::new()).unwrap().unwrap();
        assert!(engine.complete_plan(p.token + 1, vec![], vec![]).is_err());
        assert!(engine.stop_accepting().is_err());
        assert!(engine.is_busy());
        engine.complete_plan(p.token, vec![], vec![]).unwrap();
        engine.stop_accepting().unwrap();
        assert!(!engine.is_busy());
    }
    #[test]
    fn extreme_mode_maps_raw224_to_legacy223() {
        assert_eq!(power_to_legacy(224).unwrap(), 223);
        assert_eq!(power_to_ui(223).unwrap(), 224);
    }
    #[test]
    fn legacy_extreme_action_and_string_trigger_project_roundtrip_and_plan_without_activation() {
        let raw=JsonNode::parse(r#"{"$type":"LenovoLegionToolkit.Lib.Automation.Steps.PowerModeAutomationStep, LenovoLegionToolkit.Lib.Automation","State":223,"Future":1e9999}"#).unwrap();
        let step=StepDto::parse(raw).unwrap();let ui=step_to_ui(&step).unwrap();
        let restored=step_from_ui(&ui).unwrap();assert_eq!(restored.field("State").and_then(JsonNode::as_i64),Some(223));assert!(restored.raw().to_json().contains("1e9999"));
        let PlannedAction::Command(command)=translate_step(&restored,&BTreeMap::new()).unwrap() else{panic!()};assert_eq!(command.payload.get("power.mode").and_then(JsonNode::as_i64),Some(224));
        let trigger=TriggerDto::new(TriggerKind::PowerMode,JsonNode::parse(r#"{"PowerModeState":"Extreme","Future":-0}"#).unwrap()).unwrap();
        let projected=trigger_to_ui(Some(&trigger)).unwrap();let reparsed=trigger_from_ui(&projected).unwrap().unwrap();
        assert_eq!(reparsed.raw().get_ci("PowerModeState").and_then(JsonNode::as_i64),Some(223));assert!(reparsed.raw().to_json().contains("-0"));
        assert_eq!(firmware_power_to_legacy(224).unwrap(),223);
    }
}
