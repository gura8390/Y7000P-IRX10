//! Pure semantic settings edits. All documents validate before the caller writes
//! any file. A returned multi-document change is NOT a multi-file transaction.
use crate::{
    automation_runtime as automation,
    macros::*,
    service::{Choice, Item, Range},
};
use anyhow::{Context, Result, bail, ensure};
use jiks_core::{
    automation::{AutomationDocument, PipelineDto, expand_pipeline, normalize_guid},
    settings::{ApplicationThermalSettings, JsonNode, SettingsDocument},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub type Documents = BTreeMap<String, SettingsDocument>;
pub const NOTIFICATION_TYPES: [&str; 43] = [
    "ACAdapterConnected",
    "ACAdapterConnectedLowWattage",
    "ACAdapterDisconnected",
    "AirplaneModeOn",
    "AirplaneModeOff",
    "AutomationNotification",
    "CameraOn",
    "CameraOff",
    "CapsLockOn",
    "CapsLockOff",
    "FnLockOn",
    "FnLockOff",
    "MicrophoneOff",
    "MicrophoneOn",
    "NumLockOn",
    "NumLockOff",
    "PanelLogoLightingOn",
    "PanelLogoLightingOff",
    "PortLightingOn",
    "PortLightingOff",
    "PowerModeQuiet",
    "PowerModeBalance",
    "PowerModePerformance",
    "PowerModeExtreme",
    "PowerModeGodMode",
    "RefreshRate",
    "RGBKeyboardBacklightChanged",
    "RGBKeyboardBacklightOff",
    "SmartKeyDoublePress",
    "SmartKeySinglePress",
    "SpectrumBacklightChanged",
    "SpectrumBacklightOff",
    "SpectrumBacklightPresetChanged",
    "TouchpadOn",
    "TouchpadOff",
    "UpdateAvailable",
    "WhiteKeyboardBacklightChanged",
    "WhiteKeyboardBacklightChangedSpecial",
    "WhiteKeyboardBacklightOff",
    "ITSModeAuto",
    "ITSModeCool",
    "ITSModePerformance",
    "ITSModeGeek",
];
const NOTIFICATION_FLAGS: &[&str] = &[
    "UpdateAvailable",
    "CapsLock",
    "NumLock",
    "FnLock",
    "TouchpadLock",
    "KeyboardBacklight",
    "CameraLock",
    "AirplaneMode",
    "Microphone",
    "PowerMode",
    "RefreshRate",
    "ACAdapter",
    "SmartKey",
    "AutomationNotification",
    "ITSMode",
];
const NOTIFICATION_TABLES: &[&str] = &[
    "IconOverrides",
    "ColorOverrides",
    "TextColorOverrides",
    "PositionOverrides",
    "DurationOverrides",
];
fn notification_type_known(name: &str) -> bool {
    NOTIFICATION_TYPES
        .iter()
        .any(|known| known.eq_ignore_ascii_case(name))
        || name
            .parse::<usize>()
            .is_ok_and(|index| index < NOTIFICATION_TYPES.len())
}
pub const THERMAL_EDITOR_FIELDS: &[&str] = &[
    "thermal.enabled",
    "thermal.shadow",
    "thermal.sampling_ms",
    "thermal.firmware_limit_c",
];
/// Shared by the real native editor and the coordinator contract fixture. The
/// editor's opening baseline distinguishes user edits from concurrent changes.
pub fn sensor_editor_command(
    baseline: &Value,
    current: &Value,
    mut payload: Value,
    thermal_allowed: bool,
) -> Result<(&'static str, Value)> {
    fn same(a: &Value, b: &Value) -> bool {
        if a.is_number() && b.is_number() {
            a.as_f64() == b.as_f64()
        } else {
            a == b
        }
    }
    let changed = THERMAL_EDITOR_FIELDS.iter().any(|key| {
        payload
            .get(*key)
            .is_some_and(|value| !same(value, &baseline[*key]))
    });
    if changed {
        ensure!(
            THERMAL_EDITOR_FIELDS
                .iter()
                .all(|key| same(&current[*key], &baseline[*key])),
            "温控合同已由其他操作更改，请重新打开传感器设置后再修改"
        );
        ensure!(thermal_allowed, "当前不允许改变温控合同，请核对后台状态");
        Ok(("thermal.apply", payload))
    } else {
        let map = payload.as_object_mut().context("传感器设置格式错误")?;
        for key in THERMAL_EDITOR_FIELDS {
            map.remove(*key);
        }
        Ok(("sensors.settings.save", payload))
    }
}
/// Pure validation shared by raw saves, normal changes and restore preflight.
/// Unknown extension nodes remain opaque. Known malformed data never reaches
/// disk and no stored type name is instantiated by this function.
pub fn validate_storage_document(document: &str, settings: &SettingsDocument) -> Result<()> {
    use jiks_core::automation::{StepDto, StepKind, TriggerDto, TriggerKind};
    let document = match document {
        "app" | "settings" => "application",
        "macro" => "macros",
        key => key,
    };
    let root = settings.root();
    if document == "application" {
        ApplicationThermalSettings::from_document(&settings.clone().normalized_application()?)?;
    }
    let entries = root.as_object().context("Settings object required")?;
    for (key, entry) in entries {
        if !storage_field_known(document, key) {
            continue;
        }
        if matches!(entry, JsonNode::Null)
            && [
                "Groups",
                "VisibleItems",
                "CardOrder",
                "ExcludedRefreshRates",
                "PowerPlans",
                "PowerModes",
                "IncludedProcesses",
                "ExcludedProcesses",
                "GameDetection",
                "Notifications",
                "HiddenKeys",
                "KeyModes",
                "KeyDescriptions",
                "KeySinglePressActions",
                "KeyDoublePressActions",
            ]
            .contains(&key.as_str())
        {
            continue;
        }
        if [
            "GameDetection",
            "Notifications",
            "PowerPlans",
            "PowerModes",
            "KeyModes",
            "KeyDescriptions",
            "KeySinglePressActions",
            "KeyDoublePressActions",
        ]
        .contains(&key.as_str())
        {
            for (child, value) in entry
                .as_object()
                .context("Typed settings dictionary required")?
            {
                if key == "Notifications"
                    && !NOTIFICATION_FLAGS.contains(&child.as_str())
                    && !NOTIFICATION_TABLES.contains(&child.as_str())
                {
                    continue;
                }
                validate_field(document, &[key.clone(), child.clone()], value)?;
            }
        } else {
            // WPF ColorConverter also permits #AARRGGBB. Preserve valid legacy
            // alpha values while normal structured edits use #RRGGBB.
            if key.ends_with("Color")
                && entry.as_str().is_some_and(|s| {
                    s.len() == 9
                        && s.starts_with('#')
                        && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
                })
            {
                continue;
            }
            validate_field(document, &[key.clone()], entry)?;
        }
    }
    if document == "automation" {
        let parsed = parse_automation(settings)?;
        fn known_trigger(trigger: &TriggerDto, enabled: bool) -> Result<TriggerDto> {
            if trigger.kind == TriggerKind::Unknown {
                ensure!(!enabled, "Unknown future trigger must remain disabled");
                return Ok(TriggerDto::new(TriggerKind::OnStartup, JsonNode::object())?);
            }
            let mut checked = trigger.clone();
            checked.children = checked
                .children
                .iter()
                .map(|child| known_trigger(child, enabled))
                .collect::<Result<_>>()?;
            checked.validate()?;
            Ok(checked)
        }
        let mut graph = parsed.pipelines.clone();
        for pipeline in &mut graph {
            if let Some(trigger) = &pipeline.trigger {
                known_trigger(trigger, parsed.enabled)?;
            }
            for step in &mut pipeline.steps {
                if step.kind == StepKind::Unknown {
                    ensure!(!parsed.enabled, "Unknown future step must remain disabled");
                    *step =
                        StepDto::new(StepKind::Delay, JsonNode::parse("{\"DelaySeconds\":0}")?)?;
                } else {
                    step.validate()?;
                }
            }
        }
        for pipeline in &graph {
            expand_pipeline(pipeline, &graph)?;
        }
    }
    if document == "macros" {
        let macros = MacroDocument::from_settings(settings.clone())?;
        for entry in macros.sequences {
            let mut sequence = entry.sequence;
            ensure!(
                (0..=10).contains(&sequence.repeat_count),
                "Macro repeat count must be 0..10"
            );
            for event in &sequence.events {
                ensure!(event.delay_ticks >= 0, "Negative macro delay is invalid");
            }
            let mut future = false;
            sequence.events.retain(|event| {
                let opaque = matches!(event.source, Source::Future(_))
                    || matches!(event.direction, Direction::Future(_));
                future |= opaque;
                !opaque
            });
            ensure!(
                !future || !macros.enabled,
                "Unknown future macro events must remain disabled"
            );
            sequence.validate_playback()?;
        }
    }
    let projection = project(&BTreeMap::from([(document.to_owned(), settings.clone())]))?;
    ensure!(
        !projection
            .values
            .keys()
            .any(|key| key.ends_with("validation_error")),
        "Malformed typed settings document"
    );
    Ok(())
}
#[derive(Default)]
pub struct UiProjection {
    pub values: BTreeMap<String, Value>,
    pub options: BTreeMap<String, Vec<Choice>>,
    pub collections: BTreeMap<String, Vec<Item>>,
}
pub struct ConfigChange {
    pub documents: Vec<(String, SettingsDocument)>,
    pub projection: UiProjection,
    pub message: String,
}
#[derive(Default)]
pub struct ActionContext {
    pub new_id: Option<String>,
    pub ranges: BTreeMap<String, Range>,
    pub discrete: BTreeMap<String, Vec<i32>>,
}
fn value(v: Value) -> Result<JsonNode> {
    Ok(JsonNode::parse(&serde_json::to_string(&v)?)?)
}
fn number(v: impl ToString) -> JsonNode {
    JsonNode::Number(v.to_string())
}
fn str_node(v: impl Into<String>) -> JsonNode {
    JsonNode::String(v.into())
}
fn obj() -> JsonNode {
    JsonNode::object()
}
fn text<'a>(n: &'a JsonNode, key: &str) -> Result<&'a str> {
    n.get_ci(key)
        .and_then(JsonNode::as_str)
        .with_context(|| format!("Missing {key} string"))
}
fn boolean(n: &JsonNode) -> Result<bool> {
    n.as_bool().context("Boolean setting required")
}
fn integer(n: &JsonNode, min: i64, max: i64) -> Result<i64> {
    let value = n.as_i64().context("Integer setting required")?;
    ensure!((min..=max).contains(&value), "Setting must be {min}..{max}");
    Ok(value)
}
fn real(n: &JsonNode, min: f64, max: f64) -> Result<f64> {
    let value = n
        .as_f64()
        .filter(|v| v.is_finite())
        .context("Finite numeric setting required")?;
    ensure!((min..=max).contains(&value), "Setting must be {min}..{max}");
    Ok(value)
}
fn set_path(root: &mut JsonNode, path: &[&str], entry: JsonNode) -> Result<()> {
    let (key, rest) = path.split_first().context("Empty setting path")?;
    if rest.is_empty() {
        root.set_known(key, entry)?;
    } else {
        let mut child = root.get_ci(key).cloned().unwrap_or_else(obj);
        ensure!(
            child.as_object().is_some(),
            "Setting path parent is not object"
        );
        set_path(&mut child, rest, entry)?;
        root.set_known(key, child)?;
    }
    Ok(())
}
fn remove_path(root: &mut JsonNode, path: &[&str]) {
    if let Some((key, rest)) = path.split_first() {
        if let JsonNode::Object(fields) = root {
            if rest.is_empty() {
                fields.retain(|(name, _)| !name.eq_ignore_ascii_case(key));
            } else if let Some((_, child)) = fields
                .iter_mut()
                .rev()
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
            {
                remove_path(child, rest);
            }
        }
    }
}
fn edit<'a>(
    pending: &'a mut BTreeMap<String, JsonNode>,
    docs: &Documents,
    key: &str,
) -> Result<&'a mut JsonNode> {
    if !pending.contains_key(key) {
        pending.insert(
            key.into(),
            docs.get(key)
                .with_context(|| format!("Missing document {key}"))?
                .root()
                .clone(),
        );
    }
    Ok(pending.get_mut(key).unwrap())
}
fn enum_name<'a>(value: &'a JsonNode, names: &[&'a str]) -> Result<&'a str> {
    if let Some(name) = value.as_str() {
        return names
            .iter()
            .copied()
            .find(|n| n.eq_ignore_ascii_case(name))
            .with_context(|| format!("Unknown enum {name}"));
    }
    let index = integer(value, 0, names.len().saturating_sub(1) as i64)?;
    Ok(names[index as usize])
}
fn enum_ui(value: &JsonNode, choices: &[(&str, &str)]) -> Result<JsonNode> {
    let key = value.as_str().context("Enum setting string required")?;
    Ok(str_node(
        choices
            .iter()
            .find(|(ui, _)| *ui == key)
            .map(|(_, legacy)| *legacy)
            .context("Unsupported setting choice")?,
    ))
}
fn small(value: &JsonNode) -> Result<Value> {
    ensure!(
        value.to_json().len() <= 64 * 1024,
        "Metadata scalar exceeds 64KiB budget"
    );
    Ok(value.to_value()?)
}
const APP_BOOLEANS: &[(&str, &str, bool)] = &[
    ("app.minimize_to_tray", "MinimizeToTray", true),
    ("app.close_to_tray", "MinimizeOnClose", false),
    ("app.always_on_top", "AlwaysOnTop", false),
    ("app.remember_position", "RememberWindowPosition", true),
    ("app.lock_size", "LockWindowSize", false),
    ("app.compact", "CompactMode", false),
    ("app.logging", "EnableLogging", false),
    ("sensors.enabled", "EnableHardwareSensors", false),
    ("sensors.new_dashboard", "UseNewSensorDashboard", false),
    (
        "display.sync_brightness",
        "SynchronizeBrightnessToAllPowerPlans",
        false,
    ),
    (
        "battery.reset_timer_on_boot",
        "ResetBatteryOnSinceTimerOnReboot",
        false,
    ),
];
fn preference(
    pending: &mut BTreeMap<String, JsonNode>,
    docs: &Documents,
    key: &str,
    v: &JsonNode,
) -> Result<()> {
    if let Some((_, legacy, _)) = APP_BOOLEANS.iter().find(|(ui, _, _)| *ui == key) {
        set_path(
            edit(pending, docs, "application")?,
            &[legacy],
            JsonNode::Bool(boolean(v)?),
        )?;
        return Ok(());
    }
    let (document, path, entry) = match key {
        "app.window" => {
            let window_number = |key: &str, min: f64, max: f64| -> Result<f64> {
                let entry = v.get(key).with_context(|| format!("Window {key}"))?;
                ensure!(
                    matches!(entry, JsonNode::Number(_)),
                    "Window {key} must be a number"
                );
                real(entry, min, max)
            };
            let width = window_number("width", 300., 8192.)?;
            let height = window_number("height", 300., 8192.)?;
            let position = match (v.get("left"), v.get("top")) {
                (None, None) => JsonNode::Null,
                (Some(_), Some(_)) => {
                    let left = window_number("left", -100000., 100000.)?;
                    let top = window_number("top", -100000., 100000.)?;
                    value(json!({"Left":left,"Top":top}))?
                }
                _ => bail!("Window left and top must both be present or absent"),
            };
            let size = value(json!({"Width":width,"Height":height}))?;
            let app = edit(pending, docs, "application")?;
            set_path(app, &["WindowSize"], size)?;
            set_path(app, &["WindowPosition"], position)?;
            return Ok(());
        }
        "osd.enabled" => ("osd", vec!["ShowOsd"], JsonNode::Bool(boolean(v)?)),
        "appearance.theme" => (
            "application",
            vec!["Theme"],
            enum_ui(
                v,
                &[("system", "System"), ("light", "Light"), ("dark", "Dark")],
            )?,
        ),
        "appearance.language" => ("application", vec!["JiksLanguage"], {
            let text = v.as_str().context("Language string")?;
            ensure!(
                text.len() <= 32 && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "Invalid language identifier"
            );
            v.clone()
        }),
        "appearance.accent" => ("application", vec!["AccentColor"], {
            let c = v.as_str().context("Accent #RRGGBB")?;
            ensure!(
                c.len() == 7 && c.starts_with('#') && c[1..].bytes().all(|b| b.is_ascii_hexdigit()),
                "Accent must be #RRGGBB"
            );
            value(
                json!({"R":u8::from_str_radix(&c[1..3],16)?,"G":u8::from_str_radix(&c[3..5],16)?,"B":u8::from_str_radix(&c[5..7],16)?}),
            )?
        }),
        "appearance.background" => ("application", vec!["BackGroundImageFilePath"], {
            ensure!(v.as_str().is_some(), "Background path string");
            v.clone()
        }),
        "appearance.opacity" => (
            "application",
            vec!["Opacity"],
            number(real(v, 0., 100.)? / 100.),
        ),
        "appearance.blur" => (
            "application",
            vec!["BackgroundImageBlur"],
            number(integer(v, 0, 100)?),
        ),
        "power.mapping" => (
            "application",
            vec!["PowerModeMappingMode"],
            enum_ui(
                v,
                &[
                    ("disabled", "Disabled"),
                    ("mode", "WindowsPowerMode"),
                    ("plan", "WindowsPowerPlan"),
                ],
            )?,
        ),
        "power.fnq_custom" => (
            "application",
            vec!["JiksFnQCustom"],
            JsonNode::Bool(boolean(v)?),
        ),
        "keys.smart_fn_lock" => (
            "application",
            vec!["SmartFnLockFlags"],
            number(integer(v, 0, 7)?),
        ),
        "game.detection" => ("application", vec!["GameDetection"], {
            ensure!(
                v.as_object().is_some(),
                "Game detection requires flags object"
            );
            for (_, item) in v.as_object().unwrap() {
                boolean(item)?;
            }
            v.clone()
        }),
        "update.channel" => (
            "update",
            vec!["UpdateChannel"],
            enum_ui(
                v,
                &[("stable", "Stable"), ("preview", "Beta"), ("beta", "Beta")],
            )?,
        ),
        "update.method" => (
            "update",
            vec!["UpdateMethod"],
            str_node(enum_name(v, &["GitHub", "Server"])?),
        ),
        "update.interval_hours" => ("update", vec!["UpdateCheckFrequency"], {
            let h = integer(v, 0, 720)?;
            str_node(match h {
                0 => "Never",
                1 => "PerHour",
                3 => "PerThreeHours",
                12 => "PerTwelveHours",
                24 => "PerDay",
                168 => "PerWeek",
                720 => "PerMonth",
                _ => bail!("Legacy update frequency supports 0/1/3/12/24/168/720 hours"),
            })
        }),
        "app.autorun" => bail!(
            "Autorun requires its explicit OS registration route; preferences cannot pretend registry/scheduled-task success"
        ),
        _ => bail!("Unknown preference field {key}"),
    };
    set_path(edit(pending, docs, document)?, &path, entry)
}
/// Generic saves may retain an already enabled controller. They cannot enable
/// it or change its policy/sampling/firmware contract behind the native owner.
pub fn validate_transition(
    document: &str,
    old: &SettingsDocument,
    new: &SettingsDocument,
) -> Result<()> {
    if !["application", "app", "settings"].contains(&document) {
        return Ok(());
    }
    let old_enabled = old
        .root()
        .get_ci("EnableCpuThermalControl")
        .and_then(JsonNode::as_bool)
        .unwrap_or(false);
    let new_enabled = new
        .root()
        .get_ci("EnableCpuThermalControl")
        .and_then(JsonNode::as_bool)
        .unwrap_or(false);
    ensure!(
        old_enabled == new_enabled,
        "Use thermal.apply/thermal.stop to change CPU thermal control activation"
    );
    let old_view = ApplicationThermalSettings::from_document(old)?;
    let new_view = ApplicationThermalSettings::from_document(new)?;
    ensure!(
        old_view.options == new_view.options
            && old_view.shadow_enabled == new_view.shadow_enabled
            && old_view.sampling_ms == new_view.sampling_ms
            && old_view.firmware_limit_c == new_view.firmware_limit_c,
        "CPU thermal policy/sampling/firmware changes require dedicated thermal.apply"
    );
    Ok(())
}
fn parse_macro_event(ui: &JsonNode) -> Result<MacroEvent> {
    if ui.get_ci("Source").is_some() {
        return MacroEvent::from_json(ui);
    }
    let kind = text(ui, "kind")?;
    let old = ui.get("legacy").map(MacroEvent::from_json).transpose()?;
    let ms = ui
        .get_ci("delay_ms")
        .map_or(Ok(0), |n| integer(n, 0, 3_600_000))?;
    let ticks = old
        .as_ref()
        .filter(|e| e.delay_ticks / 10000 == ms)
        .map_or(ms * 10000, |e| e.delay_ticks);
    let key = ui.get_ci("key").and_then(JsonNode::as_i64).unwrap_or(0);
    let x = ui.get_ci("x").and_then(JsonNode::as_i64).unwrap_or(0);
    let y = ui.get_ci("y").and_then(JsonNode::as_i64).unwrap_or(0);
    let (source, direction) = match kind {
        "key_down" => (Source::Keyboard, Direction::Down),
        "key_up" => (Source::Keyboard, Direction::Up),
        "mouse_down" => (Source::Mouse, Direction::Down),
        "mouse_up" => (Source::Mouse, Direction::Up),
        "mouse_move" => (Source::Mouse, Direction::Move),
        "wheel" => (Source::Mouse, Direction::Wheel),
        "horizontal_wheel" => (Source::Mouse, Direction::HorizontalWheel),
        "delay" => (Source::Unknown, Direction::Unknown),
        _ => bail!("Unknown macro event kind"),
    };
    let key = if matches!(direction, Direction::Wheel | Direction::HorizontalWheel) {
        i32::try_from(key)? as u32
    } else {
        u32::try_from(key)?
    };
    let mut raw = old.map(|e| e.to_json()).transpose()?.unwrap_or_else(obj);
    for (name, v) in [
        (
            "Source",
            number(match source {
                Source::Keyboard => 1,
                Source::Mouse => 2,
                _ => 0,
            }),
        ),
        (
            "Direction",
            number(match direction {
                Direction::Down => 1,
                Direction::Up => 2,
                Direction::Wheel => 3,
                Direction::HorizontalWheel => 4,
                Direction::Move => 5,
                _ => 0,
            }),
        ),
        ("Key", number(if kind == "delay" { 0 } else { key })),
        (
            "Point",
            str_node(format!("{}, {}", i32::try_from(x)?, i32::try_from(y)?)),
        ),
    ] {
        raw.set_known(name, v)?;
    }
    raw.set_known("JiksDelayOnly", JsonNode::Bool(kind == "delay"))?;
    let mut event = MacroEvent::from_json(&raw)?;
    event.delay_ticks = ticks;
    Ok(event)
}
pub fn macro_event_to_ui(event: &MacroEvent) -> Result<JsonNode> {
    let kind = match (event.source, event.direction) {
        (Source::Keyboard, Direction::Down) => "key_down",
        (Source::Keyboard, Direction::Up) => "key_up",
        (Source::Mouse, Direction::Down) => "mouse_down",
        (Source::Mouse, Direction::Up) => "mouse_up",
        (Source::Mouse, Direction::Move) => "mouse_move",
        (Source::Mouse, Direction::Wheel) => "wheel",
        (Source::Mouse, Direction::HorizontalWheel) => "horizontal_wheel",
        _ => "delay",
    };
    let key = if matches!(
        event.direction,
        Direction::Wheel | Direction::HorizontalWheel
    ) {
        i64::from(event.key as i32)
    } else {
        i64::from(event.key)
    };
    let mut result = value(
        json!({"kind":kind,"key":key,"delay_ms":event.delay_ticks/10000,"x":event.point.x,"y":event.point.y}),
    )?;
    result.set("legacy", event.to_json()?)?;
    Ok(result)
}
fn parse_automation(doc: &SettingsDocument) -> Result<AutomationDocument> {
    let parsed = AutomationDocument::parse(&doc.serialize()?, || {
        "00000000-0000-0000-0000-000000000000".into()
    })?;
    let mut ids = BTreeSet::new();
    for pipeline in &parsed.pipelines {
        ensure!(ids.insert(pipeline.id.clone()), "Duplicate pipeline GUID");
    }
    Ok(parsed)
}
fn selected(payload: &JsonNode, collection: &str) -> Result<String> {
    payload
        .get_ci("id")
        .or_else(|| payload.get(&format!("selection.{collection}")))
        .and_then(JsonNode::as_str)
        .map(str::to_owned)
        .context("Selected item ID required")
}
fn generated(payload: &JsonNode, context: &ActionContext) -> Result<String> {
    normalize_guid(
        payload
            .get_ci("id")
            .and_then(JsonNode::as_str)
            .or(context.new_id.as_deref())
            .context("New item requires a caller-generated GUID")?,
    )
    .map_err(Into::into)
}
pub fn apply_action(command: &str, payload: &JsonNode, docs: &Documents) -> Result<ConfigChange> {
    apply_action_with_context(command, payload, docs, &ActionContext::default())
}
pub fn apply_action_with_id(
    command: &str,
    payload: &JsonNode,
    docs: &Documents,
    new_id: &str,
) -> Result<ConfigChange> {
    apply_action_with_context(
        command,
        payload,
        docs,
        &ActionContext {
            new_id: Some(new_id.into()),
            ..Default::default()
        },
    )
}
pub fn apply_action_with_context(
    command: &str,
    payload: &JsonNode,
    docs: &Documents,
    context: &ActionContext,
) -> Result<ConfigChange> {
    ensure!(
        payload.as_object().is_some(),
        "Settings action payload must be object"
    );
    let mut pending = BTreeMap::new();
    match command {
        "power.ai.settings" => {
            let enabled = payload
                .get("power.ai")
                .or_else(|| payload.get("enabled"))
                .context("power.ai required")?;
            set_path(
                edit(&mut pending, docs, "balance")?,
                &["AIModeEnabled"],
                JsonNode::Bool(boolean(enabled)?),
            )?;
        }
        "gpu.settings.save" => {
            for (key, value) in payload.as_object().unwrap() {
                let (legacy, validated) = match key.as_str() {
                    "gpu.keep_awake" => ("KeepDgpuAwake", JsonNode::Bool(boolean(value)?)),
                    "gpu.locked_pstate" => ("LockedPStateId", number(integer(value, -1, 15)?)),
                    "gpu.poll_ms" => ("GPUMonitoringInterval", number(integer(value, 250, 60000)?)),
                    _ => bail!("Unknown GPU preference field {key}"),
                };
                set_path(
                    edit(&mut pending, docs, "application")?,
                    &[legacy],
                    validated,
                )?;
            }
        }
        "app.preferences.save" => {
            for (key, v) in payload.as_object().unwrap() {
                preference(&mut pending, docs, key, v)?;
            }
        }
        "sensors.settings.save" => sensor_fields(payload, docs, &mut pending, false)?,
        "settings.fields.save" => {
            let document = text(payload, "document")?;
            ensure!(
                document != "integrations",
                "集成开关须通过integration.apply注册并核对实际结果"
            );
            ensure!(docs.contains_key(document), "Unknown settings document");
            let fields = payload
                .get_ci("fields")
                .and_then(JsonNode::as_object)
                .context("Fields object required")?;
            for (path, v) in fields {
                let path = decode_pointer(path)?;
                validate_field(document, &path, v)?;
                let path = path.iter().map(String::as_str).collect::<Vec<_>>();
                if document == "notifications"
                    && path.len() == 3
                    && path[0] == "Notifications"
                    && NOTIFICATION_TABLES.contains(&path[1])
                    && matches!(v, JsonNode::Null)
                {
                    remove_path(edit(&mut pending, docs, document)?, &path);
                } else {
                    set_path(edit(&mut pending, docs, document)?, &path, v.clone())?;
                }
            }
        }
        "automation.settings.save" => {
            let enabled = payload
                .get("automation.enabled")
                .or_else(|| payload.get_ci("enabled"))
                .context("automation.enabled required")?;
            let enabled = boolean(enabled)?;
            if enabled {
                parse_automation(docs.get("automation").context("Missing automation")?)?
                    .validate_execution()?;
            }
            set_path(
                edit(&mut pending, docs, "automation")?,
                &["IsEnabled"],
                JsonNode::Bool(enabled),
            )?;
        }
        "automation.save" | "automation.delete" | "automation.move_up" | "automation.move_down" => {
            let mut document = parse_automation(
                docs.get("automation")
                    .context("Missing automation document")?,
            )?;
            if command == "automation.save" {
                let id = generated(payload, context)?;
                let prior = document.pipelines.iter().find(|p| p.id == id);
                let mut raw = prior.map(PipelineDto::to_json_node).unwrap_or_else(obj);
                for (name, entry) in [
                    ("Id", str_node(id.clone())),
                    ("Name", str_node(text(payload, "name")?)),
                    (
                        "IsExclusive",
                        JsonNode::Bool(payload.get_ci("exclusive").map_or(Ok(true), boolean)?),
                    ),
                    (
                        "RunOnStartup",
                        JsonNode::Bool(
                            payload
                                .get_ci("run_on_startup")
                                .map_or(Ok(false), boolean)?,
                        ),
                    ),
                    (
                        "JiksEnabled",
                        JsonNode::Bool(payload.get_ci("enabled").map_or(Ok(true), boolean)?),
                    ),
                ] {
                    raw.set_known(name, entry)?;
                }
                if let Some(icon) = payload.get_ci("icon") {
                    let icon = icon.as_str().context("快捷操作图标必须是名称")?;
                    ensure!(
                        icon.chars().count() <= 128 && !icon.chars().any(char::is_control),
                        "快捷操作图标名称无效"
                    );
                    raw.set_known("IconName", str_node(icon))?;
                }
                let trigger = automation::trigger_from_ui(
                    payload.get_ci("trigger").unwrap_or(&JsonNode::Null),
                )?;
                raw.set_known(
                    "Trigger",
                    trigger
                        .as_ref()
                        .map(|t| t.raw().clone())
                        .unwrap_or(JsonNode::Null),
                )?;
                let steps = payload
                    .get_ci("steps")
                    .and_then(JsonNode::as_array)
                    .context("Automation steps array required")?
                    .iter()
                    .map(automation::step_from_ui)
                    .collect::<Result<Vec<_>>>()?;
                ensure!(
                    !steps.is_empty() && steps.len() <= 10000,
                    "Rule needs 1..10000 steps"
                );
                raw.set_known(
                    "Steps",
                    JsonNode::Array(steps.iter().map(|s| s.raw().clone()).collect()),
                )?;
                let pipeline = PipelineDto::parse(raw, || id.clone())?;
                if let Some(existing) = document.pipelines.iter_mut().find(|p| p.id == id) {
                    *existing = pipeline.clone()
                } else {
                    document.pipelines.push(pipeline.clone())
                }
                expand_pipeline(&pipeline, &document.pipelines)?;
            } else {
                let id = normalize_guid(&selected(payload, "automation")?)?;
                let index = document
                    .pipelines
                    .iter()
                    .position(|p| p.id == id)
                    .context("Rule ID not found")?;
                match command {
                    "automation.delete" => {
                        document.pipelines.remove(index);
                    }
                    "automation.move_up" if index > 0 => document.pipelines.swap(index, index - 1),
                    "automation.move_down" if index + 1 < document.pipelines.len() => {
                        document.pipelines.swap(index, index + 1)
                    }
                    _ => {}
                }
            }
            pending.insert("automation".into(), JsonNode::parse(&document.to_json())?);
        }
        "macro.settings.save" => {
            let enabled = payload
                .get("macro.enabled")
                .or_else(|| payload.get_ci("enabled"))
                .context("macro.enabled required")?;
            set_path(
                edit(&mut pending, docs, "macros")?,
                &["IsEnabled"],
                JsonNode::Bool(boolean(enabled)?),
            )?;
        }
        "macro.save" | "macro.delete" => {
            let mut document = MacroDocument::from_settings(
                docs.get("macros")
                    .context("Missing macros document")?
                    .clone(),
            )?;
            if command == "macro.delete" {
                document.remove(&MacroIdentifier::parse(&selected(payload, "macros")?)?);
            } else {
                let key = payload
                    .get_ci("trigger_key")
                    .context("Macro trigger_key required")?;
                let key = if let Some(n) = key.as_i64() {
                    u64::try_from(n)?
                } else {
                    key.as_str()
                        .context("Trigger key integer/string")?
                        .parse::<u64>()?
                };
                ensure!(
                    TRIGGER_KEYS.iter().any(|n| u64::from(*n) == key),
                    "Macro trigger must be numpad 0..9 (VK96..105)"
                );
                let identifier = MacroIdentifier {
                    source: "Keyboard".into(),
                    key,
                };
                let old_id = payload
                    .get_ci("id")
                    .and_then(JsonNode::as_str)
                    .map(MacroIdentifier::parse)
                    .transpose()?;
                let prior = old_id
                    .as_ref()
                    .and_then(|id| document.get(id))
                    .or_else(|| document.get(&identifier));
                let mut raw = prior
                    .map(MacroSequence::to_json)
                    .transpose()?
                    .unwrap_or_else(obj);
                for (name, v) in [
                    (
                        "RepeatCount",
                        number(integer(
                            payload.get_ci("repeat").context("Macro repeat")?,
                            1,
                            10,
                        )?),
                    ),
                    (
                        "IgnoreDelays",
                        JsonNode::Bool(payload.get_ci("ignore_delays").map_or(Ok(false), boolean)?),
                    ),
                    (
                        "InterruptOnOtherKey",
                        JsonNode::Bool(
                            payload
                                .get_ci("interrupt_on_other_key")
                                .map_or(Ok(false), boolean)?,
                        ),
                    ),
                    ("JiksName", str_node(text(payload, "name")?)),
                ] {
                    raw.set_known(name, v)?;
                }
                let events = payload
                    .get_ci("events")
                    .and_then(JsonNode::as_array)
                    .context("Macro events required")?
                    .iter()
                    .map(parse_macro_event)
                    .collect::<Result<Vec<_>>>()?;
                raw.set_known(
                    "Events",
                    JsonNode::Array(
                        events
                            .iter()
                            .map(MacroEvent::to_json)
                            .collect::<Result<_>>()?,
                    ),
                )?;
                let mut sequence = MacroSequence::from_json(&raw)?;
                sequence.validate_playback()?;
                sequence.clean_legacy();
                if let Some(old) = old_id {
                    if old != identifier {
                        document.remove(&old)
                    }
                }
                document.upsert(identifier, sequence);
            }
            pending.insert("macros".into(), JsonNode::parse(&document.serialize()?)?);
        }
        "preset.save" | "preset.delete" => {
            preset_action(command, payload, docs, context, &mut pending)?
        }
        _ => bail!("Unsupported configuration action {command}"),
    }
    finish(docs, pending, false)
}
fn finish(
    docs: &Documents,
    pending: BTreeMap<String, JsonNode>,
    thermal: bool,
) -> Result<ConfigChange> {
    let mut updated = docs.clone();
    let mut changes = Vec::new();
    for (key, root) in pending {
        let document = SettingsDocument::from_root(root)?;
        let document = if key == "application" {
            docs.get(&key)
                .context("Missing application")?
                .merged_application(document.root())?
                .normalized_application()?
        } else {
            document
        };
        if !thermal {
            validate_transition(
                &key,
                docs.get(&key).context("Missing original document")?,
                &document,
            )?;
        }
        document.serialize()?;
        updated.insert(key.clone(), document.clone());
        changes.push((key, document));
    }
    Ok(ConfigChange {
        documents: changes,
        projection: project(&updated)?,
        message: "配置已验证；由调用者逐文档原子提交".into(),
    })
}
/// Dedicated validation only. The caller must acquire/stop/verify the physical
/// session BEFORE committing these documents and publishing enabled=true.
pub fn thermal_change(payload: &JsonNode, docs: &Documents) -> Result<ConfigChange> {
    let mut pending = BTreeMap::new();
    sensor_fields(payload, docs, &mut pending, true)?;
    let application = pending.get("application").cloned().unwrap_or(
        docs.get("application")
            .context("Missing application")?
            .root()
            .clone(),
    );
    let typed =
        ApplicationThermalSettings::from_document(&SettingsDocument::from_root(application)?)?;
    typed.validate_control()?;
    finish(docs, pending, true)
}
fn sensor_fields(
    payload: &JsonNode,
    docs: &Documents,
    pending: &mut BTreeMap<String, JsonNode>,
    allow_thermal: bool,
) -> Result<()> {
    for (key, v) in payload.as_object().context("Sensor settings object")? {
        let (document, path, entry) = match key.as_str() {
            "sensors.cpu_frequency" => (
                "hardware_sensors",
                "ShowCpuAverageFrequency",
                JsonNode::Bool(match v.as_str() {
                    Some("average") => true,
                    Some("maximum") => false,
                    _ => bail!("CPU frequency average/maximum"),
                }),
            ),
            "sensors.temperature_source" => (
                "hardware_sensors",
                "CpuTemperatureSource",
                enum_ui(
                    v,
                    &[
                        ("auto", "Auto"),
                        ("average", "Average"),
                        ("package", "Package"),
                        ("core_max", "CoreMax"),
                        ("coremax", "CoreMax"),
                    ],
                )?,
            ),
            "sensors.voltage_mode" => (
                "hardware_sensors",
                "CpuVoltageMode",
                enum_ui(
                    v,
                    &[
                        ("average", "Average"),
                        ("maximum", "Maximum"),
                        ("core", "Core"),
                    ],
                )?,
            ),
            "sensors.voltage_core" => (
                "hardware_sensors",
                "CpuVoltageCoreIndex",
                number(integer(v, 0, 255)?),
            ),
            "sensors.gpu" => (
                "hardware_sensors",
                "SelectedGpuIsIgpu",
                JsonNode::Bool(match v.as_str() {
                    Some("integrated") => true,
                    Some("discrete") => false,
                    _ => bail!("GPU choice integrated/discrete"),
                }),
            ),
            "sensors.memory_mode" => (
                "hardware_sensors",
                "DisplayMemoryInGigabytes",
                JsonNode::Bool(match v.as_str() {
                    Some("gb") => true,
                    Some("percent") => false,
                    _ => bail!("Memory percent/gb"),
                }),
            ),
            "sensors.temperature_unit" => (
                "application",
                "TemperatureUnit",
                str_node(enum_name(v, &["C", "F"])?),
            ),
            "thermal.shadow" => (
                "application",
                "EnableThermalShadow",
                JsonNode::Bool(boolean(v)?),
            ),
            "thermal.enabled" => (
                "application",
                "EnableCpuThermalControl",
                JsonNode::Bool(boolean(v)?),
            ),
            "thermal.sampling_ms" => (
                "application",
                "CpuThermalSamplingMilliseconds",
                number(integer(v, 100, 1000)?),
            ),
            "thermal.firmware_limit_c" => (
                "application",
                "CpuThermalFirmwareLimitC",
                number(integer(v, 50, 88)?),
            ),
            _ => bail!("Unknown sensor setting {key}"),
        };
        // Generic transition validation compares typed defaults after all fields
        // have been assembled, so an omitted legacy default is not a change.
        let _ = allow_thermal;
        set_path(edit(pending, docs, document)?, &[path], entry)?;
    }
    Ok(())
}
fn decode_pointer(path: &str) -> Result<Vec<String>> {
    ensure!(
        path.starts_with('/') && path.len() <= 512,
        "Expected bounded RFC6901 field pointer"
    );
    let parts = path[1..]
        .split('/')
        .map(|part| {
            let mut decoded = String::new();
            let mut chars = part.chars();
            while let Some(c) = chars.next() {
                if c == '~' {
                    decoded.push(match chars.next() {
                        Some('0') => '~',
                        Some('1') => '/',
                        _ => bail!("Bad RFC6901 escape"),
                    })
                } else {
                    decoded.push(c)
                }
            }
            ensure!(
                !decoded.is_empty() && !decoded.starts_with('$'),
                "Empty/metadata field path forbidden"
            );
            Ok(decoded)
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(parts.len() <= 8, "Field path exceeds depth budget");
    Ok(parts)
}
fn storage_field_known(document: &str, top: &str) -> bool {
    match document {
        "integrations" => ["HWiNFO", "CLI", "JiksPathAdded"].contains(&top),
        "balance" => top == "AIModeEnabled",
        "application" => [
            "GameDetection",
            "IncludedProcesses",
            "ExcludedProcesses",
            "PowerPlans",
            "PowerModes",
            "ExcludedRefreshRates",
        ]
        .contains(&top),
        "hardware_sensors" => [
            "SelectedGpuIsIgpu",
            "ShowCpuAverageFrequency",
            "DisplayMemoryInGigabytes",
            "CpuVoltageMode",
            "CpuVoltageCoreIndex",
            "CpuTemperatureSource",
        ]
        .contains(&top),
        "dashboard" | "sensor_dashboard" => [
            "ShowSensors",
            "SensorsRefreshIntervalSeconds",
            "Groups",
            "VisibleItems",
            "CardOrder",
        ]
        .contains(&top),
        "notifications" => [
            "DontShowNotifications",
            "NotificationPosition",
            "NotificationDuration",
            "NotificationAlwaysOnTop",
            "NotificationOnAllScreens",
            "Notifications",
        ]
        .contains(&top),
        "osd" => [
            "ShowOsd",
            "SelectedStyleIndex",
            "OsdRefreshInterval",
            "IsLocked",
            "SnapThreshold",
            "BackgroundOpacity",
            "FontSize",
            "CornerRadiusTop",
            "CornerRadiusBottom",
            "BackgroundColor",
            "CategoryColor",
            "LabelColor",
            "ValueColor",
            "WarningColor",
            "CriticalColor",
            "TempThresholdWarning",
            "TempThresholdCritical",
            "UsageThresholdWarning",
            "UsageThresholdCritical",
            "FpsThresholdCritical",
            "LowFpsDeltaThreshold",
        ]
        .contains(&top),
        "special_keys" => [
            "KeyModes",
            "KeyDescriptions",
            "KeySinglePressActions",
            "KeyDoublePressActions",
            "HiddenKeys",
        ]
        .contains(&top),
        _ => false,
    }
}
fn validate_field(document: &str, path: &[String], v: &JsonNode) -> Result<()> {
    let top = path[0].as_str();
    ensure!(
        storage_field_known(document, top),
        "Field {document}/{top} is not a supported typed edit"
    );
    match top {
        "AIModeEnabled" | "HWiNFO" | "CLI" | "JiksPathAdded" => {
            boolean(v)?;
        }
        "SelectedGpuIsIgpu"
        | "ShowCpuAverageFrequency"
        | "DisplayMemoryInGigabytes"
        | "ShowSensors"
        | "DontShowNotifications"
        | "NotificationAlwaysOnTop"
        | "NotificationOnAllScreens"
        | "ShowOsd"
        | "IsLocked" => {
            boolean(v)?;
        }
        "GameDetection" => {
            ensure!(path.len() == 2, "Choose an individual boolean flag");
            boolean(v)?;
        }
        "Notifications" => {
            ensure!(path.len() >= 2, "请选择个别通知设置");
            let member = path[1].as_str();
            if NOTIFICATION_FLAGS.contains(&member) {
                ensure!(path.len() == 2, "通知类别路径无效");
                boolean(v)?;
            } else {
                ensure!(
                    NOTIFICATION_TABLES.contains(&member),
                    "未知通知设置，请通过高级配置保留扩展"
                );
                if path.len() == 2 {
                    if !matches!(v, JsonNode::Null) {
                        for (kind, value) in v.as_object().context("通知覆盖表必须为对象")?
                        {
                            if notification_type_known(kind) {
                                validate_field(
                                    document,
                                    &[top.into(), member.into(), kind.clone()],
                                    value,
                                )?;
                            }
                        }
                    }
                } else {
                    ensure!(
                        path.len() == 3 && notification_type_known(&path[2]),
                        "未知或无效的通知类型"
                    );
                    if !matches!(v, JsonNode::Null) {
                        match member {
                            "IconOverrides" => {
                                integer(v, 0, u32::MAX as i64)?;
                            }
                            "ColorOverrides" | "TextColorOverrides" => {
                                let object = v.as_object().context("颜色必须为RGB对象或null")?;
                                for channel in ["R", "G", "B"] {
                                    integer(
                                        object
                                            .iter()
                                            .find(|(name, _)| name.eq_ignore_ascii_case(channel))
                                            .map(|(_, value)| value)
                                            .context("颜色通道缺失")?,
                                        0,
                                        255,
                                    )?;
                                }
                            }
                            "PositionOverrides" => {
                                enum_name(
                                    v,
                                    &[
                                        "BottomRight",
                                        "BottomCenter",
                                        "BottomLeft",
                                        "CenterLeft",
                                        "TopLeft",
                                        "TopCenter",
                                        "TopRight",
                                        "CenterRight",
                                        "Center",
                                    ],
                                )?;
                            }
                            "DurationOverrides" => {
                                enum_name(v, &["Short", "Normal", "Long"])?;
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            }
        }
        "CpuVoltageCoreIndex" => {
            integer(v, 0, 255)?;
        }
        "CpuVoltageMode" => {
            enum_name(v, &["Average", "Maximum", "Core"])?;
        }
        "CpuTemperatureSource" => {
            enum_name(v, &["Auto", "Average", "Package", "CoreMax"])?;
        }
        "SensorsRefreshIntervalSeconds" | "OsdRefreshInterval" => {
            real(v, 0.1, 60.)?;
        }
        "BackgroundOpacity" => {
            real(v, 0., 1.)?;
        }
        "SelectedStyleIndex" => {
            integer(v, 0, 1)?;
        }
        "FontSize" => {
            integer(v, 8, 72)?;
        }
        "SnapThreshold" => {
            integer(v, 0, 200)?;
        }
        "CornerRadiusTop" | "CornerRadiusBottom" => {
            integer(v, 0, 50)?;
        }
        "TempThresholdWarning" | "TempThresholdCritical" => {
            real(v, 0., 110.)?;
        }
        "UsageThresholdWarning" | "UsageThresholdCritical" => {
            real(v, 0., 100.)?;
        }
        "FpsThresholdCritical" => {
            integer(v, 1, 1000)?;
        }
        "LowFpsDeltaThreshold" => {
            integer(v, 0, 1000)?;
        }
        "NotificationPosition" => {
            enum_name(
                v,
                &[
                    "BottomRight",
                    "BottomCenter",
                    "BottomLeft",
                    "CenterLeft",
                    "TopLeft",
                    "TopCenter",
                    "TopRight",
                    "CenterRight",
                    "Center",
                ],
            )?;
        }
        "NotificationDuration" => {
            enum_name(v, &["Short", "Normal", "Long"])?;
        }
        "PowerPlans" => {
            ensure!(path.len() == 2, "Individual power plan mapping required");
            if !matches!(v, JsonNode::Null) {
                normalize_guid(v.as_str().context("Power plan GUID")?)?;
            }
        }
        "PowerModes" => {
            ensure!(path.len() == 2, "Individual power mode mapping required");
            if !matches!(v, JsonNode::Null) {
                enum_name(v, &["BestPowerEfficiency", "Balanced", "BestPerformance"])?;
            }
        }
        "IncludedProcesses" | "ExcludedProcesses" => {
            for item in v.as_array().context("Process name array")? {
                ensure!(item.as_str().is_some(), "Process name string");
            }
        }
        "Groups" => {
            let groups = v.as_array().context("Groups array")?;
            ensure!(groups.len() <= 64, "Too many groups");
            for group in groups {
                ensure!(group.as_object().is_some(), "Group object");
                let ty = group.get_ci("Type").context("Group Type required")?;
                ensure!(
                    ty.as_str().is_some() || ty.as_i64().is_some(),
                    "Group Type enum"
                );
                ensure!(
                    group
                        .get_ci("Items")
                        .and_then(JsonNode::as_array)
                        .is_some_and(|items| items.len() <= 128),
                    "Group Items array capacity"
                );
            }
        }
        "VisibleItems" | "CardOrder" | "ExcludedRefreshRates" => {
            ensure!(
                v.as_array().is_some_and(|a| a.len() <= 256),
                "Bounded typed array required"
            );
        }
        "HiddenKeys" => {
            for key in v.as_array().context("HiddenKeys array")? {
                integer(key, 0, i32::MAX as i64)?;
            }
        }
        "KeyModes" | "KeyDescriptions" | "KeySinglePressActions" | "KeyDoublePressActions" => {
            ensure!(
                path.len() == 2 && path[1].parse::<u32>().is_ok(),
                "Special key must have integer dictionary key"
            );
            match top {
                "KeyModes" => {
                    enum_name(v, &["Default", "Custom"])?;
                }
                "KeyDescriptions" => {
                    ensure!(v.as_str().is_some(), "Key description string");
                }
                _ => {
                    for id in v.as_array().context("Key action GUID array")? {
                        normalize_guid(id.as_str().context("Key action GUID")?)?;
                    }
                }
            }
        }
        key if key.ends_with("Color") => {
            let color = v.as_str().context("Color string")?;
            ensure!(
                color.len() == 7
                    && color.starts_with('#')
                    && color[1..].bytes().all(|b| b.is_ascii_hexdigit()),
                "Expected #RRGGBB"
            );
        }
        _ => {}
    }
    Ok(())
}

const PRESET_FIELDS: &[(&str, &str)] = &[
    ("cpu.pl1", "CPULongTermPowerLimit"),
    ("cpu.pl2", "CPUShortTermPowerLimit"),
    ("cpu.peak", "CPUPeakPowerLimit"),
    ("cpu.temperature", "CPUTemperatureLimit"),
    ("cpu.sppt", "APUsPPTPowerLimit"),
    ("cpu.cross_loading", "CPUCrossLoadingPowerLimit"),
    ("cpu.pl2_tau", "CPUPL2Tau"),
    ("gpu.dynamic_boost", "GPUPowerBoost"),
    ("gpu.tgp", "GPUConfigurableTGP"),
    ("gpu.temperature", "GPUTemperatureLimit"),
    (
        "gpu.total_power_target",
        "GPUTotalProcessingPowerTargetOnAcOffsetFromBaseline",
    ),
    ("gpu.to_cpu_boost", "GPUToCPUDynamicBoost"),
];
fn preset_action(
    command: &str,
    payload: &JsonNode,
    docs: &Documents,
    context: &ActionContext,
    pending: &mut BTreeMap<String, JsonNode>,
) -> Result<()> {
    let mut root = docs
        .get("godmode")
        .context("Missing godmode document")?
        .root()
        .clone();
    let mut presets = root.get_ci("Presets").cloned().unwrap_or_else(obj);
    ensure!(presets.as_object().is_some(), "Presets dictionary required");
    let id = if command == "preset.delete" {
        normalize_guid(&selected(payload, "presets")?)?
    } else {
        generated(payload, context)?
    };
    let old_key = presets
        .as_object()
        .unwrap()
        .iter()
        .find(|(key, _)| normalize_guid(key).as_ref().ok() == Some(&id))
        .map(|(key, _)| key.clone());
    if command == "preset.delete" {
        let key = old_key.context("Preset ID not found")?;
        if let JsonNode::Object(items) = &mut presets {
            items.retain(|(name, _)| *name != key);
        }
        if root
            .get_ci("ActivePresetId")
            .and_then(JsonNode::as_str)
            .and_then(|s| normalize_guid(s).ok())
            .as_deref()
            == Some(&id)
        {
            root.set_known(
                "ActivePresetId",
                str_node("00000000-0000-0000-0000-000000000000"),
            )?;
        }
    } else {
        let mut preset = old_key
            .as_ref()
            .and_then(|k| presets.get(k))
            .cloned()
            .unwrap_or_else(obj);
        let name = text(payload, "name")?.trim();
        ensure!(
            !name.is_empty() && name.len() <= 256,
            "Preset name needs 1..256 bytes"
        );
        preset.set_known("Name", str_node(name))?;
        let supplied = payload
            .get_ci("values")
            .and_then(JsonNode::as_object)
            .context("Preset values object required")?;
        for (semantic, legacy) in PRESET_FIELDS {
            if !supplied.iter().any(|(key, _)| key == semantic) {
                preset.set_known(legacy, JsonNode::Null)?;
            }
        }
        for (key, entry) in payload
            .get_ci("values")
            .and_then(JsonNode::as_object)
            .context("Preset values object required")?
        {
            let legacy = PRESET_FIELDS
                .iter()
                .find(|(semantic, _)| *semantic == key)
                .map(|(_, legacy)| *legacy)
                .with_context(|| format!("Unknown/unported preset field {key}"))?;
            if matches!(entry, JsonNode::Null) {
                preset.set_known(legacy, JsonNode::Null)?;
                continue;
            }
            let number = integer(entry, i32::MIN as i64, i32::MAX as i64)?;
            let original = preset.get_ci(legacy).filter(|n| n.as_object().is_some());
            let discrete = context
                .discrete
                .get(key)
                .filter(|v| !v.is_empty())
                .map(|v| v.iter().map(|v| i64::from(*v)).collect::<Vec<_>>())
                .or_else(|| {
                    original
                        .and_then(|o| o.get_ci("Steps"))
                        .and_then(JsonNode::as_array)
                        .filter(|v| !v.is_empty())
                        .and_then(|a| a.iter().map(JsonNode::as_i64).collect::<Option<Vec<_>>>())
                });
            let mut stepper = original.cloned().unwrap_or_else(obj);
            if let Some(steps) = discrete {
                ensure!(
                    steps.contains(&number),
                    "Preset {key} is not a verified discrete value"
                );
                stepper.set_known("Value", JsonNode::Number(number.to_string()))?;
                if original.is_none() {
                    stepper.set_known(
                        "Min",
                        JsonNode::Number(steps.iter().min().unwrap().to_string()),
                    )?;
                    stepper.set_known(
                        "Max",
                        JsonNode::Number(steps.iter().max().unwrap().to_string()),
                    )?;
                    stepper.set_known("Step", JsonNode::Number("1".into()))?;
                }
                // Preserve original ordered Steps and opaque members when editing an existing preset.
                if stepper.get_ci("Steps").is_none() {
                    stepper.set_known(
                        "Steps",
                        JsonNode::Array(
                            steps
                                .iter()
                                .map(|n| JsonNode::Number(n.to_string()))
                                .collect(),
                        ),
                    )?;
                }
            } else {
                let range = context
                    .ranges
                    .get(key)
                    .map(|r| {
                        (
                            i64::from(r.minimum),
                            i64::from(r.maximum),
                            i64::from(r.step),
                        )
                    })
                    .or_else(|| {
                        let o = original?;
                        Some((
                            o.get_ci("Min")?.as_i64()?,
                            o.get_ci("Max")?.as_i64()?,
                            o.get_ci("Step")?.as_i64()?,
                        ))
                    })
                    .with_context(|| format!("Verified local range required for {key}"))?;
                ensure!(
                    range.2 > 0
                        && range.1 >= range.0
                        && number >= range.0
                        && number <= range.1
                        && (number - range.0) % range.2 == 0,
                    "Preset {key} is outside verified range/grid"
                );
                for (name, v) in [
                    ("Value", number),
                    ("Min", range.0),
                    ("Max", range.1),
                    ("Step", range.2),
                ] {
                    stepper.set_known(name, JsonNode::Number(v.to_string()))?;
                }
                if stepper.get_ci("Steps").is_none() {
                    stepper.set("Steps", JsonNode::Array(vec![]))?;
                }
            }
            if stepper.get_ci("DefaultValue").is_none() {
                stepper.set("DefaultValue", JsonNode::Null)?;
            }
            preset.set_known(legacy, stepper)?;
        }
        if let (Some(long), Some(short)) = (
            preset
                .get_ci("CPULongTermPowerLimit")
                .and_then(|n| n.get_ci("Value"))
                .and_then(JsonNode::as_i64),
            preset
                .get_ci("CPUShortTermPowerLimit")
                .and_then(|n| n.get_ci("Value"))
                .and_then(JsonNode::as_i64),
        ) {
            ensure!(long > 0 && long <= short, "Preset must keep 0 < PL1 <= PL2");
        }
        if let Some(fan) = payload
            .get_ci("fan")
            .filter(|n| !matches!(n, JsonNode::Null))
        {
            ensure!(fan.as_object().is_some(), "Preset fan object");
            if let Some(full) = fan.get_ci("full_speed") {
                preset.set_known(
                    "FanFullSpeed",
                    if matches!(full, JsonNode::Null) {
                        JsonNode::Null
                    } else {
                        JsonNode::Bool(boolean(full)?)
                    },
                )?;
            }
            if fan.get_ci("table") == Some(&JsonNode::Null) {
                preset.set_known("FanTable", JsonNode::Null)?;
            }
            if let Some(table) = fan.get_ci("table").filter(|n| !matches!(n, JsonNode::Null)) {
                let table = table.as_array().context("Fan ten-node array")?;
                ensure!(table.len() == 10, "Fan table must have exactly ten nodes");
                let mut raw = preset
                    .get_ci("FanTable")
                    .filter(|n| n.as_object().is_some())
                    .cloned()
                    .unwrap_or_else(obj);
                for (name, v) in [("FSTM", 1), ("FSID", 0), ("FSTL", 10)] {
                    raw.set_known(name, number(v))?;
                }
                let mut prior = 0;
                for (index, entry) in table.iter().enumerate() {
                    let level = integer(entry, 0, 10)?;
                    ensure!(level >= prior, "Fan nodes must be nondecreasing");
                    prior = level;
                    raw.set_known(&format!("FSS{index}"), number(level))?;
                }
                preset.set_known("FanTable", raw)?;
            }
        }
        if let Some(overrides) = payload.get_ci("overrides") {
            let mut original = preset.get_ci("Overrides").cloned().unwrap_or_else(obj);
            ensure!(original.as_object().is_some(), "Overrides dictionary");
            for (key, v) in overrides.as_object().context("Overrides object")? {
                let legacy = match key.as_str() {
                    "power.plan" => "PowerPlan",
                    "power.mode_ac" => "PowerModeOnAc",
                    "power.mode_dc" => "PowerModeOnDc",
                    _ => bail!("Unknown preset override {key}"),
                };
                if matches!(v, JsonNode::Null) || v.as_str() == Some("") {
                    if let JsonNode::Object(items) = &mut original {
                        items.retain(|(key, _)| key != legacy);
                    }
                    continue;
                }
                let text = v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_json());
                if legacy == "PowerPlan" {
                    normalize_guid(&text)?;
                } else {
                    ensure!(
                        [
                            "0",
                            "1",
                            "2",
                            "BestPowerEfficiency",
                            "Balanced",
                            "BestPerformance",
                            "efficiency",
                            "balanced",
                            "performance"
                        ]
                        .contains(&text.as_str()),
                        "Unsupported Windows power override"
                    );
                }
                original.set(legacy, str_node(text))?;
            }
            preset.set_known("Overrides", original)?;
        }
        if let Some(key) = old_key {
            if let JsonNode::Object(items) = &mut presets {
                let entry = items.iter_mut().find(|(name, _)| *name == key).unwrap();
                *entry = (id, preset);
            }
        } else {
            presets.set(id, preset)?;
        }
    }
    root.set_known("Presets", presets)?;
    pending.insert("godmode".into(), root);
    Ok(())
}

pub fn preset_to_ui(id: &str, preset: &JsonNode) -> Result<JsonNode> {
    let mut values = obj();
    for (semantic, legacy) in PRESET_FIELDS {
        if let Some(v) = preset.get_ci(legacy) {
            if let Some(v) = v.get_ci("Value") {
                values.set(*semantic, v.clone())?;
            } else if matches!(v, JsonNode::Null) {
                values.set(*semantic, JsonNode::Null)?;
            }
        }
    }
    let table = preset
        .get_ci("FanTable")
        .filter(|n| n.as_object().is_some())
        .map(|table| {
            JsonNode::Array(
                (0..10)
                    .map(|i| {
                        table
                            .get_ci(&format!("FSS{i}"))
                            .cloned()
                            .unwrap_or(JsonNode::Null)
                    })
                    .collect(),
            )
        })
        .unwrap_or(JsonNode::Null);
    let mut overrides = obj();
    if let Some(old) = preset.get_ci("Overrides") {
        for (ui, legacy) in [
            ("power.plan", "PowerPlan"),
            ("power.mode_ac", "PowerModeOnAc"),
            ("power.mode_dc", "PowerModeOnDc"),
        ] {
            if let Some(v) = old.get_ci(legacy) {
                overrides.set(ui, v.clone())?;
            }
        }
    }
    let mut result = value(
        json!({"id":id,"name":preset.get_ci("Name").and_then(JsonNode::as_str).unwrap_or(""),"fan":{"full_speed":preset.get_ci("FanFullSpeed").and_then(JsonNode::as_bool)}}),
    )?;
    result.set("values", values)?;
    result.set("overrides", overrides)?;
    let mut fan = result.get("fan").unwrap().clone();
    fan.set("table", table)?;
    result.set("fan", fan)?;
    Ok(result)
}
pub fn project(docs: &Documents) -> Result<UiProjection> {
    let mut output = UiProjection::default();
    if let Some(document) = docs.get("gpu_oc") {
        let enabled = document.root().get_ci("Enabled").and_then(JsonNode::as_bool).unwrap_or(false);
        output.values.insert("gpu.oc.enabled".into(), json!(enabled));
    }
    if let Some(root) = docs.get("integrations").map(SettingsDocument::root) {
        for (ui, key) in [
            ("integration.hwinfo", "HWiNFO"),
            ("integration.cli", "CLI"),
            ("integration.path", "JiksPathAdded"),
        ] {
            output.values.insert(
                ui.into(),
                json!(
                    root.get_ci(key)
                        .and_then(JsonNode::as_bool)
                        .unwrap_or(false)
                ),
            );
        }
    }
    if let Some(root) = docs.get("application").map(SettingsDocument::root) {
        for (ui, legacy, default) in [
            ("gpu.keep_awake", "KeepDgpuAwake", json!(false)),
            ("gpu.locked_pstate", "LockedPStateId", json!(-1)),
            ("gpu.poll_ms", "GPUMonitoringInterval", json!(5000)),
        ] {
            output.values.insert(
                ui.into(),
                root.get_ci(legacy)
                    .map(JsonNode::to_value)
                    .transpose()?
                    .unwrap_or(default),
            );
        }
    }
    if let Some(app) = docs.get("application") {
        for (ui, legacy, default) in APP_BOOLEANS {
            output.values.insert(
                (*ui).into(),
                app.root()
                    .get_ci(legacy)
                    .map(small)
                    .transpose()?
                    .unwrap_or(json!(default)),
            );
        }
        let root = app.root();
        output.values.insert("app.window".into(),json!({"position":root.get_ci("WindowPosition").map(JsonNode::to_value).transpose()?.unwrap_or(Value::Null),"size":root.get_ci("WindowSize").map(JsonNode::to_value).transpose()?.unwrap_or(Value::Null)}));
        let name = |key: &str, names: &[&str], default: usize| -> String {
            root.get_ci(key)
                .and_then(|n| {
                    n.as_str().map(str::to_owned).or_else(|| {
                        n.as_i64()
                            .and_then(|n| usize::try_from(n).ok())
                            .and_then(|i| names.get(i).map(|s| s.to_string()))
                    })
                })
                .unwrap_or_else(|| names[default].into())
        };
        output.values.insert(
            "appearance.theme".into(),
            json!(name("Theme", &["System", "Light", "Dark"], 0).to_ascii_lowercase()),
        );
        output.values.insert(
            "appearance.language".into(),
            json!(
                root.get_ci("JiksLanguage")
                    .and_then(JsonNode::as_str)
                    .unwrap_or("zh-Hans")
            ),
        );
        let language = root
            .get_ci("JiksLanguage")
            .and_then(JsonNode::as_str)
            .unwrap_or("zh-Hans");
        output.values.insert(
            "appearance.language_saved".into(),
            json!(
                root.get_ci("JiksLanguage")
                    .and_then(JsonNode::as_str)
                    .is_some_and(|value| !value.is_empty())
            ),
        );
        output.values.insert(
            "appearance.language_status".into(),
            json!(if ["zh-Hans", "zh-CN", "zh", "en", "en-US", "en-GB"]
                .iter()
                .any(|known| known.eq_ignore_ascii_case(language))
            {
                ""
            } else {
                "导入的未知语言标识已保留；当前显示简体中文，请选择已提供的语言后应用"
            }),
        );
        output.values.insert(
            "appearance.background".into(),
            json!(
                root.get_ci("BackGroundImageFilePath")
                    .and_then(JsonNode::as_str)
                    .unwrap_or("")
            ),
        );
        output.values.insert(
            "appearance.opacity".into(),
            json!(
                root.get_ci("Opacity")
                    .and_then(JsonNode::as_f64)
                    .unwrap_or(0.3)
                    * 100.
            ),
        );
        output.values.insert(
            "appearance.blur".into(),
            json!(
                root.get_ci("BackgroundImageBlur")
                    .and_then(JsonNode::as_i64)
                    .unwrap_or(0)
            ),
        );
        output.values.insert(
            "power.mapping".into(),
            json!(match name(
                "PowerModeMappingMode",
                &["Disabled", "WindowsPowerMode", "WindowsPowerPlan"],
                0
            )
            .as_str()
            {
                "WindowsPowerMode" => "mode",
                "WindowsPowerPlan" => "plan",
                _ => "disabled",
            }),
        );
        output.values.insert(
            "keys.smart_fn_lock".into(),
            json!(crate::special_keys::modifier_flags(root.get_ci("SmartFnLockFlags")).ok()),
        );
        for (ui, legacy, default) in [
            ("thermal.enabled", "EnableCpuThermalControl", false),
            ("thermal.shadow", "EnableThermalShadow", false),
        ] {
            output.values.insert(
                ui.into(),
                json!(
                    root.get_ci(legacy)
                        .and_then(JsonNode::as_bool)
                        .unwrap_or(default)
                ),
            );
        }
        for (ui, legacy, default) in [
            ("thermal.sampling_ms", "CpuThermalSamplingMilliseconds", 250),
            ("thermal.firmware_limit_c", "CpuThermalFirmwareLimitC", 88),
        ] {
            output.values.insert(
                ui.into(),
                json!(
                    root.get_ci(legacy)
                        .and_then(JsonNode::as_i64)
                        .unwrap_or(default)
                ),
            );
        }
        output.values.insert(
            "sensors.temperature_unit".into(),
            json!(name("TemperatureUnit", &["C", "F"], 0)),
        );
        if let Some(accent) = root
            .get_ci("AccentColor")
            .filter(|n| n.as_object().is_some())
        {
            let component = |key: &str| -> u8 {
                accent
                    .get_ci(key)
                    .and_then(JsonNode::as_i64)
                    .and_then(|n| u8::try_from(n).ok())
                    .unwrap_or(0)
            };
            output.values.insert(
                "appearance.accent".into(),
                json!(format!(
                    "#{:02X}{:02X}{:02X}",
                    component("R"),
                    component("G"),
                    component("B")
                )),
            );
        }
        if let Some(game) = root.get_ci("GameDetection") {
            output.values.insert("game.detection".into(), small(game)?);
        }
    }
    if let Some(sensor) = docs.get("hardware_sensors") {
        let r = sensor.root();
        for (ui, key, on, off) in [
            (
                "sensors.cpu_frequency",
                "ShowCpuAverageFrequency",
                "average",
                "maximum",
            ),
            ("sensors.gpu", "SelectedGpuIsIgpu", "integrated", "discrete"),
            (
                "sensors.memory_mode",
                "DisplayMemoryInGigabytes",
                "gb",
                "percent",
            ),
        ] {
            output.values.insert(
                ui.into(),
                json!(
                    if r.get_ci(key).and_then(JsonNode::as_bool).unwrap_or(false) {
                        on
                    } else {
                        off
                    }
                ),
            );
        }
        for (ui, key, names) in [
            (
                "sensors.temperature_source",
                "CpuTemperatureSource",
                &["Auto", "Average", "Package", "CoreMax"][..],
            ),
            (
                "sensors.voltage_mode",
                "CpuVoltageMode",
                &["Average", "Maximum", "Core"][..],
            ),
        ] {
            let raw = r.get_ci(key);
            let value = raw
                .and_then(|v| {
                    v.as_str().map(str::to_owned).or_else(|| {
                        v.as_i64()
                            .and_then(|n| usize::try_from(n).ok())
                            .and_then(|n| names.get(n).map(|s| s.to_string()))
                    })
                })
                .unwrap_or_else(|| names[0].into());
            output.values.insert(
                ui.into(),
                json!(if value == "CoreMax" {
                    "core_max".into()
                } else {
                    value.to_ascii_lowercase()
                }),
            );
        }
        output.values.insert(
            "sensors.voltage_core".into(),
            json!(
                r.get_ci("CpuVoltageCoreIndex")
                    .and_then(JsonNode::as_i64)
                    .unwrap_or(0)
            ),
        );
    }
    if let Some(document) = docs.get("automation") {
        match parse_automation(document) {
            Ok(auto) => {
                output
                    .values
                    .insert("automation.enabled".into(), json!(auto.enabled));
                let mut items = Vec::new();
                for p in auto.pipelines {
                    let mut item = value(
                        json!({"id":p.id,"name":p.name,"icon":p.to_json_node().get_ci("IconName").and_then(JsonNode::as_str).unwrap_or(""),"enabled":p.to_json_node().get_ci("JiksEnabled").and_then(JsonNode::as_bool).unwrap_or(true),"exclusive":p.is_exclusive,"run_on_startup":p.run_on_startup}),
                    )?;
                    item.set(
                        "trigger",
                        automation::trigger_to_ui(p.trigger.as_ref()).or_else(
                            |_| -> Result<JsonNode> {
                                let mut unsupported =
                                    value(json!({"type":"unsupported","parameters":{}}))?;
                                if let Some(trigger) = p.trigger.as_ref() {
                                    unsupported.set("legacy", trigger.raw().clone())?;
                                }
                                Ok(unsupported)
                            },
                        )?,
                    )?;
                    let steps = p
                        .steps
                        .iter()
                        .map(|s| {
                            automation::step_to_ui(s).or_else(|_| {
                                let mut unknown =
                                    value(json!({"type":"unsupported","parameters":{}}))?;
                                unknown.set("legacy", s.raw().clone())?;
                                Ok(unknown)
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    item.set("steps", JsonNode::Array(steps))?;
                    items.push(Item {
                        id: p.id.clone(),
                        label: p.name.unwrap_or_else(|| p.id.clone()),
                        detail: format!("{} 个动作", p.steps.len()),
                        value: item.to_value()?,
                    });
                }
                let quick = items
                    .iter()
                    .filter(|item| {
                        item.value.get("trigger").is_some_and(Value::is_null)
                            && item.value.get("enabled").and_then(Value::as_bool) != Some(false)
                    })
                    .cloned()
                    .collect();
                output.collections.insert("quick_actions".into(), quick);
                output.collections.insert("automation".into(), items);
            }
            Err(error) => {
                output.values.insert(
                    "automation.validation_error".into(),
                    json!(format!("{error:#}")),
                );
            }
        }
    }
    if let Some(document) = docs.get("macros") {
        match MacroDocument::from_settings(document.clone()) {
            Ok(macros) => {
                output
                    .values
                    .insert("macro.enabled".into(), json!(macros.enabled));
                let mut items = Vec::new();
                for entry in macros.sequences {
                    let id = entry.identifier.legacy_key();
                    let raw = entry.sequence.to_json()?;
                    let name = raw
                        .get_ci("JiksName")
                        .and_then(JsonNode::as_str)
                        .unwrap_or(&id)
                        .to_owned();
                    let mut item = value(
                        json!({"id":id,"name":name,"trigger_key":entry.identifier.key,"repeat":entry.sequence.repeat_count,"ignore_delays":entry.sequence.ignore_delays,"interrupt_on_other_key":entry.sequence.interrupt_on_other_key}),
                    )?;
                    item.set(
                        "events",
                        JsonNode::Array(
                            entry
                                .sequence
                                .events
                                .iter()
                                .map(macro_event_to_ui)
                                .collect::<Result<_>>()?,
                        ),
                    )?;
                    items.push(Item {
                        id,
                        label: name,
                        detail: format!("{} 个事件", entry.sequence.events.len()),
                        value: item.to_value()?,
                    });
                }
                output.collections.insert("macros".into(), items);
            }
            Err(error) => {
                output
                    .values
                    .insert("macro.validation_error".into(), json!(format!("{error:#}")));
            }
        }
    }
    if let Some(document) = docs.get("godmode") {
        if let Some(id) = document
            .root()
            .get_ci("ActivePresetId")
            .and_then(JsonNode::as_str)
        {
            output.values.insert("selection.presets".into(), json!(id));
            if let Some(name) = document
                .root()
                .get_ci("Presets")
                .and_then(|presets| presets.get_ci(id))
                .and_then(|preset| preset.get_ci("Name"))
                .and_then(JsonNode::as_str)
            {
                output
                    .values
                    .insert("preset.active_name".into(), json!(name));
            }
        }
        let mut items = Vec::new();
        if let Some(presets) = document
            .root()
            .get_ci("Presets")
            .and_then(JsonNode::as_object)
        {
            for (id, preset) in presets {
                let item = preset_to_ui(id, preset)?;
                items.push(Item {
                    id: id.clone(),
                    label: preset
                        .get_ci("Name")
                        .and_then(JsonNode::as_str)
                        .unwrap_or(id)
                        .into(),
                    detail: "自定义性能预设".into(),
                    value: item.to_value()?,
                });
            }
        }
        output.collections.insert("presets".into(), items);
    }
    for (key, choices) in [
        (
            "sensors.cpu_frequency",
            vec![("最高", "maximum"), ("平均", "average")],
        ),
        (
            "sensors.temperature_source",
            vec![
                ("自动", "auto"),
                ("平均", "average"),
                ("Package", "package"),
                ("最高核心", "core_max"),
            ],
        ),
        (
            "sensors.voltage_mode",
            vec![
                ("平均", "average"),
                ("最高", "maximum"),
                ("指定核心", "core"),
            ],
        ),
        (
            "sensors.gpu",
            vec![("独显", "discrete"), ("集显", "integrated")],
        ),
        (
            "sensors.memory_mode",
            vec![("占用率", "percent"), ("GB", "gb")],
        ),
        (
            "sensors.temperature_unit",
            vec![("摄氏", "C"), ("华氏", "F")],
        ),
        (
            "appearance.language",
            vec![("简体中文", "zh-Hans"), ("English", "en")],
        ),
    ] {
        output.options.insert(
            key.into(),
            choices
                .into_iter()
                .map(|(label, value)| Choice {
                    label: label.into(),
                    value: json!(value),
                })
                .collect(),
        );
    }
    output.options.insert(
        "macro.trigger_key".into(),
        TRIGGER_KEYS
            .iter()
            .enumerate()
            .map(|(index, key)| Choice {
                label: format!("数字键盘 {index}"),
                value: json!(key),
            })
            .collect(),
    );
    for (collection, key) in [
        ("presets", "automation.preset"),
        ("automation", "automation.pipeline"),
    ] {
        if let Some(items) = output.collections.get(collection) {
            output.options.insert(
                key.into(),
                items
                    .iter()
                    .map(|item| Choice {
                        label: item.label.clone(),
                        value: json!(item.id),
                    })
                    .collect(),
            );
        }
    }
    if let Some(document) = docs.get("update") {
        let root = document.root();
        let channel = root.get_ci("UpdateChannel");
        output.values.insert(
            "update.channel".into(),
            json!(if channel.is_some_and(|v| v.as_i64() == Some(1)
                || v.as_str().is_some_and(|v| v.eq_ignore_ascii_case("Beta")))
            {
                "preview"
            } else {
                "stable"
            }),
        );
        let method = root
            .get_ci("UpdateMethod")
            .and_then(JsonNode::as_str)
            .unwrap_or("GitHub");
        output.values.insert("update.method".into(), json!(method));
        output.options.insert(
            "update.method".into(),
            vec![Choice {
                label: "GitHub · Jiks发行资产".into(),
                value: json!("GitHub"),
            }],
        );
        let names = [
            "Never",
            "PerHour",
            "PerThreeHours",
            "PerTwelveHours",
            "PerDay",
            "PerWeek",
            "PerMonth",
        ];
        let index = root
            .get_ci("UpdateCheckFrequency")
            .and_then(|v| {
                v.as_i64()
                    .and_then(|v| usize::try_from(v).ok())
                    .or_else(|| {
                        v.as_str()
                            .and_then(|s| names.iter().position(|n| n.eq_ignore_ascii_case(s)))
                    })
            })
            .unwrap_or(4);
        output.values.insert(
            "update.interval_hours".into(),
            json!([0, 1, 3, 12, 24, 168, 720].get(index).copied().unwrap_or(0)),
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn doc(text: &str) -> SettingsDocument {
        SettingsDocument::parse(text).unwrap()
    }
    fn docs() -> Documents {
        BTreeMap::from([
            (
                "application".into(),
                doc(
                    r#"{"Future":1e9999,"future":-0,"GameDetection":{"UseDiscreteGPU":true,"Future":1.234567890123456789}}"#,
                ),
            ),
            ("hardware_sensors".into(), doc("{}")),
            (
                "automation".into(),
                doc(r#"{"IsEnabled":false,"Pipelines":[]}"#),
            ),
            ("macros".into(), doc(r#"{"Sequences":{}}"#)),
            ("godmode".into(), doc(r#"{"Presets":{}}"#)),
            ("osd".into(), doc("{}")),
            ("notifications".into(), doc("{}")),
            ("update".into(), doc("{}")),
            ("sensor_dashboard".into(), doc("{}")),
        ])
    }
    fn payload(v: Value) -> JsonNode {
        value(v).unwrap()
    }
    #[test]
    fn notification_overrides_validate_ordinary_form_edits_reset_leaves_and_preserve_unknown_types()
    {
        let mut d = docs();
        d.insert("notifications".into(),doc(r#"{"Notifications":{"ColorOverrides":{"FutureNotice":{"Future":-0},"CapsLockOn":{"R":1,"G":2,"B":3}},"FutureTable":{"opaque":1e9999}}}"#));
        let change=apply_action("settings.fields.save",&payload(json!({"document":"notifications","fields":{"/Notifications/ColorOverrides/CapsLockOn":{"R":7,"G":8,"B":9},"/Notifications/PositionOverrides/CapsLockOn":"BottomRight","/Notifications/DurationOverrides/CapsLockOn":"Long"}})),&d).unwrap();
        let document = &change
            .documents
            .iter()
            .find(|row| row.0 == "notifications")
            .unwrap()
            .1;
        let text = document.serialize().unwrap();
        assert!(text.contains("1e9999") && text.contains("-0"));
        d.insert("notifications".into(), document.clone());
        let reset=apply_action("settings.fields.save",&payload(json!({"document":"notifications","fields":{"/Notifications/ColorOverrides/CapsLockOn":null}})),&d).unwrap();
        assert!(
            reset.documents[0]
                .1
                .root()
                .get_ci("Notifications")
                .unwrap()
                .get_ci("ColorOverrides")
                .unwrap()
                .get_ci("CapsLockOn")
                .is_none()
        );
        assert!(apply_action("settings.fields.save",&payload(json!({"document":"notifications","fields":{"/Notifications/ColorOverrides/CapsLockOn":{"R":256,"G":0,"B":0}}})),&d).is_err());
    }
    #[test]
    fn gpu_overclock_toggle_projects_the_persisted_enabled_state() {
        for enabled in [false, true] {
            let document = SettingsDocument::from_root(JsonNode::parse(
                &json!({"Enabled":enabled}).to_string(),
            ).unwrap()).unwrap();
            let projection = project(&BTreeMap::from([("gpu_oc".into(), document)])).unwrap();
            assert_eq!(projection.values["gpu.oc.enabled"], json!(enabled));
        }
    }
    #[test]
    fn manual_quick_actions_project_chosen_icon_without_translating_user_names() {
        let d = docs();
        let change=apply_action_with_context("automation.save",&payload(json!({"id":null,"name":"Settings 用户原名","icon":"Lightning","enabled":true,"exclusive":false,"trigger":null,"steps":[{"type":"delay","parameters":{"milliseconds":1}}]})),&d,&ActionContext{new_id:Some("11111111-1111-1111-1111-111111111111".into()),..Default::default()}).unwrap();
        let item = &change.projection.collections["quick_actions"][0];
        assert_eq!(item.label, "Settings 用户原名");
        assert_eq!(item.value["icon"], "Lightning");
        assert!(
            change.documents[0]
                .1
                .serialize()
                .unwrap()
                .contains("IconName")
        );
    }
    #[test]
    fn app_known_updates_preserve_raw_root_and_nested_extensions() {
        let d = docs();
        let change=apply_action("app.preferences.save",&payload(json!({"app.close_to_tray":true,"appearance.theme":"dark","game.detection":{"UseDiscreteGPU":false,"UseGameConfigStore":true,"UseEffectiveGameMode":true}})),&d).unwrap();
        let app = &change.documents[0].1;
        let text = app.serialize().unwrap();
        assert!(
            text.contains("1e9999") && text.contains("-0") && text.contains("1.234567890123456789")
        );
        assert_eq!(
            app.root().get("Theme").and_then(JsonNode::as_str),
            Some("Dark")
        );
        assert!(!change.projection.values.contains_key("Future"));
        assert_eq!(d["application"].root().get("MinimizeOnClose"), None);
    }
    #[test]
    fn window_size_only_forgets_position_and_preserves_size_extensions() {
        let mut d = docs();
        d.insert(
            "application".into(),
            doc(
                r#"{"RememberWindowPosition":false,"WindowSize":{"Width":900,"Height":700,"Future":1e9999,"future":-0},"WindowPosition":{"Left":50,"Top":60,"FuturePosition":1.234567890123456789}}"#,
            ),
        );
        let before = d["application"].serialize().unwrap();
        let change = apply_action(
            "app.preferences.save",
            &payload(json!({"app.window":{"width":1024,"height":768}})),
            &d,
        )
        .unwrap();
        assert_eq!(change.documents.len(), 1);
        let root = change.documents[0].1.root();
        assert_eq!(root.get_ci("WindowPosition"), Some(&JsonNode::Null));
        let size = root.get_ci("WindowSize").unwrap();
        assert_eq!(size.get_ci("Width").and_then(JsonNode::as_f64), Some(1024.));
        assert_eq!(size.get_ci("Height").and_then(JsonNode::as_f64), Some(768.));
        assert_eq!(size.get("Future"), Some(&JsonNode::Number("1e9999".into())));
        assert_eq!(size.get("future"), Some(&JsonNode::Number("-0".into())));
        assert_eq!(
            change.projection.values["app.window"]["position"],
            Value::Null
        );
        assert_eq!(d["application"].serialize().unwrap(), before);
    }
    #[test]
    fn window_position_updates_preserve_nested_extensions() {
        let mut d = docs();
        d.insert(
            "application".into(),
            doc(
                r#"{"RememberWindowPosition":true,"WindowSize":{"Width":900,"Height":700,"FutureSize":-0},"WindowPosition":{"Left":50,"Top":60,"FuturePosition":1.234567890123456789}}"#,
            ),
        );
        let change = apply_action(
            "app.preferences.save",
            &payload(
                json!({"app.window":{"width":1000.5,"height":650.25,"left":-1920.5,"top":80.25}}),
            ),
            &d,
        )
        .unwrap();
        let root = change.documents[0].1.root();
        let size = root.get_ci("WindowSize").unwrap();
        let position = root.get_ci("WindowPosition").unwrap();
        assert_eq!(
            size.get_ci("Width").and_then(JsonNode::as_f64),
            Some(1000.5)
        );
        assert_eq!(
            size.get_ci("Height").and_then(JsonNode::as_f64),
            Some(650.25)
        );
        assert_eq!(
            position.get_ci("Left").and_then(JsonNode::as_f64),
            Some(-1920.5)
        );
        assert_eq!(
            position.get_ci("Top").and_then(JsonNode::as_f64),
            Some(80.25)
        );
        assert_eq!(size.get("FutureSize"), Some(&JsonNode::Number("-0".into())));
        assert_eq!(
            position.get("FuturePosition"),
            Some(&JsonNode::Number("1.234567890123456789".into()))
        );
        assert_eq!(
            change.projection.values["app.window"]["position"]["Left"],
            -1920.5
        );
    }
    #[test]
    fn window_invalid_payload_leaves_pending_and_documents_unchanged() {
        let d = docs();
        let original = d["application"].serialize().unwrap();
        let mut invalid = vec![
            json!(null),
            json!([]),
            json!({}),
            json!({"width":900}),
            json!({"height":700}),
            json!({"width":900,"height":700,"left":0}),
            json!({"width":900,"height":700,"top":0}),
            json!({"width":900,"height":700,"left":null,"top":0}),
            json!({"width":900,"height":700,"left":0,"top":"0"}),
            json!({"width":"900","height":700}),
            json!({"width":900,"height":true}),
            json!({"width":299.9,"height":700}),
            json!({"width":8192.1,"height":700}),
            json!({"width":900,"height":299.9}),
            json!({"width":900,"height":8192.1}),
            json!({"width":900,"height":700,"left":-100000.1,"top":0}),
            json!({"width":900,"height":700,"left":0,"top":100000.1}),
        ]
        .into_iter()
        .map(payload)
        .collect::<Vec<_>>();
        invalid.push(JsonNode::parse(r#"{"width":1e9999,"height":700}"#).unwrap());
        invalid
            .push(JsonNode::parse(r#"{"width":900,"height":700,"left":0,"top":-1e9999}"#).unwrap());
        for field in ["width", "height", "left", "top"] {
            let mut candidate = payload(json!({"width":900,"height":700,"left":0,"top":0}));
            candidate
                .set(field, JsonNode::Number("NaN".into()))
                .unwrap();
            invalid.push(candidate);
        }
        for candidate in invalid {
            let mut pending = BTreeMap::from([
                ("application".into(), d["application"].root().clone()),
                ("osd".into(), payload(json!({"Future":"pending"}))),
            ]);
            let before = pending.clone();
            assert!(
                preference(&mut pending, &d, "app.window", &candidate).is_err(),
                "{candidate:?}"
            );
            assert_eq!(
                pending, before,
                "Rejected window edit changed pending state"
            );
            let envelope = JsonNode::Object(vec![("app.window".into(), candidate)]);
            assert!(apply_action("app.preferences.save", &envelope, &d).is_err());
        }
        assert_eq!(d["application"].serialize().unwrap(), original);
    }
    #[test]
    fn window_numeric_bounds_accept_both_save_shapes() {
        let d = docs();
        for window in [
            json!({"width":300,"height":8192}),
            json!({"width":8192,"height":300,"left":-100000,"top":100000}),
        ] {
            assert!(
                apply_action(
                    "app.preferences.save",
                    &payload(json!({"app.window":window})),
                    &d
                )
                .is_ok()
            );
        }
    }
    #[test]
    fn generic_aliases_cannot_enable_or_disable_an_actual_controller() {
        for alias in ["application", "app", "settings"] {
            for (old, new) in [(false, true), (true, false)] {
                let old = doc(&format!("{{\"EnableCpuThermalControl\":{old}}}"));
                let new = doc(&format!("{{\"EnableCpuThermalControl\":{new}}}"));
                assert!(validate_transition(alias, &old, &new).is_err());
            }
            let old = doc(r#"{"EnableCpuThermalControl":true}"#);
            assert!(validate_transition(alias, &old, &old).is_ok());
        }
    }
    #[test]
    fn generic_policy_changes_are_rejected_but_missing_defaults_are_equivalent() {
        let old = doc("{}");
        let defaults =
            doc(r#"{"CpuThermalSamplingMilliseconds":250,"CpuThermalFirmwareLimitC":88}"#);
        assert!(validate_transition("application", &old, &defaults).is_ok());
        let bad = doc(r#"{"CpuThermalSamplingMilliseconds":100}"#);
        assert!(validate_transition("application", &old, &bad).is_err());
        let d = docs();
        assert!(
            apply_action(
                "settings.fields.save",
                &payload(
                    json!({"document":"application","fields":{"/EnableCpuThermalControl":true}})
                ),
                &d
            )
            .is_err()
        );
    }
    #[test]
    fn dedicated_thermal_validation_is_inert_and_allows_only_safe_contract() {
        let d = docs();
        let change=thermal_change(&payload(json!({"thermal.enabled":true,"thermal.sampling_ms":250,"thermal.firmware_limit_c":88,"sensors.voltage_core":2,"sensors.cpu_frequency":"average"})),&d).unwrap();
        assert_eq!(change.documents.len(), 2);
        assert!(
            change
                .documents
                .iter()
                .find(|(k, _)| k == "application")
                .unwrap()
                .1
                .root()
                .get("EnableCpuThermalControl")
                .unwrap()
                .as_bool()
                .unwrap()
        );
        assert!(
            d["application"]
                .root()
                .get("EnableCpuThermalControl")
                .is_none()
        );
        assert!(
            thermal_change(
                &payload(json!({"thermal.enabled":true,"thermal.sampling_ms":50})),
                &d
            )
            .is_err()
        );
        assert!(thermal_change(&payload(json!({"thermal.firmware_limit_c":85})), &d).is_err());
    }
    #[test]
    fn sensor_display_save_accepts_unchanged_omitted_thermal_defaults() {
        let change=apply_action("sensors.settings.save",&payload(json!({"thermal.enabled":false,"thermal.sampling_ms":250,"thermal.firmware_limit_c":88,"sensors.gpu":"integrated","sensors.memory_mode":"gb","sensors.temperature_source":"core_max"})),&docs()).unwrap();
        assert_eq!(change.projection.values["sensors.gpu"], "integrated");
        assert_eq!(
            change.projection.values["sensors.temperature_source"],
            "core_max"
        );
    }
    #[test]
    fn field_paths_and_late_invalid_values_fail_before_any_document_is_returned() {
        let d = docs();
        for p in [
            json!({"document":"application","fields":{"/GameDetection/UseDiscreteGPU":true,"/../EnableCpuThermalControl":true}}),
            json!({"document":"osd","fields":{"/FontSize":16,"/BackgroundOpacity":2}}),
            json!({"document":"application","fields":{"/$type":"Bad"}}),
            json!({"document":"hardware_sensors","fields":{"/CpuVoltageCoreIndex":-1}}),
        ] {
            assert!(apply_action("settings.fields.save", &payload(p), &d).is_err());
        }
        assert_eq!(d["osd"].serialize().unwrap(), "{}");
    }
    #[test]
    fn macro_ui_save_roundtrip_supports_delay_only_and_signed_wheels() {
        let d = docs();
        let p = payload(
            json!({"id":null,"name":"测试宏","trigger_key":96,"repeat":2,"ignore_delays":false,"interrupt_on_other_key":true,"events":[{"kind":"delay","delay_ms":123},{"kind":"key_down","key":65,"delay_ms":7},{"kind":"key_up","key":65,"delay_ms":0},{"kind":"horizontal_wheel","key":-120,"delay_ms":1}]}),
        );
        let change = apply_action("macro.save", &p, &d).unwrap();
        let document = MacroDocument::from_settings(change.documents[0].1.clone()).unwrap();
        let sequence = &document.sequences[0].sequence;
        assert_eq!(sequence.events.len(), 4);
        sequence.validate_playback().unwrap();
        assert_eq!(sequence.events[0].delay_ticks, 1_230_000);
        assert_eq!(sequence.events[3].key as i32, -120);
        assert_eq!(
            change.projection.collections["macros"][0].value["name"],
            "测试宏"
        );
    }
    #[test]
    fn macro_event_edit_keeps_100ns_precision_when_ms_field_is_unchanged() {
        let event = MacroEvent::from_json(
            &JsonNode::parse(
                r#"{"Source":1,"Direction":2,"Key":65,"Delay":"00:00:00.0000007","Future":1e9999}"#,
            )
            .unwrap(),
        )
        .unwrap();
        let ui = macro_event_to_ui(&event).unwrap();
        let edited = parse_macro_event(&ui).unwrap();
        assert_eq!(edited.delay_ticks, 7);
        assert!(edited.to_json().unwrap().to_json().contains("1e9999"));
    }
    #[test]
    fn new_pipeline_needs_injected_guid_and_saves_real_legacy_dto() {
        let d = docs();
        let p = payload(
            json!({"name":"Fixture","exclusive":false,"enabled":true,"trigger":{"type":"ac_connected","parameters":{}},"steps":[{"type":"delay","parameters":{"milliseconds":250}}]}),
        );
        assert!(apply_action("automation.save", &p, &d).is_err());
        let change = apply_action_with_id(
            "automation.save",
            &p,
            &d,
            "11111111-1111-1111-1111-111111111111",
        )
        .unwrap();
        let parsed = parse_automation(&change.documents[0].1).unwrap();
        assert_eq!(parsed.pipelines.len(), 1);
        assert_eq!(
            change.projection.collections["automation"][0].value["steps"][0]["parameters"]["milliseconds"],
            250
        );
    }
    #[test]
    fn preset_save_keeps_order_unknown_fields_and_explicit_null_clear() {
        let mut d = docs();
        let first = "11111111-1111-1111-1111-111111111111";
        let second = "22222222-2222-2222-2222-222222222222";
        d.insert("godmode".into(),doc(&format!("{{\"Presets\":{{\"{first}\":{{\"Name\":\"old\",\"Future\":1e9999,\"CPULongTermPowerLimit\":{{\"Value\":70,\"Min\":50,\"Max\":135,\"Step\":1,\"Future\":-0}},\"FanTable\":{{\"FSTM\":1}}}},\"{second}\":{{\"Name\":\"second\"}}}}}}")));
        let change=apply_action("preset.save",&payload(json!({"id":first,"name":"updated","values":{"cpu.pl1":71},"fan":{"table":null,"full_speed":null}})),&d).unwrap();
        let presets = change.documents[0].1.root().get("Presets").unwrap();
        assert_eq!(presets.as_object().unwrap()[0].0, first);
        assert!(presets.to_json().contains("1e9999") && presets.to_json().contains("-0"));
        assert_eq!(
            presets.get(first).unwrap().get("FanTable"),
            Some(&JsonNode::Null)
        );
    }
    #[test]
    fn preset_creation_requires_verified_ranges_and_cross_field_validity() {
        let d = docs();
        let p = payload(
            json!({"name":"New","values":{"cpu.pl1":70,"cpu.pl2":125,"cpu.temperature":88}}),
        );
        let id = "11111111-1111-1111-1111-111111111111";
        assert!(apply_action_with_id("preset.save", &p, &d, id).is_err());
        let mut context = ActionContext {
            new_id: Some(id.into()),
            ..Default::default()
        };
        for (key, min, max) in [
            ("cpu.pl1", 50, 135),
            ("cpu.pl2", 60, 195),
            ("cpu.temperature", 50, 100),
        ] {
            context.ranges.insert(
                key.into(),
                Range {
                    minimum: min,
                    maximum: max,
                    step: 1,
                    unit: "fixture".into(),
                },
            );
        }
        assert!(apply_action_with_context("preset.save", &p, &d, &context).is_ok());
        let bad = payload(
            json!({"name":"Bad","values":{"cpu.pl1":130,"cpu.pl2":100,"cpu.temperature":88}}),
        );
        assert!(apply_action_with_context("preset.save", &bad, &d, &context).is_err());
    }
    #[test]
    fn irregular_discrete_preset_values_never_accept_the_gap() {
        let d = docs();
        let mut c = ActionContext {
            new_id: Some("11111111-1111-1111-1111-111111111111".into()),
            ..Default::default()
        };
        c.discrete.insert("cpu.pl1".into(), vec![50, 70, 95]);
        let valid = payload(json!({"name":"Discrete","values":{"cpu.pl1":70}}));
        assert!(apply_action_with_context("preset.save", &valid, &d, &c).is_ok());
        let gap = payload(json!({"name":"Gap","values":{"cpu.pl1":71}}));
        assert!(apply_action_with_context("preset.save", &gap, &d, &c).is_err());
    }
}
