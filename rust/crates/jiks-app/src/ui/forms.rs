//! Fixed, user-facing schemas for legacy settings. JSON paths are never user input.
use super::Kind;
use crate::ui::i18n::{format_product, tr, tr_static};
use crate::ui_model::{UiChoice, UiModel};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) struct Field {
    pub path: String,
    pub label: String,
    pub kind: Kind,
    pub default: Value,
    pub range: Option<(f64, f64)>,
}
pub(super) struct Section {
    pub title: &'static str,
    pub fields: Vec<Field>,
}
pub(super) struct Form {
    pub title: &'static str,
    pub document: &'static str,
    pub sections: Vec<Section>,
}
fn field(path: &str, label: &str, kind: Kind, default: Value) -> Field {
    Field {
        path: path.into(),
        label: tr(label).into_owned(),
        kind,
        default,
        range: None,
    }
}
fn check(path: &str, label: &str, default: bool) -> Field {
    field(path, label, Kind::Check, json!(default))
}
fn number(path: &str, label: &str, default: f64, min: f64, max: f64) -> Field {
    Field {
        range: Some((min, max)),
        ..field(path, label, Kind::Number, json!(default))
    }
}
fn text(path: &str, label: &str, default: &str) -> Field {
    field(path, label, Kind::Text, json!(default))
}
fn select(path: &str, label: &str, default: &str, options: &[(&str, &str)]) -> Field {
    field(
        path,
        label,
        Kind::Choice(
            options
                .iter()
                .map(|(label, value)| UiChoice::new(tr(label).into_owned(), *value))
                .collect(),
        ),
        json!(default),
    )
}
fn section(title: &'static str, fields: Vec<Field>) -> Section {
    Section {
        title: tr_static(title),
        fields,
    }
}
pub(super) fn schema(id: &str, model: &UiModel) -> Option<Form> {
    let (title, document, sections) = match id {
        "balance" => (
            "均衡 AI 设置",
            "balance",
            vec![section(
                "根据供电、均衡模式与实际游戏状态切换",
                vec![check("/AIModeEnabled", "启用均衡 AI 条件监听", false)],
            )],
        ),
        "notifications" => (
            "提示弹窗",
            "notifications",
            vec![
                section(
                    "通用",
                    vec![
                        check("/DontShowNotifications", "禁用提示弹窗", false),
                        select(
                            "/NotificationPosition",
                            "弹窗位置",
                            "BottomCenter",
                            &[
                                ("右下", "BottomRight"),
                                ("下中", "BottomCenter"),
                                ("左下", "BottomLeft"),
                                ("左中", "CenterLeft"),
                                ("左上", "TopLeft"),
                                ("上中", "TopCenter"),
                                ("右上", "TopRight"),
                                ("右中", "CenterRight"),
                                ("居中", "Center"),
                            ],
                        ),
                        select(
                            "/NotificationDuration",
                            "显示时长",
                            "Normal",
                            &[("短", "Short"), ("正常", "Normal"), ("长", "Long")],
                        ),
                        check("/NotificationAlwaysOnTop", "始终置顶", false),
                        check("/NotificationOnAllScreens", "在所有显示器上显示", false),
                    ],
                ),
                section(
                    "提示类型",
                    [
                        ("UpdateAvailable", "有可用更新", true),
                        ("AutomationNotification", "自动化通知", true),
                        ("CapsLock", "大写锁定", true),
                        ("NumLock", "数字锁定", true),
                        ("FnLock", "Fn 锁定", true),
                        ("TouchpadLock", "触控板", true),
                        ("KeyboardBacklight", "键盘背光", true),
                        ("CameraLock", "摄像头", true),
                        ("Microphone", "麦克风", true),
                        ("AirplaneMode", "飞行模式", true),
                        ("PowerMode", "性能模式", true),
                        ("RefreshRate", "刷新率", true),
                        ("ACAdapter", "电源适配器", false),
                        ("SmartKey", "智能按键", false),
                    ]
                    .iter()
                    .map(|(key, label, default)| {
                        check(&format!("/Notifications/{key}"), label, *default)
                    })
                    .collect(),
                ),
                section("单类型定制", vec![]),
            ],
        ),
        "osd" => (
            "OSD 设置",
            "osd",
            vec![
                section(
                    "显示",
                    vec![
                        check("/ShowOsd", "显示 OSD", false),
                        field(
                            "/SelectedStyleIndex",
                            "显示样式",
                            Kind::Choice(vec![
                                UiChoice::new(tr_static("面板"), 0),
                                UiChoice::new(tr_static("横条"), 1),
                            ]),
                            json!(0),
                        ),
                        number("/OsdRefreshInterval", "刷新间隔 · 秒", 1., 0.1, 60.),
                        check("/IsLocked", "锁定位置", false),
                        number("/SnapThreshold", "边缘吸附距离", 20., 0., 200.),
                        number("/BackgroundOpacity", "背景透明度 · 0–1", 0.6, 0., 1.),
                        number("/FontSize", "字体大小", 12., 8., 72.),
                        number("/CornerRadiusTop", "顶部圆角", 6., 0., 50.),
                        number("/CornerRadiusBottom", "底部圆角", 6., 0., 50.),
                    ],
                ),
                section(
                    "配色",
                    vec![
                        text("/BackgroundColor", "背景色 · #RRGGBB", "#1E1E1E"),
                        text("/CategoryColor", "类别颜色", "#2196F3"),
                        text("/LabelColor", "名称颜色", "#ADFF2F"),
                        text("/ValueColor", "数值颜色", "#FFFFFF"),
                        text("/WarningColor", "警告颜色", "#FFFF00"),
                        text("/CriticalColor", "危险颜色", "#FF0000"),
                    ],
                ),
                section(
                    "阈值",
                    vec![
                        number("/TempThresholdWarning", "温度警告 · °C", 75., 0., 110.),
                        number("/TempThresholdCritical", "温度危险 · °C", 90., 0., 110.),
                        number("/UsageThresholdWarning", "占用率警告 · %", 70., 0., 100.),
                        number("/UsageThresholdCritical", "占用率危险 · %", 90., 0., 100.),
                        number("/FpsThresholdCritical", "FPS 危险阈值", 30., 1., 1000.),
                        number("/LowFpsDeltaThreshold", "低帧率差值阈值", 30., 0., 1000.),
                    ],
                ),
                section(
                    "传感器项目",
                    vec![field(
                        "/Items",
                        "在 OSD 中显示 · 可多选",
                        Kind::MultiChoice(crate::ui::overlay::item_choices()),
                        json!(
                            crate::ui::overlay::item_choices()
                                .into_iter()
                                .map(|c| c.value)
                                .collect::<Vec<_>>()
                        ),
                    )],
                ),
            ],
        ),
        "game_detection" => (
            "游戏检测",
            "application",
            vec![
                section(
                    "检测方式",
                    vec![
                        check("/GameDetection/UseDiscreteGPU", "检测独立显卡进程", true),
                        check(
                            "/GameDetection/UseGameConfigStore",
                            "使用 Windows 游戏列表",
                            true,
                        ),
                        check(
                            "/GameDetection/UseEffectiveGameMode",
                            "检测 Windows 游戏模式",
                            true,
                        ),
                    ],
                ),
                section(
                    "进程列表",
                    vec![
                        field(
                            "/IncludedProcesses",
                            "始终视为游戏 · 每行一个进程",
                            Kind::Lines,
                            json!([]),
                        ),
                        field(
                            "/ExcludedProcesses",
                            "忽略的进程 · 每行一个进程",
                            Kind::Lines,
                            json!([]),
                        ),
                    ],
                ),
            ],
        ),
        "power_modes" | "power_mapping" => {
            let plans = id == "power_mapping";
            let mut fields = Vec::new();
            for (key, label) in [
                ("Quiet", "安静"),
                ("Balance", "均衡"),
                ("Performance", "野兽"),
                ("Extreme", "超能"),
                ("GodMode", "自定义"),
            ] {
                if key == "Extreme"
                    && !model.options.get("power.mode").is_some_and(|options| {
                        options
                            .iter()
                            .any(|option| option.value.as_u64() == Some(224))
                    })
                {
                    continue;
                }
                let group = if plans { "PowerPlans" } else { "PowerModes" };
                let number = match key {
                    "Quiet" => "0",
                    "Balance" => "1",
                    "Performance" => "2",
                    "Extreme" => "223",
                    _ => "254",
                };
                let root = model
                    .documents
                    .get("application")
                    .map(|document| &document.value);
                let field_key = if root
                    .and_then(|root| get(root, &format!("/{group}/{key}")))
                    .is_none()
                    && root
                        .and_then(|root| get(root, &format!("/{group}/{number}")))
                        .is_some()
                {
                    number
                } else {
                    key
                };
                let path = format!("/{group}/{field_key}");
                let numeric = root
                    .and_then(|root| get(root, &path))
                    .and_then(Value::as_i64)
                    .is_some_and(|value| (0..=2).contains(&value));
                let options = if plans {
                    model.options.get("power.plan").cloned().unwrap_or_default()
                } else {
                    vec![
                        UiChoice {
                            label: tr_static("最佳能效").into(),
                            value: if numeric {
                                json!(0)
                            } else {
                                json!("BestPowerEfficiency")
                            },
                        },
                        UiChoice {
                            label: tr_static("平衡").into(),
                            value: if numeric { json!(1) } else { json!("Balanced") },
                        },
                        UiChoice {
                            label: tr_static("最佳性能").into(),
                            value: if numeric {
                                json!(2)
                            } else {
                                json!("BestPerformance")
                            },
                        },
                    ]
                };
                fields.push(field(&path, label, Kind::Choice(options), Value::Null));
            }
            (
                if plans {
                    "Windows 电源计划"
                } else {
                    "Windows 电源模式"
                },
                "application",
                vec![section("对应关系", fields)],
            )
        }
        "hardware_sensors" => (
            "传感器显示",
            "hardware_sensors",
            vec![section(
                "传感器",
                vec![
                    check("/SelectedGpuIsIgpu", "显示集成显卡", false),
                    check("/ShowCpuAverageFrequency", "显示 CPU 平均频率", false),
                    check("/DisplayMemoryInGigabytes", "按 GB 显示内存", false),
                    select(
                        "/CpuVoltageMode",
                        "电压显示",
                        "Average",
                        &[
                            ("平均", "Average"),
                            ("最高", "Maximum"),
                            ("指定核心", "Core"),
                        ],
                    ),
                    number("/CpuVoltageCoreIndex", "核心编号", 0., 0., 255.),
                    select(
                        "/CpuTemperatureSource",
                        "温度来源",
                        "Auto",
                        &[
                            ("自动", "Auto"),
                            ("平均", "Average"),
                            ("CPU Package", "Package"),
                            ("最高核心温度", "CoreMax"),
                        ],
                    ),
                ],
            )],
        ),
        "excluded_refresh_rates" => (
            "排除刷新率",
            "application",
            vec![section(
                "刷新率",
                vec![field(
                    "/ExcludedRefreshRates",
                    "不在快捷切换中使用 · 可多选",
                    Kind::MultiChoice(excluded_rate_options(model)),
                    json!([]),
                )],
            )],
        ),
        "sensor_dashboard" => (
            "传感器分组",
            "sensor_dashboard",
            vec![section(
                "显示",
                vec![
                    check("/ShowSensors", "显示传感器", true),
                    number(
                        "/SensorsRefreshIntervalSeconds",
                        "刷新间隔 · 秒",
                        1.,
                        0.1,
                        60.,
                    ),
                ],
            )],
        ),
        "dashboard" => (
            "自定义控制台",
            "dashboard",
            vec![section(
                "传感器",
                vec![
                    check("/ShowSensors", "显示传感器", true),
                    number(
                        "/SensorsRefreshIntervalSeconds",
                        "刷新间隔 · 秒",
                        1.,
                        0.1,
                        60.,
                    ),
                ],
            )],
        ),
        _ => return None,
    };
    Some(Form {
        title: tr_static(title),
        document,
        sections,
    })
}
fn rate(value: &Value) -> Option<(u32, bool, u32)> {
    if let Some(frequency) = value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0 && *value <= 1000)
    {
        return Some((frequency, false, 0));
    }
    if let Some(text) = value.as_str() {
        if let Some(rest) = text.strip_prefix("dynamic:") {
            let mut parts = rest.split(':');
            let base = parts.next()?.parse().ok()?;
            let frequency = parts.next()?.parse().ok()?;
            if parts.next().is_none() && base > 0 && frequency >= base && frequency <= 1000 {
                return Some((frequency, true, base));
            }
        } else if let Ok(frequency) = text.parse::<u32>() {
            if frequency > 0 && frequency <= 1000 {
                return Some((frequency, false, 0));
            }
        }
    }
    let frequency = get(value, "/Frequency")?
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0 && *value <= 1000)?;
    let dynamic = get(value, "/IsDynamic")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let base = if dynamic {
        get(value, "/BaseFrequency")?
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0 && *value <= frequency)?
    } else {
        0
    };
    Some((frequency, dynamic, base))
}
fn excluded_rate_options(model: &UiModel) -> Vec<UiChoice> {
    let existing = model
        .documents
        .get("application")
        .and_then(|document| get(&document.value, "/ExcludedRefreshRates"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut options = Vec::new();
    for option in model
        .options
        .get("display.refresh_rate")
        .into_iter()
        .flatten()
    {
        if let Some((frequency, dynamic, base)) = rate(&option.value) {
            let value = existing
                .iter()
                .find(|value| rate(value) == Some((frequency, dynamic, base)))
                .cloned()
                .unwrap_or_else(
                    || json!({"Frequency":frequency,"IsDynamic":dynamic,"BaseFrequency":base}),
                );
            if !options
                .iter()
                .any(|option: &UiChoice| option.value == value)
            {
                options.push(UiChoice {
                    label: option.label.clone(),
                    value,
                });
            }
        }
    }
    for (index, value) in existing.into_iter().enumerate() {
        if !options.iter().any(|option| option.value == value) {
            let label = rate(&value)
                .map(|(frequency, dynamic, base)| {
                    if dynamic {
                        tr_static("保留的动态刷新率 {base}–{frequency} Hz")
                            .replace("{base}", &base.to_string())
                            .replace("{frequency}", &frequency.to_string())
                    } else {
                        tr_static("保留的刷新率 {frequency} Hz")
                            .replace("{frequency}", &frequency.to_string())
                    }
                })
                .unwrap_or_else(|| {
                    tr_static("保留的原有排除项 {}").replacen("{}", &(index + 1).to_string(), 1)
                });
            options.push(UiChoice { label, value });
        }
    }
    options
}
/// Exact spelling wins; case-insensitive legacy spelling is used only as fallback.
pub(super) fn get<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut node = root;
    for key in path.trim_start_matches('/').split('/') {
        let object = node.as_object()?;
        node = object.get(key).or_else(|| {
            object
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v)
        })?;
    }
    Some(node)
}
pub(super) fn seed(form: &Form, data: &Value) -> BTreeMap<String, Value> {
    form.sections
        .iter()
        .flat_map(|s| &s.fields)
        .map(|f| {
            (
                f.path.clone(),
                get(data, &f.path)
                    .cloned()
                    .unwrap_or_else(|| f.default.clone()),
            )
        })
        .collect()
}
pub(super) fn validate(form: &Form, drafts: &BTreeMap<String, Value>) -> Result<Value> {
    let mut map = serde_json::Map::new();
    for f in form.sections.iter().flat_map(|s| &s.fields) {
        let mut v = drafts.get(&f.path).cloned().unwrap_or(Value::Null);
        if v.is_null() {
            continue;
        }
        if let Some((min, max)) = f.range {
            let Some(n) = v.as_f64().filter(|n| n.is_finite()) else {
                bail!("{}", format_product("{}需要有效数值。", &[&f.label]))
            };
            if n < min || n > max {
                bail!(
                    "{}",
                    format_product(
                        "{}范围为 {}–{}。",
                        &[&f.label, &min.to_string(), &max.to_string()]
                    )
                );
            }
            // Integer settings retain an integer JSON type.
            if f.path != "/OsdRefreshInterval"
                && f.path != "/SensorsRefreshIntervalSeconds"
                && f.path != "/BackgroundOpacity"
                && n.fract() != 0.
            {
                bail!("{}", format_product("{}需要整数。", &[&f.label]));
            }
        }
        if f.path.ends_with("Color") {
            let color = v.as_str().unwrap_or("");
            if color.len() != 7
                || !color.starts_with('#')
                || !color[1..].bytes().all(|b| b.is_ascii_hexdigit())
            {
                bail!("{}", format_product("{}请使用 #RRGGBB。", &[&f.label]));
            }
        }
        if let Kind::MultiChoice(options) = &f.kind {
            if options.is_empty() {
                continue;
            }
            if !v
                .as_array()
                .is_some_and(|a| a.iter().all(|v| options.iter().any(|o| o.value == *v)))
            {
                bail!("{}", tr_static("请选择本机提供的刷新率。"));
            }
        }
        if f.range.is_some()
            && ![
                "/OsdRefreshInterval",
                "/SensorsRefreshIntervalSeconds",
                "/BackgroundOpacity",
            ]
            .contains(&f.path.as_str())
        {
            if let Some(n) = v.as_f64() {
                v = json!(n as i64);
            }
        }
        map.insert(f.path.clone(), v);
    }
    for (warning, critical) in [
        ("/TempThresholdWarning", "/TempThresholdCritical"),
        ("/UsageThresholdWarning", "/UsageThresholdCritical"),
    ] {
        if let (Some(w), Some(c)) = (
            map.get(warning).and_then(Value::as_f64),
            map.get(critical).and_then(Value::as_f64),
        ) {
            if w > c {
                bail!("{}", tr_static("警告阈值不能高于危险阈值。"));
            }
        }
    }
    Ok(Value::Object(map))
}

const NOTIFICATION_TABLES: [(&str, &str); 5] = [
    ("IconOverrides", "图标编号（空白继承）"),
    ("ColorOverrides", "图标颜色（空白继承）"),
    ("TextColorOverrides", "文字颜色（空白继承）"),
    ("PositionOverrides", "弹窗位置"),
    ("DurationOverrides", "显示时长"),
];
pub(super) fn notification_types() -> Vec<UiChoice> {
    crate::ui::notifications::TYPES
        .iter()
        .enumerate()
        .map(|(index, _)| {
            UiChoice::new(
                tr_static(crate::ui::notifications::default_text(index)),
                index as u32,
            )
        })
        .collect()
}
fn notification_key(index: usize, table: &str) -> String {
    format!("@notification/{index}/{table}")
}
fn original_override<'a>(data: &'a Value, index: usize, table: &str) -> Option<&'a Value> {
    let root = get(data, &format!("/Notifications/{table}"))?;
    root.get(crate::ui::notifications::TYPES[index])
        .filter(|v| !v.is_null())
        .or_else(|| root.get(index.to_string()).filter(|v| !v.is_null()))
}
fn color_editor_value(value: &Value) -> Value {
    let component = |key: &str| {
        value
            .get(key)
            .or_else(|| value.get(key.to_ascii_lowercase()))
            .and_then(Value::as_u64)
            .filter(|v| *v <= 255)
    };
    match (component("R"), component("G"), component("B")) {
        (Some(r), Some(g), Some(b)) => json!(format!("#{r:02X}{g:02X}{b:02X}")),
        _ => json!(tr_static("原有值无法预览（保持不变则保留）")),
    }
}
fn notification_editor_value(value: Option<&Value>, table: &str) -> Value {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Value::Null;
    };
    match table {
        "ColorOverrides" | "TextColorOverrides" => color_editor_value(value),
        "IconOverrides" => value
            .as_u64()
            .filter(|value| *value <= u32::MAX as u64)
            .map(|_| value.clone())
            .unwrap_or_else(|| json!(tr_static("原有值无法预览（保持不变则保留）"))),
        "PositionOverrides" | "DurationOverrides" => {
            let names: &[&str] = if table == "PositionOverrides" {
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
                ]
            } else {
                &["Short", "Normal", "Long"]
            };
            if let Some(index) = value.as_u64().and_then(|v| usize::try_from(v).ok()) {
                return names
                    .get(index)
                    .map(|name| json!(name))
                    .unwrap_or_else(|| value.clone());
            }
            value.clone()
        }
        _ => value.clone(),
    }
}
pub(super) fn notification_fields(index: usize, data: &Value) -> Vec<Field> {
    if index >= crate::ui::notifications::TYPES.len() {
        return vec![];
    }
    NOTIFICATION_TABLES
        .iter()
        .map(|(table, label)| {
            let default = notification_editor_value(original_override(data, index, table), table);
            let kind = match *table {
                "PositionOverrides" | "DurationOverrides" => {
                    let choices: &[(&str, &str)] = if *table == "PositionOverrides" {
                        &[
                            ("右下", "BottomRight"),
                            ("下中", "BottomCenter"),
                            ("左下", "BottomLeft"),
                            ("左中", "CenterLeft"),
                            ("左上", "TopLeft"),
                            ("上中", "TopCenter"),
                            ("右上", "TopRight"),
                            ("右中", "CenterRight"),
                            ("居中", "Center"),
                        ]
                    } else {
                        &[("短", "Short"), ("正常", "Normal"), ("长", "Long")]
                    };
                    let mut options = vec![UiChoice::new(tr_static("继承通用设置"), Value::Null)];
                    options.extend(
                        choices
                            .iter()
                            .map(|(label, value)| UiChoice::new(tr_static(label), *value)),
                    );
                    // Unknown legacy values are retained until the user explicitly chooses a replacement.
                    if !options.iter().any(|option| option.value == default) {
                        options.push(UiChoice::new(tr_static("保留原有值"), default.clone()));
                    }
                    Kind::Choice(options)
                }
                "IconOverrides" if default.is_string() => Kind::Text,
                "IconOverrides" => Kind::Number,
                _ => Kind::Text,
            };
            field(&notification_key(index, table), label, kind, default)
        })
        .collect()
}
pub(super) fn reset_notification_draft(index: usize, drafts: &mut BTreeMap<String, Value>) {
    if index < crate::ui::notifications::TYPES.len() {
        for (table, _) in NOTIFICATION_TABLES {
            drafts.insert(notification_key(index, table), Value::Null);
        }
    }
}
/// Draft selectors never become JSON paths. Only changed leaf overrides are emitted.
/// Unknown dictionaries and untouched malformed values stay in the original document.
pub(super) fn notification_patch(data: &Value, drafts: &BTreeMap<String, Value>) -> Result<Value> {
    let mut patch = serde_json::Map::new();
    for (index, name) in crate::ui::notifications::TYPES.iter().enumerate() {
        for (table, label) in NOTIFICATION_TABLES {
            let Some(draft) = drafts.get(&notification_key(index, table)) else {
                continue;
            };
            let old = original_override(data, index, table);
            // Native empty text and nullable inherited fields are equivalent.
            if old.is_none()
                && (draft.is_null() || draft.as_str().is_some_and(|text| text.trim().is_empty()))
            {
                continue;
            }
            let default = notification_editor_value(old, table);
            let opaque = "原有值无法预览（保持不变则保留）";
            let untouched_opaque = default
                .as_str()
                .is_some_and(|value| crate::ui::i18n::matches_product_label(value, opaque))
                && draft
                    .as_str()
                    .is_some_and(|value| crate::ui::i18n::matches_product_label(value, opaque));
            if *draft == default || untouched_opaque {
                continue;
            }
            let value = if draft.is_null() || draft.as_str().is_some_and(|v| v.trim().is_empty()) {
                Value::Null
            } else {
                match table {
                    "IconOverrides" => {
                        let number = draft
                            .as_u64()
                            .or_else(|| draft.as_str()?.trim().parse().ok())
                            .filter(|v| *v <= u32::MAX as u64)
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "{}",
                                    format_product("{}需要非负整数。", &[tr_static(label)])
                                )
                            })?;
                        json!(number)
                    }
                    "ColorOverrides" | "TextColorOverrides" => {
                        let text = draft.as_str().map(str::trim).unwrap_or("");
                        if text.len() != 7
                            || !text.starts_with('#')
                            || !text.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
                        {
                            bail!(
                                "{}",
                                format_product(
                                    "{}请使用 #RRGGBB 或留空继承。",
                                    &[tr_static(label)]
                                )
                            );
                        }
                        let rgb = u32::from_str_radix(&text[1..], 16)?;
                        let mut color = old
                            .filter(|v| v.is_object())
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        for (key, number) in
                            [("R", rgb >> 16), ("G", (rgb >> 8) & 255), ("B", rgb & 255)]
                        {
                            let object = color.as_object_mut().unwrap();
                            let actual = object
                                .keys()
                                .find(|old| old.eq_ignore_ascii_case(key))
                                .cloned()
                                .unwrap_or_else(|| key.into());
                            object.insert(actual, json!(number));
                        }
                        color
                    }
                    "PositionOverrides" | "DurationOverrides" => {
                        let choices = notification_fields(index, data);
                        let field = choices
                            .iter()
                            .find(|f| f.path == notification_key(index, table))
                            .unwrap();
                        let Kind::Choice(options) = &field.kind else {
                            unreachable!()
                        };
                        if !options.iter().any(|option| option.value == *draft) {
                            bail!(
                                "{}",
                                format_product("{}请选择有效选项。", &[tr_static(label)])
                            );
                        }
                        draft.clone()
                    }
                    _ => unreachable!(),
                }
            };
            let root = get(data, &format!("/Notifications/{table}"));
            let numeric = index.to_string();
            let key = if root.and_then(|root| root.get(*name)).is_some() {
                *name
            } else if root.and_then(|root| root.get(&numeric)).is_some() {
                &numeric
            } else {
                *name
            };
            patch.insert(format!("/Notifications/{table}/{key}"), value.clone());
            if value.is_null() {
                for alias in [*name, numeric.as_str()] {
                    if alias != key && root.and_then(|root| root.get(alias)).is_some() {
                        patch.insert(format!("/Notifications/{table}/{alias}"), Value::Null);
                    }
                }
            }
        }
    }
    Ok(Value::Object(patch))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_case_and_unshown_values_are_not_saved_as_defaults() {
        let f = schema("notifications", &UiModel::default()).unwrap();
        let d = json!({"dontshownotifications":true,"Notifications":{"CapsLock":false,"Future":17},"Future":42});
        let values = validate(&f, &seed(&f, &d)).unwrap();
        assert_eq!(values["/DontShowNotifications"], true);
        assert_eq!(values["/Notifications/CapsLock"], false);
        assert!(values.get("/Future").is_none() && values.get("/Notifications/Future").is_none());
    }
    #[test]
    fn invalid_threshold_and_color_are_rejected() {
        let f = schema("osd", &UiModel::default()).unwrap();
        let mut d = seed(&f, &json!({}));
        d.insert("/TempThresholdWarning".into(), json!(99));
        assert!(validate(&f, &d).is_err());
        d.insert("/TempThresholdWarning".into(), json!(75));
        d.insert("/ValueColor".into(), json!("bad"));
        assert!(validate(&f, &d).is_err());
    }
}

#[cfg(test)]
mod translation_compatibility_tests {
    use super::*;
    use crate::ui::i18n::TestLanguage;
    use crate::ui_model::UiDocument;
    #[test]
    fn product_labels_translate_without_changing_power_plan_names_or_saved_values() {
        let _restore = TestLanguage::new("en");
        let mut model = UiModel::default();
        model.options.insert(
            "power.plan".into(),
            vec![UiChoice::new("设置", "user-plan-id")],
        );
        let form = schema("power_mapping", &model).unwrap();
        assert_eq!(form.title, "Windows Power Plans");
        let fields = &form.sections[0].fields;
        assert_eq!(fields[0].label, "Quiet");
        let Kind::Choice(options) = &fields[0].kind else {
            panic!("expected plans")
        };
        assert_eq!(options[0].label, "设置");
        let data = json!({"PowerPlans":{"Quiet":"user-plan-id"},"unknown":17});
        let saved = validate(&form, &seed(&form, &data)).unwrap();
        assert_eq!(saved["/PowerPlans/Quiet"], "user-plan-id");
        assert!(saved.get("/unknown").is_none());
    }
    #[test]
    fn excluded_rate_dtos_and_unknown_existing_choices_survive_localized_round_trip() {
        let _restore = TestLanguage::new("en");
        let mut model = UiModel::default();
        let prior = json!([{"Frequency":240,"BaseFrequency":120,"IsDynamic":true,"Future":17},{"FutureFormat":true}]);
        model.documents.insert(
            "application".into(),
            UiDocument {
                value: json!({"ExcludedRefreshRates":prior}),
                ..Default::default()
            },
        );
        model.options.insert(
            "display.refresh_rate".into(),
            vec![UiChoice::new("120–240 Hz", "dynamic:120:240")],
        );
        let form = schema("excluded_refresh_rates", &model).unwrap();
        let values = seed(&form, &model.documents["application"].value);
        let saved = validate(&form, &values).unwrap();
        assert_eq!(saved["/ExcludedRefreshRates"], prior);
    }
}

#[cfg(test)]
mod notification_customization_tests {
    use super::*;
    #[test]
    fn language_change_preserves_opaque_overrides_and_does_not_block_unrelated_edits() {
        let _restore = crate::ui::i18n::TestLanguage::new("zh-Hans");
        let data = json!({"Notifications":{"ColorOverrides":{"CameraOn":{"future":17}},"IconOverrides":{"CameraOn":{"future":9}}}});
        let mut drafts = BTreeMap::new();
        for field in notification_fields(6, &data) {
            if field.path.ends_with("IconOverrides") {
                assert!(matches!(field.kind, Kind::Text));
            }
            drafts.insert(field.path, field.default);
        }
        crate::ui::i18n::set_language("en");
        assert_eq!(notification_patch(&data, &drafts).unwrap(), json!({}));
        drafts.insert(notification_key(6, "DurationOverrides"), json!("Long"));
        let patch = notification_patch(&data, &drafts).unwrap();
        assert_eq!(
            patch,
            json!({"/Notifications/DurationOverrides/CameraOn":"Long"})
        );
    }
    #[test]
    fn selector_has_all_original_43_types_and_does_not_enter_the_patch() {
        let types = notification_types();
        assert_eq!(types.len(), 43);
        for (index, choice) in types.iter().enumerate() {
            assert_eq!(choice.value, json!(index));
            assert!(!choice.label.is_empty());
        }
        let mut drafts = BTreeMap::new();
        drafts.insert("@notification.selected_type".into(), json!(42));
        assert_eq!(notification_patch(&json!({}), &drafts).unwrap(), json!({}));
    }
    #[test]
    fn visits_and_unknown_malformed_values_do_not_modify_configuration() {
        let data = json!({"Notifications":{"PositionOverrides":{"5":222,"Future":17},"ColorOverrides":{"AutomationNotification":{"R":1,"G":2,"B":3,"Future":17}}},"Future":99});
        let mut drafts = BTreeMap::new();
        for index in [5, 6] {
            for f in notification_fields(index, &data) {
                drafts.insert(f.path, f.default);
            }
        }
        assert_eq!(notification_patch(&data, &drafts).unwrap(), json!({}));
        for index in [5, 6] {
            for (table, _) in NOTIFICATION_TABLES {
                let key = notification_key(index, table);
                if drafts.get(&key) == Some(&Value::Null)
                    && table != "PositionOverrides"
                    && table != "DurationOverrides"
                {
                    drafts.insert(key, json!(""));
                }
            }
        }
        assert_eq!(notification_patch(&data, &drafts).unwrap(), json!({}));
        let bad = json!({"Notifications":{"ColorOverrides":{"CameraOn":{"future":17}},"IconOverrides":{"CameraOn":{"future":9}}}});
        let mut captured = BTreeMap::new();
        for field in notification_fields(6, &bad) {
            captured.insert(field.path, field.default);
        }
        assert_eq!(notification_patch(&bad, &captured).unwrap(), json!({}));
        drafts.insert(notification_key(5, "ColorOverrides"), json!("#102030"));
        let patch = notification_patch(&data, &drafts).unwrap();
        assert_eq!(
            patch,
            json!({"/Notifications/ColorOverrides/AutomationNotification":{"R":16,"G":32,"B":48,"Future":17}})
        );
    }
    #[test]
    fn reset_clears_named_and_numeric_fallback_only_for_selected_type() {
        let data = json!({"Notifications":{"PositionOverrides":{"AutomationNotification":"TopLeft","5":"TopRight","6":"Center"},"DurationOverrides":{"5":0}}});
        let mut drafts = BTreeMap::new();
        reset_notification_draft(5, &mut drafts);
        let patch = notification_patch(&data, &drafts).unwrap();
        assert_eq!(
            patch["/Notifications/PositionOverrides/AutomationNotification"],
            Value::Null
        );
        assert_eq!(patch["/Notifications/PositionOverrides/5"], Value::Null);
        assert_eq!(patch["/Notifications/DurationOverrides/5"], Value::Null);
        assert!(patch.get("/Notifications/PositionOverrides/6").is_none());
    }
    #[test]
    fn edits_validate_numbers_colors_and_retain_changes_across_type_switches() {
        let data = json!({});
        let mut drafts = BTreeMap::new();
        drafts.insert(notification_key(5, "IconOverrides"), json!(-1));
        assert!(notification_patch(&data, &drafts).is_err());
        drafts.insert(notification_key(5, "IconOverrides"), json!(1234));
        drafts.insert(notification_key(6, "ColorOverrides"), json!("#01abFF"));
        let patch = notification_patch(&data, &drafts).unwrap();
        assert_eq!(
            patch["/Notifications/IconOverrides/AutomationNotification"],
            1234
        );
        assert_eq!(
            patch["/Notifications/ColorOverrides/CameraOn"],
            json!({"R":1,"G":171,"B":255})
        );
        drafts.insert(notification_key(6, "ColorOverrides"), json!("#notrgb"));
        assert!(notification_patch(&data, &drafts).is_err());
    }
}
