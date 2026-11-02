//! Automatic Windows policies consume completed reads and use the same single
//! writer/actual receipts as user actions. There is no I/O in the pure resolver.
use super::*;
use jiks_platform::system_controls::{
    PowerOverlay, PowerPolicySnapshot, SystemAction, WindowsPowerMode as Mode, WindowsPowerPolicy,
};
const BALANCED: &str = "381b4222-f694-41f0-9685-ff5bb260df2e";
#[derive(Default)]
pub(super) struct PolicyState {
    mapping_attempt: Option<String>,
    brightness_attempt: Option<String>,
    pending: Option<PendingPolicy>,
    brightness_debounce: Option<(String, u64, u32)>,
    mapping_status: String,
    brightness_status: String,
    mapping_ready: bool,
    brightness_ready: bool,
    mapping_failed: bool,
    brightness_failed: bool,
}
#[derive(Clone, Copy)]
enum Kind {
    Mapping,
    Brightness,
}
struct PendingPolicy {
    id: u64,
    kind: Kind,
}
impl PolicyState {
    pub(super) fn allows_thermal_autostart(&self) -> bool {
        self.mapping_ready
            && self.brightness_ready
            && self.pending.is_none()
            && !self.mapping_failed
            && !self.brightness_failed
    }
    fn status(&mut self, engine: &mut Engine, kind: Kind, text: impl Into<String>) {
        let text = text.into();
        let prior = match kind {
            Kind::Mapping => &mut self.mapping_status,
            Kind::Brightness => &mut self.brightness_status,
        };
        if *prior != text {
            engine.value(
                match kind {
                    Kind::Mapping => "power.mapping.status",
                    Kind::Brightness => "display.brightness_sync.status",
                },
                json!(text),
            );
            *prior = text;
        }
    }
    fn poll(&mut self, engine: &mut Engine) -> Result<()> {
        if let Some(pending) = self.pending.take() {
            if let Some(result) = engine.take_internal_result(pending.id) {
                match result {
                    Ok(()) => {
                        match pending.kind {
                            Kind::Mapping => self.mapping_ready = true,
                            Kind::Brightness => self.brightness_ready = true,
                        };
                        self.status(
                            engine,
                            pending.kind,
                            "策略操作已实际返回并核对；节电状态以当前读取为准",
                        );
                    }
                    Err(error) => {
                        match pending.kind {
                            Kind::Mapping => self.mapping_failed = true,
                            Kind::Brightness => self.brightness_failed = true,
                        };
                        self.status(
                            engine,
                            pending.kind,
                            format!("策略未完成，同一输入已锁存，不自动热重试：{error}"),
                        );
                    }
                }
            } else {
                self.pending = Some(pending);
                return Ok(());
            }
        }
        if engine.read_only
            || engine.shutdown.is_some()
            || engine.faulted
            || engine.initializing
            || engine.pending.is_some()
        {
            return Ok(());
        }
        let app = engine.repository.document("application")?.root();
        let enabled = match mapping_kind(app) {
            Ok(value) => value != 0,
            Err(error) => {
                self.mapping_failed = true;
                self.mapping_ready = false;
                self.status(
                    engine,
                    Kind::Mapping,
                    format!("未知电源映射设置保持惰性，未执行：{error:#}"),
                );
                return Ok(());
            }
        };
        let sync = app
            .get_ci("SynchronizeBrightnessToAllPowerPlans")
            .and_then(JsonNode::as_bool)
            == Some(true);
        if !enabled {
            self.mapping_ready = true;
            self.mapping_attempt = None;
            self.mapping_failed = false;
            self.status(engine, Kind::Mapping, "Windows电源映射已关闭");
        }
        if !sync {
            self.brightness_ready = true;
            self.brightness_attempt = None;
            self.brightness_debounce = None;
            self.brightness_failed = false;
            self.status(engine, Kind::Brightness, "跨方案亮度同步已关闭");
        }
        if !enabled && !sync {
            return Ok(());
        }
        let mapping_held = engine.controller.is_some() || engine.shadow.is_some();
        if enabled && mapping_held {
            self.status(
                engine,
                Kind::Mapping,
                "温控会话运行期间暂停自动电源方案映射；不切换方案或overlay",
            );
            if !sync {
                return Ok(());
            }
        }
        let snapshot = engine
            .cache
            .get("system")
            .and_then(|v| v.get("power_policy"))
            .filter(|v| v.get("available").and_then(Value::as_bool) == Some(true))
            .and_then(|v| v.get("value"))
            .map(|v| serde_json::from_value::<PowerPolicySnapshot>(v.clone()))
            .transpose()?;
        let Some(snapshot) = snapshot else {
            self.status(
                engine,
                Kind::Mapping,
                "等待Windows电源策略实际读取，未知状态不应用",
            );
            return Ok(());
        };
        if enabled && !mapping_held {
            let raw = engine
                .cache
                .get("power_modes")
                .and_then(|v| v.get("current_raw"))
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok());
            let Some(raw) = raw else {
                self.status(engine, Kind::Mapping, "等待实际固件模式读取");
                return Ok(());
            };
            let Some(ac) = snapshot.ac_connected else {
                self.status(engine, Kind::Mapping, "AC/DC状态未知，未应用电源映射");
                return Ok(());
            };
            let app = engine.repository.document("application")?.root();
            let god = engine.repository.document("godmode")?.root();
            let vantage = engine
                .cache
                .get("software")
                .and_then(|v| v.pointer("/products/vantage/status"))
                .and_then(Value::as_str);
            let signature = source_signature(app, god, raw, ac, snapshot.battery_saver, vantage);
            if self.mapping_attempt.as_ref() != Some(&signature) {
                self.mapping_attempt = Some(signature);
                self.mapping_ready = false;
                self.mapping_failed = false;
                match resolve_mapping(app, god, raw, ac, &snapshot, vantage) {
                    Ok(Some(policy)) => {
                        let id = engine.new_internal_request()?;
                        match engine.start_work(
                            id,
                            "Windows电源映射策略",
                            vec![A::System(SystemAction::PowerPolicy(policy))],
                            After::Hardware,
                        ) {
                            Ok(()) => {}
                            Err(error) => engine.finish(id, false, format!("{error:#}")),
                        };
                        self.pending = Some(PendingPolicy {
                            id,
                            kind: Kind::Mapping,
                        });
                        self.status(engine, Kind::Mapping, "等待Windows电源映射实际读回");
                        return Ok(());
                    }
                    Ok(None) => {
                        self.mapping_ready = true;
                        self.status(
                            engine,
                            Kind::Mapping,
                            if snapshot.battery_saver == Some(true) {
                                "节电模式下保留系统overlay；已核对当前策略"
                            } else {
                                "Windows电源映射已核对，无需setter"
                            },
                        );
                    }
                    Err(error) => {
                        self.mapping_failed = true;
                        self.status(
                            engine,
                            Kind::Mapping,
                            format!("电源映射未应用，同输入锁存：{error:#}"),
                        );
                    }
                }
            }
        }
        if sync {
            let brightness = engine
                .cache
                .get("system")
                .and_then(|v| v.pointer("/brightness/value/current"))
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v <= 100);
            let Some(brightness) = brightness else {
                self.status(
                    engine,
                    Kind::Brightness,
                    "等待实际内部屏幕亮度，未知值不写电源方案",
                );
                return Ok(());
            };
            let ids = snapshot
                .plans
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>();
            let signature = format!("{brightness}/{ids:?}");
            if self.brightness_attempt.as_ref() != Some(&signature) {
                let now = monotonic_ms();
                if self
                    .brightness_debounce
                    .as_ref()
                    .is_none_or(|(prior, _, _)| prior != &signature)
                {
                    self.brightness_debounce =
                        Some((signature.clone(), now.saturating_add(2000), brightness));
                    self.brightness_ready = false;
                }
                if self
                    .brightness_debounce
                    .as_ref()
                    .is_some_and(|(_, due, _)| now >= *due)
                {
                    self.brightness_debounce = None;
                    self.brightness_attempt = Some(signature);
                    self.brightness_failed = false;
                    let id = engine.new_internal_request()?;
                    match engine.start_work(
                        id,
                        "跨电源方案亮度同步",
                        vec![A::System(
                            SystemAction::SynchronizeBrightnessToAllPowerPlans(brightness),
                        )],
                        After::Hardware,
                    ) {
                        Ok(()) => {}
                        Err(error) => engine.finish(id, false, format!("{error:#}")),
                    };
                    self.pending = Some(PendingPolicy {
                        id,
                        kind: Kind::Brightness,
                    });
                    self.status(
                        engine,
                        Kind::Brightness,
                        "等待全部电源方案AC/DC亮度实际读回",
                    );
                }
            }
        }
        Ok(())
    }
}
impl Engine {
    pub(super) fn reconcile_power_policies(&mut self) -> Result<()> {
        let mut policy = std::mem::take(&mut self.policy);
        let result = policy.poll(self);
        self.policy = policy;
        result
    }
}
fn mapping_kind(app: &JsonNode) -> Result<u8> {
    let Some(value) = app.get_ci("PowerModeMappingMode") else {
        return Ok(0);
    };
    let mode = value
        .as_i64()
        .or_else(|| {
            value.as_str().and_then(|v| match v {
                "Disabled" => Some(0),
                "WindowsPowerMode" => Some(1),
                "WindowsPowerPlan" => Some(2),
                _ => v.parse().ok(),
            })
        })
        .context("Invalid PowerModeMappingMode")?;
    ensure!((0..=2).contains(&mode), "Unknown PowerModeMappingMode");
    Ok(mode as u8)
}
fn mode_name(raw: u32) -> Result<(&'static str, &'static str)> {
    Ok(match raw {
        1 => ("0", "Quiet"),
        2 => ("1", "Balance"),
        3 => ("2", "Performance"),
        224 => ("223", "Extreme"),
        255 => ("254", "GodMode"),
        _ => bail!("Unknown firmware power mode"),
    })
}
fn mode_entry<'a>(app: &'a JsonNode, field: &str, raw: u32) -> Option<&'a JsonNode> {
    let (number, name) = mode_name(raw).ok()?;
    let map = app.get_ci(field)?;
    map.get_ci(name).or_else(|| map.get(number))
}
fn override_value<'a>(
    overrides: Option<&'a JsonNode>,
    key: &str,
    number: &str,
) -> Option<&'a JsonNode> {
    let overrides = overrides?;
    overrides.get_ci(key).or_else(|| overrides.get(number))
}
fn windows_mode(value: Option<&JsonNode>) -> Option<Mode> {
    let value = value?;
    match value.as_i64().or_else(|| {
        value.as_str().and_then(|s| match s {
            "BestPowerEfficiency" | "Efficiency" => Some(0),
            "Balanced" => Some(1),
            "BestPerformance" | "Performance" => Some(2),
            _ => s.parse().ok(),
        })
    })? {
        0 => Some(Mode::Efficiency),
        1 => Some(Mode::Balanced),
        2 => Some(Mode::Performance),
        _ => None,
    }
}
fn preset<'a>(god: &'a JsonNode, raw: u32) -> Option<&'a JsonNode> {
    if raw != 255 {
        return None;
    }
    let id = god.get_ci("ActivePresetId")?.as_str()?;
    god.get_ci("Presets")?.get_ci(id)
}
fn source_signature(
    app: &JsonNode,
    god: &JsonNode,
    raw: u32,
    ac: bool,
    saver: Option<bool>,
    vantage: Option<&str>,
) -> String {
    let text = |n: Option<&JsonNode>| n.map(JsonNode::to_json).unwrap_or_default();
    format!(
        "{raw}/{ac}/{saver:?}/{vantage:?}/{}/{}/{}/{}/{}/{}",
        text(app.get_ci("PowerModeMappingMode")),
        text(mode_entry(app, "PowerModes", raw)),
        text(mode_entry(app, "PowerPlans", raw)),
        text(mode_entry(app, "Overrides", raw)),
        text(god.get_ci("ActivePresetId")),
        text(preset(god, raw).and_then(|p| p.get_ci("Overrides")))
    )
}
fn guid(value: Option<&JsonNode>) -> Option<String> {
    let value = value?.as_str()?;
    Some(jiks_platform::system_controls::guid_string(
        jiks_platform::system_controls::parse_guid(value).ok()?,
    ))
}
fn mode_guid(mode: Mode) -> &'static str {
    match mode {
        Mode::Efficiency => "961cc777-2547-4f9d-8174-7d86181b8a7a",
        Mode::Balanced => "00000000-0000-0000-0000-000000000000",
        Mode::Performance => "ded574b5-45a0-4f42-8737-46345c09c238",
    }
}
fn resolve_mapping(
    app: &JsonNode,
    god: &JsonNode,
    raw: u32,
    ac: bool,
    snapshot: &PowerPolicySnapshot,
    vantage: Option<&str>,
) -> Result<Option<WindowsPowerPolicy>> {
    let kind = mapping_kind(app)?;
    if kind == 0 {
        return Ok(None);
    }
    mode_name(raw)?;
    let overrides = mode_entry(app, "Overrides", raw);
    let preset = preset(god, raw).and_then(|p| p.get_ci("Overrides"));
    let current = snapshot
        .plans
        .iter()
        .find(|plan| plan.active)
        .context("Active power plan unknown")?;
    let (plan, ensure_balanced_plan, overlay_only_if_balanced, ac_mode, dc_mode) = if kind == 1 {
        let default = match mode_entry(app, "PowerModes", raw) {
            Some(value) => {
                windows_mode(Some(value)).context("Unknown configured Windows power mode")?
            }
            None => Mode::Balanced,
        };
        let ac_mode = windows_mode(override_value(preset, "PowerModeOnAc", "1"))
            .or_else(|| windows_mode(override_value(overrides, "PowerModeOnAc", "1")))
            .unwrap_or(default);
        let dc_mode = windows_mode(override_value(preset, "PowerModeOnDc", "2"))
            .or_else(|| windows_mode(override_value(overrides, "PowerModeOnDc", "2")))
            .unwrap_or(default);
        if snapshot
            .battery_saver
            .context("Battery saver state unknown")?
        {
            return Ok(None);
        }
        (None, true, false, ac_mode, dc_mode)
    } else {
        let selected = guid(override_value(preset, "PowerPlan", "0"))
            .or_else(|| guid(mode_entry(app, "PowerPlans", raw)));
        let is_default = selected
            .as_deref()
            .is_none_or(|id| id == "00000000-0000-0000-0000-000000000000");
        let target = if is_default {
            BALANCED.to_owned()
        } else {
            selected.unwrap()
        };
        // PowerModeFeature calls the old controller with alwaysActivateDefaults=true.
        ensure!(
            is_default || matches!(vantage, Some("disabled" | "not_found")),
            "Vantage status does not allow a nondefault Windows power-plan mapping"
        );
        let preset_ac = windows_mode(override_value(preset, "PowerPlanBalanceOnAc", "3"));
        let preset_dc = windows_mode(override_value(preset, "PowerPlanBalanceOnDc", "4"));
        let any = preset_ac.is_some() || preset_dc.is_some();
        let (ac_mode, dc_mode) = if is_default {
            (Mode::Balanced, Mode::Balanced)
        } else if any {
            (
                preset_ac.unwrap_or(Mode::Balanced),
                preset_dc.unwrap_or(Mode::Balanced),
            )
        } else {
            (
                windows_mode(override_value(overrides, "PowerPlanBalanceOnAc", "3"))
                    .unwrap_or(Mode::Balanced),
                windows_mode(override_value(overrides, "PowerPlanBalanceOnDc", "4"))
                    .unwrap_or(Mode::Balanced),
            )
        };
        (Some(target), false, true, ac_mode, dc_mode)
    };
    let target = if ensure_balanced_plan
        && !current
            .based_on_balanced
            .context("Active plan personality unknown")?
    {
        BALANCED
    } else {
        plan.as_deref().unwrap_or(&current.id)
    };
    let target_plan = snapshot
        .plans
        .iter()
        .find(|p| p.id.eq_ignore_ascii_case(target))
        .context("Configured Windows power plan is missing")?;
    let can_overlay = !overlay_only_if_balanced
        || target_plan
            .based_on_balanced
            .context("Target plan personality unknown")?;
    let saver = if can_overlay {
        snapshot
            .battery_saver
            .context("Battery saver state unknown")?
    } else {
        false
    };
    let overlay = (!saver && can_overlay).then_some(PowerOverlay {
        ac: ac_mode,
        dc: dc_mode,
        use_ac: ac,
    });
    let same_plan = target.eq_ignore_ascii_case(&current.id);
    let same_overlay = overlay.as_ref().is_none_or(|o| {
        snapshot.overlay_ac.as_deref() == Some(mode_guid(o.ac))
            && snapshot.overlay_dc.as_deref() == Some(mode_guid(o.dc))
            && snapshot.effective_power_mode.as_deref()
                == Some(mode_guid(if o.use_ac { o.ac } else { o.dc }))
    });
    if same_plan && same_overlay {
        return Ok(None);
    }
    Ok(Some(WindowsPowerPolicy {
        expected_raw_mode: raw,
        plan,
        ensure_balanced_plan,
        overlay,
        overlay_only_if_balanced,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiks_platform::system_controls::PowerPolicyPlan;
    fn node(text: &str) -> JsonNode {
        JsonNode::parse(text).unwrap()
    }
    fn snapshot() -> PowerPolicySnapshot {
        PowerPolicySnapshot {
            plans: vec![
                PowerPolicyPlan {
                    id: BALANCED.into(),
                    name: "Balanced".into(),
                    active: true,
                    based_on_balanced: Some(true),
                },
                PowerPolicyPlan {
                    id: "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c".into(),
                    name: "Custom".into(),
                    active: false,
                    based_on_balanced: Some(true),
                },
            ],
            battery_saver: Some(false),
            ac_connected: Some(true),
            effective_power_mode: Some(mode_guid(Mode::Efficiency).into()),
            overlay_ac: Some(mode_guid(Mode::Efficiency).into()),
            overlay_dc: Some(mode_guid(Mode::Efficiency).into()),
            errors: BTreeMap::new(),
        }
    }
    #[test]
    fn disabled_mapping_never_needs_mode_or_getters() {
        let mut view = snapshot();
        view.plans.clear();
        view.battery_saver = None;
        assert!(
            resolve_mapping(&node("{}"), &node("{}"), 999, false, &view, None)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn extreme_uses_legacy_223_and_per_supply_overrides() {
        let app = node(
            r#"{"PowerModeMappingMode":1,"PowerModes":{"223":0},"Overrides":{"Extreme":{"PowerModeOnDc":"BestPerformance"}}}"#,
        );
        let result = resolve_mapping(&app, &node("{}"), 224, false, &snapshot(), None)
            .unwrap()
            .unwrap();
        assert_eq!(result.expected_raw_mode, 224);
        let overlay = result.overlay.unwrap();
        assert_eq!(mode_guid(overlay.ac), mode_guid(Mode::Efficiency));
        assert_eq!(mode_guid(overlay.dc), mode_guid(Mode::Performance));
        assert!(!overlay.use_ac);
    }
    #[test]
    fn godmode_preset_mode_override_is_per_side_and_beats_global() {
        let app = node(
            r#"{"PowerModeMappingMode":"WindowsPowerMode","PowerModes":{"GodMode":1},"Overrides":{"GodMode":{"PowerModeOnAc":"BestPowerEfficiency","PowerModeOnDc":"BestPerformance"}}}"#,
        );
        let god = node(
            r#"{"ActivePresetId":"fixture","Presets":{"fixture":{"Overrides":{"PowerModeOnAc":"BestPerformance"}}}}"#,
        );
        let overlay = resolve_mapping(&app, &god, 255, false, &snapshot(), None)
            .unwrap()
            .unwrap()
            .overlay
            .unwrap();
        assert_eq!(mode_guid(overlay.ac), mode_guid(Mode::Performance));
        assert_eq!(mode_guid(overlay.dc), mode_guid(Mode::Performance));
    }
    #[test]
    fn preset_balanced_overlay_any_side_masks_global_other_side_like_csharp() {
        let app = node(
            r#"{"PowerModeMappingMode":2,"Overrides":{"GodMode":{"PowerPlanBalanceOnDc":"BestPowerEfficiency"}}}"#,
        );
        let god = node(
            r#"{"ActivePresetId":"fixture","Presets":{"fixture":{"Overrides":{"PowerPlan":"8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c","PowerPlanBalanceOnAc":"BestPerformance"}}}}"#,
        );
        let result = resolve_mapping(&app, &god, 255, true, &snapshot(), Some("disabled"))
            .unwrap()
            .unwrap();
        assert_eq!(
            result.plan.as_deref(),
            Some("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c")
        );
        let overlay = result.overlay.unwrap();
        assert_eq!(mode_guid(overlay.ac), mode_guid(Mode::Performance));
        assert_eq!(mode_guid(overlay.dc), mode_guid(Mode::Balanced));
    }
    #[test]
    fn default_balanced_fallback_ignores_overlays_and_honors_always_activate_defaults() {
        let app = node(
            r#"{"PowerModeMappingMode":2,"Overrides":{"Quiet":{"PowerPlanBalanceOnAc":"BestPerformance"}}}"#,
        );
        let overlay = resolve_mapping(&app, &node("{}"), 1, true, &snapshot(), Some("enabled"))
            .unwrap()
            .unwrap()
            .overlay
            .unwrap();
        assert_eq!(mode_guid(overlay.ac), mode_guid(Mode::Balanced));
        assert_eq!(mode_guid(overlay.dc), mode_guid(Mode::Balanced));
    }
    #[test]
    fn battery_saver_unknown_and_missing_plan_do_not_create_a_partial_plan() {
        let app = node(r#"{"PowerModeMappingMode":1}"#);
        let mut view = snapshot();
        view.battery_saver = None;
        assert!(resolve_mapping(&app, &node("{}"), 1, true, &view, None).is_err());
        view.battery_saver = Some(true);
        assert!(
            resolve_mapping(&app, &node("{}"), 1, true, &view, None)
                .unwrap()
                .is_none()
        );
        let app = node(
            r#"{"PowerModeMappingMode":2,"PowerPlans":{"Quiet":"99999999-9999-9999-9999-999999999999"}}"#,
        );
        assert!(resolve_mapping(&app, &node("{}"), 1, true, &view, Some("disabled")).is_err());
    }
    #[test]
    fn actual_policy_failure_latches_until_policy_input_changes_not_poll_count() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        f.engine
            .repository
            .save(
                "application",
                SettingsDocument::parse(r#"{"PowerModeMappingMode":1,"PowerModes":{"Quiet":2}}"#)
                    .unwrap(),
            )
            .unwrap();
        f.engine.cache.insert(
            "system".into(),
            json!({"power_policy":{"available":true,"value":snapshot()}}),
        );
        f.engine
            .cache
            .insert("power_modes".into(), json!({"current_raw":1}));
        let answer = f.answer();
        f.engine.reconcile_power_policies().unwrap();
        assert!(f.engine.pending.is_some());
        assert!(!f.engine.policy.allows_thermal_autostart());
        f.engine.reconcile_power_policies().unwrap();
        assert_eq!(f.log.lock().unwrap().len(), 1);
        *answer.lock().unwrap() = Some(Err("synthetic policy failure".into()));
        f.engine.poll_write().unwrap();
        f.engine.reconcile_power_policies().unwrap();
        f.engine.reconcile_power_policies().unwrap();
        assert_eq!(f.log.lock().unwrap().len(), 1);
        assert!(!f.engine.policy.allows_thermal_autostart());
    }
}
