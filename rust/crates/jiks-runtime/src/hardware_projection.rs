//! Pure projection of completed native reads. No constructors, probes, I/O,
//! clock reads, setters, or invented hardware defaults occur here.
use crate::service::{Capability, Choice, Item, Metric, Range};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Default)]
pub struct Projection {
    pub values: BTreeMap<String, Value>,
    pub capabilities: BTreeMap<String, Capability>,
    pub options: BTreeMap<String, Vec<Choice>>,
    pub ranges: BTreeMap<String, Range>,
    pub collections: BTreeMap<String, Vec<Item>>,
    pub metrics: Vec<Metric>,
}
struct Builder<'a> {
    out: Projection,
    read_only: bool,
    conflict: Option<&'a str>,
}
impl Builder<'_> {
    fn cap(&mut self, key: &str, available: bool, reason: &str) {
        let blocked = self.conflict.filter(|v| !v.is_empty());
        let reason = if !available {
            reason.to_string()
        } else if self.read_only {
            "当前为只读模式，硬件写入未启用".into()
        } else if let Some(reason) = blocked {
            reason.into()
        } else {
            String::new()
        };
        self.out.capabilities.insert(
            key.into(),
            Capability {
                available: available && !self.read_only && blocked.is_none(),
                reason,
            },
        );
    }
    fn read_cap(&mut self, key: &str, available: bool, reason: &str) {
        self.out.capabilities.insert(
            key.into(),
            Capability {
                available,
                reason: if available {
                    String::new()
                } else {
                    reason.into()
                },
            },
        );
    }
    fn scalar(&mut self, key: &str, value: Value) {
        if value.is_null() || value.is_boolean() || value.is_number() || value.is_string() {
            self.out.values.insert(key.into(), value);
        }
    }
    fn choices(&mut self, key: &str, values: impl IntoIterator<Item = (String, Value)>) {
        self.out.options.insert(
            key.into(),
            values
                .into_iter()
                .map(|(label, value)| Choice { label, value })
                .collect(),
        );
    }
    fn range(&mut self, key: &str, min: i32, max: i32, step: i32, unit: &str) {
        if min <= max && step > 0 {
            self.out.ranges.insert(
                key.into(),
                Range {
                    minimum: min,
                    maximum: max,
                    step,
                    unit: unit.into(),
                },
            );
        }
    }
    fn metric(&mut self, key: &str, label: &str, value: String, detail: String) {
        self.out.metrics.retain(|metric| metric.key != key);
        self.out.metrics.push(Metric {
            key: key.into(),
            label: label.into(),
            value,
            detail,
        });
    }
}
fn completed(value: &Value) -> Option<&Value> {
    (value.get("available").and_then(Value::as_bool) == Some(true))
        .then(|| value.get("value"))
        .flatten()
}
fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value)
}
fn number(v: &Value) -> Option<f64> {
    v.as_f64().filter(|value| value.is_finite())
}
fn int(v: &Value) -> Option<i32> {
    v.as_i64().and_then(|n| i32::try_from(n).ok())
}
fn mapped(cache: &BTreeMap<String, Value>, key: &str) -> Option<u32> {
    cache
        .get(key)?
        .get("value")?
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
}
fn inner<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    completed(value.get(key)?)
}
fn power_choices() -> [(String, Value); 3] {
    [
        ("最佳能效".into(), json!(0)),
        ("均衡".into(), json!(1)),
        ("最佳性能".into(), json!(2)),
    ]
}

pub fn project(
    cache: &BTreeMap<String, Value>,
    read_only: bool,
    conflict: Option<&str>,
) -> Projection {
    let mut b = Builder {
        out: Projection::default(),
        read_only,
        conflict,
    };
    for key in [
        "power.apply",
        "network.apply",
        "preset.apply",
        "display.apply",
        "display.hdr",
        "display.acm",
        "display.overdrive",
        "battery.apply",
        "battery.threshold",
        "battery.night_charge",
        "power.accessories.apply",
        "keys.apply",
        "keyboard.white",
        "keyboard.logo",
        "keyboard.ports",
        "keyboard.power_led",
        "keyboard.apply",
        "keyboard.preview",
        "keyboard.effects.refresh",
        "keyboard.effects.apply",
        "keyboard.effects.reset",
        "gpu.hybrid.apply",
        "gpu.oc.apply",
        "gpu.settings.save",
        "gpu.sleep",
        "gpu.restart",
        "gpu.process.preference",
        "gpu.process.kill",
        "app.autorun",
        "app.autorun.apply",
        "display.boot_logo.apply",
        "display.boot_logo.path",
        "display.boot_logo.enabled",
        "display.boot_logo.disable",
        "display.boot_logo.restore",
        "display.boot_logo.retry_recovery",
        "display.boot_animation",
    ] {
        b.cap(key, false, "尚无已完成的本机能力读取，或该操作尚无适配器");
    }
    // Explicit read buttons are not permission to probe GPU/HID during startup.
    b.read_cap("gpu.refresh", true, "");
    b.read_cap(
        "gpu.oc.refresh",
        cache
            .get("gpu")
            .and_then(Value::as_array)
            .is_some_and(|rows| rows.len() == 1),
        "请先显式读取唯一独显库存；此操作可能唤醒独显",
    );
    b.read_cap("read.boot_logo", true, "");
    b.read_cap("read.autorun", true, "");
    b.read_cap(
        "gpu.processes.refresh",
        cache
            .get("gpu")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty()),
        "请先显式刷新独显库存；此操作可能唤醒独显",
    );
    b.read_cap(
        "gpu.pstate.refresh",
        cache
            .get("gpu")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty()),
        "先显式刷新独显库存；性能状态读取可能唤醒独显",
    );
    b.cap("gpu.pstate.apply", false, "尚无独显性能状态读取");
    b.cap("gpu.keep_awake.apply", false, "尚无已核对的独显库存及路由");
    b.read_cap(
        "sensors.read",
        cache.contains_key("telemetry")
            || cache.contains_key("firmware")
            || cache.contains_key("intel"),
        "尚无采样快照",
    );
    let cpu = cache.get("cpu");
    let mode = cpu
        .and_then(|v| v.get("raw_mode"))
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    if let Some(mode) = mode {
        b.scalar("power.mode", json!(mode));
        b.cap(
            "power.apply",
            [1, 2, 3, 255].contains(&mode),
            "固件模式无效",
        );
    }
    let mut custom_supported = false;
    if let Some(god) = cache
        .get("godmode")
        .and_then(|v| v.get("fields"))
        .and_then(Value::as_array)
    {
        let mut protected = 0;
        for row in god {
            let Some(key) = row.get("key").and_then(Value::as_str) else {
                continue;
            };
            if let Some(current) = row
                .get("current")
                .and_then(completed)
                .filter(|v| v.is_number())
            {
                b.scalar(key, current.clone());
            }
            let spec = row.get("capability").and_then(completed);
            let Some(spec) = spec else {
                b.cap(key, false, "未发现该字段的Custom合法范围/离散值");
                continue;
            };
            let min = spec.get("min").and_then(int);
            let max = spec.get("max").and_then(int);
            let values = spec
                .get("discrete")
                .and_then(Value::as_array)
                .map(|v| v.iter().filter_map(int).collect::<Vec<_>>())
                .unwrap_or_default();
            let unit = if key.contains("temperature") {
                "°C"
            } else if key.ends_with("tau") {
                "s"
            } else if key.contains("overclock") {
                ""
            } else {
                "W"
            };
            if !values.is_empty() {
                b.choices(key, values.iter().map(|n| (n.to_string(), json!(n))));
                // Only a genuine arithmetic progression can be represented by a slider.
                let step = if values.len() == 1 {
                    Some(1)
                } else {
                    let step = i64::from(values[1]) - i64::from(values[0]);
                    (step > 0
                        && step <= i64::from(i32::MAX)
                        && values
                            .windows(2)
                            .all(|pair| i64::from(pair[1]) - i64::from(pair[0]) == step))
                    .then_some(step as i32)
                };
                if let (Some(step), Some(first), Some(last)) = (step, values.first(), values.last())
                {
                    b.range(key, *first, *last, step, unit);
                }
            } else if let (Some(min), Some(max), Some(step)) =
                (min, max, spec.get("step").and_then(int))
            {
                b.range(key, min, max, step, unit);
            }
            let usable = min.zip(max).is_some_and(|(min, max)| min <= max)
                && (!values.is_empty() || b.out.ranges.contains_key(key));
            b.cap(key, usable, "Custom能力网格无效");
            if usable && ["cpu.pl1", "cpu.pl2", "cpu.temperature"].contains(&key) {
                protected += 1;
            }
        }
        custom_supported = protected == 3;
        b.cap(
            "preset.apply",
            custom_supported,
            "PL1/PL2/温限的Custom能力不完整",
        );
    }
    // The active limits can be shown even when a capability read failed. They
    // never manufacture a range/default or enable a write control.
    if let Some(cpu) = cpu {
        for (key, names) in [
            ("cpu.pl1", ["long_watts", "long_watts"]),
            ("cpu.pl2", ["short_watts", "short_watts"]),
            (
                "cpu.temperature",
                ["temperature_limit", "temperature_limit"],
            ),
        ] {
            let v = cpu.get("limits").and_then(|v| v.get(names[0])).or_else(|| {
                if key == "cpu.temperature" {
                    cpu.get(names[1])
                } else {
                    cpu.get("power").and_then(|v| v.get(names[1]))
                }
            });
            if let Some(v) = v.filter(|v| v.is_number()) {
                b.scalar(key, v.clone());
            }
        }
    }
    if mode.is_some() {
        let mut choices = vec![
            ("安静".into(), json!(1)),
            ("均衡".into(), json!(2)),
            ("性能".into(), json!(3)),
        ];
        if custom_supported {
            choices.push(("自定义".into(), json!(255)));
        }
        b.choices("power.mode", choices);
    }
    if let Some(modes) = cache
        .get("power_modes")
        .and_then(|value| value.get("modes"))
        .and_then(Value::as_array)
    {
        let options = modes
            .iter()
            .filter_map(|value| {
                Some((
                    value.get("name")?.as_str()?.to_owned(),
                    json!(value.get("raw")?.as_u64()?),
                ))
            })
            .collect::<Vec<_>>();
        if !options.is_empty() {
            b.choices("power.mode", options);
            b.cap("power.apply", true, "");
        }
        if let Some(current) = cache
            .get("power_modes")
            .and_then(|value| value.get("current_raw"))
            .and_then(Value::as_u64)
        {
            b.scalar("power.mode", json!(current));
        }
    }

    let mut key_supported = false;
    let mut accessories_supported = false;
    for (cache_key, key, cap) in [
        ("mapped.overdrive", "display.overdrive", "display.overdrive"),
        (
            "mapped.flip_to_start",
            "power.flip_to_start",
            "power.accessories.apply",
        ),
        ("mapped.fnq", "power.fnq_custom", "power.apply"),
        ("mapped.fan_full_speed", "fan.full_speed", "preset.apply"),
    ] {
        if let Some(value) = mapped(cache, cache_key).filter(|v| *v <= 1) {
            b.scalar(key, json!(value != 0));
            if cap != "preset.apply" {
                b.cap(cap, true, "");
            }
            if cap == "power.accessories.apply" {
                accessories_supported = true;
            }
        }
    }
    if let Some(sync) = mapped(cache, "mapped.power_led").filter(|v| [0, 2].contains(v)) {
        b.scalar("keyboard.power_led_sync", json!(sync != 0));
    }
    if let (Some(ac), Some(usb)) = (
        mapped(cache, "mapped.instant_ac"),
        mapped(cache, "mapped.instant_usb"),
    ) {
        if ac <= 1 && usb <= 1 {
            b.scalar("power.instant_boot", json!(ac | (usb << 1)));
            b.choices(
                "power.instant_boot",
                [
                    ("关闭".into(), json!(0)),
                    ("接入电源开机".into(), json!(1)),
                    ("USB-C供电开机".into(), json!(2)),
                    ("两者均启用".into(), json!(3)),
                ],
            );
            accessories_supported = true;
        }
    }
    if let Some(igpu) = mapped(cache, "mapped.igpu").filter(|v| *v <= 2) {
        b.scalar("gpu.igpu_mode", json!(igpu));
    }
    if let Some(hybrid) = cache.get("hybrid") {
        if let Some(snapshot) = hybrid.get("snapshot").and_then(|v| {
            serde_json::from_value::<jiks_platform::hybrid::HybridSnapshot>(v.clone()).ok()
        }) {
            if let Some(mode) = snapshot.selected() {
                b.scalar("gpu.hybrid_mode", json!(mode as u32));
            }
            if let Some(gsync) = snapshot.gsync {
                b.scalar("gpu.gsync", json!(gsync));
            }
            if let Some(igpu) = snapshot.igpu_mode {
                b.scalar("gpu.igpu_mode", json!(igpu));
            }
            let modes = snapshot.modes();
            b.choices(
                "gpu.hybrid_mode",
                modes.iter().map(|m| (m.label().into(), json!(*m as u32))),
            );
            b.cap(
                "gpu.hybrid.apply",
                !modes.is_empty(),
                "固件未完成GSync/IGPU/BIOS能力发现",
            );
            b.scalar("gpu.hybrid.physical_topology_verified", json!(false));
            let ac = cache
                .get("system")
                .and_then(|v| v.pointer("/ac_connected/value"))
                .and_then(Value::as_bool)
                .or_else(|| {
                    cache
                        .get("telemetry")
                        .and_then(|v| v.get("ac_connected"))
                        .and_then(Value::as_bool)
                });
            if let Some(ac) = ac {
                b.cap(
                    "gpu.sleep",
                    snapshot.igpu_backend.is_some()
                        && jiks_platform::gpu_sleep::validate_policy(&snapshot, ac).is_ok(),
                    "仅IGPU-only/UMA/Auto DC可发休眠通知；不是物理sleep保证",
                );
                b.cap(
                    "gpu.keep_awake.apply",
                    cache
                        .get("gpu")
                        .and_then(Value::as_array)
                        .is_some_and(|v| v.len() == 1)
                        && jiks_platform::gpu_awake::validate_policy(&snapshot, ac).is_ok(),
                    "当前路由/AC状态不允许保持独显唤醒",
                );
            }
        }
    }
    b.cap("power.ai.apply", false, "尚无完成的AI子模式读取");
    if let Some(ai) = cache
        .get("ai")
        .and_then(|v| serde_json::from_value::<jiks_platform::ai::AiSnapshot>(v.clone()).ok())
    {
        b.scalar("power.ai_submode", json!(ai.submode));
        b.choices(
            "power.ai_submode",
            ai.modes.iter().map(|m| {
                (
                    if *m == 0 {
                        "关闭".into()
                    } else {
                        format!("AI 子模式 {m}")
                    },
                    json!(m),
                )
            }),
        );
        b.cap(
            "power.ai.apply",
            ai.raw_mode == 2,
            "仅均衡模式可修改AI子模式，启用还需接通AC",
        );
    }

    if let Some(energy) = cache.get("energy") {
        if let Some(mode) = energy.get("battery_mode").and_then(Value::as_str) {
            if ["Normal", "Rapid", "Conservation"].contains(&mode) {
                b.scalar("battery.mode", json!(mode));
                b.choices(
                    "battery.mode",
                    [
                        ("养护充电".into(), json!("Conservation")),
                        ("正常".into(), json!("Normal")),
                        ("快充".into(), json!("Rapid")),
                    ],
                );
                b.cap("battery.apply", true, "");
            }
        }
        if let Some(on) = energy.get("night_charge").and_then(Value::as_bool) {
            b.scalar("battery.night_charge", json!(on));
            b.cap("battery.night_charge", true, "");
        }
        if let Some(on) = energy.get("fn_lock").and_then(Value::as_bool) {
            b.scalar("keys.fn_lock", json!(on));
            key_supported = true;
        }
        if let Some(on) = energy.get("one_level_white").and_then(Value::as_bool) {
            b.scalar("keyboard.white_single", json!(on));
            b.cap("keyboard.white_single", true, "");
            key_supported = true;
        }
        if let Some(mode) = energy
            .get("always_on_usb")
            .and_then(Value::as_str)
            .filter(|v| ["Off", "Sleeping", "Always"].contains(v))
        {
            b.scalar("power.always_on_usb", json!(mode));
            b.choices(
                "power.always_on_usb",
                [
                    ("关闭".into(), json!("Off")),
                    ("睡眠时供电".into(), json!("Sleeping")),
                    ("始终供电".into(), json!("Always")),
                ],
            );
            accessories_supported = true;
        }
        if let (Some(max), Some(level)) = (
            energy.get("keyboard_maximum").and_then(Value::as_u64),
            energy.get("keyboard_level").and_then(Value::as_u64),
        ) {
            if (1..=3).contains(&max) && level <= max {
                b.scalar("keyboard.white_level", json!(level));
                b.scalar("keyboard.white_backend", json!("energy"));
                b.range("keyboard.white_level", 0, max as i32, 1, "");
                b.cap("keyboard.white", true, "");
                key_supported = true;
            }
        }
    }
    if let Some(system) = cache.get("system") {
        for (name, key) in [
            ("win_key", "keys.win_lock"),
            ("touchpad_lock", "keys.touchpad_lock"),
        ] {
            if let Some(value) = inner(system, name)
                .and_then(|v| v.get("value"))
                .and_then(Value::as_u64)
                .filter(|v| *v <= 1)
            {
                b.scalar(key, json!(value != 0));
                key_supported = true;
            }
        }
        if let Some(brightness) = inner(system, "brightness") {
            if let Some(current) = brightness
                .get("current")
                .and_then(Value::as_u64)
                .filter(|v| *v <= 100)
            {
                b.scalar("display.brightness", json!(current));
                b.range("display.brightness", 0, 100, 1, "%");
                b.cap("display.brightness", true, "");
                b.cap("display.apply", true, "");
            }
            if let Some(levels) = brightness.get("levels").and_then(Value::as_array) {
                b.choices(
                    "display.brightness",
                    levels
                        .iter()
                        .filter_map(Value::as_u64)
                        .filter(|n| *n <= 100)
                        .map(|n| (format!("{n}%"), json!(n))),
                );
            }
        }
        if let Some(plans) = inner(system, "power_plans").and_then(Value::as_array) {
            let choices = plans
                .iter()
                .filter_map(|p| Some((p.get("name")?.as_str()?.to_owned(), p.get("id")?.clone())))
                .collect::<Vec<_>>();
            b.choices("power.plan", choices);
            for plan in plans {
                if plan.get("active").and_then(Value::as_bool) == Some(true) {
                    if let Some(id) = plan.get("id").and_then(Value::as_str) {
                        b.scalar("power.plan", json!(id));
                    }
                }
            }
            b.cap(
                "power.plan.apply",
                !plans.is_empty(),
                "未发现Windows电源计划",
            );
        }
        if let Some(overlay) = inner(system, "effective_power_mode").and_then(Value::as_str) {
            let mode = match overlay.to_ascii_lowercase().as_str() {
                "961cc777-2547-4f9d-8174-7d86181b8a7a" => Some(0),
                "00000000-0000-0000-0000-000000000000" => Some(1),
                "ded574b5-45a0-4f42-8737-46345c09c238" => Some(2),
                _ => None,
            };
            if let Some(mode) = mode {
                b.scalar("power.overlay", json!(mode));
            }
            b.choices("power.overlay", power_choices());
            b.choices("power.mode_ac", power_choices());
            b.choices("power.mode_dc", power_choices());
            b.cap("power.overlay.apply", true, "");
        }
    }
    for (id, key) in [(3, "keyboard.logo"), (5, "keyboard.ports")] {
        if !crate::hardware_routes::lighting_supported(cache, id) {
            continue;
        }
        if let Some(state) = cache
            .get(&format!("lighting.{id}"))
            .and_then(|v| field(v, "Current_State_Type"))
            .and_then(Value::as_u64)
            .filter(|n| *n <= 1)
        {
            b.scalar(key, json!(state != 0));
            b.cap(key, true, "");
            key_supported = true;
        }
    }
    if let Some(level) = cache
        .get("lighting.0")
        .and_then(|v| field(v, "Current_Brightness_Level"))
        .and_then(Value::as_u64)
        .filter(|n| (1..=3).contains(n))
        .filter(|_| crate::hardware_routes::lighting_supported(cache, 0))
    {
        b.scalar("keyboard.white_level", json!(level - 1));
        b.scalar("keyboard.white_backend", json!("wmi"));
        b.range("keyboard.white_level", 0, 2, 1, "");
        b.cap("keyboard.white", true, "");
        key_supported = true;
    }
    if let Some(led) = cache.get("lighting.4") {
        if let (Some(raw), Some(offset)) = (
            field(led, "Current_Brightness_Level").and_then(Value::as_u64),
            cache
                .get("power_led.level_offset")
                .and_then(Value::as_u64)
                .filter(|v| *v <= 1),
        ) {
            if let Some(normalized) = raw.checked_add(offset).filter(|n| (1..=4).contains(n)) {
                b.scalar("keyboard.power_led", json!(normalized));
                b.range("keyboard.power_led", 1, 4, 1, "");
                b.cap("keyboard.power_led", true, "");
                key_supported = true;
            }
        }
    }
    if let Some(audio) = cache
        .get("devices")
        .and_then(|v| inner(v, "audio"))
        .and_then(Value::as_array)
    {
        for (capture, key) in [(true, "keys.microphone_muted"), (false, "speaker.muted")] {
            let group = audio
                .iter()
                .filter(|endpoint| {
                    endpoint.get("capture").and_then(Value::as_bool) == Some(capture)
                })
                .collect::<Vec<_>>();
            if !group.is_empty()
                && group
                    .iter()
                    .all(|e| e.get("muted").and_then(Value::as_bool).is_some())
            {
                // Original MicrophoneFeature/SpeakerFeature aggregate all active
                // endpoints. Picking the first makes mixed states order-dependent.
                b.scalar(
                    key,
                    json!(
                        group
                            .iter()
                            .all(|e| e.get("muted").and_then(Value::as_bool) == Some(true))
                    ),
                );
                key_supported = true;
            }
            if !capture && !group.is_empty() {
                let volumes = group
                    .iter()
                    .filter_map(|e| e.get("volume_percent").and_then(number))
                    .filter(|v| (0.0..=100.0).contains(v))
                    .collect::<Vec<_>>();
                if volumes.len() == group.len()
                    && volumes.iter().all(|v| (v - volumes[0]).abs() <= 0.5)
                {
                    b.scalar("speaker.volume", json!(volumes[0].round() as u32));
                }
            }
        }
    }
    b.cap("keys.apply", key_supported, "尚无已读取的按键/指示灯能力");
    b.cap(
        "power.accessories.apply",
        accessories_supported,
        "尚无已读取的供电/开机功能能力",
    );

    let displays = cache
        .get("devices")
        .and_then(|v| inner(v, "displays"))
        .and_then(Value::as_array);
    if let Some(controls) = cache.get("display_controls").and_then(Value::as_array) {
        let mut items = vec![];
        for control in controls {
            let Some(path) = control
                .get("monitor_path")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            else {
                continue;
            };
            let name = control.get("name").and_then(Value::as_str).unwrap_or(path);
            let source = control
                .get("source_name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let display = displays.and_then(|rows| {
                rows.iter()
                    .find(|row| row.get("name").and_then(Value::as_str) == Some(source))
            });
            let current = display.and_then(|row| row.get("current"));
            items.push(Item{id:path.into(),label:name.into(),detail:source.into(),value:json!({"device":source,"monitor_path":path,"internal":control.get("internal").and_then(Value::as_bool).unwrap_or(false),"width":current.and_then(|v|v.get("width")),"height":current.and_then(|v|v.get("height")),"hertz":current.and_then(|v|v.get("hertz"))})});
        }
        b.choices(
            "display.monitor",
            items
                .iter()
                .map(|item| (item.label.clone(), json!(item.id))),
        );
        let selected = controls
            .iter()
            .find(|v| v.get("internal").and_then(Value::as_bool) == Some(true))
            .or_else(|| controls.first());
        if let Some(selected) = selected {
            if let Some(path) = selected.get("monitor_path").and_then(Value::as_str) {
                b.scalar("display.monitor", json!(path));
            }
            let source = selected.get("source_name").and_then(Value::as_str);
            if let Some(display) = displays.and_then(|rows| {
                rows.iter()
                    .find(|row| row.get("name").and_then(Value::as_str) == source)
            }) {
                if let Some(current) = display.get("current") {
                    if let (Some(w), Some(h)) = (
                        current.get("width").and_then(Value::as_u64),
                        current.get("height").and_then(Value::as_u64),
                    ) {
                        b.scalar("display.resolution", json!(format!("{w}x{h}")));
                    }
                    if let Some(hz) = current.get("hertz").and_then(Value::as_u64) {
                        b.scalar("display.refresh_rate", json!(hz));
                    }
                }
                if let Some(modes) = display.get("modes").and_then(Value::as_array) {
                    let mut resolutions = BTreeMap::new();
                    let mut rates = std::collections::BTreeSet::new();
                    for mode in modes {
                        if let (Some(w), Some(h), Some(hz)) = (
                            mode.get("width").and_then(Value::as_u64),
                            mode.get("height").and_then(Value::as_u64),
                            mode.get("hertz").and_then(Value::as_u64),
                        ) {
                            if w > 0 && h > 0 && hz > 0 {
                                let name = format!("{w}x{h}");
                                resolutions.insert(name.clone(), json!(name));
                                rates.insert(hz);
                            }
                        }
                    }
                    b.choices("display.resolution", resolutions);
                    b.choices(
                        "display.refresh_rate",
                        rates.into_iter().map(|hz| (format!("{hz} Hz"), json!(hz))),
                    );
                    b.cap("display.apply", !modes.is_empty(), "显示器未报告可用模式");
                }
            }
            if let Some(color) = selected.get("color").filter(|v| v.is_object()) {
                for (field_name, support, key) in [
                    ("hdr_enabled", "hdr_supported", "display.hdr"),
                    ("acm_enabled", "acm_supported", "display.acm"),
                ] {
                    let supported = color.get(support).and_then(Value::as_bool) == Some(true);
                    if supported {
                        if let Some(enabled) = color.get(field_name).and_then(Value::as_bool) {
                            b.scalar(key, json!(enabled));
                        }
                    }
                    b.cap(
                        key,
                        supported
                            && color.get("limited_by_policy").and_then(Value::as_bool)
                                != Some(true),
                        "显示器不支持此色彩功能，或Windows策略限制",
                    );
                }
            }
            if let Some(dpi) = selected.get("dpi").filter(|v| v.is_object()) {
                if let Some(percent) = dpi.get("current_percent").and_then(Value::as_u64) {
                    b.scalar("display.dpi", json!(percent));
                }
                if let Some(values) = dpi.get("available_percentages").and_then(Value::as_array) {
                    b.choices(
                        "display.dpi",
                        values
                            .iter()
                            .filter_map(Value::as_u64)
                            .map(|n| (format!("{n}%"), json!(n))),
                    );
                    b.cap("display.dpi", !values.is_empty(), "DPI合法集合为空");
                }
            }
            if let Some(refresh) = selected.get("refresh") {
                if let Some(current) = refresh
                    .get("current")
                    .filter(|v| v.get("IsDynamic").and_then(Value::as_bool) == Some(true))
                {
                    if let Ok(rate) = serde_json::from_value::<
                        jiks_platform::dynamic_refresh::RefreshRate,
                    >(current.clone())
                    {
                        b.scalar(
                            "display.refresh_rate",
                            json!(format!(
                                "dynamic:{}:{}",
                                rate.base_frequency, rate.frequency
                            )),
                        );
                    }
                }
                if let Some(choices) = refresh.get("choices").and_then(Value::as_array) {
                    let options = b
                        .out
                        .options
                        .entry("display.refresh_rate".into())
                        .or_default();
                    for choice in choices {
                        if let Ok(rate) = serde_json::from_value::<
                            jiks_platform::dynamic_refresh::RefreshRate,
                        >(choice.clone())
                        {
                            options.push(Choice {
                                label: format!(
                                    "动态 {} / {} Hz",
                                    rate.base_frequency, rate.frequency
                                ),
                                value: json!(format!(
                                    "dynamic:{}:{}",
                                    rate.base_frequency, rate.frequency
                                )),
                            });
                        }
                    }
                }
            }
        }
        b.out.collections.insert("displays".into(), items);
        b.cap("display.turn_off", !controls.is_empty(), "尚无活动显示器");
    }
    if let Some(candidates) = cache
        .get("spectrum")
        .and_then(|v| v.get("candidates"))
        .and_then(Value::as_array)
    {
        let items = candidates
            .iter()
            .filter_map(|candidate| {
                let path = candidate.get("path")?.as_str()?;
                Some(Item {
                    id: path.into(),
                    label: "83NN 24区Spectrum候选接口".into(),
                    detail: "只读描述符匹配；尚未发送兼容性探测报告".into(),
                    value: json!({"path":path,"zones":24,"protocol_probed":false}),
                })
            })
            .collect::<Vec<_>>();
        let single = items.len() == 1;
        b.out
            .collections
            .insert("keyboard.interfaces".into(), items);
        for key in [
            "keyboard.apply",
            "keyboard.preview",
            "keyboard.effects.refresh",
            "keyboard.effects.apply",
            "keyboard.effects.reset",
        ] {
            b.cap(
                key,
                single,
                "需要明确唯一的Spectrum候选接口；无自动SetFeature探测",
            );
        }
        if single {
            b.choices(
                "keyboard.profile",
                (0..=6).map(|n| {
                    (
                        if n == 0 {
                            "关闭".into()
                        } else {
                            format!("预设{n}")
                        },
                        json!(n),
                    )
                }),
            );
            b.range("keyboard.brightness", 0, 9, 1, "");
            b.range("keyboard.speed", 0, 3, 1, "");
            b.choices(
                "keyboard.effect",
                [
                    ("常亮", "Always"),
                    ("色彩波浪", "ColorWave"),
                    ("呼吸", "ColorPulse"),
                    ("平滑", "Smooth"),
                    ("音乐律动", "AudioBounce"),
                ]
                .into_iter()
                .map(|(label, value)| (label.into(), json!(value))),
            );
            b.choices(
                "keyboard.direction",
                [
                    ("无", "None"),
                    ("从左向右", "LeftToRight"),
                    ("从右向左", "RightToLeft"),
                    ("从下向上", "BottomToTop"),
                    ("从上向下", "TopToBottom"),
                ]
                .into_iter()
                .map(|(label, value)| (label.into(), json!(value))),
            );
        }
    }
    if let Some(profile) = cache
        .get("keyboard.effects")
        .and_then(|v| v.get("profile"))
        .and_then(Value::as_u64)
    {
        b.scalar("keyboard.profile", json!(profile));
    }
    if let Some(effects) = cache
        .get("keyboard.effects")
        .and_then(|v| v.get("effects"))
        .and_then(Value::as_array)
    {
        let items = effects
            .iter()
            .enumerate()
            .map(|(index, effect)| Item {
                id: index.to_string(),
                label: format!(
                    "{} · {}",
                    index + 1,
                    effect
                        .get("Type")
                        .and_then(Value::as_str)
                        .unwrap_or("未知灯效")
                ),
                detail: format!(
                    "{}区 · {}色",
                    effect
                        .get("Keys")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len),
                    effect
                        .get("Colors")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                ),
                value: effect.clone(),
            })
            .collect();
        b.out.collections.insert("keyboard.effects".into(), items);
    }
    if let Some(gpus) = cache.get("gpu").and_then(Value::as_array) {
        let items = gpus
            .iter()
            .filter_map(|gpu| {
                Some(Item {
                    id: gpu.get("index")?.as_u64()?.to_string(),
                    label: gpu.get("name")?.as_str()?.into(),
                    detail: "仅用户显式刷新；NVAPI读取可能唤醒独显".into(),
                    value: json!({"index":gpu.get("index"),"name":gpu.get("name")}),
                })
            })
            .collect::<Vec<_>>();
        if items.len() == 1 {
            b.scalar("gpu.index", items[0].value["index"].clone());
            b.scalar("gpu.name", items[0].value["name"].clone());
        }
        b.out.collections.insert("gpu.devices".into(), items);
    }
    if let Some(nodes) = cache.get("gpu.nodes").and_then(Value::as_array) {
        let known = nodes
            .iter()
            .filter_map(|v| {
                serde_json::from_value::<jiks_platform::gpu_management::GpuDeviceNode>(v.clone())
                    .ok()
            })
            .collect::<Vec<_>>();
        b.cap(
            "gpu.restart",
            known.len() == 1,
            "NVIDIA PnP设备缺失/不唯一，不能猜测重启目标",
        );
        if known.len() == 1 {
            b.scalar("gpu.instance_id", json!(known[0].instance_id));
        }
        b.out.collections.insert(
            "gpu.nodes".into(),
            known
                .iter()
                .map(|node| Item {
                    id: node.instance_id.clone(),
                    label: node.description.clone(),
                    detail: format!(
                        "PnP started={} / problem={}",
                        node.started, node.problem_code
                    ),
                    value: json!({"instance_id":node.instance_id}),
                })
                .collect(),
        );
    }
    if let Some(processes) = cache
        .get("gpu.processes")
        .and_then(|v| v.get("processes"))
        .and_then(Value::as_array)
    {
        let known = processes
            .iter()
            .filter_map(|p| {
                serde_json::from_value::<jiks_platform::gpu_management::GpuProcess>(p.clone()).ok()
            })
            .collect::<Vec<_>>();
        let available = !known.is_empty();
        b.cap(
            "gpu.process.kill",
            available,
            "暂无可精确验证PID/创建时间/路径的占用进程",
        );
        b.cap(
            "gpu.process.preference",
            available,
            "暂无可精确验证的应用路径",
        );
        b.out.collections.insert(
            "gpu.processes".into(),
            known
                .iter()
                .map(|p| Item {
                    id: format!("{}:{}", p.identity.pid, p.identity.creation_filetime),
                    label: std::path::Path::new(&p.identity.executable)
                        .file_name()
                        .map(|v| v.to_string_lossy().into_owned())
                        .unwrap_or_else(|| format!("PID {}", p.identity.pid)),
                    detail: format!("PID {} · {:?}", p.identity.pid, p.preference),
                    value: json!({"identity":p.identity,"preference":p.preference}),
                })
                .collect(),
        );
    }
    if let Some(oc) = cache.get("gpu.oc") {
        let mut valid = true;
        for (name, key) in [
            ("core_range", "gpu.oc.core_offset_mhz"),
            ("memory_range", "gpu.oc.memory_offset_mhz"),
        ] {
            let range = oc.get(name);
            let (min, max) = (
                range.and_then(|v| v.get("min_mhz")).and_then(int),
                range.and_then(|v| v.get("max_mhz")).and_then(int),
            );
            if range
                .and_then(|v| v.get("driver_reported"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                if let (Some(min), Some(max)) = (min, max) {
                    b.range(key, min, max, 1, "MHz");
                } else {
                    valid = false;
                }
            } else {
                valid = false;
            }
        }
        if let Some(values) = oc.get("overclock") {
            for (from, to) in [
                ("CoreDeltaMhz", "gpu.oc.core_offset_mhz"),
                ("MemoryDeltaMhz", "gpu.oc.memory_offset_mhz"),
            ] {
                if let Some(value) = values.get(from).and_then(int) {
                    b.scalar(to, json!(value));
                }
            }
            let lock = values.get("VoltageLockMv").and_then(int).unwrap_or(0);
            let cap = values.get("VoltageCapMv").and_then(int).unwrap_or(0);
            b.scalar(
                "gpu.oc.voltage_mv",
                json!(if lock > 0 { lock } else { cap }),
            );
            b.scalar("gpu.oc.lock_voltage", json!(lock > 0));
        }
        b.cap(
            "gpu.oc.apply",
            valid,
            "驱动未提供真实OC偏移范围；UI回退范围不能作为写能力",
        );
    }
    // A requested lock is not the GPU's currently observed P-state. Keep the
    // getter timestamp so a visible status view can expire it without new I/O.
    b.scalar("gpu.current_pstate", Value::Null);
    b.scalar("gpu.current_pstate_at_tick_ms", Value::Null);
    b.scalar("gpu.state", Value::Null);
    b.scalar("gpu.state_at_tick_ms", Value::Null);
    if let Some(sample) = cache.get("gpu.pstate") {
        if let Some((state, tick)) = sample
            .get("current_pstate")
            .and_then(Value::as_u64)
            .filter(|state| *state < 16)
            .zip(sample.get("completed_at_tick_ms").and_then(Value::as_u64))
        {
            let requested = sample.get("requested_at_tick_ms").and_then(Value::as_u64);
            if let Some(requested) =
                requested.filter(|requested| tick >= *requested && tick - *requested <= 3000)
            {
                b.scalar("gpu.current_pstate", json!(state));
                b.scalar("gpu.current_pstate_at_tick_ms", json!(requested));
            }
        }
    }
    if let Some(states) = cache
        .get("gpu.pstate")
        .and_then(|v| v.get("states"))
        .and_then(Value::as_array)
    {
        let mut choices = vec![("自动（解除P-state锁定）".into(), json!(-1))];
        for state in states {
            if let Some(id) = state
                .get("state")
                .and_then(Value::as_u64)
                .filter(|id| *id < 16)
            {
                choices.push((format!("P{id}"), json!(id)));
            }
        }
        b.choices("gpu.locked_pstate", choices);
        b.cap(
            "gpu.pstate.apply",
            !states.is_empty(),
            "驱动未报告可用性能状态",
        );
    }
    if let Some(brightness) = cache
        .get("keyboard.brightness")
        .and_then(Value::as_u64)
        .filter(|v| *v <= 9)
    {
        b.scalar("keyboard.brightness", json!(brightness));
    }
    if let Some(active) = cache
        .get("gpu.awake")
        .and_then(|v| v.get("active"))
        .and_then(Value::as_bool)
    {
        b.scalar("gpu.keep_awake.active", json!(active));
        if active {
            b.cap("gpu.keep_awake.apply", true, "");
        }
    }
    if let Some(wireless) = cache.get("wireless") {
        let mut available = false;
        if let Some(wifi) = inner(wireless, "wifi").and_then(|v| {
            serde_json::from_value::<jiks_platform::wireless::WifiSnapshot>(v.clone()).ok()
        }) {
            if let Some(on) = wifi.any_software_radio_on {
                b.scalar("network.wifi", json!(on));
            }
            if let Some(connected) = wifi.any_connected {
                b.scalar("network.wifi_connected", json!(connected));
            }
            b.scalar(
                "network.wifi_connections_known",
                json!(wifi.all_connections_known),
            );
            if let Some(ssid) = wifi.connected_ssids.first() {
                b.scalar("network.ssid", json!(ssid));
            }
            available |= !wifi.interfaces.is_empty()
                && wifi.interfaces.iter().all(|i| {
                    !i.radios.is_empty() && i.radios.iter().all(|r| r.software_on.is_some())
                });
        }
        if let Some(airplane) = inner(wireless, "airplane")
            .and_then(|v| v.get("airplane_on"))
            .and_then(Value::as_bool)
        {
            b.scalar("network.airplane", json!(airplane));
            available = true;
        }
        b.cap(
            "network.apply",
            available,
            "无线状态尚未完成读取；权限失败不等于不支持",
        );
    }
    b.read_cap("software.refresh", true, "");
    let mut software_available = false;
    if let Some(snapshot) = cache.get("software").and_then(|value| {
        serde_json::from_value::<jiks_platform::software::SoftwareSnapshot>(value.clone()).ok()
    }) {
        for (id, label) in [
            ("vantage", "Vantage / ImController"),
            ("zone", "Legion Zone"),
            ("space", "Legion Space"),
            ("hotkeys", "Lenovo Hotkeys"),
        ] {
            if let Some(state) = snapshot.products.get(id) {
                use jiks_platform::software::Status;
                let key = format!("software.{id}_disabled");
                let known = matches!(state.status, Status::Enabled | Status::Disabled);
                if known {
                    b.scalar(&key, json!(state.status == Status::Disabled));
                }
                b.scalar(
                    &format!("software.{id}.status"),
                    serde_json::to_value(state.status).unwrap_or(Value::Null),
                );
                let reason = if state.status == Status::Unknown && state.errors.is_empty() {
                    "服务仍处于启动或停止过渡状态".into()
                } else {
                    state.errors.join("; ")
                };
                b.out.capabilities.insert(
                    key,
                    Capability {
                        available: known && !read_only,
                        reason: if read_only {
                            "当前为只读模式".into()
                        } else {
                            reason
                        },
                    },
                );
                software_available |= known;
                b.metric(
                    &format!("software.{id}"),
                    label,
                    match state.status {
                        Status::Enabled => "已启用",
                        Status::Disabled => "已禁用",
                        Status::NotFound => "未发现",
                        Status::Unknown => "状态未知",
                    }
                    .into(),
                    state.errors.join("; "),
                );
            }
        }
    }
    // Disabling the conflicting vendor controller is itself an explicit software
    // operation. Native still excludes old LLT/unselected controllers and checks
    // the exact disable scope at every mutation; a blanket firmware-conflict UI
    // gate would make this recovery operation impossible to request.
    b.out.capabilities.insert(
        "software.apply".into(),
        Capability {
            available: software_available && !read_only,
            reason: if read_only {
                "当前为只读模式".into()
            } else if !software_available {
                "请先读取软件状态；读取失败不等于未安装".into()
            } else {
                String::new()
            },
        },
    );
    if let Some(autorun) = cache
        .get("autorun")
        .and_then(|v| serde_json::from_value::<jiks_platform::autorun::Status>(v.clone()).ok())
    {
        b.scalar("app.autorun", json!(autorun.mode));
        b.scalar("app.autorun.action_matches", json!(autorun.action_matches));
        b.cap(
            "app.autorun.apply",
            autorun.owned && autorun.action_matches,
            "当前用户的同名启动任务身份或启动路径不匹配",
        );
        b.cap(
            "app.autorun",
            autorun.owned && autorun.action_matches,
            "当前用户的同名启动任务身份或启动路径不匹配",
        );
    }
    if let Some(error) = cache.get("error.autorun").and_then(Value::as_str) {
        let reason = format!(
            "无法核对当前用户的开机任务：{}",
            error.chars().take(300).collect::<String>()
        );
        b.cap("app.autorun", false, &reason);
        b.cap("app.autorun.apply", false, &reason);
    }
    if let Some(boot) = cache.get("boot_logo") {
        let pending = inner(boot, "pending").and_then(Value::as_bool);
        let info = inner(boot, "info");
        let animation = inner(boot, "animation");
        if let Some(enabled) = info.and_then(|v| v.get("enabled")).and_then(Value::as_bool) {
            b.scalar("display.boot_logo.enabled", json!(enabled));
        }
        if let Some(disabled) = animation
            .and_then(|v| v.get("disabled"))
            .and_then(Value::as_bool)
        {
            b.scalar("display.boot_animation", json!(!disabled));
        }
        b.scalar("display.boot_logo.recovery_required", json!(pending));
        b.cap(
            "display.boot_logo.apply",
            pending == Some(false) && (info.is_some() || animation.is_some()),
            "UEFI/BCD权限或能力未知，或存在待恢复事务",
        );
        b.cap(
            "display.boot_logo.disable",
            pending == Some(false) && info.is_some(),
            "需要完成UEFI能力读取并先恢复待处理事务",
        );
        for key in ["display.boot_logo.path", "display.boot_logo.enabled"] {
            b.cap(
                key,
                pending == Some(false) && info.is_some(),
                "需要完成UEFI能力读取并先恢复待处理事务",
            );
        }
        b.cap(
            "display.boot_animation",
            pending == Some(false) && animation.is_some(),
            "BCD读取失败或恢复未完成，不能更改启动动画",
        );
        b.cap(
            "display.boot_logo.retry_recovery",
            pending == Some(true),
            "没有已知待恢复事务",
        );
        if let Some(backups) = inner(boot, "backups").and_then(Value::as_array) {
            let items = backups
                .iter()
                .filter_map(|v| {
                    let id = v.get("id")?.as_str()?;
                    Some(Item {
                        id: id.into(),
                        label: format!(
                            "备份 {}",
                            v.get("created_unix_ms")
                                .and_then(Value::as_u64)
                                .unwrap_or(0)
                        ),
                        detail: format!("{:?}", v.get("state")),
                        value: json!(id),
                    })
                })
                .collect::<Vec<_>>();
            b.cap(
                "display.boot_logo.restore",
                pending == Some(false) && !items.is_empty(),
                "没有可验证的原值备份，或当前恢复尚未完成",
            );
            b.out.collections.insert("boot_logo_backups".into(), items);
        }
    }
    if let Some(profile) = cache
        .get("keyboard.active_profile")
        .and_then(|v| v.get("profile"))
        .and_then(Value::as_u64)
        .filter(|v| *v <= 6)
    {
        b.scalar("keyboard.profile", json!(profile));
    }
    project_discovery_errors(&mut b, cache);
    project_metrics(&mut b, cache);
    project_aux_metrics(&mut b, cache);
    b.out
}
fn project_discovery_errors(b: &mut Builder<'_>, cache: &BTreeMap<String, Value>) {
    fn permission(error: &str) -> bool {
        let lower = error.to_ascii_lowercase();
        [
            "0x80041003",
            "-2147217405",
            "0x80070005",
            "access denied",
            "access_denied",
        ]
        .iter()
        .any(|v| lower.contains(v))
    }
    let mut errors = BTreeMap::<String, Vec<String>>::new();
    for (key, value) in cache {
        if let Some(source) = key.strip_prefix("error.") {
            if let Some(error) = value.as_str() {
                errors.entry(source.into()).or_default().push(error.into());
            }
        }
    }
    for key in [
        "hybrid",
        "system",
        "energy",
        "display_controls",
        "capabilities",
    ] {
        fn collect(value: &Value, out: &mut Vec<String>) {
            match value {
                Value::Object(object) => {
                    for (key, value) in object {
                        if key == "error" {
                            if let Some(s) = value.as_str() {
                                out.push(s.into());
                            }
                        } else if key == "errors" {
                            match value {
                                Value::Object(v) => out.extend(
                                    v.values().filter_map(Value::as_str).map(str::to_string),
                                ),
                                Value::Array(v) => out
                                    .extend(v.iter().filter_map(Value::as_str).map(str::to_string)),
                                _ => {}
                            }
                        } else if value.is_object() || value.is_array() {
                            collect(value, out);
                        }
                    }
                }
                Value::Array(array) => {
                    for value in array {
                        collect(value, out)
                    }
                }
                _ => {}
            }
        }
        if let Some(value) = cache.get(key) {
            collect(value, errors.entry(key.into()).or_default());
        }
    }
    for (key, cap) in &mut b.out.capabilities {
        if cap.available {
            continue;
        }
        let candidates: &[&str] = if key.starts_with("power.ai") || key.starts_with("balance.ai") {
            &["ai"]
        } else if key.starts_with("gpu.hybrid") {
            &["hybrid", "mapped.igpu"]
        } else if key.starts_with("gpu.oc") {
            &["gpu.oc"]
        } else if key.starts_with("cpu.")
            || key.starts_with("preset")
            || key.starts_with("god_mode")
        {
            &["cpu", "godmode"]
        } else if key.starts_with("thermal") {
            &["cpu", "godmode"]
        } else if key.starts_with("power.") {
            &["power_modes", "cpu", "system"]
        } else if key.starts_with("display.") {
            &["display_controls", "system", "mapped.overdrive"]
        } else if key.starts_with("battery.") {
            &["energy", "devices"]
        } else if key.starts_with("keys.") {
            &["system", "energy"]
        } else if key.starts_with("keyboard.") {
            &[
                "spectrum",
                "energy",
                "capabilities",
                "lighting.0",
                "lighting.3",
                "lighting.5",
                "mapped.power_led",
            ]
        } else {
            &[]
        };
        if let Some((source, error)) = candidates.iter().find_map(|source| {
            errors
                .get(*source)
                .and_then(|values| values.iter().find(|error| permission(error)))
                .map(|error| (*source, error))
        }) {
            cap.reason = format!(
                "当前权限不足，无法核对硬件能力；{source}: {}",
                error.chars().take(240).collect::<String>()
            );
        }
    }
}
fn project_metrics(b: &mut Builder<'_>, cache: &BTreeMap<String, Value>) {
    if let Some(sample) = cache.get("telemetry") {
        let source = sample
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("Windows只读采样")
            .to_owned();
        if let Some(value) = sample
            .get("cpu_utilization_percent")
            .and_then(number)
            .filter(|n| (0.0..=100.0).contains(n))
        {
            b.metric(
                "CpuUtilization",
                "CPU 使用率",
                format!("{value:.1}%"),
                source.clone(),
            );
        }
        if let Some(value) = sample
            .get("os_reported_frequency_mhz")
            .and_then(number)
            .filter(|n| *n > 0.0)
        {
            for (key, label) in [
                ("CpuFrequency", "CPU 频率"),
                ("CpuAverageFrequency", "CPU 平均频率"),
            ] {
                b.metric(
                    key,
                    label,
                    format!("{value:.0} MHz"),
                    "Windows power-manager报告值；不是effective core clock".into(),
                );
            }
        }
        if let Some(max) = sample
            .get("per_cpu_os_reported_mhz")
            .and_then(Value::as_array)
            .and_then(|v| v.iter().filter_map(Value::as_u64).max())
            .filter(|n| *n > 0)
        {
            b.metric(
                "CpuMaxFrequency",
                "CPU 最高报告频率",
                format!("{max} MHz"),
                "各逻辑CPU的Windows报告MHz最大值；无P/E核分类".into(),
            );
        }
        if let (Some(total), Some(used)) = (
            sample.get("memory_total_bytes").and_then(Value::as_u64),
            sample.get("memory_used_bytes").and_then(Value::as_u64),
        ) {
            if total > 0 && used <= total {
                b.metric(
                    "MemoryUsed",
                    "内存使用",
                    format!("{:.2} GiB", used as f64 / 1073741824.0),
                    "GlobalMemoryStatusEx".into(),
                );
                b.metric(
                    "MemoryTotal",
                    "内存总量",
                    format!("{:.2} GiB", total as f64 / 1073741824.0),
                    "GlobalMemoryStatusEx".into(),
                );
                b.metric(
                    "MemoryUtilization",
                    "内存使用率",
                    format!("{:.1}%", used as f64 * 100.0 / total as f64),
                    "GlobalMemoryStatusEx".into(),
                );
            }
        }
    }
    if let Some(firmware) = cache.get("firmware") {
        let requested = firmware.get("requested_at_tick_ms").and_then(Value::as_u64);
        let completed_at = firmware.get("completed_at_tick_ms").and_then(Value::as_u64);
        let late = requested
            .zip(completed_at)
            .is_none_or(|(start, end)| end < start || end - start > 3000);
        for (name, key, label, temperature) in [
            (
                "cpu_aggregate_celsius",
                "CpuTemperature",
                "CPU 固件聚合温度",
                true,
            ),
            (
                "gpu_aggregate_celsius",
                "GpuCoreTemperature",
                "GPU 固件温度",
                true,
            ),
            ("pch_celsius", "PchTemperature", "PCH 温度", true),
            ("cpu_fan_rpm", "CpuFanSpeed", "CPU 风扇", false),
            ("gpu_fan_rpm", "GpuFanSpeed", "GPU 风扇", false),
            ("pch_fan_rpm", "PchFanSpeed", "PCH 风扇", false),
        ] {
            let value = firmware
                .get("values")
                .and_then(|v| v.get(name))
                .and_then(completed)
                .and_then(Value::as_u64);
            if let Some(value) = value.filter(|n| {
                if temperature {
                    *n > 0 && *n < 150
                } else {
                    *n <= 30000
                }
            }) {
                let detail = format!(
                    "Lenovo固件聚合数据（非package/core双源最大值）；请求tick={requested:?}，完成tick={completed_at:?}{}",
                    if late { "；迟到快照" } else { "" }
                );
                b.metric(
                    key,
                    label,
                    if late {
                        "—".into()
                    } else if temperature {
                        format!("{value} °C")
                    } else {
                        format!("{value} RPM")
                    },
                    detail,
                );
            }
        }
    }
    if let Some(sample) = cache.get("intel").and_then(|v| {
        serde_json::from_value::<jiks_platform::intel_sensors::IntelSample>(v.clone()).ok()
    }) {
        let late = sample.completed_at_tick_ms < sample.requested_at_tick_ms
            || sample
                .completed_at_tick_ms
                .saturating_sub(sample.requested_at_tick_ms)
                > 3000;
        let detail = format!(
            "{}；package={:?}°C，core-max={:?}°C；请求tick={}，完成tick={}{}",
            sample.source,
            sample.package_temperature_celsius,
            sample.maximum_core_temperature_celsius,
            sample.requested_at_tick_ms,
            sample.completed_at_tick_ms,
            if late { "；迟到快照" } else { "" }
        );
        if let Some(temp) = sample.conservative_temperature_celsius {
            b.metric(
                "CpuTemperature",
                "CPU 温度（封装/核心最大值）",
                if late {
                    "—".into()
                } else {
                    format!("{temp:.0} °C")
                },
                detail.clone(),
            );
        }
        for (key, label, power) in [
            ("CpuPower", "CPU 封装功率", sample.package_power_watts),
            ("CpuVoltage", "CPU 请求 VID", sample.core_vid_average_volts),
        ] {
            if let Some(v) = power.filter(|v| v.is_finite() && *v >= 0.0) {
                b.metric(
                    key,
                    label,
                    if late {
                        "—".into()
                    } else if key == "CpuPower" {
                        format!("{v:.1} W")
                    } else {
                        format!("{v:.3} V")
                    },
                    if key == "CpuVoltage" {
                        format!("{}；VID请求值，并非直接测量Vcore", detail)
                    } else {
                        format!(
                            "{}；RAPL计数差值起始tick={:?}",
                            detail, sample.power_interval_start_tick_ms
                        )
                    },
                );
            }
        }
        for (kind, maxkey, avgkey, label) in [
            (None, "CpuMaxFrequency", "CpuAverageFrequency", "全部核心"),
            (
                Some(jiks_platform::intel_sensors::CoreKind::Performance),
                "CpuPMaxFrequency",
                "CpuPAverageFrequency",
                "P核",
            ),
            (
                Some(jiks_platform::intel_sensors::CoreKind::Efficiency),
                "CpuEMaxFrequency",
                "CpuEAverageFrequency",
                "E核",
            ),
        ] {
            let values = sample
                .cores
                .iter()
                .filter(|c| kind.is_none_or(|kind| c.address.kind == kind))
                .filter_map(|c| c.ratio_frequency_mhz)
                .filter(|v| v.is_finite() && *v > 0.0)
                .collect::<Vec<_>>();
            if !values.is_empty() {
                let avg = values.iter().sum::<f64>() / values.len() as f64;
                let max = values.iter().copied().reduce(f64::max).unwrap();
                for (key, title, value) in [
                    (maxkey, format!("{label}最高频率"), max),
                    (avgkey, format!("{label}平均频率"), avg),
                ] {
                    b.metric(
                        key,
                        &title,
                        if late {
                            "—".into()
                        } else {
                            format!("{value:.0} MHz")
                        },
                        format!("{}；MSR倍频×CPUID总线MHz；非effective clock", detail),
                    );
                }
            }
        }
    }
    if let Some(sample) = cache.get("gpu.sensors").and_then(|v| {
        serde_json::from_value::<jiks_platform::gpu_sensors::GpuSample>(v.clone()).ok()
    }) {
        b.scalar("sensors.gpu.is_igpu", json!(sample.is_igpu));
        b.scalar("sensors.gpu.name", json!(sample.name));
        // A selected Intel sample must not inherit NVIDIA firmware temperature
        // or fan RPM merely because its own driver returned unknown.
        if sample.is_igpu
            || sample
                .status
                .as_ref()
                .is_some_and(|status| matches!(status.raw_coproc_state, Some(0 | 2)))
        {
            b.out.metrics.retain(|v| !v.key.starts_with("Gpu"));
        }
        let late = sample.completed_at_tick_ms < sample.requested_at_tick_ms
            || sample
                .completed_at_tick_ms
                .saturating_sub(sample.requested_at_tick_ms)
                > 3000;
        // The selected integrated GPU has no NVAPI P-state. Never inherit an
        // older discrete sample/lock or turn an unavailable read into P0.
        b.scalar("gpu.current_pstate", Value::Null);
        b.scalar("gpu.current_pstate_at_tick_ms", Value::Null);
        if !late && !sample.is_igpu {
            if let Some(state) = sample.performance_state.filter(|state| *state < 16) {
                b.scalar("gpu.current_pstate", json!(state));
                b.scalar(
                    "gpu.current_pstate_at_tick_ms",
                    json!(sample.requested_at_tick_ms),
                );
            }
            if let Some(status) = sample.status.as_ref().filter(|status| {
                status.requested_at_tick_ms >= sample.requested_at_tick_ms
                    && status.completed_at_tick_ms <= sample.completed_at_tick_ms
                    && status.completed_at_tick_ms >= status.requested_at_tick_ms
                    && status.completed_at_tick_ms - status.requested_at_tick_ms <= 3000
            }) {
                let state = status.classified(sample.hybrid_off);
                if state != jiks_platform::gpu_status::GpuState::Unknown {
                    b.scalar("gpu.state", json!(state));
                    b.scalar("gpu.state_at_tick_ms", json!(status.requested_at_tick_ms));
                }
            }
        }
        for (key, label) in [
            ("GpuCoreTemperature", "GPU 核心温度"),
            ("GpuVramTemperature", "GPU 显存结温"),
            ("GpuHotSpotTemperature", "GPU 热点温度"),
            ("GpuFrequency", "GPU 核心频率"),
            ("GpuMemoryFrequency", "GPU 显存频率"),
            ("GpuUtilization", "GPU 使用率"),
            ("GpuVramTotal", "显存总量"),
            ("GpuVramUsed", "显存已用"),
            ("GpuVramUtilization", "显存使用率"),
            ("GpuPower", "GPU 板卡功率"),
            ("GpuChipPower", "GPU 芯片功率"),
            ("GpuFanSpeed", "GPU 风扇"),
            ("GpuVoltage", "GPU 核心电压"),
        ] {
            if let Some(sensor) = sample.values.get(key) {
                if !sample.is_igpu && sensor.value.is_none() {
                    if let Some(existing) = b
                        .out
                        .metrics
                        .iter_mut()
                        .find(|v| v.key == key && v.value != "—")
                    {
                        existing.detail.push_str(&format!(
                            "；NVIDIA getter未返回值：{}",
                            sensor.error.as_deref().unwrap_or("not advertised")
                        ));
                        continue;
                    }
                }
                let value = if late {
                    "—".into()
                } else {
                    sensor
                        .value
                        .filter(|v| v.is_finite() && *v >= 0.)
                        .map(|v| format!("{v:.2} {}", sensor.unit))
                        .unwrap_or_else(|| "—".into())
                };
                let label = if sample.is_igpu {
                    match key {
                        "GpuVramTotal" => "集显共享内存总量",
                        "GpuVramUsed" => "集显共享内存已用",
                        "GpuVramUtilization" => "集显共享内存使用率",
                        _ => label,
                    }
                } else {
                    label
                };
                b.metric(
                    key,
                    label,
                    value,
                    format!(
                        "{} · {}；请求tick={}，完成tick={}{}{}",
                        sample.name,
                        sensor.source,
                        sample.requested_at_tick_ms,
                        sample.completed_at_tick_ms,
                        if late { "；迟到快照" } else { "" },
                        sensor
                            .error
                            .as_ref()
                            .map(|e| format!("；{e}"))
                            .unwrap_or_default()
                    ),
                );
            } else if [
                "GpuCoreTemperature",
                "GpuVramTemperature",
                "GpuFrequency",
                "GpuUtilization",
                "GpuVramTotal",
                "GpuVramUsed",
                "GpuVramUtilization",
                "GpuPower",
                "GpuFanSpeed",
            ]
            .contains(&key)
            {
                b.metric(
                    key,
                    label,
                    "—".into(),
                    format!(
                        "{}；所选GPU未返回该指标；{}",
                        sample.name,
                        sample
                            .errors
                            .values()
                            .take(2)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("；")
                    ),
                );
            }
        }
    }
    if let Some(batteries) = cache
        .get("devices")
        .and_then(|v| inner(v, "batteries"))
        .and_then(Value::as_array)
    {
        if let Some(battery) = batteries.first() {
            if let Some(level) = field(battery, "EstimatedChargeRemaining")
                .and_then(Value::as_u64)
                .filter(|n| *n <= 100)
            {
                b.metric(
                    "BatteryLevel",
                    "电池电量",
                    format!("{level}%"),
                    "Win32_Battery.EstimatedChargeRemaining".into(),
                );
            }
            if let Some(state) = field(battery, "BatteryStatus").and_then(Value::as_u64) {
                b.metric(
                    "BatteryState",
                    "电池状态",
                    format!("状态 {state}"),
                    "Win32_Battery.BatteryStatus原始枚举；不冒称放电功率测量".into(),
                );
            }
        }
    }
}
fn project_aux_metrics(b: &mut Builder<'_>, cache: &BTreeMap<String, Value>) {
    fn fresh(v: &Value) -> bool {
        v.get("requested_at_tick_ms")
            .and_then(Value::as_u64)
            .zip(v.get("completed_at_tick_ms").and_then(Value::as_u64))
            .is_some_and(|(r, c)| c >= r && c - r < 2000)
    }
    fn errors(v: &Value) -> String {
        v.get("errors")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .take(4)
                    .collect::<Vec<_>>()
                    .join("；")
            })
            .unwrap_or_default()
    }
    if let Some(sample) = cache.get("storage") {
        if let Some(disks) = sample.get("disks").and_then(Value::as_array) {
            for (index, key) in ["Disk1Temperature", "Disk2Temperature"]
                .into_iter()
                .enumerate()
            {
                let disk = disks.get(index);
                let value = disk
                    .and_then(|v| v.get("celsius"))
                    .and_then(number)
                    .filter(|v| fresh(sample) && *v > 0. && *v < 150.);
                let detail = disk
                    .map(|v| {
                        format!(
                            "{} · PhysicalDrive{} · {}{}；{}",
                            v.get("model").and_then(Value::as_str).unwrap_or("未知NVMe"),
                            v.get("index")
                                .and_then(Value::as_u64)
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "?".into()),
                            v.get("source").and_then(Value::as_str).unwrap_or(""),
                            if fresh(sample) {
                                ""
                            } else {
                                "；迟到/无时间戳快照"
                            },
                            v.get("error").and_then(Value::as_str).unwrap_or("")
                        )
                    })
                    .unwrap_or_else(|| "尚无第二块已完成的NVMe健康日志读取".into());
                b.metric(
                    key,
                    if index == 0 {
                        "磁盘 1 温度"
                    } else {
                        "磁盘 2 温度"
                    },
                    value
                        .map(|v| format!("{v:.1} °C"))
                        .unwrap_or_else(|| "—".into()),
                    detail,
                );
            }
        }
    }
    if let Some(sample) = cache.get("memory.thermal") {
        let value = sample
            .get("maximum_celsius")
            .and_then(number)
            .filter(|v| fresh(sample) && *v > 0. && *v < 150.);
        b.metric(
            "MemoryTemperature",
            "内存温度",
            value
                .map(|v| format!("{v:.1} °C"))
                .unwrap_or_else(|| "—".into()),
            format!(
                "{}{}；{}",
                sample
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("DDR5 hub thermal"),
                if fresh(sample) {
                    ""
                } else {
                    "；迟到/无时间戳快照"
                },
                errors(sample)
            ),
        );
        if value.is_none() {
            let reason = sample
                .get("dimms")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(|r| r.get("error").and_then(Value::as_str))
                        .take(2)
                        .collect::<Vec<_>>()
                        .join("；")
                })
                .unwrap_or_default();
            if let Some(metric) = b
                .out
                .metrics
                .iter_mut()
                .find(|m| m.key == "MemoryTemperature")
            {
                metric.detail.push_str(&format!("；{reason}"));
            }
        }
    }
    if let Some(sample) = cache.get("battery") {
        // This machine has one system battery. Never merge ambiguous batteries
        // or substitute a relative-capacity rate for a measured milliwatt rate.
        if let Some(rows) = sample
            .get("batteries")
            .and_then(Value::as_array)
            .filter(|rows| rows.len() == 1)
        {
            let row = &rows[0];
            if fresh(sample) {
                for field in [
                    "slot",
                    "tag",
                    "capacity",
                    "designed_capacity",
                    "full_charged_capacity",
                    "cycle_count",
                    "health_percent",
                    "rate",
                    "rate_unit",
                    "capacity_unit",
                    "voltage_millivolts",
                    "power_state",
                ] {
                    if let Some(value) = row.get(field) {
                        b.scalar(&format!("battery.{field}"), value.clone());
                    }
                }
                let relative = row.get("capacity_unit").and_then(Value::as_str) == Some("relative");
                if let Some(value) = row
                    .get("health_percent")
                    .and_then(number)
                    .filter(|v| *v >= 0.)
                {
                    b.metric("BatteryHealth","电池健康",format!("{value:.2}%"),"Windows BATTERY_INFORMATION.FullChargedCapacity / DesignedCapacity；同一battery tag".into());
                }
                if let Some(value) = row.get("cycle_count").and_then(Value::as_u64) {
                    b.metric(
                        "BatteryCycleCount",
                        "电池循环",
                        value.to_string(),
                        "Windows BATTERY_INFORMATION.CycleCount".into(),
                    );
                }
                if let Some(rate) = row.get("rate").and_then(Value::as_i64) {
                    let text = if relative {
                        format!("{rate} relative/hour")
                    } else {
                        format!("{:.2} W", rate as f64 / 1000.)
                    };
                    b.metric(
                        "BatteryPower",
                        "电池充放电功率",
                        text,
                        "Windows BATTERY_STATUS.Rate；正为充电、负为放电，未知值不填0".into(),
                    );
                }
            }
        }
    }
    if let Some(sample) = cache.get("battery.thermal") {
        let value = sample
            .get("celsius")
            .and_then(number)
            .filter(|v| fresh(sample) && *v >= 0. && *v < 150.);
        b.scalar("battery.temperature_celsius", json!(value));
        for field in ["manufacture_date", "first_use_date"] {
            if let Some(value) = sample.get(field).filter(|_| fresh(sample)) {
                b.scalar(&format!("battery.{field}"), value.clone());
            }
        }
        b.metric(
            "BatteryTemperature",
            "电池温度",
            value
                .map(|v| format!("{v:.1} °C"))
                .unwrap_or_else(|| "—".into()),
            format!(
                "Lenovo EnergyDrv 0x83102138{}；{}",
                if fresh(sample) {
                    ""
                } else {
                    "；迟到/无时间戳快照"
                },
                errors(sample)
            ),
        );
    }
    for (cache_key, keys) in [
        ("storage", &["Disk1Temperature", "Disk2Temperature"][..]),
        ("memory.thermal", &["MemoryTemperature"][..]),
        ("battery.thermal", &["BatteryTemperature"][..]),
    ] {
        if let Some(error) = cache
            .get(&format!("error.{cache_key}"))
            .and_then(Value::as_str)
        {
            for key in keys {
                b.metric(
                    key,
                    match *key {
                        "Disk1Temperature" => "磁盘 1 温度",
                        "Disk2Temperature" => "磁盘 2 温度",
                        "MemoryTemperature" => "内存温度",
                        _ => "电池温度",
                    },
                    "—".into(),
                    format!(
                        "读取失败/能力未知：{}",
                        error.chars().take(300).collect::<String>()
                    ),
                );
            }
            if cache_key == "battery.thermal" {
                b.scalar("battery.temperature_celsius", Value::Null);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gpu_status_projects_observed_classification_and_expires_older_embedded_status() {
        use jiks_platform::gpu_status::{GpuState, GpuStatusSnapshot};
        let mut sample = jiks_platform::gpu_sensors::GpuSample::new(0, "Discrete fixture", 100);
        sample.completed_at_tick_ms = 200;
        sample.hybrid_off = Some(false);
        sample.status = Some(GpuStatusSnapshot {
            state: GpuState::Unknown,
            raw_coproc_state: Some(3),
            filtered_process_count: Some(1),
            display_connected: Some(false),
            requested_at_tick_ms: 110,
            completed_at_tick_ms: 120,
            errors: BTreeMap::new(),
        });
        let project_sample = |sample: &jiks_platform::gpu_sensors::GpuSample| {
            project(
                &BTreeMap::from([("gpu.sensors".into(), serde_json::to_value(sample).unwrap())]),
                false,
                None,
            )
        };
        let p = project_sample(&sample);
        assert_eq!(p.values["gpu.state"], "Active");
        assert_eq!(p.values["gpu.state_at_tick_ms"], 110);
        sample.hybrid_off = Some(true);
        assert_eq!(
            project_sample(&sample).values["gpu.state"],
            "MonitorConnected"
        );
        sample.hybrid_off = None;
        assert!(project_sample(&sample).values["gpu.state"].is_null());
        sample.status.as_mut().unwrap().raw_coproc_state = Some(2);
        assert_eq!(project_sample(&sample).values["gpu.state"], "PoweredOff");
        sample.status.as_mut().unwrap().requested_at_tick_ms = 90;
        assert!(project_sample(&sample).values["gpu.state"].is_null());
        sample.status.as_mut().unwrap().requested_at_tick_ms = 110;
        sample.is_igpu = true;
        assert!(project_sample(&sample).values["gpu.state"].is_null());
    }
    #[test]
    fn actual_pstate_is_timestamped_and_never_inherited_by_selected_igpu_or_unknown_getter() {
        let mut sample = jiks_platform::gpu_sensors::GpuSample::new(0, "GPU fixture", 100);
        sample.completed_at_tick_ms = 200;
        sample.performance_state = Some(8);
        let mut cache = BTreeMap::from([
            ("gpu.sensors".into(), serde_json::to_value(&sample).unwrap()),
            (
                "gpu.pstate".into(),
                json!({"current_pstate":0,"requested_at_tick_ms":50,"completed_at_tick_ms":60}),
            ),
        ]);
        let p = project(&cache, false, None);
        assert_eq!(p.values["gpu.current_pstate"], 8);
        assert_eq!(p.values["gpu.current_pstate_at_tick_ms"], 100);
        for (igpu, value) in [(false, None), (true, Some(0))] {
            sample.is_igpu = igpu;
            sample.performance_state = value;
            cache.insert("gpu.sensors".into(), serde_json::to_value(&sample).unwrap());
            assert!(project(&cache, false, None).values["gpu.current_pstate"].is_null());
        }
        sample.is_igpu = false;
        sample.performance_state = Some(0);
        sample.completed_at_tick_ms = 4000;
        cache.insert("gpu.sensors".into(), serde_json::to_value(sample).unwrap());
        assert!(project(&cache, false, None).values["gpu.current_pstate"].is_null());
    }
    #[test]
    fn aux_sensor_projection_uses_completed_native_values_units_and_freshness() {
        let mut cache = BTreeMap::from([
            (
                "storage".into(),
                json!({"requested_at_tick_ms":1,"completed_at_tick_ms":2,"disks":[{"index":0,"model":"NVMe fixture","celsius":40,"source":"Windows health","error":null},{"index":1,"model":"second fixture","celsius":null,"source":"Windows health","error":"permission denied"}]}),
            ),
            (
                "memory.thermal".into(),
                json!({"requested_at_tick_ms":1,"completed_at_tick_ms":2,"maximum_celsius":50,"source":"DDR5 hub","errors":[],"dimms":[]}),
            ),
            (
                "battery".into(),
                json!({"requested_at_tick_ms":1,"completed_at_tick_ms":2,"batteries":[{"slot":0,"tag":3,"capacity_unit":"relative","rate":-12,"rate_unit":"relative/hour","health_percent":95,"cycle_count":123}]}),
            ),
            (
                "battery.thermal".into(),
                json!({"requested_at_tick_ms":1,"completed_at_tick_ms":2,"celsius":35.5,"errors":[]}),
            ),
        ]);
        let p = project(&cache, false, None);
        assert_eq!(p.values["battery.temperature_celsius"], 35.5);
        assert!(
            p.metrics
                .iter()
                .find(|m| m.key == "MemoryTemperature")
                .unwrap()
                .value
                .contains("50.0")
        );
        assert!(
            p.metrics
                .iter()
                .find(|m| m.key == "BatteryPower")
                .unwrap()
                .value
                .contains("relative/hour")
        );
        assert_eq!(
            p.metrics
                .iter()
                .find(|m| m.key == "Disk2Temperature")
                .unwrap()
                .value,
            "—"
        );
        assert!(
            p.metrics
                .iter()
                .find(|m| m.key == "Disk2Temperature")
                .unwrap()
                .detail
                .contains("permission")
        );
        for key in ["storage", "memory.thermal", "battery.thermal"] {
            cache.get_mut(key).unwrap()["completed_at_tick_ms"] = json!(2001);
        }
        let p = project(&cache, false, None);
        assert!(p.values["battery.temperature_celsius"].is_null());
        for key in [
            "Disk1Temperature",
            "MemoryTemperature",
            "BatteryTemperature",
        ] {
            assert_eq!(p.metrics.iter().find(|m| m.key == key).unwrap().value, "—");
        }
    }
    #[test]
    fn boot_and_autorun_editors_require_their_own_verified_native_capture() {
        for (info_ok, animation_ok) in [(true, false), (false, true)] {
            let cache = BTreeMap::from([(
                "boot_logo".into(),
                json!({"info":if info_ok{json!({"available":true,"value":{"enabled":true}})}else{json!({"available":false,"error":"denied"})},"animation":if animation_ok{json!({"available":true,"value":{"disabled":false}})}else{json!({"available":false,"error":"denied"})},"pending":{"available":true,"value":false}}),
            )]);
            let p = project(&cache, false, None);
            assert_eq!(p.capabilities["display.boot_logo.path"].available, info_ok);
            assert_eq!(
                p.capabilities["display.boot_logo.enabled"].available,
                info_ok
            );
            assert_eq!(
                p.capabilities["display.boot_animation"].available,
                animation_ok
            );
            assert!(p.capabilities["display.boot_logo.apply"].available);
            let p = project(&cache, true, None);
            assert!(
                !p.capabilities["display.boot_animation"].available
                    && !p.capabilities["display.boot_logo.path"].available
            );
        }
        for matches in [false, true] {
            let cache = BTreeMap::from([(
                "autorun".into(),
                json!({"mode":"normal","task_present":true,"owned":true,"action_matches":matches,"settings_verified":true,"run_level_highest":true}),
            )]);
            let p = project(&cache, false, None);
            assert_eq!(p.capabilities["app.autorun"].available, matches);
            assert_eq!(p.capabilities["app.autorun.apply"].available, matches);
        }
    }
    #[test]
    fn preferred_intel_unknown_temperature_cannot_inherit_nvidia_firmware_value() {
        let mut sample = jiks_platform::gpu_sensors::GpuSample::new(0, "Intel UHD fixture", 1);
        sample.is_igpu = true;
        sample.completed_at_tick_ms = 2;
        sample
            .insert("GpuVramUsed", 2., "GB", "D3DKMT shared whole-adapter")
            .unwrap();
        sample
            .errors
            .insert("igcl".into(), "offline fixture denied".into());
        let cache = BTreeMap::from([
            ("gpu.sensors".into(), serde_json::to_value(sample).unwrap()),
            (
                "firmware".into(),
                json!({"requested_at_tick_ms":1,"completed_at_tick_ms":2,"values":{"gpu_aggregate_celsius":{"available":true,"value":88},"gpu_fan_rpm":{"available":true,"value":2500}}}),
            ),
        ]);
        let p = project(&cache, false, None);
        assert_eq!(p.values["sensors.gpu.is_igpu"], true);
        let temp = p
            .metrics
            .iter()
            .find(|v| v.key == "GpuCoreTemperature")
            .unwrap();
        assert_eq!(temp.value, "—");
        assert!(!temp.detail.contains("88"));
        assert_eq!(
            p.metrics
                .iter()
                .find(|v| v.key == "GpuFanSpeed")
                .unwrap()
                .value,
            "—"
        );
        assert!(
            p.metrics
                .iter()
                .find(|v| v.key == "GpuVramUsed")
                .unwrap()
                .label
                .contains("共享")
        );
    }
    #[test]
    fn locks_mutes_and_one_level_white_project_with_original_logical_semantics() {
        for on in [false, true] {
            let cache = BTreeMap::from([
                (
                    "system".into(),
                    json!({"win_key":{"available":true,"value":{"value":u32::from(on)}},"touchpad_lock":{"available":true,"value":{"value":u32::from(on)}}}),
                ),
                (
                    "energy".into(),
                    json!({"fn_lock":on,"night_charge":on,"one_level_white":on,"keyboard_maximum":0,"keyboard_level":0}),
                ),
            ]);
            let p = project(&cache, false, None);
            for key in [
                "keys.win_lock",
                "keys.touchpad_lock",
                "keys.fn_lock",
                "battery.night_charge",
                "keyboard.white_single",
            ] {
                assert_eq!(p.values[key], json!(on));
            }
            assert!(!p.capabilities["keyboard.white"].available);
            assert!(!p.ranges.contains_key("keyboard.white_level"));
        }
        let endpoints = json!([{"capture":true,"muted":true,"volume_percent":70.0},{"capture":true,"muted":false,"volume_percent":70.0},{"capture":false,"muted":true,"volume_percent":50.0},{"capture":false,"muted":false,"volume_percent":70.0}]);
        for reversed in [false, true] {
            let mut endpoints = endpoints.clone();
            if reversed {
                endpoints.as_array_mut().unwrap().reverse();
            }
            let cache = BTreeMap::from([(
                "devices".into(),
                json!({"audio":{"available":true,"value":endpoints}}),
            )]);
            let p = project(&cache, false, None);
            assert_eq!(p.values["keys.microphone_muted"], false);
            assert_eq!(p.values["speaker.muted"], false);
            assert!(!p.values.contains_key("speaker.volume"));
        }
    }
    #[test]
    fn access_denied_discovery_is_not_labeled_hardware_unsupported() {
        let cache = BTreeMap::from([
            (
                "error.power_modes".into(),
                json!("HRESULT Call failed with: 0x80041003"),
            ),
            (
                "hybrid".into(),
                json!({"snapshot":{"gsync":null,"gsync_supported":null,"igpu_mode":null,"igpu_backend":null,"bios_current":null,"bios_selections":[],"errors":{"gsync_support":"HRESULT Call failed with: 0x80041003"}}}),
            ),
        ]);
        let p = project(&cache, false, None);
        assert!(!p.capabilities["power.apply"].available);
        assert!(p.capabilities["power.apply"].reason.contains("权限不足"));
        assert!(
            p.capabilities["gpu.hybrid.apply"]
                .reason
                .contains("权限不足")
        );
        assert!(!p.capabilities["gpu.oc.apply"].reason.contains("权限不足"));
        assert!(
            !p.options
                .get("power.mode")
                .is_some_and(|v| v.iter().any(|v| v.value == json!(224)))
        );
    }
    #[test]
    fn native_intel_snapshot_projects_real_granularity_and_overrides_aggregate_once() {
        let intel = json!({"requested_at_tick_ms":100,"completed_at_tick_ms":101,"cores":[{"address":{"physical_id":0,"logical_processor":0,"kind":"Performance","bus_frequency_mhz":100},"temperature_celsius":90.0,"distance_to_tjmax_celsius":10.0,"tjmax_celsius":100.0,"ratio_frequency_mhz":4400.0,"requested_vid_volts":1.0}],"package_temperature_celsius":88.0,"maximum_core_temperature_celsius":90.0,"average_core_temperature_celsius":90.0,"conservative_temperature_celsius":90.0,"package_power_watts":70.0,"core_power_watts":60.0,"dram_power_watts":null,"platform_power_watts":null,"power_interval_start_tick_ms":1,"core_vid_average_volts":1.0,"core_vid_maximum_volts":1.0,"source":"offline MSR fixture; VID requested","errors":[]});
        let mut cache = BTreeMap::from([
            ("intel".into(), intel),
            (
                "firmware".into(),
                json!({"requested_at_tick_ms":100,"completed_at_tick_ms":101,"values":{"cpu_aggregate_celsius":{"available":true,"value":55}}}),
            ),
        ]);
        let p = project(&cache, false, None);
        let temperature = p
            .metrics
            .iter()
            .filter(|m| m.key == "CpuTemperature")
            .collect::<Vec<_>>();
        assert_eq!(temperature.len(), 1);
        assert_eq!(temperature[0].value, "90 °C");
        assert_eq!(
            p.metrics
                .iter()
                .find(|m| m.key == "CpuPower")
                .unwrap()
                .value,
            "70.0 W"
        );
        assert!(
            p.metrics
                .iter()
                .find(|m| m.key == "CpuVoltage")
                .unwrap()
                .detail
                .contains("并非直接测量")
        );
        assert!(p.metrics.iter().any(|m| m.key == "CpuPMaxFrequency"));
        assert!(!p.metrics.iter().any(|m| m.key == "CpuEMaxFrequency"));
        cache.get_mut("intel").unwrap()["completed_at_tick_ms"] = json!(5000);
        let p = project(&cache, false, None);
        assert_eq!(
            p.metrics
                .iter()
                .find(|m| m.key == "CpuTemperature")
                .unwrap()
                .value,
            "—"
        );
    }
    #[test]
    fn hybrid_requires_completed_companion_getters_and_never_infers_uma() {
        use jiks_platform::hybrid::{HybridSnapshot, IgpuBackend};
        let mut snapshot = HybridSnapshot {
            gsync: Some(false),
            igpu_mode: Some(0),
            igpu_backend: Some(IgpuBackend::GameZone),
            ..Default::default()
        };
        let cache = BTreeMap::from([("hybrid".into(), json!({"snapshot":snapshot}))]);
        let p = project(&cache, false, None);
        assert!(p.capabilities["gpu.hybrid.apply"].available);
        assert_eq!(p.options["gpu.hybrid_mode"].len(), 4);
        assert!(
            !p.options["gpu.hybrid_mode"]
                .iter()
                .any(|m| m.value == json!(3))
        );
        snapshot.gsync = None;
        snapshot.gsync_supported = Some(true);
        let cache = BTreeMap::from([("hybrid".into(), json!({"snapshot":snapshot}))]);
        assert!(!project(&cache, false, None).capabilities["gpu.hybrid.apply"].available);
    }
    #[test]
    fn empty_cache_never_claims_hardware_writes_available() {
        let p = project(&BTreeMap::new(), false, None);
        assert!(!p.capabilities["power.apply"].available);
        assert!(!p.capabilities["keyboard.apply"].available);
        assert!(p.metrics.is_empty());
        // Explicit nulls invalidate prior readonly status in the merged UI
        // cache; no unknown observation becomes a default P0/Inactive value.
        assert!(p.values.iter().all(|(key, value)| {
            [
                "gpu.current_pstate",
                "gpu.current_pstate_at_tick_ms",
                "gpu.state",
                "gpu.state_at_tick_ms",
            ]
            .contains(&key.as_str())
                && value.is_null()
        }));
    }
    #[test]
    fn read_only_and_legacy_conflict_disable_writes_without_hiding_actual_state() {
        let cache = BTreeMap::from([(
            "cpu".into(),
            json!({"raw_mode":3,"limits":{"long_watts":70,"short_watts":125,"temperature_limit":100}}),
        )]);
        for p in [
            project(&cache, true, None),
            project(&cache, false, Some("old Toolkit PID fixture")),
        ] {
            assert_eq!(p.values["power.mode"], 3);
            assert!(!p.capabilities["power.apply"].available);
            assert!(!p.capabilities["power.apply"].reason.is_empty());
        }
    }
    #[test]
    fn sparse_discrete_capability_produces_choices_without_fake_slider_holes() {
        let cache = BTreeMap::from([(
            "godmode".into(),
            json!({"fields":[{"key":"cpu.pl2_tau","capability":{"available":true,"value":{"min":0,"max":20,"step":null,"discrete":[0,7,20]}},"current":{"available":true,"value":7}}]}),
        )]);
        let p = project(&cache, false, None);
        assert_eq!(p.options["cpu.pl2_tau"].len(), 3);
        assert!(!p.ranges.contains_key("cpu.pl2_tau"));
        assert_eq!(p.values["cpu.pl2_tau"], 7);
    }
    #[test]
    fn absence_of_telemetry_does_not_manufacture_power_voltage_or_dual_temperature() {
        let cache = BTreeMap::from([(
            "telemetry".into(),
            json!({"cpu_utilization_percent":12.5,"os_reported_frequency_mhz":2200,"memory_total_bytes":1073741824u64,"memory_used_bytes":536870912u64}),
        )]);
        let p = project(&cache, false, None);
        assert!(p.metrics.iter().any(|v| v.key == "CpuUtilization"));
        assert!(!p.metrics.iter().any(|v| {
            [
                "CpuPower",
                "CpuVoltage",
                "GpuVramTemperature",
                "CpuPMaxFrequency",
            ]
            .contains(&v.key.as_str())
        }));
        assert!(p.values.values().all(|v| !v.is_object() && !v.is_array()));
    }
    #[test]
    fn late_firmware_snapshot_is_not_formatted_as_fresh_temperature() {
        let cache = BTreeMap::from([(
            "firmware".into(),
            json!({"requested_at_tick_ms":1,"completed_at_tick_ms":5000,"values":{"cpu_aggregate_celsius":{"available":true,"value":55}}}),
        )]);
        let p = project(&cache, false, None);
        let metric = p
            .metrics
            .iter()
            .find(|v| v.key == "CpuTemperature")
            .unwrap();
        assert_eq!(metric.value, "—");
        assert!(metric.detail.contains("迟到"));
    }
    #[test]
    fn spectrum_metadata_is_not_a_protocol_probe_or_automatic_profile() {
        let cache = BTreeMap::from([(
            "spectrum".into(),
            json!({"candidates":[{"path":"\\\\?\\hid#fixture"}],"protocol_probed":false}),
        )]);
        let p = project(&cache, false, None);
        assert!(p.capabilities["keyboard.apply"].available);
        assert!(!p.values.contains_key("keyboard.profile"));
        assert!(!p.values.contains_key("keyboard.effect"));
        assert_eq!(
            p.collections["keyboard.interfaces"][0].value["protocol_probed"],
            false
        );
    }
    #[test]
    fn failed_capture_is_not_a_capability_and_gpu_presence_is_not_oc_support() {
        let cache = BTreeMap::from([
            (
                "system".into(),
                json!({"brightness":{"available":false,"error":"fixture"}}),
            ),
            ("gpu".into(), json!([{"index":0,"name":"RTX fixture"}])),
        ]);
        let p = project(&cache, false, None);
        assert!(!p.values.contains_key("display.brightness"));
        assert!(!p.capabilities["gpu.oc.apply"].available);
        assert_eq!(p.values["gpu.name"], "RTX fixture");
    }
}
