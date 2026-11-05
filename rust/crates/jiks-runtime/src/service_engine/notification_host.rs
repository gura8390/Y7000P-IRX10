//! Normal status notifications are diffs of completed getters. First known
//! values establish baselines; missing/failed getters never become false.
use super::*;
#[derive(Default)]
pub(super) struct NotificationHost {
    active: bool,
    warm: bool,
    priming: Option<Vec<u64>>,
    last: BTreeMap<String, Value>,
    caps: Option<bool>,
    num: Option<bool>,
}
fn enabled(engine: &Engine) -> bool {
    engine
        .repository
        .document("notifications")
        .ok()
        .and_then(|doc| doc.root().get_ci("DontShowNotifications"))
        .and_then(JsonNode::as_bool)
        != Some(true)
}
fn category(engine: &Engine, name: &str) -> bool {
    engine
        .repository
        .document("notifications")
        .ok()
        .and_then(|doc| doc.root().get_ci("Notifications"))
        .and_then(|root| root.get_ci(name))
        .and_then(JsonNode::as_bool)
        .unwrap_or(!matches!(name, "ACAdapter" | "SmartKey"))
}
fn message(kind: &str, text: impl Into<String>) -> Value {
    json!({"type":kind,"text":text.into()})
}
fn changed(
    last: &mut BTreeMap<String, Value>,
    values: BTreeMap<String, Value>,
) -> Vec<(String, Value)> {
    let mut changes = Vec::new();
    for (key, value) in values {
        if let Some(before) = last.insert(key.clone(), value.clone()) {
            if before != value {
                changes.push((key, value));
            }
        }
    }
    changes
}
fn current(engine: &Engine) -> BTreeMap<String, Value> {
    // Read only small, completed scalar results. Rebuilding the full UI
    // projection here would clone package/sensor collections on every poll.
    fn captured<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
        let value = root.get(key)?;
        if value.get("available").and_then(Value::as_bool) == Some(true) {
            value.get("value")
        } else {
            None
        }
    }
    fn field<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
        root.as_object()?
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    }
    let mut values = BTreeMap::new();
    if let Some(status) = engine
        .cache
        .get("system")
        .and_then(|v| v.pointer("/power_status/status"))
        .and_then(Value::as_str)
        .filter(|v| matches!(*v, "Connected" | "ConnectedLowWattage" | "Disconnected"))
    {
        values.insert("adapter".into(), json!(status));
    }
    if let Some(raw) = engine
        .cache
        .get("power_modes")
        .and_then(|v| v.get("current_raw"))
        .and_then(Value::as_u64)
        .filter(|v| [1, 2, 3, 224, 255].contains(v))
    {
        values.insert("power".into(), json!(raw));
    }
    if let Some(snapshot) = engine
        .cache
        .get("hybrid")
        .and_then(|v| v.get("snapshot"))
        .and_then(|v| {
            serde_json::from_value::<jiks_platform::hybrid::HybridSnapshot>(v.clone()).ok()
        })
    {
        if let Some(mode) = snapshot.selected() {
            values.insert("hybrid".into(), json!(mode as u32));
        }
    }
    if let Some(system) = engine.cache.get("system") {
        for (key, getter) in [("win", "win_key"), ("touchpad", "touchpad_lock")] {
            if let Some(raw) = captured(system, getter)
                .and_then(|v| v.get("value"))
                .and_then(Value::as_u64)
                .filter(|v| *v <= 1)
            {
                values.insert(key.into(), json!(raw != 0));
            }
        }
    }
    if let Some(energy) = engine.cache.get("energy") {
        for (key, getter) in [("fn", "fn_lock"), ("single_white", "one_level_white")] {
            if let Some(on) = energy.get(getter).and_then(Value::as_bool) {
                values.insert(key.into(), json!(on));
            }
        }
        if let (Some(max), Some(level)) = (
            energy.get("keyboard_maximum").and_then(Value::as_u64),
            energy.get("keyboard_level").and_then(Value::as_u64),
        ) {
            if (1..=3).contains(&max) && level <= max {
                values.insert("white".into(), json!(level));
            }
        }
    }
    if let Some(audio) = engine
        .cache
        .get("devices")
        .and_then(|v| captured(v, "audio"))
        .and_then(Value::as_array)
    {
        let captures = audio
            .iter()
            .filter(|v| v.get("capture").and_then(Value::as_bool) == Some(true))
            .collect::<Vec<_>>();
        if !captures.is_empty()
            && captures
                .iter()
                .all(|v| v.get("muted").and_then(Value::as_bool).is_some())
        {
            values.insert(
                "mic".into(),
                json!(
                    captures
                        .iter()
                        .all(|v| v.get("muted").and_then(Value::as_bool) == Some(true))
                ),
            );
        }
    }
    if let Some(on) = engine
        .cache
        .get("wireless")
        .and_then(|v| captured(v, "airplane"))
        .and_then(|v| v.get("airplane_on"))
        .and_then(Value::as_bool)
    {
        values.insert("airplane".into(), json!(on));
    }
    for (id, key) in [(3, "panel"), (5, "ports")] {
        if hardware_routes::lighting_supported(&engine.cache, id) {
            if let Some(raw) = engine
                .cache
                .get(&format!("lighting.{id}"))
                .and_then(|v| field(v, "Current_State_Type"))
                .and_then(Value::as_u64)
                .filter(|v| *v <= 1)
            {
                values.insert(key.into(), json!(raw != 0));
            }
        }
    }
    if hardware_routes::lighting_supported(&engine.cache, 0) {
        if let Some(raw) = engine
            .cache
            .get("lighting.0")
            .and_then(|v| field(v, "Current_Brightness_Level"))
            .and_then(Value::as_u64)
            .filter(|v| (1..=3).contains(v))
        {
            values.insert("white".into(), json!(raw - 1));
        }
    }
    if let Some(profile) = engine
        .cache
        .get("keyboard.active_profile")
        .and_then(|v| v.get("profile"))
        .and_then(Value::as_u64)
        .filter(|v| *v <= 6)
    {
        values.insert("profile".into(), json!(profile));
    }
    if let Some(value) = engine
        .cache
        .get("keyboard.brightness")
        .and_then(Value::as_u64)
        .filter(|v| *v <= 9)
    {
        values.insert("spectrum_brightness".into(), json!(value));
    }
    let internal = engine
        .cache
        .get("display_controls")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter(|v| v.get("internal").and_then(Value::as_bool) == Some(true))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if internal.len() == 1 {
        let row = internal[0];
        if let Some(rate) = row
            .pointer("/refresh/current")
            .filter(|v| v.get("Frequency").and_then(Value::as_u64).is_some())
        {
            values.insert("refresh".into(), rate.clone());
        } else if let Some(hz) = engine
            .cache
            .get("devices")
            .and_then(|v| v.pointer("/displays/value"))
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter()
                    .find(|d| d.get("name") == row.get("source_name"))
            })
            .and_then(|d| d.pointer("/current/hertz"))
            .and_then(Value::as_u64)
            .filter(|hz| *hz > 0 && *hz <= 1000)
        {
            values.insert("refresh".into(), json!(hz));
        }
    }
    values
}
fn payload(key: &str, value: &Value) -> Option<(&'static str, Value)> {
    let boolean = value.as_bool();
    Some(match key {
        "adapter" => {
            let (kind, text) = match value.as_str()? {
                "Connected" => ("ACAdapterConnected", "电源适配器已连接"),
                "ConnectedLowWattage" => ("ACAdapterConnectedLowWattage", "已连接低功率适配器"),
                "Disconnected" => ("ACAdapterDisconnected", "电源适配器已断开"),
                _ => return None,
            };
            ("ACAdapter", message(kind, text))
        }
        "power" => {
            let (kind, text) = match value.as_u64()? {
                1 => ("PowerModeQuiet", "安静模式"),
                2 => ("PowerModeBalance", "均衡模式"),
                3 => ("PowerModePerformance", "野兽模式"),
                224 => ("PowerModeExtreme", "超能模式"),
                255 => ("PowerModeGodMode", "自定义模式"),
                _ => return None,
            };
            ("PowerMode", message(kind, text))
        }
        "hybrid" => {
            let mode =
                jiks_platform::hybrid::HybridMode::from_raw(value.as_u64()?.try_into().ok()?)
                    .ok()?;
            (
                "AutomationNotification",
                json!({"type":"AutomationNotification","title":"显卡模式设置","text":mode.label()}),
            )
        }
        "win" => (
            "AutomationNotification",
            message(
                "AutomationNotification",
                if boolean? {
                    "Win键已锁定"
                } else {
                    "Win键已启用"
                },
            ),
        ),
        "touchpad" => (
            "TouchpadLock",
            message(
                if boolean? {
                    "TouchpadOff"
                } else {
                    "TouchpadOn"
                },
                if boolean? {
                    "触摸板已禁用"
                } else {
                    "触摸板已启用"
                },
            ),
        ),
        "mic" => (
            "Microphone",
            message(
                if boolean? {
                    "MicrophoneOff"
                } else {
                    "MicrophoneOn"
                },
                if boolean? {
                    "麦克风已静音"
                } else {
                    "麦克风已取消静音"
                },
            ),
        ),
        "fn" => (
            "FnLock",
            keys_actions::notification(if boolean? { 2 } else { 3 })?,
        ),
        "airplane" => (
            "AirplaneMode",
            message(
                if boolean? {
                    "AirplaneModeOn"
                } else {
                    "AirplaneModeOff"
                },
                if boolean? {
                    "飞行模式已开启"
                } else {
                    "飞行模式已关闭"
                },
            ),
        ),
        "panel" => (
            "KeyboardBacklight",
            message(
                if boolean? {
                    "PanelLogoLightingOn"
                } else {
                    "PanelLogoLightingOff"
                },
                if boolean? {
                    "屏幕标志灯已开启"
                } else {
                    "屏幕标志灯已关闭"
                },
            ),
        ),
        "ports" => (
            "KeyboardBacklight",
            message(
                if boolean? {
                    "PortLightingOn"
                } else {
                    "PortLightingOff"
                },
                if boolean? {
                    "接口灯已开启"
                } else {
                    "接口灯已关闭"
                },
            ),
        ),
        "white" => {
            let level = value.as_u64()?;
            (
                "KeyboardBacklight",
                if level <= 2 {
                    keys_actions::notification(64 + level as u32)?
                } else {
                    message(
                        "WhiteKeyboardBacklightChanged",
                        format!("键盘背光档位 {level}"),
                    )
                },
            )
        }
        "single_white" => (
            "KeyboardBacklight",
            message(
                if boolean? {
                    "WhiteKeyboardBacklightChanged"
                } else {
                    "WhiteKeyboardBacklightOff"
                },
                if boolean? {
                    "单级白色键盘背光已开启"
                } else {
                    "单级白色键盘背光已关闭"
                },
            ),
        ),
        "profile" => {
            let profile = value.as_u64()?;
            (
                "KeyboardBacklight",
                if profile == 0 {
                    keys_actions::notification(24)?
                } else if profile <= 6 {
                    keys_actions::notification(31 + profile as u32)?
                } else {
                    return None;
                },
            )
        }
        "spectrum_brightness" => {
            let value = value.as_u64()?;
            (
                "KeyboardBacklight",
                message(
                    if value == 0 {
                        "SpectrumBacklightOff"
                    } else {
                        "SpectrumBacklightChanged"
                    },
                    if value == 0 {
                        "键盘背光已关闭".into()
                    } else {
                        format!("键盘背光亮度 {value}")
                    },
                ),
            )
        }
        "refresh" => {
            let text = if let Some(hz) = value.as_u64() {
                format!("内屏当前报告刷新率：{hz} Hz")
            } else {
                let hz = value.get("Frequency")?.as_u64()?;
                if value.get("IsDynamic").and_then(Value::as_bool) == Some(true) {
                    format!(
                        "内屏刷新率：{}–{hz} Hz（动态）",
                        value.get("BaseFrequency")?.as_u64()?
                    )
                } else {
                    format!("内屏刷新率：{hz} Hz")
                }
            };
            ("RefreshRate", message("RefreshRate", text))
        }
        _ => return None,
    })
}
impl NotificationHost {
    fn emit(engine: &mut Engine, payload: Value) {
        if !engine.notification_queued(&payload) {
            if let Err(error) = engine.queue_notification(payload) {
                engine.value(
                    "notifications.source_error",
                    json!(format!("通知未入队：{error:#}")),
                );
            }
        }
    }
    fn locks(&mut self, engine: &mut Engine, caps: bool, num: bool) {
        for (category_name, on, last) in [
            ("CapsLock", caps, &mut self.caps),
            ("NumLock", num, &mut self.num),
        ] {
            let changed = last.replace(on).is_some_and(|before| before != on);
            if changed
                && self.active
                && self.warm
                && !engine.read_only
                && engine.shutdown.is_none()
                && enabled(engine)
                && category(engine, category_name)
            {
                Self::emit(
                    engine,
                    message(
                        &format!("{category_name}{}", if on { "On" } else { "Off" }),
                        format!(
                            "{}{}",
                            if category_name == "CapsLock" {
                                "大写锁定"
                            } else {
                                "数字锁定"
                            },
                            if on { "已开启" } else { "已关闭" }
                        ),
                    ),
                );
            }
        }
    }
    fn poll(&mut self, engine: &mut Engine) -> Result<()> {
        let permitted =
            !engine.read_only && !engine.faulted && engine.shutdown.is_none() && enabled(engine);
        if let Some(ids) = self.priming.take() {
            let mut waiting = Vec::new();
            for id in ids {
                if engine.take_internal_result(id).is_some() {
                    engine.read_receipts.remove(&id);
                } else {
                    waiting.push(id);
                }
            }
            if !waiting.is_empty() {
                self.priming = Some(waiting);
                return Ok(());
            }
            self.last = current(engine);
            self.warm = true;
        }
        if !permitted {
            self.active = false;
            self.warm = false;
            self.last.clear();
            return Ok(());
        }
        if engine.initializing {
            return Ok(());
        }
        if !self.active {
            self.active = true;
            self.warm = false;
            let mut ids = Vec::new();
            for (key, action) in [
                ("system", A::ReadSystem),
                ("power_modes", A::ReadPowerModes),
                ("energy", A::ReadEnergy),
                ("devices", A::ReadDevices),
                ("hybrid", A::ReadHybrid),
                ("lighting.0", A::ReadLighting { lighting_id: 0 }),
                ("lighting.3", A::ReadLighting { lighting_id: 3 }),
                ("lighting.5", A::ReadLighting { lighting_id: 5 }),
            ] {
                let id = engine.new_internal_request()?;
                if let Err(error) = engine.request_read(id, key.into(), action) {
                    engine.finish(id, false, format!("{error:#}"));
                }
                ids.push(id);
            }
            self.priming = Some(ids);
            return Ok(());
        }
        if !self.warm {
            return Ok(());
        }
        for (key, value) in changed(&mut self.last, current(engine)) {
            if let Some((category_name, payload)) = payload(&key, &value) {
                if category(engine, category_name) {
                    Self::emit(engine, payload);
                }
            }
        }
        Ok(())
    }
}
impl Engine {
    pub(super) fn reconcile_notifications(&mut self) -> Result<()> {
        let mut host = std::mem::take(&mut self.notifications);
        let result = host.poll(self);
        self.notifications = host;
        result
    }
    pub(super) fn notification_lock_state(&mut self, caps: bool, num: bool) {
        let mut host = std::mem::take(&mut self.notifications);
        host.locks(self, caps, num);
        self.notifications = host;
    }
    pub(super) fn notification_source_started(&mut self) {
        self.notifications.caps = None;
        self.notifications.num = None;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_known_is_baseline_and_missing_getters_never_become_false() {
        let mut last = BTreeMap::new();
        assert!(changed(&mut last, [("mic".into(), json!(true))].into()).is_empty());
        assert!(changed(&mut last, BTreeMap::new()).is_empty());
        assert!(changed(&mut last, [("mic".into(), json!(true))].into()).is_empty());
        assert_eq!(
            changed(&mut last, [("mic".into(), json!(false))].into()).len(),
            1
        );
    }
    #[test]
    fn read_only_and_master_off_never_request_readers_or_ui_effects() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        let mut host = NotificationHost::default();
        host.poll(&mut f.engine).unwrap();
        assert!(f.engine.read_queue.is_empty());
        assert!(!f.engine.ui_effects.is_busy());
        f.engine.read_only = true;
        host.locks(&mut f.engine, true, true);
        assert!(!f.engine.ui_effects.is_busy());
        assert!(f.log.lock().unwrap().is_empty());
    }
    #[test]
    fn lock_first_sample_is_silent_then_transitions_follow_categories() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        f.engine.repository.save("notifications",SettingsDocument::parse(r#"{"DontShowNotifications":false,"Notifications":{"CapsLock":true,"NumLock":false}}"#).unwrap()).unwrap();
        let mut host = NotificationHost {
            active: true,
            warm: true,
            ..Default::default()
        };
        host.locks(&mut f.engine, false, false);
        assert!(!f.engine.ui_effects.is_busy());
        host.locks(&mut f.engine, true, true);
        assert!(f.engine.ui_effects.is_busy());
        f.engine.poll_ui_effects();
        assert_eq!(
            f.engine.state.lock().unwrap().values["native.ui_effect"]["payload"]["type"],
            "CapsLockOn"
        );
        assert!(f.log.lock().unwrap().is_empty());
    }
    #[test]
    fn actual_cache_delta_maps_existing_types_and_does_not_notify_initial_state() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        f.engine
            .repository
            .save(
                "notifications",
                SettingsDocument::parse(r#"{"DontShowNotifications":false}"#).unwrap(),
            )
            .unwrap();
        let mut host = NotificationHost {
            active: true,
            warm: true,
            ..Default::default()
        };
        f.engine
            .cache
            .insert("power_modes".into(), json!({"current_raw":3}));
        host.poll(&mut f.engine).unwrap();
        assert!(!f.engine.ui_effects.is_busy());
        f.engine.cache.get_mut("power_modes").unwrap()["current_raw"] = json!(224);
        host.poll(&mut f.engine).unwrap();
        f.engine.poll_ui_effects();
        assert_eq!(
            f.engine.state.lock().unwrap().values["native.ui_effect"]["payload"]["type"],
            "PowerModeExtreme"
        );
        host.poll(&mut f.engine).unwrap();
        assert!(f.log.lock().unwrap().is_empty());
    }
}
