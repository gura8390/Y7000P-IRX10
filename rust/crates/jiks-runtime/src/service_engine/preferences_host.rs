use super::*;
use jiks_core::automation::{AdapterState, AutomationState, ordinal_ignore_case};
impl Engine {
    pub(super) fn request_app_preferences(&mut self, id: u64, payload: &Value) -> Result<()> {
        self.writable()?;
        let mut fields = payload.as_object().context("应用设置必须是对象")?.clone();
        if let Some(method)=fields.get("update.method") {ensure!(method.as_str()==Some("GitHub"),"Rust机控通过GitHub的Jiks发行资产更新；旧版Server接口不能提供本程序更新，请选择GitHub");}
        let autorun = fields.remove("app.autorun");
        let fnq = fields.get("power.fnq_custom").cloned();
        let change = config_actions::apply_action(
            "app.preferences.save",
            &JsonNode::parse(&Value::Object(fields).to_string())?,
            &self.documents(),
        )?;
        self.repository.validate_changes(&change.documents)?;
        let mut actions=Vec::new();
        if let Some(mode) = autorun {
            actions.extend(hardware_routes::plan("app.autorun.apply", &json!({"app.autorun":mode}))?);
        }
        if let Some(value)=fnq { actions.extend(hardware_routes::plan("power.apply", &json!({"power.fnq_custom":value}))?); }
        if !actions.is_empty() {
            self.start_work(
                id,
                "应用设置的实际系统操作",
                actions,
                After::Preferences { change },
            )?;
        } else {
            let message = change.message.clone();
            self.route_configuration(id, change, message)?;
        }
        Ok(())
    }
    pub(super) fn request_gpu_settings(&mut self, id: u64, payload: &Value) -> Result<()> {
        self.writable()?;
        let change = config_actions::apply_action(
            "gpu.settings.save",
            &JsonNode::parse(&payload.to_string())?,
            &self.documents(),
        )?;
        self.repository.validate_changes(&change.documents)?;
        let root = self.repository.document("application")?.root();
        let old_state = root
            .get_ci("LockedPStateId")
            .and_then(JsonNode::as_i64)
            .unwrap_or(-1);
        let mut plans = Vec::new();
        if let Some(awake) = payload.get("gpu.keep_awake").and_then(Value::as_bool) {
            let mut p = json!({"gpu.keep_awake":awake});
            self.context(&mut p)?;
            plans.extend(hardware_routes::plan("gpu.keep_awake.apply", &p)?);
        }
        if let Some(state) = payload
            .get("gpu.locked_pstate")
            .and_then(Value::as_i64)
            .filter(|state| {
                *state != old_state
                    || self
                        .state
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .values
                        .get("gpu.settings.uncertain")
                        .and_then(Value::as_bool)
                        == Some(true)
            })
        {
            let mut p = json!({"gpu.locked_pstate":state});
            self.context(&mut p)?;
            plans.extend(hardware_routes::plan("gpu.pstate.apply", &p)?);
        }
        if plans.is_empty() {
            self.commit(change)?;
            self.finish(id, true, "显卡监控设置已保存；没有需要改变的硬件状态");
        } else {
            self.start_work(id, "显卡设置", plans, After::Persist { change })?;
        }
        Ok(())
    }
    pub(super) fn request_gpu_overclock(&mut self, id: u64, payload: &Value) -> Result<()> {
        self.writable()?;
        let original = self.repository.document("gpu_oc")?;
        let info: jiks_platform::gpu::OverclockInfo = original
            .root()
            .get_ci("Info")
            .map(JsonNode::to_value)
            .transpose()?
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        let enabled = payload
            .get("gpu.oc.enabled")
            .and_then(Value::as_bool)
            .context("请选择是否启用独显配置")?;
        let mut p = json!({"gpu.oc.enabled":enabled,"gpu.oc.core_offset_mhz":info.core_delta_mhz,"gpu.oc.memory_offset_mhz":info.memory_delta_mhz,"gpu.oc.voltage_mv":if info.voltage_lock_mv>0{info.voltage_lock_mv}else{info.voltage_cap_mv},"gpu.oc.lock_voltage":info.voltage_lock_mv>0});
        for (key, value) in payload.as_object().context("显卡配置必须是对象")? {
            p[key] = value.clone();
        }
        self.context(&mut p)?;
        let actions = hardware_routes::plan("gpu.oc.apply", &p)?;
        let applied = actions
            .iter()
            .find_map(|action| {
                if let A::SetGpuOverclock { requested, .. } = action {
                    Some(*requested)
                } else {
                    None
                }
            })
            .context("缺少已验证的GPU配置动作")?;
        let mut root = original.root().clone();
        root.set_known("Enabled", JsonNode::Bool(enabled))?;
        if enabled {
            root.set_known("Info", JsonNode::parse(&serde_json::to_string(&applied)?)?)?;
        }
        let change = ConfigChange {
            documents: vec![("gpu_oc".into(), SettingsDocument::from_root(root)?)],
            projection: Default::default(),
            message: "GPU配置已实际核对并保存".into(),
        };
        self.repository.validate_changes(&change.documents)?;
        self.start_work(id, "独显配置", actions, After::Persist { change })
    }
    /// Native events have already reduced known game/power state. A failed
    /// attempt is latched until its policy inputs change, never retried at 40Hz.
    pub(super) fn reconcile_ai(&mut self, state: &AutomationState) -> Result<()> {
        if self.read_only
            || self.shutdown.is_some()
            || self.faulted
            || self.pending.is_some()
            || self.controller.is_some()
            || self.shadow.is_some()
        {
            return Ok(());
        }
        let enabled = self
            .repository
            .document("balance")?
            .root()
            .get_ci("AIModeEnabled")
            .and_then(JsonNode::as_bool)
            .unwrap_or(false);
        if !enabled && !self.ai_policy_owned {
            return Ok(());
        }
        if state.power_mode != Some(1) {
            self.ai_policy_attempt = None;
            return Ok(());
        }
        let Some(snapshot) = self.cache.get("ai").and_then(|value| {
            serde_json::from_value::<jiks_platform::ai::AiSnapshot>(value.clone()).ok()
        }) else {
            self.value(
                "power.ai.status",
                json!("等待AI能力与实际子模式读取；尚未应用"),
            );
            return Ok(());
        };
        if snapshot.raw_mode != 2 {
            self.enqueue_read("ai".into(), A::ReadAi, false);
            return Ok(());
        }
        let eligible =
            enabled && state.adapter == Some(AdapterState::Connected) && state.games_running;
        let target = if eligible {
            snapshot
                .process_modes
                .iter()
                .find(|mode| {
                    state.process_names.iter().any(|process| {
                        let name = mode
                            .processname
                            .rsplit(['\\', '/'])
                            .next()
                            .unwrap_or(&mode.processname);
                        let name = match name.rsplit_once('.') {
                            Some((stem, extension)) if extension.eq_ignore_ascii_case("exe") => {
                                stem
                            }
                            _ => name,
                        };
                        ordinal_ignore_case(process, name)
                    })
                })
                .map(|mode| mode.mode)
                .unwrap_or(1)
        } else {
            0
        };
        let signature = format!(
            "{enabled}/{:?}/{}/{target}/{}",
            state.adapter,
            state.games_running,
            self.repository.revision()
        );
        if self.ai_policy_attempt.as_ref() == Some(&signature) {
            return Ok(());
        }
        self.ai_policy_attempt = Some(signature);
        if snapshot.submode == target {
            self.value(
                "power.ai.status",
                json!(format!("AI条件已核对；实际子模式{target}")),
            );
            return Ok(());
        }
        let mut payload = json!({"power.ai_submode":target});
        self.context(&mut payload)?;
        match hardware_routes::plan("power.ai.apply", &payload).and_then(|actions| {
            self.start_work(0, "均衡AI条件切换", actions, After::AiPolicy { target })
        }) {
            Ok(()) => self.value("power.ai.status", json!("正在等待AI子模式实际读回")),
            Err(error) => self.value(
                "power.ai.status",
                json!(format!("AI子模式未应用：{error:#}")),
            ),
        }
        Ok(())
    }
}
