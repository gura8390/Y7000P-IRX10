//! Reduce only completed, independently verified native results into the read
//! cache before planning the next command. This module performs no I/O. A failed
//! decode commits nothing; request payload values never stand in for readback.
use anyhow::{Context, Result, ensure};
use jiks_platform::{
    HardwareAction as A, SpectrumAction,
    display_controls::DisplayAction as D,
    domain::MappedFeature as M,
    system_controls::{GameZoneSwitch, SystemAction as S},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
fn object<'a>(cache: &'a mut BTreeMap<String, Value>, key: &str) -> &'a mut Value {
    let value = cache.entry(key.into()).or_insert_with(|| json!({}));
    if !value.is_object() {
        *value = json!({});
    }
    value
}
fn put(cache: &mut BTreeMap<String, Value>, key: &str, value: Value) {
    cache.insert(key.into(), value);
    cache.remove(&format!("error.{key}"));
}
fn captured(cache: &mut BTreeMap<String, Value>, group: &str, key: &str, value: Value) {
    object(cache, group)[key] = json!({"available":true,"value":value});
    cache.remove(&format!("error.{group}"));
}
fn uint(value: &Value, name: &str) -> Result<u32> {
    value
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .with_context(|| format!("Verified result has no uint32 {name}"))
}
fn mode(cache: &mut BTreeMap<String, Value>, raw: u32, invalidate: bool) {
    object(cache, "cpu")["raw_mode"] = json!(raw);
    object(cache, "power_modes")["current_raw"] = json!(raw);
    object(cache, "godmode")["raw_mode"] = json!(raw);
    if invalidate {
        if let Some(cpu) = cache.get_mut("cpu").and_then(Value::as_object_mut) {
            for key in ["limits", "power", "temperature_limit"] {
                cpu.remove(key);
            }
        }
        if let Some(fields) = cache
            .get_mut("godmode")
            .and_then(|v| v.get_mut("fields"))
            .and_then(Value::as_array_mut)
        {
            for field in fields {
                if let Some(field) = field.as_object_mut() {
                    field.remove("current");
                }
            }
        }
        cache.remove("fan_table");
    }
}
fn cpu(cache: &mut BTreeMap<String, Value>, value: &Value) -> Result<()> {
    let snapshot: jiks_platform::CpuSnapshot = serde_json::from_value(value.clone())?;
    ensure!(
        snapshot.power.valid(),
        "Verified CPU snapshot has invalid envelope"
    );
    mode(cache, snapshot.raw_mode, false);
    object(cache, "cpu")["limits"] = json!({"long_watts":snapshot.power.long_watts,"short_watts":snapshot.power.short_watts,"temperature_limit":snapshot.temperature_limit});
    cache.remove("error.cpu");
    Ok(())
}
fn mapped_key(feature: M) -> &'static str {
    match feature {
        M::Overdrive => "mapped.overdrive",
        M::FlipToStart => "mapped.flip_to_start",
        M::IgpuMode => "mapped.igpu",
        M::InstantBootAc => "mapped.instant_ac",
        M::InstantBootUsb => "mapped.instant_usb",
        M::FanFullSpeed => "mapped.fan_full_speed",
        M::GodModeFnQ => "mapped.fnq",
        M::PowerLedSync => "mapped.power_led",
    }
}
fn display(
    cache: &mut BTreeMap<String, Value>,
    path: &str,
    part: &str,
    result: &Value,
) -> Result<()> {
    let rows = cache
        .get_mut("display_controls")
        .and_then(Value::as_array_mut)
        .context("Verified display write has no completed monitor-address inventory")?;
    let rows = rows
        .iter_mut()
        .filter(|v| {
            v.get("monitor_path")
                .and_then(Value::as_str)
                .is_some_and(|v| v.eq_ignore_ascii_case(path))
        })
        .collect::<Vec<_>>();
    ensure!(
        rows.len() == 1,
        "Verified display target is absent/ambiguous in cache"
    );
    let row = rows.into_iter().next().unwrap();
    row[part] = result.clone();
    Ok(())
}
pub fn absorb(
    cache: &mut BTreeMap<String, Value>,
    action: &A,
    verified_result: &Value,
) -> Result<()> {
    let mut next = cache.clone();
    apply(&mut next, action, verified_result)?;
    *cache = next;
    Ok(())
}
fn apply(cache: &mut BTreeMap<String, Value>, action: &A, result: &Value) -> Result<()> {
    match action {
        A::SetSoftware { .. } => {
            ensure!(
                result.get("success").and_then(Value::as_bool) == Some(true),
                "Software result reports partial failure"
            );
            let snapshot: jiks_platform::software::SoftwareSnapshot = serde_json::from_value(
                result
                    .get("snapshot")
                    .context("Software snapshot absent")?
                    .clone(),
            )?;
            put(cache, "software", serde_json::to_value(snapshot)?);
        }
        A::SetRawMode(_) => {
            let raw = uint(result, "raw mode")?;
            ensure!(
                [1, 2, 3, 224, 255].contains(&raw),
                "Unknown completed firmware mode"
            );
            mode(cache, raw, true);
            cache.remove("error.power_modes");
            cache.remove("error.cpu");
        }
        A::SetMapped { feature, .. } => {
            let value = uint(result, "mapped value")?;
            feature.validate(value)?;
            put(
                cache,
                mapped_key(*feature),
                json!({"feature":feature,"id":feature.id(),"value":value}),
            );
            if matches!(feature, M::IgpuMode) {
                cache.remove("hybrid");
            }
        }
        A::Energy(_) => {
            ensure!(
                result.is_object()
                    && result
                        .get("battery_mode")
                        .and_then(Value::as_str)
                        .is_some_and(|v| ["Normal", "Rapid", "Conservation"].contains(&v))
                    && result.get("fn_lock").and_then(Value::as_bool).is_some(),
                "Malformed verified EnergyDrv status"
            );
            put(cache, "energy", result.clone());
        }
        A::EnableCpu { .. } | A::SetCpuPower { .. } => cpu(cache, result)?,
        A::StopCpu => {
            if !result.is_null() {
                cpu(cache, result)?;
            }
        }
        A::ApplyGodMode(_) => {
            ensure!(
                result.get("complete").and_then(Value::as_bool) == Some(true)
                    && result.get("error").is_none_or(Value::is_null),
                "Incomplete GodMode operation is not a verified completion"
            );
            let observed = result
                .get("observed")
                .and_then(Value::as_object)
                .context("GodMode completion lacks observed fields")?;
            let values = observed
                .iter()
                .map(|(field, value)| {
                    Ok((
                        serde_json::from_value::<jiks_platform::god_mode::GodModeField>(json!(
                            field
                        ))?,
                        value
                            .as_i64()
                            .and_then(|v| i32::try_from(v).ok())
                            .context("Bad observed GodMode integer")?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            for (field, value) in values {
                if let Some(fields) = cache
                    .get_mut("godmode")
                    .and_then(|v| v.get_mut("fields"))
                    .and_then(Value::as_array_mut)
                {
                    for row in fields {
                        if row.get("key").and_then(Value::as_str) == Some(field.semantic_key()) {
                            row["current"] = json!({"available":true,"value":value});
                        }
                    }
                }
                let key = match field {
                    jiks_platform::god_mode::GodModeField::CpuLongPower => Some("long_watts"),
                    jiks_platform::god_mode::GodModeField::CpuShortPower => Some("short_watts"),
                    jiks_platform::god_mode::GodModeField::CpuTemperature => {
                        Some("temperature_limit")
                    }
                    _ => None,
                };
                if let Some(key) = key {
                    let cpu = object(cache, "cpu");
                    if !cpu.get("limits").is_some_and(Value::is_object) {
                        cpu["limits"] = json!({});
                    }
                    cpu["limits"][key] = json!(value);
                }
            }
            mode(cache, 255, false);
        }
        A::SetFanTable(_) => {
            ensure!(
                result
                    .get("verified_reads")
                    .and_then(Value::as_u64)
                    .is_some_and(|v| v >= 2)
                    && result
                        .get("levels")
                        .and_then(Value::as_array)
                        .is_some_and(|v| v.len() == 10),
                "Fan completion lacks repeated readback"
            );
            put(cache, "fan_table", result["levels"].clone());
        }
        A::SetLighting { lighting_id, .. } => {
            ensure!(
                result.as_object().is_some_and(|v| v
                    .keys()
                    .any(|k| k.eq_ignore_ascii_case("Current_Brightness_Level"))),
                "Lighting completion lacks physical readback"
            );
            put(cache, &format!("lighting.{lighting_id}"), result.clone());
        }
        A::System(request) => match request {
            S::Switch { which, .. } => {
                let v = uint(
                    result
                        .get("value")
                        .context("System switch readback missing")?,
                    "switch",
                )?;
                ensure!(v <= 1, "Invalid verified system switch");
                captured(
                    cache,
                    "system",
                    match which {
                        GameZoneSwitch::WinKey => "win_key",
                        GameZoneSwitch::TouchpadLock => "touchpad_lock",
                    },
                    result.clone(),
                );
            }
            S::Brightness(_) => {
                ensure!(
                    uint(
                        result
                            .get("current")
                            .context("Brightness current missing")?,
                        "brightness"
                    )? <= 100
                        && result.get("levels").is_some_and(Value::is_array),
                    "Invalid brightness readback"
                );
                captured(cache, "system", "brightness", result.clone());
            }
            S::PowerPlan(_) => {
                ensure!(
                    result.as_array().is_some_and(|v| v
                        .iter()
                        .filter(|p| p.get("active").and_then(Value::as_bool) == Some(true))
                        .count()
                        == 1),
                    "Power-plan completion lacks exactly one active scheme"
                );
                captured(cache, "system", "power_plans", result.clone());
            }
            S::PowerMode(_) => {
                ensure!(
                    result.as_str().is_some(),
                    "Effective Windows power mode readback missing"
                );
                captured(cache, "system", "effective_power_mode", result.clone());
            }
            S::PowerPolicy(_) => {
                ensure!(result.get("verified").and_then(Value::as_bool) == Some(true), "Windows power policy readback was not verified");
                let snapshot = result.get("snapshot").context("Power policy snapshot missing")?;
                let typed: jiks_platform::system_controls::PowerPolicySnapshot = serde_json::from_value(snapshot.clone())?;
                captured(cache, "system", "power_policy", snapshot.clone());
                captured(cache, "system", "power_plans", serde_json::to_value(&typed.plans)?);
                if let Some(mode) = typed.effective_power_mode { captured(cache, "system", "effective_power_mode", json!(mode)); }
            }
            S::SynchronizeBrightnessToAllPowerPlans(_) => {
                ensure!(result.get("complete").and_then(Value::as_bool) == Some(true), "Brightness synchronization did not complete");
                ensure!(result.get("verified").and_then(Value::as_array).is_some_and(|rows| !rows.is_empty()), "Brightness synchronization lacks scheme readbacks");
                put(cache, "brightness.sync", result.clone());
            }
            S::ScreenOff => {} // Delivery success is not a physical panel state.
        },
        A::Display(request) => match request {
            D::RefreshRate { monitor_path, .. } => {
                ensure!(
                    result.get("ccd_readback_verified").and_then(Value::as_bool) == Some(true),
                    "DRR CCD readback unverified"
                );
                let rate: jiks_platform::dynamic_refresh::RefreshRate = serde_json::from_value(
                    result
                        .get("refresh")
                        .context("DRR actual rate missing")?
                        .clone(),
                )?;
                let rows = cache
                    .get_mut("display_controls")
                    .and_then(Value::as_array_mut)
                    .context("DRR monitor inventory missing")?;
                let rows = rows
                    .iter_mut()
                    .filter(|v| {
                        v.get("monitor_path")
                            .and_then(Value::as_str)
                            .is_some_and(|v| v.eq_ignore_ascii_case(monitor_path))
                    })
                    .collect::<Vec<_>>();
                ensure!(rows.len() == 1, "DRR target absent/ambiguous");
                let row = rows.into_iter().next().unwrap();
                row["refresh"]["current"] = serde_json::to_value(rate)?;
                let source = row
                    .get("source_name")
                    .and_then(Value::as_str)
                    .context("DRR target source missing")?
                    .to_string();
                let w = uint(
                    result.get("width").context("DRR actual width missing")?,
                    "width",
                )?;
                let h = uint(
                    result.get("height").context("DRR actual height missing")?,
                    "height",
                )?;
                let devices = cache
                    .get_mut("devices")
                    .and_then(|v| v.pointer_mut("/displays/value"))
                    .and_then(Value::as_array_mut)
                    .context("DRR display timing cache missing")?;
                let matching = devices
                    .iter_mut()
                    .filter(|d| d.get("name").and_then(Value::as_str) == Some(source.as_str()))
                    .collect::<Vec<_>>();
                ensure!(matching.len() == 1, "DRR GDI identity absent/ambiguous");
                matching.into_iter().next().unwrap()["current"] = json!({"width":w,"height":h,"hertz":if rate.is_dynamic{rate.base_frequency}else{rate.frequency}});
            }
            D::Hdr { monitor_path, .. } | D::Acm { monitor_path, .. } => {
                let _: jiks_platform::display_controls::ColorStatus =
                    serde_json::from_value(result.clone())?;
                display(cache, monitor_path, "color", result)?;
            }
            D::Dpi { monitor_path, .. } => {
                let _: jiks_platform::display_controls::DpiStatus =
                    serde_json::from_value(result.clone())?;
                display(cache, monitor_path, "dpi", result)?;
            }
        },
        A::SetDisplayMode { device, .. } => {
            for key in ["width", "height", "hertz"] {
                ensure!(
                    uint(
                        result.get(key).context("Display timing readback missing")?,
                        key
                    )? > 0,
                    "Invalid observed display timing"
                );
            }
            let rows = cache
                .get_mut("devices")
                .and_then(|v| v.pointer_mut("/displays/value"))
                .and_then(Value::as_array_mut)
                .context("Display timing target cache missing")?;
            let rows = rows
                .iter_mut()
                .filter(|v| {
                    v.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|v| v.eq_ignore_ascii_case(device))
                })
                .collect::<Vec<_>>();
            ensure!(
                rows.len() == 1,
                "Display timing target cache absent/ambiguous"
            );
            rows.into_iter().next().unwrap()["current"] = result.clone();
            if let Some(controls) = cache
                .get_mut("display_controls")
                .and_then(Value::as_array_mut)
            {
                for control in controls {
                    if control.get("source_name").and_then(Value::as_str) == Some(device.as_str()) {
                        if let Some(refresh) =
                            control.get_mut("refresh").and_then(Value::as_object_mut)
                        {
                            refresh.remove("current");
                        }
                    }
                }
            }
        }
        A::SetSpeakerMute(_) | A::SetMicrophoneMute(_) | A::SetSpeakerVolume(_) => {
            let capture = result
                .get("capture")
                .and_then(Value::as_bool)
                .context("Audio readback group missing")?;
            let endpoints = result
                .get("endpoints")
                .and_then(Value::as_array)
                .context("Audio endpoint readback missing")?;
            ensure!(
                !endpoints.is_empty()
                    && endpoints
                        .iter()
                        .all(
                            |v| v.get("capture").and_then(Value::as_bool) == Some(capture)
                                && v.get("muted").and_then(Value::as_bool).is_some()
                                && v.get("volume_percent")
                                    .and_then(Value::as_f64)
                                    .is_some_and(|v| (0.0..=100.1).contains(&v))
                        ),
                "Malformed audio readback"
            );
            let mut values = cache
                .get("devices")
                .and_then(|v| v.pointer("/audio/value"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            values.retain(|v| v.get("capture").and_then(Value::as_bool) != Some(capture));
            values.extend(endpoints.iter().cloned());
            captured(cache, "devices", "audio", json!(values));
        }
        A::Spectrum(operation) => match operation {
            SpectrumAction::ReadActiveProfile { .. } => {
                let profile = result
                    .get("profile")
                    .and_then(Value::as_u64)
                    .context("Active Spectrum profile absent")?;
                ensure!(profile <= 6, "Active Spectrum profile out of range");
                if cache
                    .get("keyboard.effects")
                    .and_then(|v| v.get("profile"))
                    .and_then(Value::as_u64)
                    != Some(profile)
                {
                    cache.remove("keyboard.effects");
                }
                put(cache, "keyboard.active_profile", json!({"profile":profile}));
            }
            SpectrumAction::Brightness { .. } => {
                let value = uint(result, "Spectrum brightness")?;
                ensure!(value <= 9, "Spectrum brightness out of range");
                put(cache, "keyboard.brightness", json!(value));
            }
            _ => {
                ensure!(
                    result
                        .get("profile")
                        .and_then(Value::as_u64)
                        .is_some_and(|v| v <= 6)
                        && result.get("effects").is_some_and(Value::is_array),
                    "Spectrum completion lacks verified profile"
                );
                put(cache, "keyboard.effects", result.clone());
                put(
                    cache,
                    "keyboard.active_profile",
                    json!({"profile":result["profile"]}),
                );
            }
        },
        A::SetGpuOverclock { .. } => {
            ensure!(
                result.get("CoreDeltaMhz").is_some() && result.get("MemoryDeltaMhz").is_some(),
                "GPU OC completion lacks independently verified offsets"
            );
            let _: jiks_platform::gpu::OverclockInfo = serde_json::from_value(result.clone())?;
            object(cache, "gpu.oc")["overclock"] = result.clone();
        }
        A::SetHybrid { .. } => {
            ensure!(
                result
                    .get("configuration_verified")
                    .and_then(Value::as_bool)
                    == Some(true),
                "Hybrid completion lacks configuration verification"
            );
            let snapshot: jiks_platform::hybrid::HybridSnapshot = serde_json::from_value(
                result
                    .get("observed")
                    .context("Hybrid observed snapshot missing")?
                    .clone(),
            )?;
            put(
                cache,
                "hybrid",
                json!({"snapshot":snapshot,"selected":snapshot.selected().map(|v|v as u32),"modes":snapshot.modes().into_iter().map(|v|json!({"raw":v as u32,"name":v.label()})).collect::<Vec<_>>(),"pending_reboot":result.get("pending_reboot"),"physical_topology_verified":false}),
            );
        }
        A::SetAiSubmode { .. } => {
            let _: jiks_platform::ai::AiSnapshot = serde_json::from_value(result.clone())?;
            put(cache, "ai", result.clone());
        }
        A::SetGpuKeepAwake { .. } => {
            let active = result
                .get("keep_awake_active")
                .and_then(Value::as_bool)
                .context("Keep-awake completion state missing")?;
            put(
                cache,
                "gpu.awake",
                json!({"active":active,"name":result.get("name")}),
            );
        }
        A::SetGpuPerformance { .. } => {
            ensure!(
                result
                    .get("clock_lock_readback_verified")
                    .and_then(Value::as_bool)
                    == Some(true),
                "GPU performance readback missing"
            );
            object(cache, "gpu.pstate")["last_verified_result"] = result.clone();
        }
        A::RestartGpu { .. } => {
            ensure!(
                result.get("restart_api_accepted").and_then(Value::as_bool) == Some(true),
                "GPU restart completion missing"
            );
            cache.remove("gpu");
            cache.remove("gpu.oc");
            cache.remove("gpu.processes");
            cache.remove("gpu.pstate");
            cache.remove("gpu.sensors");
        }
        A::SetWifiEnabled(_) => {
            ensure!(
                result
                    .get("software_radio_verified")
                    .and_then(Value::as_bool)
                    == Some(true),
                "WLAN software state unverified"
            );
            let observed: jiks_platform::wireless::WifiSnapshot = serde_json::from_value(
                result
                    .get("wifi")
                    .context("WLAN actual snapshot missing")?
                    .clone(),
            )?;
            captured(cache, "wireless", "wifi", serde_json::to_value(observed)?);
        }
        A::SetAirplaneMode(_) => {
            ensure!(
                result
                    .get("system_radio_state_verified")
                    .and_then(Value::as_bool)
                    == Some(true),
                "Airplane system state unverified"
            );
            let observed: jiks_platform::wireless::AirplaneStatus = serde_json::from_value(
                result
                    .get("airplane")
                    .context("Airplane actual snapshot missing")?
                    .clone(),
            )?;
            captured(
                cache,
                "wireless",
                "airplane",
                serde_json::to_value(observed)?,
            );
            // Airplane changes can invalidate every WLAN connection/radio bit.
            object(cache, "wireless")
                .as_object_mut()
                .unwrap()
                .remove("wifi");
        }
        A::BootLogo(_) => {
            let op = result
                .get("operation")
                .context("Boot physical operation report missing")?;
            ensure!(
                op.get("mount_cleanup_verified").and_then(Value::as_bool) == Some(true),
                "Boot cleanup is not verified"
            );
            ensure!(
                op.get("state")
                    .and_then(Value::as_str)
                    .is_some_and(|v| ["Committed", "Restored"].contains(&v)),
                "Boot operation remains pending"
            );
            let status = result
                .get("status")
                .filter(|v| v.is_object())
                .context("Boot actual status missing")?;
            put(cache, "boot_logo", status.clone());
            put(cache, "boot_logo.last", op.clone());
        }
        A::SetAutorun(_) => {
            let observed: jiks_platform::autorun::Status = serde_json::from_value(result.clone())?;
            ensure!(
                observed.owned && observed.action_matches && observed.settings_verified,
                "Autorun actual task is not verified"
            );
            put(cache, "autorun", result.clone());
        }
        A::StopGpuSensors => {
            ensure!(
                result
                    .get("sensor_contexts_released")
                    .and_then(Value::as_bool)
                    == Some(true),
                "GPU sensor resources release unconfirmed"
            );
            cache.remove("gpu.sensors");
        }
        A::KillGpuProcesses(_) => {
            let pids = result
                .get("completed_pids")
                .and_then(Value::as_array)
                .context("Process termination completion lacks actual exited PIDs")?;
            ensure!(
                result.get("process_exit_verified").and_then(Value::as_bool) == Some(true),
                "Process exit unconfirmed"
            );
            if let Some(rows) = cache
                .get_mut("gpu.processes")
                .and_then(|v| v.get_mut("processes"))
                .and_then(Value::as_array_mut)
            {
                rows.retain(|p| {
                    !pids
                        .iter()
                        .any(|pid| p.pointer("/identity/pid") == Some(pid))
                });
            }
        }
        A::SetGpuPreference { .. } => {
            let identity = result
                .get("identity")
                .context("GPU preference identity missing")?;
            let preference = result
                .get("preference")
                .context("GPU preference readback missing")?;
            if let Some(rows) = cache
                .get_mut("gpu.processes")
                .and_then(|v| v.get_mut("processes"))
                .and_then(Value::as_array_mut)
            {
                for row in rows {
                    if row.get("identity") == Some(identity) {
                        row["preference"] = preference.clone();
                    }
                }
            }
        }
        A::RequestGpuSleep => {
            ensure!(
                result.get("notification_accepted").and_then(Value::as_bool) == Some(true),
                "dGPU notification was not confirmed"
            );
            put(cache, "gpu.awake", json!({"active":false}));
            put(cache, "gpu.sleep.last", result.clone());
        }
        _ => {} // Read requests have their own cache key; opaque input/delivery acks claim no state.
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mode_completion_updates_all_mode_consumers_and_invalidates_old_limits() {
        let mut c = BTreeMap::from([
            (
                "cpu".into(),
                json!({"raw_mode":3,"limits":{"long_watts":70,"short_watts":125,"temperature_limit":100},"ranges":[1]}),
            ),
            (
                "godmode".into(),
                json!({"raw_mode":3,"fields":[{"key":"cpu.pl1","capability":{"available":true},"current":{"available":true,"value":70}}]}),
            ),
        ]);
        absorb(&mut c, &A::SetRawMode(255), &json!(255)).unwrap();
        assert_eq!(c["cpu"]["raw_mode"], 255);
        assert!(c["cpu"].get("limits").is_none());
        assert!(c["godmode"]["fields"][0].get("current").is_none());
        assert_eq!(c["cpu"]["ranges"], json!([1]));
        let ctx = crate::hardware_routes::ActionContext::from_cache(&c, &json!({}))
            .unwrap()
            .into_value();
        assert_eq!(ctx["raw_mode"], 255);
    }
    #[test]
    fn malformed_completion_commits_nothing_and_request_is_never_readback() {
        let mut c = BTreeMap::new();
        absorb(&mut c, &A::SetRawMode(255), &json!(2)).unwrap();
        assert_eq!(c["cpu"]["raw_mode"], 2);
        let before = c.clone();
        assert!(
            absorb(
                &mut c,
                &A::SetMapped {
                    feature: M::Overdrive,
                    value: 1
                },
                &Value::Null
            )
            .is_err()
        );
        assert_eq!(c, before);
    }
    #[test]
    fn energy_and_mapped_observed_values_are_ready_for_next_toggle() {
        let mut c = BTreeMap::new();
        absorb(
            &mut c,
            &A::SetMapped {
                feature: M::Overdrive,
                value: 1,
            },
            &json!(0),
        )
        .unwrap();
        assert_eq!(c["mapped.overdrive"]["value"], 0);
        let status = json!({"battery_mode":"Conservation","night_charge":false,"fn_lock":true,"always_on_usb":"Off","keyboard_maximum":null,"keyboard_level":null});
        absorb(
            &mut c,
            &A::Energy(jiks_platform::energy::EnergyAction::FnLock(false)),
            &status,
        )
        .unwrap();
        assert_eq!(c["energy"]["fn_lock"], true);
    }
    #[test]
    fn display_feedback_updates_selected_monitor_without_touching_other() {
        let mut c = BTreeMap::from([(
            "devices".into(),
            json!({"displays":{"available":true,"value":[{"name":"display1","current":{"width":1920,"height":1080,"hertz":60}},{"name":"display2","current":{"width":2560,"height":1440,"hertz":60}}]}}),
        )]);
        absorb(
            &mut c,
            &A::SetDisplayMode {
                device: "display2".into(),
                width: 1,
                height: 1,
                hertz: 1,
            },
            &json!({"width":2560,"height":1440,"hertz":165}),
        )
        .unwrap();
        assert_eq!(
            c["devices"].pointer("/displays/value/1/current/hertz"),
            Some(&json!(165))
        );
        assert_eq!(
            c["devices"].pointer("/displays/value/0/current/hertz"),
            Some(&json!(60))
        );
    }
    #[test]
    fn screen_delivery_cannot_become_a_verified_panel_off_state() {
        let mut c = BTreeMap::new();
        absorb(
            &mut c,
            &A::System(S::ScreenOff),
            &json!({"request_delivered":true,"physical_panel_state_verified":false}),
        )
        .unwrap();
        assert!(c.is_empty());
    }
}
