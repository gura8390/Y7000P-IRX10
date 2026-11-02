//! Built-in Fn actions. Callbacks only enqueue codes; this coordinator waits
//! for fresh read receipts, actual hardware completion, and UI acknowledgements.
use super::*;
use jiks_platform::dynamic_refresh::RefreshRate;
use std::collections::BTreeSet;
#[derive(Default)]
pub(super) struct BuiltinKeys {
    queue: VecDeque<u32>,
    job: Option<Job>,
    stopping: bool,
    last_refresh: Option<u64>,
}
struct Job {
    code: u32,
    stage: Stage,
}
enum Stage {
    Reads {
        requests: Vec<(u64, String, bool)>,
        began: u64,
        fresh: BTreeSet<String>,
        failure: Option<String>,
        interference: bool,
    },
    Resolve {
        fresh: BTreeSet<String>,
        completed: u64,
    },
    Write {
        request: u64,
        notification: Value,
    },
    Ui {
        request: u64,
    },
}
enum Decision {
    Write {
        command: &'static str,
        payload: Value,
        notice: Value,
    },
    Notify(Value),
    Noop(&'static str),
}
impl BuiltinKeys {
    pub(super) fn queue(&mut self, code: u32) -> Result<()> {
        ensure!(
            matches!(
                code,
                42 | 4 | 45 | 16 | 0x0041002a | 62 | 65792 | 63 | 73728 | 65537 | 65568 | 69632
            ) || notification(code).is_some(),
            "没有该Fn事件的内置行为"
        );
        ensure!(!self.stopping, "Fn内置动作正在等待实际清理");
        ensure!(self.queue.len() < 32, "Fn内置动作队列已满");
        if matches!(code, 16 | 0x0041002a) {
            let now = monotonic_ms();
            if self
                .last_refresh
                .is_some_and(|last| now.saturating_sub(last) < 2000)
            {
                return Ok(());
            }
            self.last_refresh = Some(now);
        }
        self.queue.push_back(code);
        Ok(())
    }
    pub(super) fn is_busy(&self) -> bool {
        self.job.is_some() || !self.queue.is_empty()
    }
    pub(super) fn cancel(&mut self) {
        self.stopping = true;
        self.queue.clear();
    }
    pub(super) fn resume(&mut self) -> Result<()> {
        ensure!(!self.is_busy(), "旧Fn动作尚未实际结束");
        self.stopping = false;
        self.last_refresh = None;
        Ok(())
    }
    fn status(engine: &Engine, message: impl Into<String>) {
        engine.value("keys.builtin.status", json!(message.into()));
    }
    fn ui(&mut self, engine: &mut Engine, code: u32, kind: &str, payload: Value) -> Result<()> {
        let request = engine.request_ui_effect(kind, payload)?;
        self.job = Some(Job {
            code,
            stage: Stage::Ui { request },
        });
        Ok(())
    }
    fn begin_reads(&mut self, engine: &mut Engine, code: u32) -> Result<()> {
        let reads = match code {
            16 | 0x0041002a => vec![
                ("display_controls", A::ReadDisplayControls, true),
                ("devices", A::ReadDevices, true),
            ],
            62 | 65792 => vec![("devices", A::ReadDevices, true)],
            63 | 73728 => vec![("wireless", A::ReadWireless, true)],
            65537 => vec![("power_modes", A::ReadPowerModes, true)],
            65568 => vec![("system", A::ReadSystem, true)],
            69632 => vec![
                ("energy", A::ReadEnergy, false),
                ("lighting.0", A::ReadLighting { lighting_id: 0 }, false),
            ],
            _ => bail!("Unsupported Fn read action"),
        };
        let began = monotonic_ms();
        let mut requests = Vec::new();
        let mut failure = None;
        for (key, action, critical) in reads {
            let id = engine.new_internal_request()?;
            if let Err(error) = engine.request_read(id, key.into(), action) {
                engine.finish(id, false, format!("{error:#}"));
                if critical {
                    failure = Some(format!("{error:#}"));
                }
            }
            requests.push((id, key.into(), critical));
        }
        self.job = Some(Job {
            code,
            stage: Stage::Reads {
                requests,
                began,
                fresh: BTreeSet::new(),
                failure,
                interference: false,
            },
        });
        Ok(())
    }
    pub(super) fn poll(&mut self, engine: &mut Engine) -> Result<()> {
        if engine.read_only || engine.faulted || engine.shutdown.is_some() {
            self.cancel();
        }
        if let Some(mut job) = self.job.take() {
            match &mut job.stage {
                Stage::Reads {
                    requests,
                    began,
                    fresh,
                    failure,
                    interference,
                } => {
                    *interference |= engine.pending.is_some();
                    let mut waiting = Vec::new();
                    for (id, key, critical) in std::mem::take(requests) {
                        if let Some(result) = engine.take_internal_result(id) {
                            let receipt = engine.read_receipts.remove(&id);
                            let result = result.map_err(anyhow::Error::msg).and_then(|_| {
                                let (requested, completed) =
                                    receipt.context("读取没有实际完成时间")?;
                                ensure!(
                                    requested >= *began
                                        && completed >= requested
                                        && monotonic_ms().saturating_sub(completed) <= 1000,
                                    "Fn读取已过期或早于按键请求"
                                );
                                Ok(())
                            });
                            match result {
                                Ok(()) => {
                                    fresh.insert(key);
                                }
                                Err(error) if critical => *failure = Some(format!("{error:#}")),
                                Err(_) => {}
                            }
                        } else {
                            waiting.push((id, key, critical));
                        }
                    }
                    *requests = waiting;
                    if !requests.is_empty() {
                        self.job = Some(job);
                        return Ok(());
                    }
                    if self.stopping {
                        Self::status(engine, "Fn读取实际结束；动作已取消，未派发setter");
                    } else if let Some(error) = failure.take() {
                        Self::status(engine, format!("Fn动作未执行：{error}"));
                    } else if *interference {
                        if engine.pending.is_some() {
                            job.stage = Stage::Resolve {
                                fresh: BTreeSet::new(),
                                completed: 0,
                            };
                            self.job = Some(job);
                        } else {
                            self.begin_reads(engine, job.code)?;
                        }
                        return Ok(());
                    } else {
                        job.stage = Stage::Resolve {
                            fresh: std::mem::take(fresh),
                            completed: monotonic_ms(),
                        };
                        self.job = Some(job);
                    }
                }
                Stage::Resolve { fresh, completed } => {
                    if self.stopping {
                        Self::status(engine, "Fn动作已取消");
                    } else if engine.pending.is_some() {
                        fresh.clear();
                        *completed = 0;
                        self.job = Some(job);
                        return Ok(());
                    } else if *completed == 0 || monotonic_ms().saturating_sub(*completed) > 1000 {
                        self.begin_reads(engine, job.code)?;
                        return Ok(());
                    } else {
                        match decide(engine, job.code, fresh) {
                            Ok(Decision::Notify(payload)) => {
                                self.ui(engine, job.code, "notification", payload)?;
                                return Ok(());
                            }
                            Ok(Decision::Noop(reason)) => Self::status(engine, reason),
                            Ok(Decision::Write {
                                command,
                                mut payload,
                                notice,
                            }) => {
                                let actions = (|| -> Result<Vec<A>> {
                                    engine.context(&mut payload)?;
                                    hardware_routes::plan(command, &payload)
                                })();
                                match actions {
                                    Ok(actions) => {
                                        let request = engine.new_internal_request()?;
                                        if let Err(error) = engine.start_work(
                                            request,
                                            command,
                                            actions,
                                            After::Hardware,
                                        ) {
                                            engine.finish(request, false, format!("{error:#}"));
                                        }
                                        self.job = Some(Job {
                                            code: job.code,
                                            stage: Stage::Write {
                                                request,
                                                notification: notice,
                                            },
                                        });
                                        return Ok(());
                                    }
                                    Err(error) => {
                                        Self::status(engine, format!("Fn计划未执行：{error:#}"))
                                    }
                                }
                            }
                            Err(error) => {
                                Self::status(engine, format!("Fn实际状态未知，未执行：{error:#}"))
                            }
                        }
                    }
                }
                Stage::Write {
                    request,
                    notification,
                } => {
                    if self.stopping {
                        if let Some(pending) = engine
                            .pending
                            .as_mut()
                            .filter(|pending| pending.work.id == *request)
                        {
                            pending
                                .cancel
                                .store(true, std::sync::atomic::Ordering::Release);
                            pending.work.late = true;
                        }
                    }
                    let Some(result) = engine.take_internal_result(*request) else {
                        self.job = Some(job);
                        return Ok(());
                    };
                    if self.stopping {
                        Self::status(engine, "Fn物理调用已实际返回；取消后未发布后续通知");
                    } else {
                        match result {
                            Ok(()) => {
                                self.ui(engine, job.code, "notification", notification.clone())?;
                                return Ok(());
                            }
                            Err(error) => Self::status(engine, format!("Fn动作未完成：{error}")),
                        }
                    }
                }
                Stage::Ui { request } => {
                    let Some(result) = engine.take_internal_result(*request) else {
                        self.job = Some(job);
                        return Ok(());
                    };
                    Self::status(
                        engine,
                        match result {
                            Ok(()) => "Fn界面请求已实际返回；通知是否显示遵循用户设置".into(),
                            Err(error) => format!("Fn界面请求未完成：{error}"),
                        },
                    );
                }
            }
        }
        if self.stopping {
            if self.job.is_none() {
                self.stopping = false;
            }
            return Ok(());
        }
        if self.job.is_some() || engine.pending.is_some() {
            return Ok(());
        }
        if let Some(code) = self.queue.pop_front() {
            if code == 42 {
                return self.ui(engine, code, "show", json!({}));
            }
            if matches!(code, 4 | 45) {
                return self.ui(engine, code, "snip", json!({}));
            }
            if let Some(payload) = notification(code) {
                return self.ui(engine, code, "notification", payload);
            }
            self.begin_reads(engine, code)?;
        }
        Ok(())
    }
}
fn notice(kind: &str, text: impl Into<String>) -> Value {
    json!({"type":kind,"text":text.into()})
}
pub(super) fn notification(code: u32) -> Option<Value> {
    Some(match code {
        2 => notice("FnLockOn", "Fn锁已开启"),
        3 => notice("FnLockOff", "Fn锁已关闭"),
        12 => notice("CameraOn", "相机已开启"),
        13 => notice("CameraOff", "相机已关闭"),
        24 => notice("SpectrumBacklightOff", "键盘背光已关闭"),
        25 => notice("SpectrumBacklightChanged", "键盘背光：低"),
        26 => notice("SpectrumBacklightChanged", "键盘背光：中"),
        38 => notice("SpectrumBacklightChanged", "键盘背光：高"),
        32..=37 => notice(
            "SpectrumBacklightPresetChanged",
            format!("键盘背光预设 {}", code - 31),
        ),
        64 => notice("WhiteKeyboardBacklightOff", "白色键盘背光已关闭"),
        65 => notice("WhiteKeyboardBacklightChanged", "白色键盘背光：低"),
        66 => notice("WhiteKeyboardBacklightChanged", "白色键盘背光：高"),
        _ => return None,
    })
}
fn projected(engine: &Engine, key: &str) -> Result<bool> {
    hardware_projection::project(&engine.cache, false, None)
        .values
        .get(key)
        .and_then(Value::as_bool)
        .with_context(|| format!("{key} has no verified state"))
}
fn decide(engine: &Engine, code: u32, fresh: &BTreeSet<String>) -> Result<Decision> {
    Ok(match code {
        16 | 0x0041002a => match refresh(engine)? {
            Some((payload, text)) => Decision::Write {
                command: "display.apply",
                payload,
                notice: notice("RefreshRate", text),
            },
            None => Decision::Noop("未排除的内屏刷新率不足两项，未切换"),
        },
        62 | 65792 => {
            let muted = !projected(engine, "keys.microphone_muted")?;
            Decision::Write {
                command: "keys.apply",
                payload: json!({"keys.microphone_muted":muted}),
                notice: notice(
                    if muted {
                        "MicrophoneOff"
                    } else {
                        "MicrophoneOn"
                    },
                    if muted {
                        "麦克风已静音"
                    } else {
                        "麦克风已取消静音"
                    },
                ),
            }
        }
        63 | 73728 => {
            let on = !projected(engine, "network.airplane")?;
            Decision::Write {
                command: "network.apply",
                payload: json!({"network.airplane":on}),
                notice: notice(
                    if on {
                        "AirplaneModeOn"
                    } else {
                        "AirplaneModeOff"
                    },
                    if on {
                        "飞行模式已开启"
                    } else {
                        "飞行模式已关闭"
                    },
                ),
            }
        }
        65537 => {
            let raw = engine
                .cache
                .get("power_modes")
                .and_then(|v| v.get("current_raw"))
                .and_then(Value::as_u64)
                .context("Actual power mode missing")?;
            let (kind, text) = match raw {
                1 => ("PowerModeQuiet", "安静模式"),
                2 => ("PowerModeBalance", "均衡模式"),
                3 => ("PowerModePerformance", "野兽模式"),
                224 => ("PowerModeExtreme", "超能模式"),
                255 => ("PowerModeGodMode", "自定义模式"),
                _ => bail!("Unknown power mode"),
            };
            Decision::Notify(notice(kind, text))
        }
        65568 => {
            let locked = projected(engine, "keys.touchpad_lock")?;
            Decision::Notify(notice(
                if locked { "TouchpadOff" } else { "TouchpadOn" },
                if locked {
                    "触摸板已禁用"
                } else {
                    "触摸板已启用"
                },
            ))
        }
        69632 => {
            let level = if fresh.contains("lighting.0") {
                engine
                    .cache
                    .get("lighting.0")
                    .and_then(|v| v.as_object())
                    .and_then(|v| {
                        v.iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case("Current_Brightness_Level"))
                    })
                    .and_then(|(_, v)| v.as_u64())
                    .filter(|v| (1..=3).contains(v))
                    .map(|v| v - 1)
            } else {
                None
            }
            .or_else(|| {
                if fresh.contains("energy") {
                    engine
                        .cache
                        .get("energy")
                        .filter(|e| {
                            e.get("keyboard_maximum")
                                .and_then(Value::as_u64)
                                .is_some_and(|m| m >= 1)
                        })
                        .and_then(|e| e.get("keyboard_level"))
                        .and_then(Value::as_u64)
                } else {
                    None
                }
            });
            Decision::Notify(match level {
                Some(level @ 0..=2) => notification(64 + level as u32)
                    .expect("Canonical white backlight level is known"),
                Some(level) => notice(
                    "WhiteKeyboardBacklightChanged",
                    format!("键盘背光档位 {level}"),
                ),
                None => notice(
                    "WhiteKeyboardBacklightChangedSpecial",
                    "已收到键盘背光按键；当前档位未能读取",
                ),
            })
        }
        _ => bail!("Unknown built-in key"),
    })
}
fn same(a: &RefreshRate, b: &RefreshRate) -> bool {
    a.frequency == b.frequency
        && a.is_dynamic == b.is_dynamic
        && (!a.is_dynamic || a.base_frequency == b.base_frequency)
}
fn legacy_rate(node: &JsonNode) -> Option<RefreshRate> {
    let number = |key: &str| {
        node.get_ci(key)?
            .as_f64()
            .filter(|v| v.is_finite() && v.fract() == 0. && *v >= 0. && *v <= 1000.)
            .map(|v| v as u32)
    };
    Some(RefreshRate {
        frequency: number("Frequency")?,
        is_dynamic: node
            .get_ci("IsDynamic")
            .and_then(JsonNode::as_bool)
            .unwrap_or(false),
        base_frequency: number("BaseFrequency").unwrap_or(0),
    })
}
fn refresh(engine: &Engine) -> Result<Option<(Value, String)>> {
    let controls = engine
        .cache
        .get("display_controls")
        .and_then(Value::as_array)
        .context("Display controls missing")?;
    let internal = controls
        .iter()
        .filter(|v| v.get("internal").and_then(Value::as_bool) == Some(true))
        .collect::<Vec<_>>();
    ensure!(internal.len() == 1, "Internal monitor absent or ambiguous");
    let control = internal[0];
    let path = control
        .get("monitor_path")
        .and_then(Value::as_str)
        .context("Internal monitor path absent")?;
    let source = control
        .get("source_name")
        .and_then(Value::as_str)
        .context("Internal display source absent")?;
    let devices = engine
        .cache
        .get("devices")
        .and_then(|v| v.pointer("/displays/value"))
        .and_then(Value::as_array)
        .context("GDI display inventory absent")?;
    let matching = devices
        .iter()
        .filter(|v| {
            v.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.eq_ignore_ascii_case(source))
        })
        .collect::<Vec<_>>();
    ensure!(
        matching.len() == 1,
        "Internal GDI source absent or ambiguous"
    );
    let display = matching[0];
    let current = display
        .get("current")
        .context("Current internal mode absent")?;
    let (width, height) = (
        current
            .get("width")
            .and_then(Value::as_u64)
            .context("Width unknown")?,
        current
            .get("height")
            .and_then(Value::as_u64)
            .context("Height unknown")?,
    );
    let mut frequencies = BTreeSet::new();
    for mode in display
        .get("modes")
        .and_then(Value::as_array)
        .context("Internal modes absent")?
    {
        if mode.get("width").and_then(Value::as_u64) == Some(width)
            && mode.get("height").and_then(Value::as_u64) == Some(height)
        {
            if let Some(hz) = mode
                .get("hertz")
                .and_then(Value::as_u64)
                .filter(|v| *v > 0 && *v <= 1000)
            {
                frequencies.insert(hz as u32);
            }
        }
    }
    let mut choices = frequencies
        .into_iter()
        .map(|frequency| RefreshRate {
            frequency,
            is_dynamic: false,
            base_frequency: 0,
        })
        .collect::<Vec<_>>();
    if let Some(max) = choices.last().map(|r| r.frequency) {
        let dynamic = control
            .pointer("/refresh/choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| serde_json::from_value::<RefreshRate>(v.clone()).ok())
            .filter(|r| {
                r.is_dynamic
                    && r.frequency == max
                    && r.base_frequency > 0
                    && r.base_frequency < r.frequency
            })
            .min_by_key(|r| r.base_frequency);
        if let Some(dynamic) = dynamic {
            choices.push(dynamic);
        }
    }
    let excluded = engine
        .repository
        .document("application")?
        .root()
        .get_ci("ExcludedRefreshRates")
        .and_then(JsonNode::as_array)
        .unwrap_or(&[])
        .iter()
        .filter_map(legacy_rate)
        .collect::<Vec<_>>();
    choices.retain(|rate| !excluded.iter().any(|excluded| same(rate, excluded)));
    if choices.len() < 2 {
        return Ok(None);
    }
    let current_rate = control
        .pointer("/refresh/current")
        .and_then(|v| serde_json::from_value::<RefreshRate>(v.clone()).ok())
        .unwrap_or(RefreshRate {
            frequency: current
                .get("hertz")
                .and_then(Value::as_u64)
                .context("Current refresh unknown")? as u32,
            is_dynamic: false,
            base_frequency: 0,
        });
    let index = choices
        .iter()
        .position(|r| same(r, &current_rate))
        .or_else(|| {
            choices
                .iter()
                .position(|r| r.frequency == current_rate.frequency)
        });
    let next = choices[index.map_or(0, |i| (i + 1) % choices.len())];
    let text = if next.is_dynamic {
        format!(
            "内屏刷新率：{}–{} Hz（动态）",
            next.base_frequency, next.frequency
        )
    } else {
        format!("内屏刷新率：{} Hz", next.frequency)
    };
    Ok(Some((
        json!({"display.monitor":path,"display.refresh_rate":next}),
        text,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn audio() -> Value {
        json!({"audio":{"available":true,"value":[{"capture":true,"index":0,"muted":true,"volume_percent":100.0},{"capture":true,"index":1,"muted":false,"volume_percent":100.0}]}})
    }
    #[test]
    fn construction_queue_unknown_and_cancellation_do_not_start_native_io() {
        let mut builtin = BuiltinKeys::default();
        assert!(!builtin.is_busy());
        assert!(builtin.queue(999).is_err());
        builtin.queue(42).unwrap();
        assert!(builtin.is_busy());
        builtin.cancel();
        assert!(!builtin.is_busy());
        builtin.resume().unwrap();
        builtin.queue(4).unwrap();
    }
    #[test]
    fn show_and_snip_wait_for_matching_actual_ui_ack() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        let mut builtin = BuiltinKeys::default();
        builtin.queue(42).unwrap();
        builtin.poll(&mut f.engine).unwrap();
        assert!(builtin.is_busy());
        assert!(f.log.lock().unwrap().is_empty());
        f.engine.poll_ui_effects();
        let id = f.engine.state.lock().unwrap().values["native.ui_effect"]["effect_id"]
            .as_u64()
            .unwrap();
        builtin.poll(&mut f.engine).unwrap();
        assert!(builtin.is_busy());
        f.engine
            .complete_native_ui(&json!({"effect_id":id,"ok":true}))
            .unwrap();
        builtin.poll(&mut f.engine).unwrap();
        assert!(!builtin.is_busy());
        builtin.queue(45).unwrap();
        builtin.poll(&mut f.engine).unwrap();
        f.engine.poll_ui_effects();
        assert_eq!(
            f.engine.state.lock().unwrap().values["native.ui_effect"]["kind"],
            "snip"
        );
        assert!(f.log.lock().unwrap().is_empty());
    }
    #[test]
    fn microphone_toggle_uses_fresh_all_endpoint_aggregate_and_waits_for_write_then_ui() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        let read = f.answer();
        let write = f.answer();
        let mut builtin = BuiltinKeys::default();
        builtin.queue(65792).unwrap();
        builtin.poll(&mut f.engine).unwrap();
        f.engine.poll_read().unwrap();
        builtin.poll(&mut f.engine).unwrap();
        assert_eq!(f.log.lock().unwrap().len(), 1);
        *read.lock().unwrap() = Some(Ok(audio()));
        f.engine.poll_read().unwrap();
        builtin.poll(&mut f.engine).unwrap();
        builtin.poll(&mut f.engine).unwrap();
        assert!(matches!(
            f.log.lock().unwrap()[1],
            A::SetMicrophoneMute(true)
        ));
        assert!(builtin.is_busy());
        let endpoints = json!([{"capture":true,"index":0,"muted":true,"volume_percent":100.0},{"capture":true,"index":1,"muted":true,"volume_percent":100.0}]);
        *write.lock().unwrap() = Some(Ok(
            json!({"capture":true,"endpoints":endpoints,"audio_verified":true}),
        ));
        f.engine.poll_write().unwrap();
        builtin.poll(&mut f.engine).unwrap();
        assert!(builtin.is_busy());
        f.engine.poll_ui_effects();
        let effect = f.engine.state.lock().unwrap().values["native.ui_effect"].clone();
        assert_eq!(effect["payload"]["type"], "MicrophoneOff");
        f.engine
            .complete_native_ui(&json!({"effect_id":effect["effect_id"],"ok":true}))
            .unwrap();
        builtin.poll(&mut f.engine).unwrap();
        assert!(!builtin.is_busy());
    }
    #[test]
    fn cancel_retains_inflight_hardware_ownership_and_suppresses_followup_notification() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        let read = f.answer();
        let write = f.answer();
        let mut builtin = BuiltinKeys::default();
        builtin.queue(62).unwrap();
        builtin.poll(&mut f.engine).unwrap();
        f.engine.poll_read().unwrap();
        *read.lock().unwrap() = Some(Ok(audio()));
        f.engine.poll_read().unwrap();
        builtin.poll(&mut f.engine).unwrap();
        builtin.poll(&mut f.engine).unwrap();
        builtin.cancel();
        builtin.poll(&mut f.engine).unwrap();
        assert!(builtin.is_busy());
        assert!(
            f.engine
                .pending
                .as_ref()
                .unwrap()
                .cancel
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert!(builtin.resume().is_err());
        *write.lock().unwrap() = Some(Err("synthetic cancelled physical completion".into()));
        f.engine.poll_write().unwrap();
        builtin.poll(&mut f.engine).unwrap();
        assert!(!builtin.is_busy());
        assert!(!f.engine.ui_effects.is_busy());
        builtin.resume().unwrap();
    }
    fn display_fixture(engine: &mut Engine) {
        engine.cache.insert("display_controls".into(),json!([{"internal":false,"monitor_path":"external","source_name":"DISPLAY2"},{"internal":true,"monitor_path":"internal","source_name":"DISPLAY1","refresh":{"current":{"Frequency":60,"IsDynamic":false,"BaseFrequency":0},"choices":[{"Frequency":240,"IsDynamic":true,"BaseFrequency":120}]}}]));
        engine.cache.insert("devices".into(),json!({"displays":{"available":true,"value":[{"name":"DISPLAY1","current":{"width":1920,"height":1080,"hertz":60},"modes":[{"width":1920,"height":1080,"hertz":60},{"width":1920,"height":1080,"hertz":120},{"width":1920,"height":1080,"hertz":240},{"width":1280,"height":720,"hertz":360}]}]}}));
    }
    #[test]
    fn refresh_uses_internal_monitor_current_resolution_exclusions_and_actual_drr_option() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        display_fixture(&mut f.engine);
        f.engine.repository.save("application",SettingsDocument::parse(r#"{"ExcludedRefreshRates":[{"Frequency":120,"IsDynamic":false,"BaseFrequency":0}]}"#).unwrap()).unwrap();
        let (next, _) = refresh(&f.engine).unwrap().unwrap();
        assert_eq!(next["display.monitor"], "internal");
        assert_eq!(next["display.refresh_rate"]["Frequency"], 240);
        f.engine.cache.get_mut("display_controls").unwrap()[1]["refresh"]["current"] =
            json!({"Frequency":240,"IsDynamic":false,"BaseFrequency":0});
        let (next, _) = refresh(&f.engine).unwrap().unwrap();
        assert_eq!(next["display.refresh_rate"]["IsDynamic"], true);
        assert_eq!(next["display.refresh_rate"]["BaseFrequency"], 120);
        let mut payload = next;
        f.engine.context(&mut payload).unwrap();
        assert!(matches!(
            hardware_routes::plan("display.apply", &payload).unwrap()[0],
            A::Display(_)
        ));
        assert!(f.log.lock().unwrap().is_empty());
    }
    #[test]
    fn unknown_airplane_state_never_becomes_false_and_extreme_notification_is_correct() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        f.engine.cache.insert(
            "wireless".into(),
            json!({"airplane":{"available":false,"error":"denied"}}),
        );
        assert!(decide(&f.engine, 73728, &BTreeSet::new()).is_err());
        f.engine
            .cache
            .insert("power_modes".into(), json!({"current_raw":224}));
        assert!(
            matches!(decide(&f.engine,65537,&BTreeSet::new()).unwrap(),Decision::Notify(v)if v["type"]=="PowerModeExtreme")
        );
        assert_eq!(notification(13).unwrap()["type"], "CameraOff");
        assert_eq!(
            notification(37).unwrap()["type"],
            "SpectrumBacklightPresetChanged"
        );
    }
    #[test]
    fn optional_white_getter_failures_emit_generic_notice_once_without_read_retry_loop() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        let energy = f.answer();
        let lighting = f.answer();
        let mut builtin = BuiltinKeys::default();
        builtin.queue(69632).unwrap();
        builtin.poll(&mut f.engine).unwrap();
        f.engine.poll_read().unwrap();
        *energy.lock().unwrap() = Some(Err("synthetic no energy backlight".into()));
        f.engine.poll_read().unwrap();
        *lighting.lock().unwrap() = Some(Err("synthetic no white WMI lighting".into()));
        f.engine.poll_read().unwrap();
        builtin.poll(&mut f.engine).unwrap();
        builtin.poll(&mut f.engine).unwrap();
        f.engine.poll_ui_effects();
        assert_eq!(
            f.engine.state.lock().unwrap().values["native.ui_effect"]["payload"]["type"],
            "WhiteKeyboardBacklightChangedSpecial"
        );
        assert_eq!(f.log.lock().unwrap().len(), 2);
    }
}
