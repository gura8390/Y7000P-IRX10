use super::*;
use crate::special_keys::{self, Event, Options, Presses};
use std::collections::BTreeSet;

trait SourcePort {
    fn start(&mut self, options: Options) -> Result<()>;
    fn next(&self) -> Option<Event>;
    fn stop(&self);
    fn retry(&self);
    fn active(&self) -> bool;
    fn finish(&mut self) -> Option<Result<()>>;
}
impl SourcePort for special_keys::NativeSource {
    fn start(&mut self, options: Options) -> Result<()> {
        self.start(options)
    }
    fn next(&self) -> Option<Event> {
        self.next()
    }
    fn stop(&self) {
        self.stop()
    }
    fn retry(&self) {
        self.retry_cleanup()
    }
    fn active(&self) -> bool {
        self.active()
    }
    fn finish(&mut self) -> Option<Result<()>> {
        self.try_finish()
    }
}
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Wanted {
    source: Options,
    driver: bool,
}
struct Transition {
    id: u64,
    change: Option<ConfigChange>,
    old: Wanted,
    new: Wanted,
    restoring: bool,
    error: Option<String>,
    source_started: bool,
    config_request: Option<u64>,
    restore_failed: bool,
}
#[derive(Clone, Copy)]
enum NativeKind {
    DriverStart,
    DriverRead,
    DriverStop,
    FnRead { restore: bool },
    FnWrite { restore: bool },
}
struct NativePending {
    id: u64,
    kind: NativeKind,
}
pub(super) struct KeysHost {
    source: Box<dyn SourcePort>,
    options: Wanted,
    transition: Option<Transition>,
    source_ready: bool,
    stopping: bool,
    discovery: bool,
    discovered: BTreeSet<u32>,
    presses: Presses,
    revision: Option<(u64, bool, bool)>,
    pending: Option<NativePending>,
    driver_possible: bool,
    driver_ready: bool,
    next_driver: u64,
    modifiers: u32,
    held_since: u64,
    fn_settled_at: u64,
    fn_possible: bool,
    restore_failed: bool,
    driver_stop_failed: bool,
    hold_attempted: bool,
    manual: BTreeSet<u64>,
    cleanup_error: bool,
    builtin: super::keys_actions::BuiltinKeys,
    retry_waiters: Vec<(u64, u64)>,
    software_probe: Option<u64>,
    software_attempted: bool,
}
impl Default for KeysHost {
    fn default() -> Self {
        Self {
            source: Box::new(special_keys::NativeSource::new()),
            options: Wanted::default(),
            transition: None,
            source_ready: false,
            stopping: false,
            discovery: false,
            discovered: BTreeSet::new(),
            presses: Presses::default(),
            revision: None,
            pending: None,
            driver_possible: false,
            driver_ready: false,
            next_driver: 0,
            modifiers: 0,
            held_since: 0,
            fn_settled_at: 0,
            fn_possible: false,
            restore_failed: false,
            driver_stop_failed: false,
            hold_attempted: false,
            manual: BTreeSet::new(),
            cleanup_error: false,
            builtin: Default::default(),
            retry_waiters: vec![],
            software_probe: None,
            software_attempted: false,
        }
    }
}
fn custom(doc: &SettingsDocument) -> BTreeSet<u32> {
    doc.root()
        .get_ci("KeyModes")
        .and_then(JsonNode::as_object)
        .map(|items| {
            items
                .iter()
                .filter(|(_, value)| {
                    value
                        .as_str()
                        .is_some_and(|v| v.eq_ignore_ascii_case("Custom"))
                        || value.as_i64() == Some(1)
                })
                .filter_map(|(code, _)| code.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}
fn software_disabled(engine: &Engine) -> bool {
    engine
        .cache
        .get("software")
        .and_then(|v| v.pointer("/products/hotkeys/status"))
        .and_then(Value::as_str)
        .is_some_and(|status| matches!(status, "disabled" | "not_found"))
}
fn is_driver_code(key: u32) -> bool {
    (0x10000..=0x1ffff).contains(&key)
}
fn wanted(
    application: &SettingsDocument,
    keys: &SettingsDocument,
    discovery: bool,
    hotkeys_disabled: bool,
) -> Result<Wanted> {
    let custom = custom(keys);
    let flags = special_keys::modifier_flags(application.root().get_ci("SmartFnLockFlags"))?;
    Ok(Wanted {
        source: Options {
            utility: discovery
                || hotkeys_disabled
                || custom.iter().any(|key| !is_driver_code(*key)),
            modifiers: flags,
            escape: discovery,
        },
        driver: hotkeys_disabled,
    })
}
fn display_name(code: u32) -> String {
    match code {
        1 => "Fn+F9",
        2 => "Fn锁开启",
        3 => "Fn锁关闭",
        4 | 45 => "Fn+PrtSc",
        12 => "相机开启",
        13 => "相机关闭",
        16 | 0x0041002a => "Fn+R",
        42 => "Fn+N",
        62 => "Fn+F4",
        63 => "Fn+F8",
        65537 => "驱动 Fn+Q",
        65568 => "驱动 Fn+F10",
        65792 => "驱动 Fn+F4",
        69632 => "驱动 Fn+Space",
        73728 => "驱动 Fn+F8",
        _ => return format!("Fn事件 {code}"),
    }
    .into()
}
impl KeysHost {
    pub(super) fn is_busy(&self) -> bool {
        self.transition.is_some()
            || self.builtin.is_busy()
            || !self.retry_waiters.is_empty()
            || self.pending.is_some()
            || self.fn_possible
            || self.stopping && (self.source.active() || self.driver_possible)
    }
    pub(super) fn is_active(&self) -> bool {
        self.source.active()
            || self.transition.is_some()
            || self.driver_possible
            || self.fn_possible
    }
    pub(super) fn cancel(&mut self) {
        self.builtin.cancel();
        self.stopping = true;
        self.modifiers = 0;
        self.presses.clear();
        self.restore_failed = false;
        self.driver_stop_failed = false;
        self.source.stop();
        if self.cleanup_error {
            self.source.retry();
        }
        self.source_ready = false;
    }
    fn begin(&mut self, id: u64, change: Option<ConfigChange>, new: Wanted) -> Result<()> {
        ensure!(self.transition.is_none(), "Fn监听切换尚未完成");
        if self.stopping {
            ensure!(
                !self.source.active()
                    && !self.driver_possible
                    && !self.fn_possible
                    && self.pending.is_none()
                    && !self.builtin.is_busy(),
                "Fn前次清理尚未实际完成"
            );
            self.stopping = false;
            self.builtin.resume()?;
        }
        self.source.stop();
        self.source_ready = false;
        self.modifiers = 0;
        self.presses.clear();
        self.restore_failed = false;
        self.driver_stop_failed = false;
        self.hold_attempted = false;
        self.transition = Some(Transition {
            id,
            change,
            old: self.options,
            new,
            restoring: false,
            error: None,
            source_started: false,
            config_request: None,
            restore_failed: false,
        });
        Ok(())
    }
    pub(super) fn configuration(
        &mut self,
        engine: &Engine,
        id: u64,
        change: ConfigChange,
    ) -> Result<Option<ConfigChange>> {
        let old_app = engine.repository.document("application")?;
        let relevant = change.documents.iter().any(|(key, document)| {
            key == "special_keys"
                || key == "application"
                    && document.root().get_ci("SmartFnLockFlags")
                        != old_app.root().get_ci("SmartFnLockFlags")
        });
        if !relevant {
            return Ok(Some(change));
        }
        let mut application = engine.repository.document("application")?;
        let mut keys = engine.repository.document("special_keys")?;
        for (key, document) in &change.documents {
            if key == "application" {
                application = document;
            }
            if key == "special_keys" {
                keys = document;
            }
        }
        ensure!(
            software_disabled(engine)
                || !custom(keys)
                    .difference(&custom(engine.repository.document("special_keys")?))
                    .any(|key| is_driver_code(*key)),
            "驱动Fn按键需先在软件控制中实际禁用Lenovo Hotkeys，避免抢占现有监听"
        );
        let new = wanted(application, keys, self.discovery, software_disabled(engine))?;
        if new == self.options
            && self.transition.is_none()
            && (new.source.empty() || self.source_ready)
            && (!new.driver || self.driver_ready)
        {
            return Ok(Some(change));
        }
        self.begin(id, Some(change), new)?;
        Ok(None)
    }
    fn native(&mut self, engine: &mut Engine, kind: NativeKind, command: &str) -> Result<()> {
        ensure!(
            self.pending.is_none() && engine.pending.is_none(),
            "等待上一项设备调用"
        );
        let mut payload = json!({});
        engine.context(&mut payload)?;
        let actions = hardware_routes::plan(command, &payload)?;
        let id = engine.new_internal_request()?;
        if matches!(kind, NativeKind::DriverStart) {
            self.driver_possible = true;
        }
        if let Err(error) = engine.start_work(
            id,
            command,
            actions,
            After::SnapshotRead {
                key: "keys.driver".into(),
            },
        ) {
            engine.internal_requests.remove(&id);
            return Err(error);
        }
        self.pending = Some(NativePending { id, kind });
        Ok(())
    }
    fn fn_read(&mut self, engine: &mut Engine, restore: bool) -> Result<()> {
        let id = engine.new_internal_request()?;
        if let Err(error) = engine.request_read(id, "energy".into(), A::ReadEnergy) {
            engine.internal_requests.remove(&id);
            return Err(error);
        }
        self.pending = Some(NativePending {
            id,
            kind: NativeKind::FnRead { restore },
        });
        Ok(())
    }
    fn fn_write(&mut self, engine: &mut Engine, restore: bool) -> Result<()> {
        let id = engine.new_internal_request()?;
        self.fn_possible = true;
        engine.start_work(
            id,
            if restore {
                "恢复智能Fn锁"
            } else {
                "临时关闭Fn锁"
            },
            vec![A::Energy(jiks_platform::energy::EnergyAction::FnLock(
                restore,
            ))],
            After::SnapshotRead {
                key: "keys.fn_write".into(),
            },
        )?;
        self.pending = Some(NativePending {
            id,
            kind: NativeKind::FnWrite { restore },
        });
        Ok(())
    }
    fn status(&self, engine: &Engine, message: impl Into<String>) {
        engine.value("keys.status", json!(message.into()));
    }
    fn native_finished(&mut self, engine: &mut Engine) -> Result<()> {
        let Some(pending) = self.pending.as_ref() else {
            return Ok(());
        };
        let Some(result) = engine.take_internal_result(pending.id) else {
            return Ok(());
        };
        let pending = self.pending.take().unwrap();
        if let Err(error) = result {
            if matches!(pending.kind, NativeKind::DriverStop) {
                self.driver_stop_failed = true;
                if let Some(id) = engine.shutdown.take() {
                    engine.finish(
                        id,
                        false,
                        format!("Fn驱动监听关闭未核对，保留后台，可重试：{error}"),
                    );
                }
            }
            if matches!(pending.kind, NativeKind::DriverRead) {
                self.driver_ready = false;
            }
            if matches!(
                pending.kind,
                NativeKind::FnRead { restore: true } | NativeKind::FnWrite { restore: true }
            ) {
                self.restore_failed = true;
                if let Some(id) = engine.shutdown.take() {
                    engine.finish(
                        id,
                        false,
                        format!("Fn锁恢复未核对，保留后台，可再次退出重试：{error}"),
                    );
                }
            }
            if let Some(transition) = &mut self.transition {
                transition.error = Some(error.clone());
            }
            self.status(engine, format!("Fn操作未核对：{error}"));
            return Ok(());
        }
        match pending.kind {
            NativeKind::DriverStart => {
                ensure!(
                    engine
                        .cache
                        .get("keys.driver")
                        .and_then(|v| v.get("active"))
                        .and_then(Value::as_bool)
                        == Some(true),
                    "Fn驱动监听未返回实际active确认"
                );
                self.driver_ready = true;
            }
            NativeKind::DriverStop => {
                ensure!(
                    engine
                        .cache
                        .get("keys.driver")
                        .and_then(|v| v.get("owned_handles_closed"))
                        .and_then(Value::as_bool)
                        == Some(true),
                    "Fn驱动句柄关闭尚未确认"
                );
                self.driver_possible = false;
                self.driver_ready = false;
                self.driver_stop_failed = false;
            }
            NativeKind::DriverRead => {
                let rows = engine
                    .cache
                    .get("keys.driver")
                    .and_then(|v| v.get("raw_keys"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for raw in rows.iter().filter_map(Value::as_u64) {
                    if raw & !0xffff != 0 {
                        self.status(engine,format!("收到未定义的驱动高位标志0x{:08x}；完整原值保留，未当作已知按键执行",raw&!0xffff));
                    }
                    for bit in 0..16 {
                        if raw & (1 << bit) != 0 {
                            self.key(engine, 0x10000 + (1 << bit), monotonic_ms())?;
                        }
                    }
                }
            }
            NativeKind::FnRead { restore } => {
                let on = engine
                    .cache
                    .get("energy")
                    .and_then(|v| v.get("fn_lock"))
                    .and_then(Value::as_bool)
                    .context("Fn锁实际状态未知")?;
                if restore {
                    if on {
                        self.fn_possible = false;
                        self.restore_failed = false;
                    } else {
                        self.fn_write(engine, true)?;
                    }
                } else if on
                    && !self.stopping
                    && self.transition.is_none()
                    && self.modifiers & self.options.source.modifiers != 0
                {
                    self.fn_write(engine, false)?;
                }
            }
            NativeKind::FnWrite { restore } => {
                let observed = engine
                    .cache
                    .get("keys.fn_write")
                    .and_then(|v| v.get("fn_lock"))
                    .and_then(Value::as_bool)
                    .context("Fn锁写后实际读回未知")?;
                ensure!(observed == restore, "Fn锁写后状态不一致");
                self.fn_settled_at = monotonic_ms().saturating_add(150);
                if restore {
                    self.fn_possible = false;
                    self.restore_failed = false;
                }
                self.status(
                    engine,
                    if restore {
                        "Fn锁原开启状态已实际恢复"
                    } else {
                        "按住修饰键期间Fn锁已实际关闭"
                    },
                );
            }
        }
        Ok(())
    }
    fn publish_discovered(&self, engine: &Engine) {
        let mut state = engine.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut collections = state.collections.as_ref().clone();
        let items = collections.entry("special_keys".into()).or_default();
        let mut changed = false;
        for code in &self.discovered {
            let id = code.to_string();
            if !items.iter().any(|item| item.id == id) {
                items.push(Item {
                    id,
                    label: display_name(*code),
                    detail: "已收到原生Fn事件；尚未保存绑定".into(),
                    value: json!({"code":code}),
                });
                changed = true;
            }
        }
        if changed {
            state.collections = Arc::new(collections);
            state.metadata_revision += 1;
            state.revision += 1;
        }
    }
    fn key(&mut self, engine: &mut Engine, key: u32, at: u64) -> Result<()> {
        if self.discovery {
            self.discovered.insert(key);
            self.publish_discovered(engine);
            self.status(
                engine,
                format!("已发现 {}（{key}）；ESC停止发现", display_name(key)),
            );
            return Ok(());
        }
        if self.stopping || self.transition.is_some() || !self.source_ready {
            return Ok(());
        }
        if !software_disabled(engine) {
            self.status(
                engine,
                "Lenovo Hotkeys仍启用或状态未知；保留绑定，未并行执行Fn动作",
            );
            return Ok(());
        }
        if super::keys_actions::notification(key).is_some() {
            self.builtin.queue(key)?;
            return Ok(());
        }
        let doc = engine.repository.document("special_keys")?;
        if custom(doc).contains(&key) {
            let has_double = doc
                .root()
                .get_ci("KeyDoublePressActions")
                .and_then(|v| v.get(&key.to_string()))
                .and_then(JsonNode::as_array)
                .is_some_and(|v| !v.is_empty());
            for (code, double) in self.presses.press(key, at, has_double) {
                self.run_key(engine, code, double)?;
            }
        } else if software_disabled(engine) {
            self.builtin(engine, key)?;
        }
        Ok(())
    }
    fn run_key(&mut self, engine: &mut Engine, key: u32, double: bool) -> Result<()> {
        let field = if double {
            "KeyDoublePressActions"
        } else {
            "KeySinglePressActions"
        };
        let mut root = engine.repository.document("special_keys")?.root().clone();
        let mut map = root.get_ci(field).cloned().unwrap_or_else(JsonNode::object);
        let mut actions = map
            .get(&key.to_string())
            .and_then(JsonNode::as_array)
            .map(|a| a.to_vec())
            .unwrap_or_default();
        if actions.is_empty() {
            self.status(engine, "此按键已设自定义，但动作列表为空");
            return Ok(());
        }
        let selected = actions[0].as_str().context("Fn动作ID无效")?.to_owned();
        if actions.len() > 1 {
            actions.rotate_left(1);
            map.set(&key.to_string(), JsonNode::Array(actions))?;
            root.set_known(field, map)?;
            engine
                .repository
                .save("special_keys", SettingsDocument::from_root(root)?)?;
        }
        let id = engine.new_internal_request()?;
        if let Err(error) = engine.with_automation(|host, _| host.manual(id, &selected)) {
            engine.internal_requests.remove(&id);
            return Err(error);
        }
        self.manual.insert(id);
        self.status(
            engine,
            format!(
                "{}：{}动作已排队，等待实际完成",
                display_name(key),
                if double { "双击" } else { "单击" }
            ),
        );
        Ok(())
    }
    fn builtin(&mut self, engine: &mut Engine, key: u32) -> Result<()> {
        if let Err(error) = self.builtin.queue(key) {
            self.status(engine, format!("Fn事件未执行：{error:#}"));
        }
        Ok(())
    }
    pub(super) fn poll(&mut self, engine: &mut Engine) -> Result<()> {
        if engine.read_only {
            self.cancel();
        }
        self.builtin.poll(engine)?;
        if let Some(id) = self.software_probe {
            if let Some(result) = engine.take_internal_result(id) {
                self.software_probe = None;
                if let Err(error) = result {
                    self.status(
                        engine,
                        format!("Hotkeys状态读取失败，未并行执行Fn动作：{error}"),
                    );
                }
            }
        }
        if !engine.read_only
            && !engine.initializing
            && !self.software_attempted
            && engine.shutdown.is_none()
            && !engine.faulted
        {
            self.software_attempted = true;
            if !engine.cache.contains_key("software") {
                let id = engine.new_internal_request()?;
                match engine.request_read(id, "software".into(), A::ReadSoftware) {
                    Ok(()) => self.software_probe = Some(id),
                    Err(error) => {
                        engine.internal_requests.remove(&id);
                        self.status(engine, format!("Hotkeys状态读取未入队：{error:#}"));
                    }
                }
            }
        }
        self.native_finished(engine)?;
        for _ in 0..256 {
            let Some(event) = self.source.next() else {
                break;
            };
            match event {
                Event::Started => self.source_ready = true,
                Event::Utility { key, at_ms } => self.key(engine, key, at_ms)?,
                Event::Modifiers { mask, at_ms } => {
                    if mask & self.options.source.modifiers != 0
                        && self.modifiers & self.options.source.modifiers == 0
                    {
                        self.held_since = at_ms;
                        self.hold_attempted = false;
                    }
                    self.modifiers = mask;
                    if mask == 0 {
                        self.hold_attempted = false;
                    }
                }
                Event::Escape => {
                    if self.discovery && self.transition.is_none() {
                        self.discovery = false;
                        let new = wanted(
                            engine.repository.document("application")?,
                            engine.repository.document("special_keys")?,
                            false,
                            software_disabled(engine),
                        )?;
                        self.begin(0, None, new)?;
                    }
                }
                Event::CleanupPending(message) => {
                    self.cleanup_error = true;
                    self.status(engine, message.clone());
                    if let Some(id) = engine.shutdown.take() {
                        engine.finish(id, false, message);
                    }
                }
            }
        }
        if let Some(result) = self.source.finish() {
            self.source_ready = false;
            self.cleanup_error = false;
            if let Err(error) = result {
                self.status(engine, format!("Fn监听未启动/已停止：{error:#}"));
                if let Some(transition) = &mut self.transition {
                    transition.error = Some(format!("{error:#}"));
                    transition.restore_failed |= transition.restoring;
                }
            }
        }
        for (key, double) in self.presses.due(monotonic_ms()) {
            if !self.stopping && self.transition.is_none() {
                self.run_key(engine, key, double)?;
            }
        }
        for id in self.manual.clone() {
            if let Some(result) = engine.take_internal_result(id) {
                self.manual.remove(&id);
                self.status(
                    engine,
                    match result {
                        Ok(()) => "Fn快捷动作已实际完成".into(),
                        Err(error) => format!("Fn快捷动作失败：{error}"),
                    },
                );
            }
        }
        let now = monotonic_ms();
        let quiescent = !self.source.active()
            && !self.driver_possible
            && !self.fn_possible
            && self.pending.is_none()
            && self.transition.is_none()
            && !self.builtin.is_busy();
        self.retry_waiters.retain(|(id, deadline)| {
            if quiescent {
                engine.finish(*id, true, "Fn监听、驱动句柄与临时Fn锁状态已实际清理");
                false
            } else if now >= *deadline {
                engine.finish(
                    *id,
                    false,
                    "Fn清理尚未在期限内确认；原后台与恢复义务保留，可再重试",
                );
                false
            } else {
                true
            }
        });
        if self.pending.is_some() {
            return Ok(());
        }
        let restoring_fn = self.fn_possible
            && (self.stopping
                || self.transition.is_some()
                || self.modifiers & self.options.source.modifiers == 0);
        if restoring_fn
            && !self.restore_failed
            && engine.pending.is_none()
            && monotonic_ms() >= self.fn_settled_at
        {
            return self.fn_read(engine, true);
        }
        if self.fn_possible {
            return Ok(());
        }
        let target = self
            .transition
            .as_ref()
            .map(|t| if t.error.is_some() { t.old } else { t.new })
            .unwrap_or(self.options);
        if self.driver_possible
            && (self.stopping
                || !target.driver
                || self
                    .transition
                    .as_ref()
                    .is_some_and(|t| !t.source_started || t.error.is_some() && !t.restoring))
            && !self.driver_stop_failed
            && engine.pending.is_none()
        {
            return self.native(engine, NativeKind::DriverStop, "keys.driver.stop");
        }
        if self.stopping {
            if !self.source.active() {
                if let Some(transition) = self.transition.take() {
                    engine.finish(transition.id, false, "Fn监听切换已在实际清理后取消");
                }
            }
            return Ok(());
        }
        if let Some(mut transition) = self.transition.take() {
            if transition.restore_failed && !self.source.active() {
                self.options = transition.old;
                engine.finish(
                    transition.id,
                    false,
                    format!(
                        "Fn监听变更及恢复失败；旧配置保留，当前监听未启用：{}",
                        transition.error.unwrap_or_default()
                    ),
                );
                return Ok(());
            }
            if let Some(id) = transition.config_request {
                if let Some(result) = engine.take_internal_result(id) {
                    match result {
                        Ok(()) => {
                            self.options = transition.new;
                            engine.finish(transition.id, true, "Fn监听与设置已实际核对");
                        }
                        Err(error) => {
                            transition.error = Some(error);
                            transition.restoring = true;
                            transition.source_started = false;
                            transition.config_request = None;
                            self.source.stop();
                            self.source_ready = false;
                            self.transition = Some(transition);
                        }
                    }
                } else {
                    self.transition = Some(transition);
                }
                return Ok(());
            }
            if transition.error.is_some() && !transition.restoring {
                transition.restoring = true;
                transition.source_started = false;
                self.source.stop();
                self.source_ready = false;
            }
            let target = if transition.restoring {
                transition.old
            } else {
                transition.new
            };
            if !self.source.active() && !transition.source_started {
                transition.source_started = true;
                self.source_ready = target.source.empty();
                if let Err(error) = self.source.start(target.source) {
                    transition.error = Some(format!("{error:#}"));
                    if transition.restoring {
                        engine.finish(
                            transition.id,
                            false,
                            "Fn新监听和旧监听恢复均失败；旧配置保留，未假称已启用",
                        );
                        self.options = transition.old;
                        return Ok(());
                    }
                }
            }
            if self.source_ready && target.driver && !self.driver_ready && engine.pending.is_none()
            {
                self.transition = Some(transition);
                return self.native(engine, NativeKind::DriverStart, "keys.driver.start");
            }
            if self.source_ready && (!target.driver || self.driver_ready) {
                if transition.restoring {
                    self.options = transition.old;
                    engine.finish(
                        transition.id,
                        false,
                        format!(
                            "Fn变更失败，已恢复先前监听：{}",
                            transition.error.unwrap_or_default()
                        ),
                    );
                } else if let Some(change) = transition.change.take() {
                    let id = engine.new_internal_request()?;
                    let result = engine.with_automation(|host, engine| {
                        host.request_configuration(engine, id, change)
                    });
                    match result {
                        Ok(Some(change)) => {
                            if let Err(error) = engine.commit(change) {
                                engine.finish(id, false, format!("{error:#}"));
                            } else {
                                engine.finish(id, true, "配置已实际提交");
                            }
                        }
                        Ok(None) => {}
                        Err(error) => engine.finish(id, false, format!("{error:#}")),
                    };
                    transition.config_request = Some(id);
                    self.transition = Some(transition);
                } else {
                    self.options = transition.new;
                    engine.finish(
                        transition.id,
                        true,
                        if self.discovery {
                            "Fn发现已实际开始；ESC停止。驱动键需先禁用Hotkeys"
                        } else {
                            "Fn监听已实际完成切换"
                        },
                    );
                }
            } else {
                self.transition = Some(transition);
            }
            return Ok(());
        }
        let fingerprint = (
            engine.repository.revision(),
            software_disabled(engine),
            self.discovery,
        );
        if self.revision != Some(fingerprint) {
            self.revision = Some(fingerprint);
            let desired = match wanted(
                engine.repository.document("application")?,
                engine.repository.document("special_keys")?,
                self.discovery,
                software_disabled(engine),
            ) {
                Ok(wanted) => wanted,
                Err(error) => {
                    self.status(engine, format!("Fn未知配置保留惰性：{error:#}"));
                    Wanted::default()
                }
            };
            if desired != self.options {
                self.begin(0, None, desired)?;
                return Ok(());
            }
        }
        if self.source_ready
            && self.modifiers & self.options.source.modifiers != 0
            && !self.fn_possible
            && !self.hold_attempted
            && monotonic_ms().saturating_sub(self.held_since) >= 180
            && engine.pending.is_none()
        {
            self.hold_attempted = true;
            return self.fn_read(engine, false);
        }
        if self.driver_ready
            && monotonic_ms() >= self.next_driver
            && engine.pending.is_none()
            && !(engine.controller.is_some() && engine.next_control <= monotonic_ms())
        {
            self.next_driver = monotonic_ms().saturating_add(50);
            self.native(engine, NativeKind::DriverRead, "keys.driver.read")?;
        }
        Ok(())
    }
}
impl Engine {
    pub(super) fn with_keys<T>(
        &mut self,
        operation: impl FnOnce(&mut KeysHost, &mut Self) -> Result<T>,
    ) -> Result<T> {
        let mut keys = std::mem::take(&mut self.keys);
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(&mut keys, self)));
        self.keys = keys;
        match result {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
    pub(super) fn route_configuration(
        &mut self,
        id: u64,
        change: ConfigChange,
        message: String,
    ) -> Result<()> {
        if let Some(change) =
            self.with_keys(|keys, engine| keys.configuration(engine, id, change))?
        {
            if let Some(change) =
                self.with_automation(|host, engine| host.request_configuration(engine, id, change))?
            {
                self.commit(change)?;
                self.finish(id, true, message);
            }
        }
        Ok(())
    }
    pub(super) fn request_keys(&mut self, id: u64, command: &str) -> Result<()> {
        self.writable()?;
        self.with_keys(|keys, engine| {
            if command == "keys.retry_cleanup" {
                ensure!(keys.retry_waiters.len() < 16, "Fn清理重试等待者过多");
                keys.cancel();
                keys.restore_failed = false;
                keys.driver_stop_failed = false;
                keys.source.retry();
                keys.cleanup_error = false;
                keys.retry_waiters
                    .push((id, monotonic_ms().saturating_add(10_000)));
                return Ok(());
            }
            keys.discovery = command == "keys.discover.start";
            let new = wanted(
                engine.repository.document("application")?,
                engine.repository.document("special_keys")?,
                keys.discovery,
                software_disabled(engine),
            )?;
            keys.begin(id, None, new)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct SourceState {
        active: bool,
        stop_requested: bool,
        events: VecDeque<Event>,
        finished: Option<std::result::Result<(), String>>,
    }
    struct FakeSource(Arc<Mutex<SourceState>>);
    impl SourcePort for FakeSource {
        fn start(&mut self, options: Options) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            state.active = !options.empty();
            state.stop_requested = false;
            Ok(())
        }
        fn next(&self) -> Option<Event> {
            self.0.lock().unwrap().events.pop_front()
        }
        fn stop(&self) {
            self.0.lock().unwrap().stop_requested = true;
        }
        fn retry(&self) {}
        fn active(&self) -> bool {
            self.0.lock().unwrap().active
        }
        fn finish(&mut self) -> Option<Result<()>> {
            let mut state = self.0.lock().unwrap();
            let result = state.finished.take()?;
            state.active = false;
            Some(result.map_err(anyhow::Error::msg))
        }
    }
    fn host() -> (KeysHost, Arc<Mutex<SourceState>>) {
        let state = Arc::new(Mutex::new(SourceState::default()));
        let mut host = KeysHost::default();
        host.source = Box::new(FakeSource(state.clone()));
        (host, state)
    }
    #[test]
    fn fn_binding_file_waits_for_actual_source_started_and_configuration_ack() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        let (mut host, state) = host();
        let change=config_actions::apply_action("settings.fields.save",&JsonNode::parse(r#"{"document":"special_keys","fields":{"/KeyModes/42":"Custom","/KeyDescriptions/42":"fixture"}}"#).unwrap(),&f.engine.documents()).unwrap();
        assert!(
            host.configuration(&f.engine, 500, change)
                .unwrap()
                .is_none()
        );
        host.poll(&mut f.engine).unwrap();
        assert!(!f.engine.repository.root().join("special_key.json").exists());
        assert!(state.lock().unwrap().active);
        host.poll(&mut f.engine).unwrap();
        assert!(!f.engine.repository.root().join("special_key.json").exists());
        state.lock().unwrap().events.push_back(Event::Started);
        for _ in 0..6 {
            host.poll(&mut f.engine).unwrap();
            f.engine
                .with_automation(|host, engine| host.poll(engine))
                .unwrap();
        }
        assert!(f.engine.repository.root().join("special_key.json").exists());
        assert!(host.transition.is_none());
        assert!(f.log.lock().unwrap().is_empty());
        assert!(
            f.engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|v| v.request_id == 500 && v.ok)
        );
    }
    #[test]
    fn fn_cancel_keeps_transition_pending_until_actual_listener_cleanup() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        let (mut host, state) = host();
        state.lock().unwrap().active = true;
        host.options.source.utility = true;
        host.begin(501, None, Wanted::default()).unwrap();
        host.cancel();
        host.poll(&mut f.engine).unwrap();
        assert!(host.is_busy() && host.transition.is_some());
        assert!(state.lock().unwrap().stop_requested);
        assert!(
            !f.engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|v| v.request_id == 501)
        );
        state.lock().unwrap().finished = Some(Ok(()));
        host.poll(&mut f.engine).unwrap();
        assert!(!host.is_busy());
        assert!(
            f.engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|v| v.request_id == 501 && !v.ok)
        );
        assert!(f.log.lock().unwrap().is_empty());
    }
    #[test]
    fn smart_fn_uses_fresh_baseline_and_actual_disable_then_restore_ack() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        // This test starts after the normal software metadata baseline. Keep
        // Hotkeys enabled so the fixture exercises only the modifier source,
        // not the separate disabled-Hotkeys driver registration path.
        f.engine.cache.insert(
            "software".into(),
            json!({"products":{"hotkeys":{"status":"enabled"}}}),
        );
        f.engine
            .repository
            .update_known(
                "application",
                &[(&["SmartFnLockFlags"], JsonNode::Number("2".into()))],
            )
            .unwrap();
        let (mut host, state) = host();
        state.lock().unwrap().active = true;
        host.source_ready = true;
        host.options.source.modifiers = 2;
        host.modifiers = 2;
        host.held_since = monotonic_ms().saturating_sub(200);
        host.revision = Some((f.engine.repository.revision(), false, false));
        let read = f.answer();
        host.poll(&mut f.engine).unwrap();
        f.engine.poll_read().unwrap();
        assert!(matches!(f.log.lock().unwrap()[0], A::ReadEnergy));
        assert!(!host.fn_possible);
        let off = f.answer();
        *read.lock().unwrap() = Some(Ok(json!({"fn_lock":true})));
        f.engine.poll_read().unwrap();
        host.poll(&mut f.engine).unwrap();
        assert!(matches!(
            f.log.lock().unwrap()[1],
            A::Energy(jiks_platform::energy::EnergyAction::FnLock(false))
        ));
        assert!(host.fn_possible);
        host.poll(&mut f.engine).unwrap();
        assert_eq!(f.log.lock().unwrap().len(), 2);
        *off.lock().unwrap() = Some(Ok(json!({"fn_lock":false})));
        f.engine.poll_write().unwrap();
        host.poll(&mut f.engine).unwrap();
        assert!(host.fn_possible);
        host.fn_settled_at = 0;
        let read_back = f.answer();
        state.lock().unwrap().events.push_back(Event::Modifiers {
            mask: 0,
            at_ms: monotonic_ms(),
        });
        host.poll(&mut f.engine).unwrap();
        f.engine.poll_read().unwrap();
        let restore = f.answer();
        *read_back.lock().unwrap() = Some(Ok(json!({"fn_lock":false})));
        f.engine.poll_read().unwrap();
        host.poll(&mut f.engine).unwrap();
        assert!(host.fn_possible);
        *restore.lock().unwrap() = Some(Ok(json!({"fn_lock":true})));
        f.engine.poll_write().unwrap();
        host.poll(&mut f.engine).unwrap();
        assert!(!host.fn_possible);
        assert_eq!(f.log.lock().unwrap().len(), 4);
        assert!(matches!(
            f.log.lock().unwrap()[3],
            A::Energy(jiks_platform::energy::EnergyAction::FnLock(true))
        ));
    }
    #[test]
    fn fn_rejection_before_registration_preserves_saved_bytes_and_never_binds_driver() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        f.engine
            .repository
            .save(
                "special_keys",
                SettingsDocument::parse(r#"{"KeyModes":{},"Future":1e9999}"#).unwrap(),
            )
            .unwrap();
        let old = std::fs::read(f.engine.repository.root().join("special_key.json")).unwrap();
        let (mut host, _) = host();
        let change = config_actions::apply_action(
            "settings.fields.save",
            &JsonNode::parse(
                r#"{"document":"special_keys","fields":{"/KeyModes/65537":"Custom"}}"#,
            )
            .unwrap(),
            &f.engine.documents(),
        )
        .unwrap();
        assert!(host.configuration(&f.engine, 502, change).is_err());
        assert_eq!(
            old,
            std::fs::read(f.engine.repository.root().join("special_key.json")).unwrap()
        );
        assert!(!host.is_active());
        assert!(f.log.lock().unwrap().is_empty());
    }
}
