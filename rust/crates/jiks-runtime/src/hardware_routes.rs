//! Pure UI-to-hardware planning. This module performs no I/O and never reads a
//! clock/configuration or resolves a selection by guessing.
//!
//! Payload: flat UI keys; null means not requested. Runtime resolves preset IDs
//! into `preset:{id,name,values,fan,overrides}` before calling preset.apply.
//! Runtime injects `deadline_tick_ms` (absolute GetTickCount64) and cached
//! `_context:{display:{device,monitor_path,width,height,hertz,internal},
//! gpu:{index,name,gsync_enabled},keyboard:{path,white_backend},
//! power_led:{state_type,level_offset},ac_connected,raw_mode}` as needed.
//! Unsupported controls must be omitted/null, never silently sent as defaults.
//! Building a partial Vec then failing is harmless: callers receive no actions
//! until the complete request has validated successfully.
use anyhow::{Context, Result, bail, ensure};
use jiks_platform::{
    HardwareAction as A, SpectrumAction,
    charge_threshold::{MAXIMUM as THRESHOLD_MAXIMUM, Threshold},
    display_controls::DisplayAction as D,
    domain::MappedFeature as M,
    energy::{AlwaysOnUsb, ChargeMode, EnergyAction as E},
    god_mode::{CapabilityValue, GodModeField, GodModePatch},
    gpu::OverclockInfo,
    hybrid::{self, HybridMode, HybridSnapshot},
    spectrum::{ClockwiseDirection, ColorMode, Direction, Effect, EffectType, Profile, Rgb, Speed},
    system_controls::{GameZoneSwitch, SystemAction as S, WindowsPowerMode, parse_guid},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Trusted, read-only metadata assembled by the runtime before planning. User
/// `_context` contents are ignored and replaced, never treated as cache evidence.
#[derive(Debug, Clone)]
pub struct ActionContext(Value);
impl ActionContext {
    pub fn into_value(self) -> Value {
        self.0
    }
    pub fn from_cache(cache: &BTreeMap<String, Value>, payload: &Value) -> Result<Self> {
        ensure!(payload.is_object(), "Action payload must be an object");
        let mut ctx = json!({"cache_validated":true});
        for key in ["system", "energy"] {
            if let Some(value) = cache.get(key) {
                ctx[key] = value.clone();
            }
        }
        ctx["mapped"] = json!({});
        for (key, value) in cache.iter().filter(|(key, _)| key.starts_with("mapped.")) {
            if value.get("value").is_some() {
                ctx["mapped"][key.trim_start_matches("mapped.")] = value.clone();
            }
        }
        ctx["lighting_supported"] = json!(
            [0, 3, 5]
                .into_iter()
                .filter(|id| lighting_supported(cache, *id))
                .collect::<Vec<_>>()
        );
        if let Some(mode) = cache
            .get("power_modes")
            .and_then(|value| value.get("current_raw"))
            .or_else(|| cache.get("cpu").and_then(|value| value.get("raw_mode")))
        {
            ctx["raw_mode"] = mode.clone();
        }
        for (key, context_key) in [
            ("power_modes", "power_modes"),
            ("godmode", "godmode"),
            ("hybrid", "hybrid"),
            ("ai", "ai"),
            ("gpu.nodes", "gpu_nodes"),
            ("gpu.processes", "gpu_processes"),
            ("gpu.pstate", "gpu_performance"),
        ] {
            if let Some(value) = cache.get(key) {
                ctx[context_key] = value.clone();
            }
        }
        if let Some(ac) = cache
            .get("system")
            .and_then(|value| value.pointer("/ac_connected/value"))
            .and_then(Value::as_bool)
            .or_else(|| {
                cache
                    .get("telemetry")
                    .and_then(|value| value.get("ac_connected"))
                    .and_then(Value::as_bool)
            })
            .or_else(|| {
                cache
                    .get("energy")
                    .and_then(|value| value.get("ac_connected"))
                    .and_then(Value::as_bool)
            })
        {
            ctx["ac_connected"] = json!(ac);
        }
        let controls = cache.get("display_controls").and_then(Value::as_array);
        let selected = value(payload, "display.monitor");
        if let Some(controls) = controls {
            let matching = if let Some(selection) = selected {
                let id = if let Some(id) = selection.as_str() {
                    id
                } else {
                    selection
                        .get("monitor_path")
                        .or_else(|| selection.get("device"))
                        .and_then(Value::as_str)
                        .context("Display selection has no exact monitor/device identity")?
                };
                controls
                    .iter()
                    .filter(|control| {
                        ["monitor_path", "source_name"].into_iter().any(|key| {
                            control
                                .get(key)
                                .and_then(Value::as_str)
                                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(id))
                        })
                    })
                    .collect::<Vec<_>>()
            } else if controls.len() == 1 {
                vec![&controls[0]]
            } else {
                vec![]
            };
            if selected.is_some() {
                ensure!(
                    matching.len() == 1,
                    "Selected monitor is absent/ambiguous; no first-display fallback is allowed"
                );
            }
            if matching.len() == 1 {
                let control = matching[0];
                let path = control
                    .get("monitor_path")
                    .and_then(Value::as_str)
                    .context("Cached monitor path missing")?;
                let source = control
                    .get("source_name")
                    .and_then(Value::as_str)
                    .context("Cached display source missing")?;
                if let Some(selection) = selected.filter(|v| v.is_object()) {
                    if let Some(expected) = selection.get("device").and_then(Value::as_str) {
                        ensure!(
                            expected.eq_ignore_ascii_case(source),
                            "Display path/source identities disagree"
                        );
                    }
                    if let Some(expected) = selection.get("monitor_path").and_then(Value::as_str) {
                        ensure!(
                            expected.eq_ignore_ascii_case(path),
                            "Display path/source identities disagree"
                        );
                    }
                }
                let devices = cache
                    .get("devices")
                    .and_then(|value| value.pointer("/displays/value"))
                    .and_then(Value::as_array);
                let rows = devices
                    .map(|devices| {
                        devices
                            .iter()
                            .filter(|device| {
                                device.get("name").and_then(Value::as_str) == Some(source)
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                ensure!(
                    rows.len() == 1,
                    "Selected monitor has no unique GDI display mapping"
                );
                let device = rows[0];
                ctx["display"] = json!({"device":source,"monitor_path":path,"internal":control.get("internal"),"width":device.pointer("/current/width"),"height":device.pointer("/current/height"),"hertz":device.pointer("/current/hertz"),"modes":device.get("modes"),"dpi":control.get("dpi"),"color":control.get("color"),"refresh":control.get("refresh")});
            }
        } else {
            ensure!(
                selected.is_none(),
                "Selected monitor has no completed discovery snapshot"
            );
        }
        let candidates = cache
            .get("spectrum")
            .and_then(|value| value.get("candidates"))
            .and_then(Value::as_array);
        let selected_path = value(payload, "keyboard.path").and_then(Value::as_str);
        let matching = candidates
            .map(|items| {
                items
                    .iter()
                    .filter(|candidate| {
                        selected_path.is_none_or(|path| {
                            candidate
                                .get("path")
                                .and_then(Value::as_str)
                                .is_some_and(|actual| actual.eq_ignore_ascii_case(path))
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if selected_path.is_some() {
            ensure!(
                matching.len() == 1,
                "Explicit Spectrum interface is absent/ambiguous in completed descriptor metadata"
            );
        }
        if matching.len() == 1 {
            ctx["keyboard"] = json!({"path":matching[0].get("path")});
        }
        if lighting_supported(cache, 0) {
            ctx["keyboard"]["white_backend"] = json!("wmi");
        } else if cache
            .get("energy")
            .and_then(|value| value.get("keyboard_maximum"))
            .and_then(Value::as_u64)
            .is_some_and(|maximum| maximum >= 1)
        {
            ctx["keyboard"]["white_backend"] = json!("energy");
        }
        if let Some(effect) = cache.get("keyboard.effects") {
            ctx["keyboard"]["effects"] = effect.clone();
        }
        if let (Some(led), Some(offset)) = (
            cache.get("lighting.4"),
            cache
                .get("power_led.level_offset")
                .and_then(Value::as_u64)
                .filter(|v| *v <= 1),
        ) {
            if let Some(state) = led
                .as_object()
                .and_then(|values| {
                    values
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case("Current_State_Type"))
                })
                .map(|(_, value)| value)
            {
                ctx["power_led"] = json!({"state_type":state,"level_offset":offset});
            }
        }
        if let Some(gpus) = cache.get("gpu").and_then(Value::as_array) {
            let selected_index = value(payload, "gpu.index").and_then(Value::as_u64);
            let selected_name = value(payload, "gpu.name").and_then(Value::as_str);
            let matching = gpus
                .iter()
                .filter(|gpu| {
                    selected_index
                        .is_none_or(|index| gpu.get("index").and_then(Value::as_u64) == Some(index))
                        && selected_name.is_none_or(|name| {
                            gpu.get("name").and_then(Value::as_str) == Some(name)
                        })
                })
                .collect::<Vec<_>>();
            if selected_index.is_some() || selected_name.is_some() {
                ensure!(
                    matching.len() == 1,
                    "Explicit GPU identity no longer matches a unique completed physical inventory"
                );
            }
            if matching.len() == 1 {
                ctx["gpu"] = json!({"index":matching[0].get("index"),"name":matching[0].get("name"),"oc":cache.get("gpu.oc"),"gsync_enabled":cache.get("hybrid").and_then(|value|value.pointer("/snapshot/gsync")).or_else(||cache.get("gpu.gsync"))});
            }
        }
        Ok(Self(ctx))
    }
}
pub(crate) fn lighting_supported(cache: &BTreeMap<String, Value>, id: u32) -> bool {
    let expected = match id {
        0 => (0, 1),
        3 | 5 => (1, 0),
        _ => return false,
    };
    cache
        .get("capabilities")
        .and_then(|value| value.pointer("/lighting/value"))
        .and_then(Value::as_array)
        .is_some_and(|rows| {
            rows.iter().any(|row| {
                row.get("Lighting_ID").and_then(Value::as_u64) == Some(u64::from(id))
                    && row.get("Control_Interface").and_then(Value::as_u64) == Some(expected.0)
                    && row.get("Lighting_Type").and_then(Value::as_u64) == Some(expected.1)
            })
        })
}

fn value<'a>(payload: &'a Value, key: &str) -> Option<&'a Value> {
    payload.get(key).filter(|v| !v.is_null())
}
fn context<'a>(payload: &'a Value, area: &str) -> Option<&'a Value> {
    payload
        .get("_context")
        .and_then(|v| v.get(area))
        .filter(|v| !v.is_null())
}
fn bool_value(v: &Value, name: &str) -> Result<bool> {
    v.as_bool()
        .with_context(|| format!("{name} must be a boolean"))
}
fn integer(v: &Value, name: &str) -> Result<i32> {
    v.as_i64()
        .and_then(|v| i32::try_from(v).ok())
        .with_context(|| format!("{name} must be an int32"))
}
fn unsigned(v: &Value, name: &str, max: u32) -> Result<u32> {
    let n = v
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .with_context(|| format!("{name} must be a nonnegative integer"))?;
    ensure!(n <= max, "{name} exceeds {max}");
    Ok(n)
}
fn text<'a>(v: &'a Value, name: &str) -> Result<&'a str> {
    let s = v.as_str().with_context(|| format!("{name} must be text"))?;
    ensure!(
        !s.trim().is_empty() && !s.contains('\0'),
        "{name} cannot be empty/contain NUL"
    );
    Ok(s)
}
fn required<'a>(p: &'a Value, key: &str) -> Result<&'a Value> {
    value(p, key).with_context(|| format!("Missing {key}"))
}
fn deadline(p: &Value) -> Result<u64> {
    let v = value(p, "deadline_tick_ms")
        .or_else(|| context(p, "deadline_tick_ms"))
        .and_then(Value::as_u64)
        .context("Runtime must provide deadline_tick_ms from the monotonic command queue")?;
    ensure!(v > 0, "Deadline cannot be zero");
    Ok(v)
}
fn known(p: &Value, keys: &[&str]) -> Result<()> {
    for (key, v) in p
        .as_object()
        .context("Hardware request must be an object")?
    {
        if v.is_null() || key == "_context" || key == "deadline_tick_ms" {
            continue;
        }
        ensure!(
            keys.contains(&key.as_str()),
            "Unrecognized request field {key}; no action was planned"
        );
    }
    Ok(())
}
fn nonempty(actions: Vec<A>) -> Result<Vec<A>> {
    ensure!(!actions.is_empty(), "No hardware field was requested");
    Ok(actions)
}
fn selected_gpu_processes(
    payload: &Value,
) -> Result<Vec<jiks_platform::gpu_management::ProcessIdentity>> {
    use jiks_platform::gpu_management::{GpuProcess, ProcessIdentity};
    let selection = required(payload, "selection.gpu.processes")?;
    let ids = if let Some(values) = selection.as_array() {
        values
            .iter()
            .map(|v| text(v, "GPU process identity").map(str::to_string))
            .collect::<Result<Vec<_>>>()?
    } else {
        vec![text(selection, "GPU process identity")?.into()]
    };
    ensure!(
        !ids.is_empty() && ids.len() <= 64,
        "Select 1..64 GPU process identities"
    );
    let list = context(payload, "gpu_processes")
        .and_then(|v| v.get("processes"))
        .and_then(Value::as_array)
        .context("Completed explicit GPU process inventory is required")?;
    let processes = list
        .iter()
        .map(|v| serde_json::from_value::<GpuProcess>(v.clone()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let targets = ids
        .iter()
        .map(|id| {
            let found = processes
                .iter()
                .filter(|p| format!("{}:{}", p.identity.pid, p.identity.creation_filetime) == *id)
                .collect::<Vec<_>>();
            ensure!(
                found.len() == 1,
                "Selected GPU process is absent/ambiguous; PID alone is not an identity"
            );
            Ok(found[0].identity.clone())
        })
        .collect::<Result<Vec<ProcessIdentity>>>()?;
    jiks_platform::gpu_management::validate_identities(&targets)?;
    Ok(targets)
}
fn mode(v: &Value) -> Result<u32> {
    if let Some(s) = v.as_str() {
        return match s.to_ascii_lowercase().as_str() {
            "quiet" | "1" => Ok(1),
            "balanced" | "balance" | "2" => Ok(2),
            "performance" | "3" => Ok(3),
            "custom" | "godmode" | "255" => Ok(255),
            "extreme" | "224" => Ok(224),
            _ => bail!("Unknown firmware power mode"),
        };
    }
    let n = unsigned(v, "power.mode", 255)?;
    ensure!(
        [1, 2, 3, 224, 255].contains(&n),
        "Unsupported firmware power mode"
    );
    Ok(n)
}
fn mapped(actions: &mut Vec<A>, p: &Value, key: &str, feature: M) -> Result<()> {
    if let Some(v) = value(p, key) {
        let value = u32::from(bool_value(v, key)?);
        feature.validate(value)?;
        actions.push(A::SetMapped { feature, value });
    }
    Ok(())
}
fn windows_mode(v: &Value) -> Result<WindowsPowerMode> {
    if let Some(s) = v.as_str() {
        return match s.to_ascii_lowercase().as_str() {
            "efficiency" | "bestpowerefficiency" | "0" => Ok(WindowsPowerMode::Efficiency),
            "balanced" | "1" => Ok(WindowsPowerMode::Balanced),
            "performance" | "bestperformance" | "2" => Ok(WindowsPowerMode::Performance),
            _ => bail!("Unknown Windows power overlay"),
        };
    }
    match unsigned(v, "Windows power mode", 2)? {
        0 => Ok(WindowsPowerMode::Efficiency),
        1 => Ok(WindowsPowerMode::Balanced),
        2 => Ok(WindowsPowerMode::Performance),
        _ => unreachable!(),
    }
}
fn battery_mode(v: &Value) -> Result<ChargeMode> {
    if let Some(s) = v.as_str() {
        return match s.to_ascii_lowercase().as_str() {
            "conservation" | "0" => Ok(ChargeMode::Conservation),
            "normal" | "1" => Ok(ChargeMode::Normal),
            "rapid" | "rapidcharge" | "2" => Ok(ChargeMode::Rapid),
            _ => bail!("Unknown charge mode"),
        };
    }
    match unsigned(v, "battery.mode", 2)? {
        0 => Ok(ChargeMode::Conservation),
        1 => Ok(ChargeMode::Normal),
        2 => Ok(ChargeMode::Rapid),
        _ => unreachable!(),
    }
}
fn usb_mode(v: &Value) -> Result<AlwaysOnUsb> {
    if let Some(s) = v.as_str() {
        return match s.to_ascii_lowercase().as_str() {
            "off" | "0" => Ok(AlwaysOnUsb::Off),
            "sleeping" | "onwhensleeping" | "1" => Ok(AlwaysOnUsb::Sleeping),
            "always" | "onalways" | "2" => Ok(AlwaysOnUsb::Always),
            _ => bail!("Unknown Always-on USB mode"),
        };
    }
    match unsigned(v, "power.always_on_usb", 2)? {
        0 => Ok(AlwaysOnUsb::Off),
        1 => Ok(AlwaysOnUsb::Sleeping),
        2 => Ok(AlwaysOnUsb::Always),
        _ => unreachable!(),
    }
}
fn instant_boot(v: &Value) -> Result<u32> {
    if let Some(s) = v.as_str() {
        return match s.to_ascii_lowercase().as_str() {
            "off" | "0" => Ok(0),
            "acadapter" | "ac" | "1" => Ok(1),
            "usbpowerdelivery" | "usb" | "2" => Ok(2),
            "acadapterandusbpowerdelivery" | "both" | "3" => Ok(3),
            _ => bail!("Unknown instant boot mode"),
        };
    }
    unsigned(v, "power.instant_boot", 3)
}

fn display_context(p: &Value) -> Value {
    value(p, "display.monitor")
        .filter(|v| v.is_object())
        .cloned()
        .or_else(|| context(p, "display").cloned())
        .unwrap_or_else(|| json!({}))
}
fn target(p: &Value, key: &str) -> Result<String> {
    let ctx = display_context(p);
    let aliases = if key == "device" {
        ["device", "source_name"]
    } else {
        ["monitor_path", "path"]
    };
    for alias in aliases {
        if let Some(v) = ctx.get(alias) {
            return Ok(text(v, alias)?.into());
        }
    }
    if let Some(v) = value(p, "display.monitor").and_then(Value::as_str) {
        if (key == "device" && v.starts_with("\\\\.\\DISPLAY"))
            || (key != "device" && v.to_ascii_lowercase().starts_with("\\\\?\\display#"))
        {
            return Ok(v.into());
        }
    }
    bail!("Cached display {key} is required; no monitor address was guessed")
}
fn resolution(v: &Value) -> Result<(u32, u32)> {
    let (w, h) = if let Some(o) = v.as_object() {
        (
            unsigned(
                o.get("width").context("Missing resolution width")?,
                "width",
                32768,
            )?,
            unsigned(
                o.get("height").context("Missing resolution height")?,
                "height",
                32768,
            )?,
        )
    } else if let Some(a) = v.as_array() {
        ensure!(a.len() == 2, "Resolution needs two dimensions");
        (
            unsigned(&a[0], "width", 32768)?,
            unsigned(&a[1], "height", 32768)?,
        )
    } else {
        let s = text(v, "display.resolution")?;
        let parts = s.split(['x', 'X', '×']).collect::<Vec<_>>();
        ensure!(parts.len() == 2, "Resolution must be width x height");
        (
            parts[0].trim().parse::<u32>()?,
            parts[1].trim().parse::<u32>()?,
        )
    };
    ensure!(
        w > 0 && h > 0 && w <= 32768 && h <= 32768,
        "Invalid display dimensions"
    );
    Ok((w, h))
}
fn display(p: &Value) -> Result<Vec<A>> {
    known(
        p,
        &[
            "display.monitor",
            "display.resolution",
            "display.refresh_rate",
            "display.dpi",
            "display.brightness",
            "display.hdr",
            "display.acm",
            "display.overdrive",
        ],
    )?;
    let mut actions = vec![];
    let ctx = display_context(p);
    if value(p, "display.resolution").is_some() || value(p, "display.refresh_rate").is_some() {
        let (w, h) = if let Some(v) = value(p, "display.resolution") {
            resolution(v)?
        } else {
            (
                unsigned(
                    ctx.get("width")
                        .context("Cached current width is required for a refresh-only request")?,
                    "width",
                    32768,
                )?,
                unsigned(
                    ctx.get("height")
                        .context("Cached current height is required for a refresh-only request")?,
                    "height",
                    32768,
                )?,
            )
        };
        let refresh = if let Some(v) = value(p, "display.refresh_rate") {
            if v.is_object() {
                Some(serde_json::from_value::<
                    jiks_platform::dynamic_refresh::RefreshRate,
                >(v.clone())?)
            } else if let Some(text) = v.as_str().and_then(|v| v.strip_prefix("dynamic:")) {
                let parts = text.split(':').collect::<Vec<_>>();
                ensure!(parts.len() == 2, "Malformed dynamic refresh choice");
                Some(jiks_platform::dynamic_refresh::pair(
                    parts[1].parse::<f64>()?,
                    parts[0].parse::<f64>()?,
                )?)
            } else {
                None
            }
        } else {
            None
        };
        let hertz = if let Some(refresh) = refresh {
            refresh.frequency
        } else {
            unsigned(
                value(p, "display.refresh_rate")
                    .or_else(|| ctx.get("hertz"))
                    .context("Cached/requested refresh rate is required")?,
                "refresh rate",
                1000,
            )?
        };
        ensure!(w > 0 && h > 0 && hertz > 0, "Display timing cannot be zero");
        if let Some(rate) = refresh.filter(|r| r.is_dynamic) {
            let advertised = ctx
                .pointer("/refresh/choices")
                .and_then(Value::as_array)
                .context("Completed native DRR mode discovery is required")?;
            ensure!(
                advertised.iter().any(|v| serde_json::from_value::<
                    jiks_platform::dynamic_refresh::RefreshRate,
                >(v.clone())
                .ok()
                    == Some(rate)),
                "DRR pair was not advertised for selected monitor"
            );
            ensure!(
                ctx.get("width").and_then(Value::as_u64) == Some(w as u64)
                    && ctx.get("height").and_then(Value::as_u64) == Some(h as u64),
                "DRR mode discovery must be refreshed after a resolution change"
            );
            actions.push(A::Display(D::RefreshRate {
                monitor_path: target(p, "monitor_path")?,
                width: w,
                height: h,
                rate,
            }));
        } else {
            actions.push(A::SetDisplayMode {
                device: target(p, "device")?,
                width: w,
                height: h,
                hertz,
            });
        }
    }
    for (key, hdr) in [("display.hdr", true), ("display.acm", false)] {
        if let Some(v) = value(p, key) {
            let enabled = bool_value(v, key)?;
            let monitor_path = target(p, "monitor_path")?;
            actions.push(A::Display(if hdr {
                D::Hdr {
                    monitor_path,
                    enabled,
                }
            } else {
                D::Acm {
                    monitor_path,
                    enabled,
                }
            }));
        }
    }
    if let Some(v) = value(p, "display.dpi") {
        let percent = unsigned(v, "display.dpi", 500)?;
        ensure!(
            [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500].contains(&percent),
            "DPI is not in the fixed native scale table"
        );
        actions.push(A::Display(D::Dpi {
            monitor_path: target(p, "monitor_path")?,
            percent,
        }));
    }
    if let Some(v) = value(p, "display.brightness") {
        ensure!(
            ctx.get("internal").and_then(Value::as_bool) != Some(false),
            "External monitor brightness is not an internal WMI brightness route"
        );
        actions.push(A::System(S::Brightness(unsigned(
            v,
            "display.brightness",
            100,
        )?)));
    }
    mapped(&mut actions, p, "display.overdrive", M::Overdrive)?;
    nonempty(actions)
}
fn keys(p: &Value) -> Result<Vec<A>> {
    known(
        p,
        &[
            "keys.fn_lock",
            "keys.win_lock",
            "keys.touchpad_lock",
            "keys.microphone_muted",
            "keyboard.white_single",
            "keyboard.white_level",
            "keyboard.logo",
            "keyboard.ports",
            "keyboard.power_led",
            "keyboard.power_led_sync",
            "speaker.muted",
            "speaker.volume",
        ],
    )?;
    let mut actions = vec![];
    if let Some(v) = value(p, "keys.fn_lock") {
        actions.push(A::Energy(E::FnLock(bool_value(v, "keys.fn_lock")?)));
    }
    if let Some(v) = value(p, "keyboard.white_single") {
        actions.push(A::Energy(E::OneLevelWhite(bool_value(
            v,
            "keyboard.white_single",
        )?)));
    }
    for (key, which) in [
        ("keys.win_lock", GameZoneSwitch::WinKey),
        ("keys.touchpad_lock", GameZoneSwitch::TouchpadLock),
    ] {
        if let Some(v) = value(p, key) {
            actions.push(A::System(S::Switch {
                which,
                value: u32::from(bool_value(v, key)?),
            }));
        }
    }
    if let Some(v) = value(p, "keys.microphone_muted") {
        actions.push(A::SetMicrophoneMute(bool_value(
            v,
            "keys.microphone_muted",
        )?));
    }
    if let Some(v) = value(p, "speaker.muted") {
        actions.push(A::SetSpeakerMute(bool_value(v, "speaker.muted")?));
    }
    if let Some(v) = value(p, "speaker.volume") {
        actions.push(A::SetSpeakerVolume(unsigned(v, "speaker.volume", 100)?));
    }
    if let Some(v) = value(p, "keyboard.white_level") {
        let level = unsigned(v, "keyboard.white_level", 3)?;
        let backend = context(p, "keyboard")
            .and_then(|v| v.get("white_backend"))
            .and_then(Value::as_str)
            .context("Cached white keyboard backend is required")?;
        match backend {
            "energy" => actions.push(A::Energy(E::KeyboardLevel(level))),
            "wmi" => {
                ensure!(level <= 2, "WMI white backlight supports Off/Low/High only");
                actions.push(A::SetLighting {
                    lighting_id: 0,
                    state_type: 0,
                    brightness: level + 1,
                });
            }
            _ => bail!("Unknown white keyboard backend"),
        }
    }
    for (key, id) in [("keyboard.logo", 3), ("keyboard.ports", 5)] {
        if let Some(v) = value(p, key) {
            actions.push(A::SetLighting {
                lighting_id: id,
                state_type: u32::from(bool_value(v, key)?),
                brightness: 0,
            });
        }
    }
    if let Some(v) = value(p, "keyboard.power_led") {
        let brightness = unsigned(v, "keyboard.power_led", 4)?;
        ensure!(brightness > 0, "Power LED user brightness is 1..4");
        let ctx = context(p, "power_led")
            .context("Power LED state/verified level offset cache is required")?;
        let state_type = unsigned(
            ctx.get("state_type")
                .context("Missing current power LED state")?,
            "power LED state",
            2,
        )?;
        let offset = unsigned(
            ctx.get("level_offset")
                .context("Power LED level offset is uncalibrated; no raw level was guessed")?,
            "power LED offset",
            1,
        )?;
        actions.push(A::SetLighting {
            lighting_id: 4,
            state_type,
            brightness: brightness - offset,
        });
    }
    if let Some(v) = value(p, "keyboard.power_led_sync") {
        actions.push(A::SetMapped {
            feature: M::PowerLedSync,
            value: if bool_value(v, "keyboard.power_led_sync")? {
                2
            } else {
                0
            },
        });
    }
    nonempty(actions)
}
fn spectrum_path(p: &Value) -> Result<String> {
    let v = value(p, "keyboard.path")
        .or_else(|| context(p, "keyboard").and_then(|v| v.get("path")))
        .context("A cached Spectrum interface path is required")?;
    let path = text(v, "Spectrum interface")?;
    ensure!(
        path.to_ascii_lowercase().starts_with("\\\\?\\hid#"),
        "Invalid HID interface path"
    );
    Ok(path.into())
}
fn color(v: &Value) -> Result<Rgb> {
    let s = text(v, "keyboard.color")?
        .strip_prefix('#')
        .context("Color must be #RRGGBB")?;
    ensure!(
        s.len() == 6 && s.bytes().all(|v| v.is_ascii_hexdigit()),
        "Color must be #RRGGBB"
    );
    Ok(Rgb {
        r: u8::from_str_radix(&s[0..2], 16)?,
        g: u8::from_str_radix(&s[2..4], 16)?,
        b: u8::from_str_radix(&s[4..6], 16)?,
    })
}
fn zones(v: &Value) -> Result<Vec<u16>> {
    if let Some(text) = v.as_str() {
        if text.eq_ignore_ascii_case("all") {
            return Ok((1..=24).collect());
        }
        if text.trim().is_empty() {
            return Ok(vec![]);
        }
        return text
            .split([',', '，'])
            .map(|part| {
                let n = part
                    .trim()
                    .parse::<u16>()
                    .context("Zones must be comma-separated integers")?;
                ensure!((1..=24).contains(&n), "Zone must be 1..24");
                Ok(n)
            })
            .collect();
    }
    v.as_array()
        .context("Keyboard zones must be an array, comma-separated text or all")?
        .iter()
        .map(|v| {
            let n = unsigned(v, "zone", 24)?;
            ensure!(n > 0, "Zone must be 1..24");
            Ok(n as u16)
        })
        .collect()
}
fn keyboard(p: &Value) -> Result<Vec<A>> {
    known(
        p,
        &[
            "keyboard.path",
            "keyboard.profile",
            "keyboard.brightness",
            "keyboard.effect",
            "keyboard.zones",
            "keyboard.color",
            "keyboard.speed",
            "keyboard.direction",
            "keyboard.preview",
        ],
    )?;
    let path = spectrum_path(p)?;
    let mut actions = vec![];
    let profile = value(p, "keyboard.profile")
        .map(|v| unsigned(v, "keyboard.profile", 6))
        .transpose()?;
    if let Some(v) = value(p, "keyboard.preview") {
        bool_value(v, "keyboard.preview")?;
    }
    if let Some(v) = value(p, "keyboard.effect") {
        let kind: EffectType =
            serde_json::from_value(v.clone()).context("Unknown Spectrum effect enum")?;
        ensure!(
            kind.supported_24_zone(),
            "Effect is unsupported on 83NN 24-zone keyboard"
        );
        let profile = profile.context("Effects require an explicit target profile")?;
        ensure!(profile > 0, "Cannot write effects to profile Off");
        let keys = if matches!(kind, EffectType::AudioBounce) {
            if let Some(v) = value(p, "keyboard.zones") {
                ensure!(
                    v.as_array().is_some_and(|a| a.is_empty()) || v.as_str() == Some("all"),
                    "AudioBounce uses all lights, not a zone subset"
                );
            }
            vec![]
        } else {
            let v = required(p, "keyboard.zones")?;
            zones(v)?
        };
        let colors = value(p, "keyboard.color")
            .map(color)
            .transpose()?
            .into_iter()
            .collect();
        let speed = match value(p, "keyboard.speed")
            .map(|v| unsigned(v, "keyboard.speed", 3))
            .transpose()?
            .unwrap_or(0)
        {
            0 => Speed::None,
            1 => Speed::Speed1,
            2 => Speed::Speed2,
            3 => Speed::Speed3,
            _ => unreachable!(),
        };
        let direction = value(p, "keyboard.direction")
            .map(|v| {
                serde_json::from_value::<Direction>(v.clone())
                    .context("Unknown Spectrum direction enum")
            })
            .transpose()?
            .unwrap_or_default();
        let profile = Profile {
            profile: profile as u8,
            effects: vec![Effect {
                kind,
                speed,
                direction,
                clockwise_direction: ClockwiseDirection::None,
                colors,
                keys,
                use_vantage_color_boost: false,
                color_mode: ColorMode::None,
            }],
        };
        profile.encode_24_zone()?;
        actions.push(A::Spectrum(SpectrumAction::Effects {
            path: path.clone(),
            profile,
        }));
    } else {
        for key in [
            "keyboard.zones",
            "keyboard.color",
            "keyboard.speed",
            "keyboard.direction",
        ] {
            ensure!(
                value(p, key).is_none(),
                "{key} requires an explicit Spectrum effect"
            );
        }
    }
    if let Some(v) = value(p, "keyboard.brightness") {
        actions.push(A::Spectrum(SpectrumAction::Brightness {
            path: path.clone(),
            value: unsigned(v, "keyboard.brightness", 9)? as u8,
        }));
    }
    if let Some(profile) = profile {
        actions.push(A::Spectrum(SpectrumAction::SelectProfile {
            path,
            profile: profile as u8,
        }));
    }
    nonempty(actions)
}
fn effects(command: &str, p: &Value) -> Result<Vec<A>> {
    known(p, &["profile", "effects", "keyboard.path"])?;
    let profile = unsigned(required(p, "profile")?, "Spectrum profile", 6)? as u8;
    ensure!(profile > 0, "Effect profile must be 1..6");
    let path = spectrum_path(p)?;
    let action = match command {
        "keyboard.effects.refresh" => {
            ensure!(
                value(p, "effects").is_none(),
                "Refresh does not accept effect edits"
            );
            SpectrumAction::ReadEffects { path, profile }
        }
        "keyboard.effects.reset" => {
            ensure!(
                value(p, "effects").is_none(),
                "Reset does not accept effect edits"
            );
            SpectrumAction::Reset { path, profile }
        }
        "keyboard.effects.apply" => {
            let effects: Vec<Effect> = serde_json::from_value(required(p, "effects")?.clone())
                .context("Invalid PascalCase Spectrum Effect array")?;
            ensure!(
                !effects.is_empty(),
                "Use the explicit reset command instead of applying an empty effect list"
            );
            let profile = Profile { profile, effects };
            profile.encode_24_zone()?;
            SpectrumAction::Effects { path, profile }
        }
        _ => unreachable!(),
    };
    Ok(vec![A::Spectrum(action)])
}
fn gpu_address(p: &Value) -> Result<(u32, String)> {
    let ctx = context(p, "gpu");
    let index = value(p, "gpu.index")
        .or_else(|| ctx.and_then(|v| v.get("index")))
        .context("Cached GPU physical index is required")?;
    let name = value(p, "gpu.name")
        .or_else(|| ctx.and_then(|v| v.get("name")))
        .context("Cached GPU identity is required")?;
    Ok((
        unsigned(index, "GPU index", 63)?,
        text(name, "GPU name")?.into(),
    ))
}
fn gpu_oc(p: &Value) -> Result<Vec<A>> {
    known(
        p,
        &[
            "gpu.index",
            "gpu.name",
            "gpu.oc.enabled",
            "gpu.oc.core_offset_mhz",
            "gpu.oc.memory_offset_mhz",
            "gpu.oc.voltage_mv",
            "gpu.oc.lock_voltage",
        ],
    )?;
    let enabled = bool_value(required(p, "gpu.oc.enabled")?, "gpu.oc.enabled")?;
    let core = integer(
        required(p, "gpu.oc.core_offset_mhz")?,
        "gpu.oc.core_offset_mhz",
    )?;
    let memory = integer(
        required(p, "gpu.oc.memory_offset_mhz")?,
        "gpu.oc.memory_offset_mhz",
    )?;
    let voltage = value(p, "gpu.oc.voltage_mv")
        .map(|v| unsigned(v, "gpu.oc.voltage_mv", 2000))
        .transpose()?
        .unwrap_or(0);
    let lock = value(p, "gpu.oc.lock_voltage")
        .map(|v| bool_value(v, "gpu.oc.lock_voltage"))
        .transpose()?
        .unwrap_or(false);
    ensure!(
        !lock || voltage > 0,
        "Voltage lock requires a positive voltage"
    );
    let (index, expected_name) = gpu_address(p)?;
    let requested = if enabled {
        OverclockInfo {
            core_delta_mhz: core,
            memory_delta_mhz: memory,
            voltage_lock_mv: if lock { voltage as i32 } else { 0 },
            voltage_cap_mv: if lock { 0 } else { voltage as i32 },
        }
    } else {
        OverclockInfo::default()
    };
    Ok(vec![A::SetGpuOverclock {
        index,
        expected_name,
        requested,
        deadline_tick_ms: deadline(p)?,
    }])
}
fn field(key: &str) -> Option<GodModeField> {
    GodModeField::from_key(key).or_else(|| match key {
        "CPULongTermPowerLimit" => Some(GodModeField::CpuLongPower),
        "CPUShortTermPowerLimit" => Some(GodModeField::CpuShortPower),
        "CPUPeakPowerLimit" => Some(GodModeField::CpuPeakPower),
        "CPUCrossLoadingPowerLimit" => Some(GodModeField::CpuCrossLoading),
        "CPUPL2Tau" => Some(GodModeField::CpuPl2Tau),
        "APUsPPTPowerLimit" => Some(GodModeField::ApuSppt),
        "CPUTemperatureLimit" => Some(GodModeField::CpuTemperature),
        "GPUPowerBoost" => Some(GodModeField::GpuPowerBoost),
        "GPUConfigurableTGP" => Some(GodModeField::GpuTgp),
        "GPUTemperatureLimit" => Some(GodModeField::GpuTemperature),
        "GPUTotalProcessingPowerTargetOnAcOffsetFromBaseline" => Some(GodModeField::GpuAcOffset),
        "GPUToCPUDynamicBoost" => Some(GodModeField::GpuToCpuBoost),
        "EnableOverclocking" => Some(GodModeField::CpuOverclockEnable),
        _ => None,
    })
}
fn fan_table(v: &Value) -> Result<Vec<u8>> {
    let values = v
        .as_array()
        .context("Fan table must be ten levels or a 64-byte frame")?;
    if values.len() == 64 {
        let bytes = values
            .iter()
            .map(|v| Ok(unsigned(v, "fan byte", 255)? as u8))
            .collect::<Result<Vec<_>>>()?;
        jiks_platform::domain::validate_fan_table(&bytes)?;
        return Ok(bytes);
    }
    ensure!(
        values.len() == 10,
        "Fan table must contain exactly ten levels"
    );
    let mut bytes = vec![0u8; 64];
    bytes[0] = 1;
    bytes[2..6].copy_from_slice(&10u32.to_le_bytes());
    for (i, v) in values.iter().enumerate() {
        let level = unsigned(v, "fan level", 10)? as u16;
        bytes[6 + i * 2..8 + i * 2].copy_from_slice(&level.to_le_bytes());
    }
    jiks_platform::domain::validate_fan_table(&bytes)?;
    Ok(bytes)
}
fn preset(p: &Value, resolved: bool) -> Result<Vec<A>> {
    if resolved {
        known(p, &["selection.presets", "preset"])?;
    }
    let document = if resolved { required(p, "preset")? } else { p };
    let object = document
        .as_object()
        .context("Resolved preset must be an object")?;
    let values = object
        .get("values")
        .and_then(Value::as_object)
        .context("Preset values must be an object; IDs are resolved by runtime")?;
    let mut requested = vec![];
    let mut seen = std::collections::BTreeSet::new();
    for (key, value) in values {
        if value.is_null() {
            continue;
        }
        let field = field(key)
            .with_context(|| format!("Unknown/unported preset value {key}; nothing was planned"))?;
        ensure!(seen.insert(field), "Duplicate semantic preset field {key}");
        let value = if field == GodModeField::CpuOverclockEnable && value.is_boolean() {
            i32::from(value.as_bool().unwrap())
        } else if let Some(v) = value.as_object().and_then(|o| o.get("Value")) {
            integer(v, key)?
        } else {
            integer(value, key)?
        };
        requested.push(CapabilityValue { field, value });
    }
    if let Some(god) = context(p, "godmode") {
        for value in &requested {
            let row = god
                .get("fields")
                .and_then(Value::as_array)
                .and_then(|rows| {
                    rows.iter().find(|row| {
                        row.get("key").and_then(Value::as_str) == Some(value.field.semantic_key())
                    })
                })
                .with_context(|| {
                    format!("No cached capability for {}", value.field.semantic_key())
                })?;
            ensure!(
                row.pointer("/capability/available")
                    .and_then(Value::as_bool)
                    == Some(true),
                "{} is unavailable in the completed capability snapshot",
                value.field.semantic_key()
            );
            let spec: jiks_platform::god_mode::CapabilitySpec = serde_json::from_value(
                row.pointer("/capability/value")
                    .context("Cached capability payload missing")?
                    .clone(),
            )?;
            ensure!(
                spec.contains(value.value),
                "{} is outside the cached firmware grid/discrete values",
                value.field.semantic_key()
            );
        }
    }
    if let (Some(long), Some(short)) = (
        requested
            .iter()
            .find(|v| v.field == GodModeField::CpuLongPower),
        requested
            .iter()
            .find(|v| v.field == GodModeField::CpuShortPower),
    ) {
        ensure!(
            long.value > 0 && long.value <= short.value,
            "Preset must keep 0 < PL1 <= PL2"
        );
    }
    let mut actions = vec![];
    if !requested.is_empty() {
        if let Some(mode) = context(p, "raw_mode").and_then(Value::as_u64) {
            ensure!(
                mode == 255,
                "Select Custom mode before preset.apply; no implicit mode/default change"
            );
        }
        actions.push(A::ApplyGodMode(GodModePatch {
            values: requested,
            deadline_tick_ms: deadline(p)?,
        }));
    }
    if let Some(fan) = object.get("fan").filter(|v| !v.is_null()) {
        let fan = fan.as_object().context("fan must be an object")?;
        for key in fan.keys() {
            ensure!(
                ["full_speed", "table"].contains(&key.as_str()),
                "Unknown fan field {key}"
            );
        }
        if let Some(table) = fan.get("table").filter(|v| !v.is_null()) {
            actions.push(A::SetFanTable(fan_table(table)?));
        }
        if let Some(full) = fan.get("full_speed").filter(|v| !v.is_null()) {
            actions.push(A::SetMapped {
                feature: M::FanFullSpeed,
                value: u32::from(bool_value(full, "fan.full_speed")?),
            });
        }
    }
    if let Some(overrides) = object.get("overrides").filter(|v| !v.is_null()) {
        let overrides = overrides
            .as_object()
            .context("Preset overrides must be an object")?;
        for (key, value) in overrides {
            if value.is_null() {
                continue;
            }
            ensure!(
                ["power.plan", "power.mode_ac", "power.mode_dc"].contains(&key.as_str()),
                "Unported preset override {key}"
            );
        }
        if let Some(plan) = overrides.get("power.plan").filter(|v| !v.is_null()) {
            let id = text(plan, "power.plan")?;
            parse_guid(id)?;
            actions.push(A::System(S::PowerPlan(id.into())));
        }
        let ac = overrides
            .get("power.mode_ac")
            .filter(|v| !v.is_null())
            .map(windows_mode)
            .transpose()?;
        let dc = overrides
            .get("power.mode_dc")
            .filter(|v| !v.is_null())
            .map(windows_mode)
            .transpose()?;
        if ac.is_some() || dc.is_some() {
            let connected=context(p,"ac_connected").and_then(Value::as_bool).context("Cached AC state is required to choose the explicit active-source power override")?;
            if let Some(mode) = if connected { ac } else { dc } {
                actions.push(A::System(S::PowerMode(mode)));
            }
        }
    }
    nonempty(actions)
}

pub fn plan(command: &str, payload: &Value) -> Result<Vec<A>> {
    ensure!(payload.is_object(), "Hardware payload must be an object");
    match command {
        "power.apply" | "power.mode" => {
            known(payload, &["power.mode", "power.fnq_custom"])?;
            let mut actions = vec![];
            if let Some(v) = value(payload, "power.mode") {
                let target = mode(v)?;
                if target == 224 {
                    let confirmed = context(payload, "power_modes")
                        .and_then(|value| value.get("modes"))
                        .and_then(Value::as_array)
                        .is_some_and(|values| {
                            values
                                .iter()
                                .any(|value| value.get("raw").and_then(Value::as_u64) == Some(224))
                        });
                    ensure!(
                        confirmed,
                        "Extreme=224 needs completed firmware/version support discovery; it was not guessed"
                    );
                }
                actions.push(A::SetRawMode(target));
            }
            mapped(&mut actions, payload, "power.fnq_custom", M::GodModeFnQ)?;
            nonempty(actions)
        }
        "display.apply" => display(payload),
        "display.turn_off" => {
            known(payload, &[])?;
            Ok(vec![A::System(S::ScreenOff)])
        }
        "keys.apply" | "speaker.apply" => keys(payload),
        "battery.apply" => {
            known(payload, &["battery.mode", "battery.night_charge"])?;
            let mut actions = vec![];
            if let Some(v) = value(payload, "battery.mode") {
                actions.push(A::Energy(E::Battery(battery_mode(v)?)));
            }
            if let Some(v) = value(payload, "battery.night_charge") {
                actions.push(A::Energy(E::NightCharge(bool_value(
                    v,
                    "battery.night_charge",
                )?)));
            }
            nonempty(actions)
        }
        "battery.threshold.apply" => {
            let enabled = "battery.threshold_enabled";
            let start = "battery.start_threshold";
            let stop = "battery.stop_threshold";
            known(payload, &[enabled, start, stop])?;
            let threshold = Threshold {
                enabled: bool_value(required(payload, enabled)?, enabled)?,
                start: unsigned(required(payload, start)?, start, THRESHOLD_MAXIMUM)?,
                stop: unsigned(required(payload, stop)?, stop, THRESHOLD_MAXIMUM)?,
            };
            Ok(vec![A::ChargeThreshold(threshold.validate()?)])
        }
        "battery.threshold.read" => {
            known(payload, &[])?;
            Ok(vec![A::ReadChargeThreshold])
        }
        "power.accessories.apply" => {
            known(
                payload,
                &[
                    "power.always_on_usb",
                    "power.instant_boot",
                    "power.flip_to_start",
                ],
            )?;
            let mut actions = vec![];
            if let Some(v) = value(payload, "power.always_on_usb") {
                actions.push(A::Energy(E::AlwaysOnUsb(usb_mode(v)?)));
            }
            if let Some(v) = value(payload, "power.instant_boot") {
                let mode = instant_boot(v)?;
                actions.push(A::SetMapped {
                    feature: M::InstantBootAc,
                    value: mode & 1,
                });
                actions.push(A::SetMapped {
                    feature: M::InstantBootUsb,
                    value: (mode >> 1) & 1,
                });
            }
            mapped(&mut actions, payload, "power.flip_to_start", M::FlipToStart)?;
            nonempty(actions)
        }
        "keyboard.apply" | "keyboard.preview" => keyboard(payload),
        "keyboard.effects.refresh" | "keyboard.effects.reset" | "keyboard.effects.apply" => {
            effects(command, payload)
        }
        "gpu.oc.apply" => gpu_oc(payload),
        "gpu.keep_awake.apply" => {
            known(payload, &["gpu.keep_awake"])?;
            let enabled = bool_value(required(payload, "gpu.keep_awake")?, "GPU keep awake")?;
            if !enabled {
                return Ok(vec![A::SetGpuKeepAwake {
                    expected_name: String::new(),
                    enabled: false,
                }]);
            }
            let snapshot: HybridSnapshot = serde_json::from_value(
                context(payload, "hybrid")
                    .and_then(|v| v.get("snapshot"))
                    .context("Completed GPU routing discovery required")?
                    .clone(),
            )?;
            let ac = context(payload, "ac_connected")
                .and_then(Value::as_bool)
                .context("Completed AC status required")?;
            jiks_platform::gpu_awake::validate_policy(&snapshot, ac)?;
            let name = text(
                context(payload, "gpu")
                    .and_then(|v| v.get("name"))
                    .context("Explicit selected NVIDIA inventory required")?,
                "GPU name",
            )?;
            Ok(vec![A::SetGpuKeepAwake {
                expected_name: name.into(),
                enabled: true,
            }])
        }
        "gpu.sleep" => {
            known(payload, &[])?;
            let snapshot: HybridSnapshot = serde_json::from_value(
                context(payload, "hybrid")
                    .and_then(|v| v.get("snapshot"))
                    .context("Completed GPU routing discovery required")?
                    .clone(),
            )?;
            let ac = context(payload, "ac_connected")
                .and_then(Value::as_bool)
                .context("Completed AC status required")?;
            jiks_platform::gpu_sleep::validate_policy(&snapshot, ac)?;
            ensure!(
                snapshot.igpu_backend.is_some(),
                "No verified GPU notification backend"
            );
            Ok(vec![A::RequestGpuSleep])
        }
        "gpu.process.kill" => {
            known(payload, &["selection.gpu.processes"])?;
            Ok(vec![A::KillGpuProcesses(selected_gpu_processes(payload)?)])
        }
        "gpu.process.preference" => {
            known(
                payload,
                &["selection.gpu.processes", "gpu.process.preference"],
            )?;
            let mut targets = selected_gpu_processes(payload)?;
            ensure!(
                targets.len() == 1,
                "GPU preference expects one selected application"
            );
            let preference = jiks_platform::gpu_management::GpuPreference::parse(text(
                required(payload, "gpu.process.preference")?,
                "GPU preference",
            )?)?;
            Ok(vec![A::SetGpuPreference {
                process: targets.remove(0),
                preference,
            }])
        }
        "gpu.restart" => {
            known(payload, &["gpu.instance_id", "gpu.require_started"])?;
            let nodes = context(payload, "gpu_nodes")
                .and_then(Value::as_array)
                .context("Completed NVIDIA PnP inventory is required")?;
            let expected = value(payload, "gpu.instance_id")
                .map(|v| text(v, "GPU PnP instance"))
                .transpose()?;
            let nodes = nodes
                .iter()
                .filter_map(|node| node.get("instance_id").and_then(Value::as_str))
                .filter(|id| expected.is_none_or(|expected| id.eq_ignore_ascii_case(expected)))
                .collect::<Vec<_>>();
            ensure!(
                nodes.len() == 1,
                "Select an exact unique NVIDIA PnP device; no first-device fallback"
            );
            Ok(vec![A::RestartGpu {
                instance_id: nodes[0].into(),
                require_started: value(payload, "gpu.require_started")
                    .map(|v| bool_value(v, "GPU started-state guard"))
                    .transpose()?.unwrap_or(false),
            }])
        }
        "gpu.processes.refresh" => {
            known(payload, &["gpu.index", "gpu.name"])?;
            let gpu = context(payload, "gpu").context("Explicit GPU inventory is required")?;
            let index = unsigned(
                gpu.get("index").context("GPU index missing")?,
                "GPU index",
                63,
            )?;
            let name = text(gpu.get("name").context("GPU name missing")?, "GPU name")?;
            Ok(vec![A::ReadGpuProcesses {
                index,
                expected_name: name.into(),
            }])
        }
        "gpu.pstate.refresh" | "gpu.pstate.apply" => {
            known(payload, &["gpu.index", "gpu.name", "gpu.locked_pstate"])?;
            let gpu = context(payload, "gpu").context("Explicit GPU inventory required")?;
            let index = unsigned(
                gpu.get("index").context("GPU index missing")?,
                "GPU index",
                63,
            )?;
            let name = text(gpu.get("name").context("GPU name missing")?, "GPU name")?;
            if command == "gpu.pstate.refresh" {
                ensure!(
                    value(payload, "gpu.locked_pstate").is_none(),
                    "Refresh cannot carry a P-state change"
                );
                return Ok(vec![A::ReadGpuPerformance {
                    index,
                    expected_name: name.into(),
                }]);
            }
            let state = integer(
                required(payload, "gpu.locked_pstate")?,
                "GPU locked P-state",
            )?;
            ensure!(
                (-1..=15).contains(&state),
                "P-state must be -1(auto) or a reported 0..15 ID"
            );
            let performance = context(payload, "gpu_performance")
                .context("Completed explicit performance-state discovery required")?;
            let states = performance
                .get("states")
                .and_then(Value::as_array)
                .context("No advertised P-state list")?;
            ensure!(
                state == -1
                    || states
                        .iter()
                        .any(|v| v.get("state").and_then(Value::as_i64) == Some(i64::from(state))),
                "P-state was not advertised by selected GPU"
            );
            Ok(vec![A::SetGpuPerformance {
                index,
                expected_name: name.into(),
                state: (state >= 0).then_some(state as u32),
                deadline_tick_ms: deadline(payload)?,
            }])
        }
        "gpu.hybrid.apply" => {
            known(payload, &["gpu.hybrid_mode"])?;
            let v = required(payload, "gpu.hybrid_mode")?;
            let mode = if let Some(s) = v.as_str() {
                match s.to_ascii_lowercase().as_str() {
                    "on" | "0" => 0,
                    "onigpuonly" | "igpuonly" | "1" => 1,
                    "onauto" | "auto" | "2" => 2,
                    "uma" | "3" => 3,
                    "off" | "discrete" | "4" => 4,
                    _ => bail!("Unknown hybrid mode"),
                }
            } else {
                unsigned(v, "gpu.hybrid_mode", 4)?
            };
            let mode = HybridMode::from_raw(mode)?;
            let snapshot: HybridSnapshot = serde_json::from_value(
                context(payload, "hybrid")
                    .and_then(|v| v.get("snapshot"))
                    .context("Completed hybrid discovery is required")?
                    .clone(),
            )?;
            hybrid::plan(&snapshot, mode)?;
            Ok(vec![A::SetHybrid {
                mode,
                deadline_tick_ms: deadline(payload)?,
            }])
        }
        "power.ai.apply" | "balance.ai.apply" => {
            known(payload, &["power.ai_submode"])?;
            let submode = unsigned(
                required(payload, "power.ai_submode")?,
                "power.ai_submode",
                u32::MAX,
            )?;
            let snapshot: jiks_platform::ai::AiSnapshot = serde_json::from_value(
                context(payload, "ai")
                    .context("Completed AI discovery is required")?
                    .clone(),
            )?;
            jiks_platform::ai::validate(&snapshot, submode, false)?;
            Ok(vec![A::SetAiSubmode {
                submode,
                deadline_tick_ms: deadline(payload)?,
            }])
        }
        "preset.apply" => preset(payload, true),
        "god_mode.apply" => preset(payload, false),
        "power.plan.apply" => {
            known(payload, &["power.plan"])?;
            let id = text(required(payload, "power.plan")?, "power.plan")?;
            parse_guid(id)?;
            Ok(vec![A::System(S::PowerPlan(id.into()))])
        }
        "power.overlay.apply" => {
            known(payload, &["power.overlay"])?;
            Ok(vec![A::System(S::PowerMode(windows_mode(required(
                payload,
                "power.overlay",
            )?)?))])
        }
        "thermal.stop" => {
            known(payload, &[])?;
            Ok(vec![A::StopCpu])
        }
        "thermal.enable" => {
            known(payload, &["thermal.firmware_limit"])?;
            let limit = unsigned(
                required(payload, "thermal.firmware_limit")?,
                "thermal.firmware_limit",
                88,
            )?;
            ensure!(limit > 0, "Firmware limit must be positive");
            Ok(vec![A::EnableCpu {
                firmware_limit: limit,
            }])
        }
        "read.cpu" => Ok(vec![A::ReadCpu]),
        "read.temperature" => Ok(vec![A::ReadCpuTemperature]),
        "read.god_mode" => Ok(vec![A::ReadGodMode]),
        "read.capabilities" => Ok(vec![A::ReadCapabilities]),
        "read.power_modes" => Ok(vec![A::ReadPowerModes]),
        "read.hybrid" => Ok(vec![A::ReadHybrid]),
        "read.ai" => Ok(vec![A::ReadAi]),
        "read.intel_sensors" => Ok(vec![A::ReadIntelSensors]),
        "read.devices" => Ok(vec![A::ReadDevices]),
        "read.gpu_nodes" => Ok(vec![A::ReadGpuDeviceNodes]),
        "read.gpu_awake" => Ok(vec![A::ReadGpuKeepAwake]),
        "read.wireless" => Ok(vec![A::ReadWireless]),
        "network.apply" => {
            known(payload, &["network.wifi", "network.airplane"])?;
            let wifi = value(payload, "network.wifi")
                .map(|v| bool_value(v, "network.wifi"))
                .transpose()?;
            let airplane = value(payload, "network.airplane")
                .map(|v| bool_value(v, "network.airplane"))
                .transpose()?;
            ensure!(
                !(wifi == Some(true) && airplane == Some(true)),
                "Wi-Fi on and airplane mode on conflict in one request"
            );
            let mut actions = Vec::new();
            if let Some(on) = airplane {
                actions.push(A::SetAirplaneMode(on));
            }
            if let Some(on) = wifi {
                actions.push(A::SetWifiEnabled(on));
            }
            nonempty(actions)
        }
        "read.igpu" => Ok(vec![A::ReadIntegratedGpuInventory]),
        "gpu.sensors.stop" => {
            known(payload, &[])?;
            Ok(vec![A::StopGpuSensors])
        }
        "read.gpu_sensors" => {
            known(payload, &[])?;
            let (index, expected_name) = gpu_address(payload)?;
            Ok(vec![A::ReadGpuSensors {
                index,
                expected_name,
            }])
        }
        "read.energy" => Ok(vec![A::ReadEnergy]),
        "read.system" => Ok(vec![A::ReadSystem]),
        "keys.driver.start"=>{known(payload,&[])?;Ok(vec![A::StartDriverKeyEvents])},
        "keys.driver.read"=>{known(payload,&[])?;Ok(vec![A::ReadDriverKeyEvents])},
        "keys.driver.stop"=>{known(payload,&[])?;Ok(vec![A::StopDriverKeyEvents])},
        "read.boot_logo" => Ok(vec![A::ReadBootLogo]),
        "read.autorun" => Ok(vec![A::ReadAutorun]),
        "app.autorun.apply" => {
            known(payload, &["app.autorun"])?;
            let mode = match text(required(payload, "app.autorun")?, "autorun")? {
                "off" => jiks_platform::autorun::Mode::Off,
                "normal" => jiks_platform::autorun::Mode::Normal,
                "delayed" => jiks_platform::autorun::Mode::Delayed,
                _ => bail!("Autorun must be off/normal/delayed"),
            };
            Ok(vec![A::SetAutorun(mode)])
        }
        "display.boot_logo.apply" => {
            known(
                payload,
                &[
                    "display.boot_logo.path",
                    "display.boot_logo.enabled",
                    "display.boot_animation",
                ],
            )?;
            let enabled = value(payload, "display.boot_logo.enabled")
                .map(|v| bool_value(v, "boot logo enabled"))
                .transpose()?;
            let source = value(payload, "display.boot_logo.path")
                .map(|v| text(v, "boot logo path"))
                .transpose()?
                .filter(|v| !v.trim().is_empty());
            let animation_disabled = value(payload, "display.boot_animation")
                .map(|v| bool_value(v, "boot animation").map(|v| !v))
                .transpose()?;
            let logo = if enabled == Some(false) {
                Some(jiks_platform::boot_logo::LogoChange::Disable)
            } else if let Some(source) = source {
                ensure!(
                    std::path::Path::new(source).is_absolute(),
                    "Boot logo requires an absolute image path"
                );
                Some(jiks_platform::boot_logo::LogoChange::Enable {
                    source: source.into(),
                })
            } else {
                ensure!(
                    enabled != Some(true),
                    "Select an image before enabling the custom logo"
                );
                None
            };
            ensure!(
                logo.is_some() || animation_disabled.is_some(),
                "Select a boot image, disable the logo, or change boot animation"
            );
            Ok(vec![A::BootLogo(
                jiks_platform::boot_logo::Request::Apply {
                    logo,
                    animation_disabled,
                },
            )])
        }
        "display.boot_logo.disable" => {
            known(payload, &[])?;
            Ok(vec![A::BootLogo(
                jiks_platform::boot_logo::Request::Apply {
                    logo: Some(jiks_platform::boot_logo::LogoChange::Disable),
                    animation_disabled: None,
                },
            )])
        }
        "display.boot_logo.restore" => {
            known(payload, &["selection.boot_logo_backups"])?;
            let id = text(
                required(payload, "selection.boot_logo_backups")?,
                "boot backup",
            )?;
            ensure!(
                id.starts_with("boot-logo-")
                    && id.len() <= 100
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "Invalid boot backup selection"
            );
            Ok(vec![A::BootLogo(
                jiks_platform::boot_logo::Request::Restore {
                    backup_id: id.into(),
                },
            )])
        }
        "display.boot_logo.retry_recovery" => {
            known(payload, &[])?;
            Ok(vec![A::BootLogo(
                jiks_platform::boot_logo::Request::RetryRecovery,
            )])
        }
        "software.refresh" | "read.software" => Ok(vec![A::ReadSoftware]),
        "software.apply" => {
            let keys = [
                "software.vantage_disabled",
                "software.zone_disabled",
                "software.space_disabled",
                "software.hotkeys_disabled",
            ];
            known(payload, &keys)?;
            let mut changes = std::collections::BTreeMap::new();
            for (key, id) in keys
                .into_iter()
                .zip(["vantage", "zone", "space", "hotkeys"])
            {
                if let Some(value) = value(payload, key) {
                    changes.insert(id.into(), bool_value(value, key)?);
                }
            }
            ensure!(!changes.is_empty(), "Select at least one software product");
            let deadline_tick_ms = payload
                .get("deadline_tick_ms")
                .and_then(Value::as_u64)
                .context("Software absolute deadline is required")?;
            Ok(vec![A::SetSoftware {
                changes,
                deadline_tick_ms,
            }])
        }
        "read.display" => Ok(vec![A::ReadDisplayControls]),
        "read.firmware" => Ok(vec![A::ReadFirmwareSensors]),
        "read.telemetry" => Ok(vec![A::ReadTelemetry]),
        "read.fan_table" => Ok(vec![A::ReadFanTable]),
        "keyboard.candidates" => Ok(vec![A::ReadSpectrumCandidates]),
        "gpu.refresh" => Ok(vec![A::ReadGpuInventory]),
        "gpu.oc.refresh" => {
            let (index, expected_name) = gpu_address(payload)?;
            Ok(vec![A::ReadGpuOverclock {
                index,
                expected_name,
            }])
        }
        _ => bail!(
            "{command} has no standalone hardware route; runtime must handle persistence/coordination or keep this capability unavailable"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_request_is_fully_validated_before_radio_actions() {
        assert!(
            plan(
                "network.apply",
                &json!({"network.wifi":true,"network.airplane":true})
            )
            .is_err()
        );
        assert!(
            plan(
                "network.apply",
                &json!({"network.wifi":true,"network.airplane":"invalid"})
            )
            .is_err()
        );
        let p = plan(
            "network.apply",
            &json!({"network.wifi":true,"network.airplane":false}),
        )
        .unwrap();
        assert!(matches!(
            p.as_slice(),
            [A::SetAirplaneMode(false), A::SetWifiEnabled(true)]
        ));
    }
    #[test]
    fn drr_routes_keep_legacy_pair_and_require_completed_advertisement() {
        let rate = json!({"Frequency":240,"IsDynamic":true,"BaseFrequency":120});
        let context = json!({"display":{"device":"\\\\.\\DISPLAY1","monitor_path":"fake-path","width":2560,"height":1600,"hertz":120,"refresh":{"choices":[rate]}}});
        let p = json!({"display.refresh_rate":rate,"_context":context});
        assert!(
            matches!(plan("display.apply",&p).unwrap().as_slice(),[A::Display(D::RefreshRate{rate,..})]if rate.frequency==240&&rate.base_frequency==120)
        );
        let mut p = p;
        p["display.refresh_rate"] = json!("dynamic:120:240");
        assert!(plan("display.apply", &p).is_ok());
        p["display.refresh_rate"] = json!("dynamic:60:240");
        assert!(plan("display.apply", &p).is_err());
        p["display.refresh_rate"] = rate;
        p["display.resolution"] = json!("1920x1080");
        assert!(plan("display.apply", &p).is_err());
    }
    #[test]
    fn logical_locks_mutes_and_white_levels_follow_original_feature_truth_table() {
        for on in [false, true] {
            let actions=plan("keys.apply",&json!({"keys.win_lock":on,"keys.touchpad_lock":on,"keys.microphone_muted":on,"speaker.muted":on,"keys.fn_lock":on,"keyboard.white_single":on})).unwrap();
            assert!(actions.iter().any(|a|matches!(a,A::System(S::Switch{which:GameZoneSwitch::WinKey,value})if *value==u32::from(on))));
            assert!(actions.iter().any(|a|matches!(a,A::System(S::Switch{which:GameZoneSwitch::TouchpadLock,value})if *value==u32::from(on))));
            assert!(
                actions
                    .iter()
                    .any(|a| matches!(a,A::SetMicrophoneMute(value)if *value==on))
            );
            assert!(
                actions
                    .iter()
                    .any(|a| matches!(a,A::SetSpeakerMute(value)if *value==on))
            );
            assert!(
                actions
                    .iter()
                    .any(|a| matches!(a,A::Energy(E::FnLock(value))if *value==on))
            );
            assert!(
                actions
                    .iter()
                    .any(|a| matches!(a,A::Energy(E::OneLevelWhite(value))if *value==on))
            );
        }
        for level in 0..=2 {
            let actions=plan("keys.apply",&json!({"keyboard.white_level":level,"_context":{"keyboard":{"white_backend":"wmi"}}})).unwrap();
            assert!(
                matches!(actions[0],A::SetLighting{lighting_id:0,state_type:0,brightness}if brightness==level+1)
            );
        }
    }
    #[test]
    fn gpu_awake_cleanup_needs_no_inventory_but_enable_and_sleep_need_verified_routing() {
        assert!(matches!(
            plan("gpu.keep_awake.apply", &json!({"gpu.keep_awake":false})).unwrap()[0],
            A::SetGpuKeepAwake { enabled: false, .. }
        ));
        assert!(plan("gpu.keep_awake.apply", &json!({"gpu.keep_awake":true})).is_err());
        let snapshot = HybridSnapshot {
            gsync: Some(false),
            igpu_mode: Some(1),
            igpu_backend: Some(jiks_platform::hybrid::IgpuBackend::GameZone),
            ..Default::default()
        };
        let context = json!({"hybrid":{"snapshot":snapshot},"ac_connected":true,"gpu":{"name":"GeForce fixture","index":0}});
        assert!(matches!(
            plan("gpu.sleep", &json!({"_context":context})).unwrap()[0],
            A::RequestGpuSleep
        ));
        assert!(
            plan(
                "gpu.keep_awake.apply",
                &json!({"gpu.keep_awake":true,"_context":context})
            )
            .is_err()
        );
    }
    #[test]
    fn gpu_process_selection_uses_creation_token_and_preflights_entire_batch() {
        let ctx = json!({"gpu_processes":{"processes":[{"identity":{"pid":10,"creation_filetime":100,"executable":"C:\\fixture\\game.exe"},"preference":"Automatic"}]}});
        let p = json!({"selection.gpu.processes":"10:100","_context":ctx});
        assert!(
            matches!(&plan("gpu.process.kill",&p).unwrap()[0],A::KillGpuProcesses(v)if v.len()==1&&v[0].creation_filetime==100)
        );
        let mut bad = p;
        bad["selection.gpu.processes"] = json!(["10:100", "11:100"]);
        assert!(plan("gpu.process.kill", &bad).is_err());
        bad["selection.gpu.processes"] = json!("10");
        assert!(plan("gpu.process.kill", &bad).is_err());
    }
    #[test]
    fn gpu_restart_has_no_implicit_first_device_fallback() {
        let mut p = json!({"_context":{"gpu_nodes":[{"instance_id":"PCI\\VEN_10DE&DEV_A\\1"},{"instance_id":"PCI\\VEN_10DE&DEV_B\\2"}]}});
        assert!(plan("gpu.restart", &p).is_err());
        p["gpu.instance_id"] = json!("PCI\\VEN_10DE&DEV_B\\2");
        assert!(
            matches!(&plan("gpu.restart",&p).unwrap()[0],A::RestartGpu{instance_id, require_started: false}if instance_id.ends_with("\\2"))
        );
    }
    #[test]
    fn late_invalid_field_never_returns_a_partial_plan() {
        assert!(
            plan(
                "keys.apply",
                &json!({"keys.fn_lock":true,"speaker.volume":101})
            )
            .is_err()
        );
        assert!(
            plan(
                "power.apply",
                &json!({"power.mode":3,"power.fnq_custom":"bad"})
            )
            .is_err()
        );
        assert!(
            plan(
                "battery.apply",
                &json!({"battery.mode":"Normal","battery.start_threshold":50})
            )
            .is_err()
        );
    }
    #[test]
    fn structured_display_targets_are_explicit_and_complete() {
        let p = json!({"display.resolution":"2560x1600","display.refresh_rate":165,"display.hdr":false,"_context":{"display":{"device":"\\\\.\\DISPLAY1","monitor_path":"\\\\?\\DISPLAY#fixture","internal":true}}});
        let actions = plan("display.apply", &p).unwrap();
        assert!(matches!(
            &actions[0],
            A::SetDisplayMode {
                width: 2560,
                height: 1600,
                hertz: 165,
                ..
            }
        ));
        assert_eq!(actions.len(), 2);
        assert!(plan("display.apply", &json!({"display.refresh_rate":165})).is_err());
    }
    #[test]
    fn external_brightness_and_bad_late_hdr_fail_before_any_action() {
        assert!(plan("display.apply",&json!({"display.overdrive":true,"display.brightness":50,"_context":{"display":{"internal":false}}})).is_err());
        assert!(
            plan(
                "display.apply",
                &json!({"display.brightness":50,"display.hdr":3})
            )
            .is_err()
        );
    }
    #[test]
    fn preset_omission_never_invents_a_fan_table() {
        let actions=plan("preset.apply",&json!({"deadline_tick_ms":100,"preset":{"values":{"cpu.pl1":70,"cpu.pl2":125,"cpu.temperature":88}}})).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0],A::ApplyGodMode(p)if p.values.len()==3));
    }
    #[test]
    fn entire_preset_and_override_is_checked_before_return() {
        assert!(plan("preset.apply",&json!({"deadline_tick_ms":100,"preset":{"values":{"cpu.pl1":70},"fan":{"table":[1,1,1,1,1,1,1,1,1,1]}}})).is_err());
        assert!(plan("preset.apply",&json!({"deadline_tick_ms":100,"preset":{"values":{"cpu.pl1":70},"overrides":{"power.plan":"bad"}}})).is_err());
        assert!(
            plan(
                "preset.apply",
                &json!({"deadline_tick_ms":100,"preset":{"values":{"cpu.pl1":140,"cpu.pl2":125}}})
            )
            .is_err()
        );
    }
    #[test]
    fn valid_explicit_fan_frame_has_ten_levels_and_no_defaults() {
        let actions=plan("preset.apply",&json!({"preset":{"values":{},"fan":{"table":[1,1,1,1,1,1,1,1,3,5],"full_speed":false}}})).unwrap();
        assert_eq!(actions.len(), 2);
        assert!(
            matches!(&actions[0],A::SetFanTable(v)if v.len()==64&&v[2..6]==10u32.to_le_bytes())
        );
    }
    #[test]
    fn keyboard_validates_codec_before_brightness_or_profile() {
        let p = json!({"keyboard.path":"\\\\?\\hid#fixture","keyboard.profile":2,"keyboard.brightness":9,"keyboard.effect":"Always","keyboard.zones":[1,24],"keyboard.color":"#ff0000","keyboard.speed":2,"keyboard.direction":"LeftToRight"});
        assert_eq!(plan("keyboard.apply", &p).unwrap().len(), 3);
        let mut bad = p;
        bad["keyboard.color"] = json!("#gg0000");
        assert!(plan("keyboard.apply", &bad).is_err());
        assert!(plan("keyboard.apply",&json!({"keyboard.path":"\\\\?\\hid#fixture","keyboard.profile":2,"keyboard.effect":"Always"})).is_err());
    }
    #[test]
    fn gpu_identity_and_monotonic_deadline_are_mandatory() {
        let p = json!({"gpu.oc.enabled":true,"gpu.oc.core_offset_mhz":100,"gpu.oc.memory_offset_mhz":200,"gpu.oc.voltage_mv":900,"gpu.oc.lock_voltage":false,"deadline_tick_ms":100,"_context":{"gpu":{"index":0,"name":"RTX fixture"}}});
        assert!(
            matches!(&plan("gpu.oc.apply",&p).unwrap()[0],A::SetGpuOverclock{deadline_tick_ms:100,requested,..}if requested.voltage_cap_mv==900)
        );
        let mut bad = p;
        bad.as_object_mut().unwrap().remove("deadline_tick_ms");
        assert!(plan("gpu.oc.apply", &bad).is_err());
    }
    #[test]
    fn igpu_enum_never_claims_a_mux_transition() {
        assert!(plan("gpu.hybrid.apply", &json!({"gpu.hybrid_mode":"Off"})).is_err());
        assert!(plan("gpu.hybrid.apply", &json!({"gpu.hybrid_mode":"OnIGPUOnly"})).is_err());
        assert!(
            plan(
                "gpu.hybrid.apply",
                &json!({"gpu.hybrid_mode":"OnIGPUOnly","_context":{"gpu":{"gsync_enabled":false}}})
            )
            .is_err()
        );
        let snapshot = HybridSnapshot {
            gsync: Some(false),
            igpu_mode: Some(1),
            igpu_backend: Some(jiks_platform::hybrid::IgpuBackend::GameZone),
            ..Default::default()
        };
        let p = json!({"gpu.hybrid_mode":"Off","deadline_tick_ms":10,"_context":{"hybrid":{"snapshot":snapshot}}});
        assert!(matches!(
            plan("gpu.hybrid.apply", &p).unwrap()[0],
            A::SetHybrid {
                mode: HybridMode::Off,
                deadline_tick_ms: 10
            }
        ));
        let mut p = p;
        p["gpu.hybrid_mode"] = json!("UMA");
        assert!(plan("gpu.hybrid.apply", &p).is_err());
    }
    #[test]
    fn command_names_do_not_deserialize_arbitrary_raw_hardware_actions() {
        assert!(plan("hardware.action", &json!({"SetRawMode":255})).is_err());
        assert!(plan("power.apply", &json!({"power.mode":255,"unexpected":1})).is_err());
        assert!(plan("preset.apply", &json!({"selection.presets":"unresolved"})).is_err());
    }
    #[test]
    fn comma_zone_input_and_multieffect_contract_use_the_same_verified_codec() {
        assert_eq!(zones(&json!("1, 2，24")).unwrap(), vec![1, 2, 24]);
        assert!(zones(&json!("1,,2")).is_err());
        assert!(zones(&json!("25")).is_err());
        let p = json!({"profile":2,"effects":[{"Type":"Always","Speed":"None","Direction":"None","ClockwiseDirection":"None","ColorMode":"ColorList","Colors":[{"R":255,"G":0,"B":0}],"Keys":[1,24],"UseVantageColorBoost":false}],"_context":{"keyboard":{"path":"\\\\?\\hid#fixture"}}});
        assert!(
            matches!(&plan("keyboard.effects.apply",&p).unwrap()[0],A::Spectrum(SpectrumAction::Effects{profile,..})if profile.effects.len()==1)
        );
        assert!(matches!(
            &plan(
                "keyboard.effects.reset",
                &json!({"profile":2,"_context":{"keyboard":{"path":"\\\\?\\hid#fixture"}}})
            )
            .unwrap()[0],
            A::Spectrum(SpectrumAction::Reset { profile: 2, .. })
        ));
        let mut invalid = p;
        invalid["effects"][0]["Keys"] = json!([25]);
        assert!(plan("keyboard.effects.apply", &invalid).is_err());
    }
    #[test]
    fn extreme_route_requires_explicit_cached_support() {
        assert!(plan("power.apply", &json!({"power.mode":224})).is_err());
        assert!(matches!(&plan("power.apply",&json!({"power.mode":"Extreme","_context":{"power_modes":{"modes":[{"raw":224,"name":"超能"}]}}})).unwrap()[0],A::SetRawMode(224)));
    }
    #[test]
    fn context_resolves_the_selected_external_monitor_without_first_display_fallback() {
        let cache = BTreeMap::from([
            (
                "devices".into(),
                json!({"displays":{"available":true,"value":[{"name":"\\\\.\\DISPLAY1","current":{"width":1920,"height":1080,"hertz":60}},{"name":"\\\\.\\DISPLAY2","current":{"width":2560,"height":1440,"hertz":144}}]}}),
            ),
            (
                "display_controls".into(),
                json!([{"monitor_path":"\\\\?\\DISPLAY#one","source_name":"\\\\.\\DISPLAY1","internal":true},{"monitor_path":"\\\\?\\DISPLAY#two","source_name":"\\\\.\\DISPLAY2","internal":false}]),
            ),
        ]);
        let context =
            ActionContext::from_cache(&cache, &json!({"display.monitor":"\\\\?\\DISPLAY#two"}))
                .unwrap()
                .into_value();
        assert_eq!(context["display"]["device"], "\\\\.\\DISPLAY2");
        assert_eq!(context["display"]["hertz"], 144);
        assert!(ActionContext::from_cache(&cache, &json!({"display.monitor":"missing"})).is_err());
        assert!(ActionContext::from_cache(&cache,&json!({"display.monitor":{"monitor_path":"\\\\?\\DISPLAY#two","device":"\\\\.\\DISPLAY1"}})).is_err());
        assert!(
            ActionContext::from_cache(&cache, &json!({}))
                .unwrap()
                .into_value()
                .get("display")
                .is_none()
        );
    }
    #[test]
    fn context_uses_completed_power_gpu_effect_metadata_and_ignores_injected_context() {
        let cache = BTreeMap::from([
            (
                "system".into(),
                json!({"ac_connected":{"available":true,"value":false}}),
            ),
            ("gpu".into(), json!([{"index":0,"name":"RTX fixture"}])),
            (
                "gpu.oc".into(),
                json!({"core_range":{"driver_reported":true,"min_mhz":-100,"max_mhz":100}}),
            ),
            ("keyboard.effects".into(), json!({"profile":2,"effects":[]})),
        ]);
        let context = ActionContext::from_cache(
            &cache,
            &json!({"_context":{"ac_connected":true,"gpu":{"index":999,"name":"forged"}}}),
        )
        .unwrap()
        .into_value();
        assert_eq!(context["ac_connected"], false);
        assert_eq!(context["gpu"]["index"], 0);
        assert_eq!(context["keyboard"]["effects"]["profile"], 2);
        assert!(context["gpu"]["oc"].is_object());
        assert!(
            ActionContext::from_cache(&cache, &json!({"gpu.index":1,"gpu.name":"RTX fixture"}))
                .is_err()
        );
    }
}
