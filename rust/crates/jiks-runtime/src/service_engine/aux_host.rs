//! Slow sensors own a lazy third worker. A pending native cleanup keeps that
//! worker alive; it is never replaced merely because a request timed out.
use super::*;

type Factory = Box<dyn FnMut(PathBuf) -> Result<Box<dyn PhysicalPort>>>;
struct Read {
    key: &'static str,
    ticket: Box<dyn PhysicalTicket>,
    ids: Vec<u64>,
}
pub(super) struct AuxHost {
    factory: Factory,
    port: Option<Box<dyn PhysicalPort>>,
    reading: Option<Read>,
    closing: Option<Box<dyn PhysicalTicket>>,
    queue: VecDeque<(&'static str, A, Vec<u64>)>,
    next_sample: u64,
    stopping: bool,
    cleanup_failed: bool,
    wanted: bool,
    retry_waiters: Vec<u64>,
    battery_rates: Option<(i64, i64)>,
    battery_identity: Option<(u64, u64)>,
    since_override: Option<(u64, String)>,
    history_ac: Option<Option<bool>>,
}
impl Default for AuxHost {
    fn default() -> Self {
        Self {
            factory: Box::new(|root| Ok(Box::new(PlatformWorker::connect(root)?))),
            port: None,
            reading: None,
            closing: None,
            queue: VecDeque::new(),
            next_sample: 0,
            stopping: false,
            cleanup_failed: false,
            wanted: false,
            retry_waiters: Vec::new(),
            battery_rates: None,
            battery_identity: None,
            since_override: None,
            history_ac: None,
        }
    }
}
impl AuxHost {
    pub(super) fn is_busy(&self) -> bool {
        self.port.is_some() || self.reading.is_some() || self.closing.is_some()
    }
    pub(super) fn polling(&self) -> bool {
        self.reading.is_some()
            || self.closing.is_some()
            || !self.queue.is_empty()
            || self.stopping && !self.cleanup_failed
    }
    pub(super) fn next_deadline(&self) -> Option<u64> {
        // A retained, idle port is not pending work. Wake at its actual slow
        // sample deadline, and never turn a latched cleanup failure into a retry.
        (self.wanted && !self.stopping && !self.cleanup_failed && !self.polling())
            .then_some(self.next_sample)
    }
    pub(super) fn cancel(&mut self) {
        self.stopping = true;
    }
    pub(super) fn retry(&mut self, id: u64) {
        self.cleanup_failed = false;
        self.stopping = true;
        self.retry_waiters.push(id);
    }
    fn queue(&mut self, key: &'static str, action: A, id: Option<u64>) -> Result<()> {
        if let Some(read) = self.reading.as_mut().filter(|read| read.key == key) {
            ensure!(read.ids.len() < 32, "辅助读取等待者过多");
            if let Some(id) = id {
                read.ids.push(id);
            }
        } else if let Some((_, _, ids)) = self.queue.iter_mut().find(|row| row.0 == key) {
            ensure!(ids.len() < 32, "辅助读取等待者过多");
            if let Some(id) = id {
                ids.push(id);
            }
        } else {
            ensure!(self.queue.len() < 8, "辅助读取队列已满");
            self.queue
                .push_back((key, action, id.into_iter().collect()));
        }
        Ok(())
    }
    pub(super) fn battery_information(&mut self, engine: &mut Engine, id: u64) -> Result<()> {
        ensure!(
            engine.shutdown.is_none() && !self.stopping && !self.cleanup_failed,
            "辅助读取正在停止或等待恢复"
        );
        self.queue("battery", A::ReadBatteryInformation, Some(id))
    }
    pub(super) fn battery_history(&mut self, engine: &mut Engine, id: u64) -> Result<()> {
        ensure!(
            engine.shutdown.is_none() && !self.stopping && !self.cleanup_failed,
            "辅助读取正在停止或等待恢复"
        );
        let reset = engine
            .repository
            .document("application")?
            .root()
            .get_ci("ResetBatteryOnSinceTimerOnReboot")
            .and_then(JsonNode::as_bool)
            == Some(true);
        self.queue(
            "battery.history",
            A::ReadBatteryHistory {
                reset_on_reboot: reset,
            },
            Some(id),
        )
    }
    pub(super) fn poll(&mut self, engine: &mut Engine) -> Result<()> {
        let enabled =
            ApplicationThermalSettings::from_document(engine.repository.document("application")?)?
                .hardware_sensors_enabled;
        let demand = engine.sensor_demand()?;
        let status_visible = demand.status_visible;
        let full_view = demand.full_view;
        let thermal_desired = !engine.read_only && enabled && (full_view || status_visible)
            || !engine.read_only && engine.hwinfo_consumer();
        let full_sensors = !engine.read_only && (enabled && full_view || engine.hwinfo_consumer());
        let desired =
            engine.shutdown.is_none() && !engine.faulted && (thermal_desired || status_visible);
        if status_visible && desired && !self.stopping && !self.cleanup_failed {
            let ac = engine
                .cache
                .get("system")
                .and_then(|v| v.pointer("/ac_connected/value"))
                .and_then(Value::as_bool);
            if self.history_ac != Some(ac) {
                let reset = engine
                    .repository
                    .document("application")?
                    .root()
                    .get_ci("ResetBatteryOnSinceTimerOnReboot")
                    .and_then(JsonNode::as_bool)
                    == Some(true);
                self.queue(
                    "battery.history",
                    A::ReadBatteryHistory {
                        reset_on_reboot: reset,
                    },
                    None,
                )?;
                self.history_ac = Some(ac);
                if ac != Some(false) {
                    self.since_override = None;
                }
            }
        } else if !status_visible {
            self.history_ac = None;
        }
        if desired != self.wanted {
            if !desired {
                self.stopping = self.port.is_some();
            }
            // Only an explicit off -> on transition retries an initialization
            // failure. A failed owned cleanup still needs retry/verified stop.
            if desired && self.port.is_none() {
                self.stopping = false;
                self.cleanup_failed = false;
                self.next_sample = 0;
            }
            self.wanted = desired;
        }
        if engine.shutdown.is_some() || engine.faulted {
            self.stopping = true;
        }
        if let Some(mut read) = self.reading.take() {
            match read.ticket.wait(Duration::ZERO) {
                Ok(Completion::Pending { .. }) => self.reading = Some(read),
                Ok(Completion::Completed(value)) => {
                    if read.key == "battery" {
                        self.reduce_battery(engine, &value);
                    }
                    if read.key == "battery.history" {
                        let actual = value.get("since_utc").and_then(Value::as_str);
                        let actual_ms = actual.and_then(crate::time_values::parse);
                        if self
                            .since_override
                            .as_ref()
                            .is_some_and(|(reset, _)| actual_ms.is_some_and(|since| since > *reset))
                        {
                            self.since_override = None;
                        }
                        engine.value(
                            "battery.on_battery_since",
                            self.since_override
                                .as_ref()
                                .map(|(_, iso)| json!(iso))
                                .unwrap_or_else(|| {
                                    value.get("since_utc").cloned().unwrap_or(Value::Null)
                                }),
                        );
                    }
                    engine.cache.remove(&format!("error.{}", read.key));
                    engine.cache.insert(read.key.into(), value);
                    engine.publish_native(true)?;
                    for id in read.ids {
                        engine.finish(id, true, "辅助传感/电池信息读取已实际返回");
                    }
                }
                Err(error) => {
                    let message = format!("辅助读取未完成：{error:#}");
                    engine
                        .cache
                        .insert(format!("error.{}", read.key), json!(message));
                    engine.cache.remove(read.key);
                    engine.publish_native(true)?;
                    for id in read.ids {
                        engine.finish(id, false, &message);
                    }
                }
            }
        }
        if let Some(mut close) = self.closing.take() {
            match close.wait(Duration::ZERO) {
                Ok(Completion::Pending { .. }) => self.closing = Some(close),
                Ok(Completion::Completed(value)) => {
                    if value.get("shutdown_confirmed").and_then(Value::as_bool) == Some(true)
                        && value.get("owner_released").and_then(Value::as_bool) == Some(true)
                    {
                        self.port = None;
                        self.stopping = false;
                        self.cleanup_failed = false;
                        engine.value(
                            "sensors.aux.status",
                            json!("辅助读取已停止，原生资源已实际释放"),
                        );
                        for id in self.retry_waiters.drain(..) {
                            engine.finish(id, true, "辅助后台已实际恢复并释放资源");
                        }
                    } else {
                        self.cleanup_failed = true;
                        engine.value(
                            "sensors.aux.status",
                            json!("辅助后台未确认恢复与资源释放；保留后台，可重试清理"),
                        );
                        self.failed_cleanup(engine, "辅助后台未确认恢复与资源释放");
                    }
                }
                Err(error) => {
                    self.cleanup_failed = true;
                    engine.value(
                        "sensors.aux.status",
                        json!(format!("辅助清理未确认，保留原后台与恢复义务：{error:#}")),
                    );
                    self.failed_cleanup(engine, &format!("辅助清理未确认：{error:#}"));
                }
            }
        }
        if self.stopping {
            for (_, _, ids) in self.queue.drain(..) {
                for id in ids {
                    engine.finish(id, false, "辅助读取在开始前取消");
                }
            }
            if self.reading.is_none() && self.closing.is_none() && !self.cleanup_failed {
                if let Some(port) = &self.port {
                    self.closing = Some(port.shutdown_ticket()?);
                } else {
                    self.stopping = false;
                    for id in self.retry_waiters.drain(..) {
                        engine.finish(id, true, "辅助后台没有待释放资源");
                    }
                }
            }
            return Ok(());
        }
        if desired
            && monotonic_ms() >= self.next_sample
            && self.queue.is_empty()
            && self.reading.is_none()
        {
            self.next_sample = monotonic_ms().saturating_add(2000);
            if thermal_desired {
                self.queue("battery.thermal", A::ReadBatteryTemperature, None)?;
            }
            if full_sensors || status_visible {
                self.queue("battery", A::ReadBatteryInformation, None)?;
            }
            if full_sensors {
                self.queue("storage", A::ReadStorageSensors, None)?;
                self.queue("memory.thermal", A::ReadMemorySensors, None)?;
            }
        }
        if self.reading.is_none() && self.closing.is_none() {
            if let Some((key, action, ids)) = self.queue.pop_front() {
                if self.port.is_none() {
                    match (self.factory)(engine.repository.root().join("aux-unused.json")) {
                        Ok(port) => self.port = Some(port),
                        Err(error) => {
                            self.cleanup_failed = true;
                            self.stopping = true;
                            for id in ids {
                                engine.finish(id, false, format!("辅助后台未创建：{error:#}"));
                            }
                            return Ok(());
                        }
                    }
                }
                let permit = ExplicitAction::with_deadline(
                    "用户已启用的辅助传感读取",
                    monotonic_ms().saturating_add(2500),
                )?;
                match self.port.as_ref().unwrap().dispatch(action, permit) {
                    Ok(ticket) => self.reading = Some(Read { key, ticket, ids }),
                    Err(error) => {
                        for id in ids {
                            engine.finish(id, false, format!("辅助读取未提交：{error:#}"));
                        }
                    }
                }
            } else if !desired && self.port.is_some() {
                self.stopping = true;
            }
        }
        Ok(())
    }
    fn failed_cleanup(&mut self, engine: &mut Engine, message: &str) {
        for id in self.retry_waiters.drain(..) {
            engine.finish(id, false, message);
        }
        if let Some(id) = engine.shutdown.take() {
            engine.finish(id, false, format!("{message}；后台保留，可重试清理后退出"));
        }
    }
    fn reduce_battery(&mut self, engine: &Engine, value: &Value) {
        let row = value
            .get("batteries")
            .and_then(Value::as_array)
            .filter(|rows| rows.len() == 1)
            .map(|rows| &rows[0]);
        let identity =
            row.and_then(|row| Some((row.get("slot")?.as_u64()?, row.get("tag")?.as_u64()?)));
        if identity != self.battery_identity {
            self.battery_identity = identity;
            self.battery_rates = None;
        }
        let rate = row
            .filter(|row| row.get("rate_unit").and_then(Value::as_str) == Some("mW"))
            .and_then(|row| row.get("rate"))
            .and_then(Value::as_i64);
        if let Some(rate) = rate {
            if rate == 0
                || self
                    .battery_rates
                    .is_some_and(|(min, _)| min.signum() != rate.signum())
            {
                self.battery_rates = None;
            }
            if rate != 0 {
                self.battery_rates = Some(
                    self.battery_rates
                        .map(|(min, max)| {
                            (
                                if rate.abs() < min.abs() { rate } else { min },
                                if rate.abs() > max.abs() { rate } else { max },
                            )
                        })
                        .unwrap_or((rate, rate)),
                );
            }
        } else {
            self.battery_rates = None;
        }
        engine.value(
            "battery.minimum_rate_w",
            self.battery_rates
                .map(|(rate, _)| json!(rate as f64 / 1000.))
                .unwrap_or(Value::Null),
        );
        engine.value(
            "battery.maximum_rate_w",
            self.battery_rates
                .map(|(_, rate)| json!(rate as f64 / 1000.))
                .unwrap_or(Value::Null),
        );
    }
    pub(super) fn reset_timer(&mut self, engine: &Engine) -> Result<()> {
        let ac = engine
            .cache
            .get("system")
            .and_then(|v| v.pointer("/ac_connected/value"))
            .and_then(Value::as_bool);
        self.since_override = if ac == Some(false) {
            let now = crate::time_values::now_ms();
            Some((now, crate::time_values::iso(now)?))
        } else {
            None
        };
        engine.value(
            "battery.on_battery_since",
            self.since_override
                .as_ref()
                .map(|(_, iso)| json!(iso))
                .unwrap_or(Value::Null),
        );
        Ok(())
    }
}
impl Engine {
    pub(super) fn with_aux<T>(
        &mut self,
        action: impl FnOnce(&mut AuxHost, &mut Engine) -> Result<T>,
    ) -> Result<T> {
        let mut aux = std::mem::take(&mut self.aux);
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| action(&mut aux, self)));
        self.aux = aux;
        match result {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
    pub(super) fn hwinfo_consumer(&self) -> bool {
        self.repository
            .document("integrations")
            .ok()
            .is_some_and(|doc| {
                doc.root().get_ci("HWiNFO").and_then(JsonNode::as_bool) == Some(true)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    type Answer = Arc<Mutex<Option<std::result::Result<Value, String>>>>;
    struct Ticket(Answer);
    impl PhysicalTicket for Ticket {
        fn wait(&mut self, _: Duration) -> Result<Completion> {
            match self.0.lock().unwrap().take() {
                None => Ok(Completion::Pending { operation_id: 1 }),
                Some(Ok(v)) => Ok(Completion::Completed(v)),
                Some(Err(e)) => bail!(e),
            }
        }
    }
    struct Port {
        answers: Arc<Mutex<VecDeque<Answer>>>,
        calls: Arc<AtomicUsize>,
    }
    impl PhysicalPort for Port {
        fn dispatch(&self, _: A, _: ExplicitAction) -> Result<Box<dyn PhysicalTicket>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(Ticket(
                self.answers
                    .lock()
                    .unwrap()
                    .pop_front()
                    .context("fixture answer")?,
            )))
        }
        fn shutdown_ticket(&self) -> Result<Box<dyn PhysicalTicket>> {
            self.dispatch(A::ShutdownWorker, ExplicitAction::new("fake shutdown")?)
        }
    }
    fn fake() -> (
        AuxHost,
        Arc<Mutex<VecDeque<Answer>>>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let answers = Arc::new(Mutex::new(VecDeque::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let created = Arc::new(AtomicUsize::new(0));
        let (a, c, n) = (answers.clone(), calls.clone(), created.clone());
        let mut host = AuxHost::default();
        host.factory = Box::new(move |_| {
            n.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(Port {
                answers: a.clone(),
                calls: c.clone(),
            }))
        });
        (host, answers, calls, created)
    }
    #[test]
    fn visible_status_is_a_consumer_but_without_permission_only_reads_battery_metadata() {
        let mut fixture = super::super::lifecycle_tests::Fixture::new();
        fixture.engine.read_only = true;
        fixture.engine.value("ui.status_visible", json!(true));
        assert!(fixture.engine.sensor_consumers().unwrap());
        let (mut host, answers, calls, _) = fake();
        let history = Arc::new(Mutex::new(Some(Ok(json!({"since_utc":null})))));
        answers
            .lock()
            .unwrap()
            .extend([history, Arc::new(Mutex::new(None))]);
        host.poll(&mut fixture.engine).unwrap();
        assert_eq!(host.reading.as_ref().unwrap().key, "battery.history");
        host.poll(&mut fixture.engine).unwrap();
        assert_eq!(host.reading.as_ref().unwrap().key, "battery");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert!(host.queue.is_empty());
    }
    #[test]
    fn idle_status_battery_refresh_wakes_at_its_own_two_second_deadline() {
        let mut fixture = super::super::lifecycle_tests::Fixture::new();
        fixture.engine.read_only = true;
        fixture.engine.repository.save("application", SettingsDocument::parse(
            r#"{"EnableHardwareSensors":false}"#).unwrap()).unwrap();
        fixture.engine.value("ui.status_visible", json!(true));
        let (mut host, answers, calls, created) = fake();
        answers.lock().unwrap().extend([
            Arc::new(Mutex::new(Some(Ok(json!({"since_utc": null}))))),
            Arc::new(Mutex::new(Some(Ok(json!({"batteries": []}))))),
        ]);
        host.poll(&mut fixture.engine).unwrap();
        assert_eq!(host.reading.as_ref().unwrap().key, "battery.history");
        host.poll(&mut fixture.engine).unwrap();
        assert_eq!(host.reading.as_ref().unwrap().key, "battery");
        host.poll(&mut fixture.engine).unwrap();
        assert!(host.wanted && host.is_busy() && !host.polling());
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(created.load(Ordering::Relaxed), 1);
        assert!(fixture.log.lock().unwrap().is_empty());

        let now = monotonic_ms();
        host.next_sample = now.saturating_add(2000);
        fixture.engine.aux = host;
        fixture.engine.next_temperature = now.saturating_add(123);
        let temperature_deadline = fixture.engine.next_temperature;
        assert_eq!(
            fixture.engine.wait_duration(now).unwrap(),
            Duration::from_secs(2)
        );
        assert_eq!(fixture.engine.next_temperature, temperature_deadline);
        assert!(fixture.engine.last_temperature.is_none());
        // The same permission-off status demand exists in normal production,
        // not only in the application's explicit read-only diagnostic mode.
        fixture.engine.read_only = false;
        assert_eq!(
            fixture.engine.wait_duration(now).unwrap(),
            Duration::from_secs(2)
        );
        assert_eq!(fixture.engine.next_temperature, temperature_deadline);
        assert!(fixture.log.lock().unwrap().is_empty());
    }
    #[test]
    fn idle_aux_deadline_is_due_bounded_and_preserves_earlier_metric_work() {
        let mut fixture = super::super::lifecycle_tests::Fixture::new();
        let now = monotonic_ms();
        let mut host = AuxHost::default();
        host.wanted = true;
        host.next_sample = now.saturating_add(2000);
        fixture.engine.aux = host;
        fixture.engine.read_only = true;
        assert_eq!(fixture.engine.wait_duration(now).unwrap(), Duration::from_secs(2));
        assert_eq!(fixture.engine.wait_duration(now + 1999).unwrap(), Duration::from_millis(1));
        assert_eq!(fixture.engine.wait_duration(now + 2000).unwrap(), Duration::from_millis(1));
        assert_eq!(fixture.engine.wait_duration(now + 3000).unwrap(), Duration::from_millis(1));

        fixture.engine.read_only = false;
        fixture.engine.value("ui.status_visible", json!(true));
        fixture.engine.repository.save("application", SettingsDocument::parse(
            r#"{"EnableHardwareSensors":true}"#).unwrap()).unwrap();
        fixture.engine.next_metrics = now + 50;
        assert_eq!(fixture.engine.wait_duration(now).unwrap(), Duration::from_millis(50));
        fixture.engine.next_metrics = now + 500;
        assert_eq!(fixture.engine.wait_duration(now).unwrap(), Duration::from_millis(250));
        assert!(fixture.log.lock().unwrap().is_empty());
    }
    #[test]
    fn actual_pending_aux_read_retains_the_completion_poll_without_idle_busy_spin() {
        let mut fixture = super::super::lifecycle_tests::Fixture::new();
        let (mut host, answers, calls, _) = fake();
        answers.lock().unwrap().push_back(Arc::new(Mutex::new(None)));
        host.battery_information(&mut fixture.engine, 42).unwrap();
        host.poll(&mut fixture.engine).unwrap();
        assert!(host.polling());
        assert_eq!(host.next_deadline(), None);
        fixture.engine.aux = host;
        assert_eq!(fixture.engine.wait_duration(monotonic_ms()).unwrap(), Duration::from_millis(25));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn failed_aux_cleanup_has_no_automatic_timer_or_retry_and_idle_demand_off_sleeps() {
        let mut fixture = super::super::lifecycle_tests::Fixture::new();
        fixture.engine.read_only = true;
        assert_eq!(fixture.engine.wait_duration(monotonic_ms()).unwrap(), Duration::from_secs(30));
        fixture.engine.value("ui.status_visible", json!(true));
        let (mut host, _, calls, created) = fake();
        host.port = Some((host.factory)(fixture.engine.repository.root().join("fake-unused.json")).unwrap());
        host.wanted = true;
        host.stopping = true;
        host.cleanup_failed = true;
        host.next_sample = 0;
        host.poll(&mut fixture.engine).unwrap();
        assert!(host.is_busy() && !host.polling());
        assert_eq!(host.next_deadline(), None);
        fixture.engine.aux = host;
        assert_eq!(fixture.engine.wait_duration(monotonic_ms()).unwrap(), Duration::from_secs(30));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(created.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn readonly_idle_and_enabled_without_consumers_create_no_aux_worker() {
        let mut fixture = super::super::lifecycle_tests::Fixture::new();
        let (mut host, _, calls, created) = fake();
        fixture.engine.read_only = true;
        host.poll(&mut fixture.engine).unwrap();
        fixture.engine.read_only = false;
        let mut root = fixture
            .engine
            .repository
            .document("application")
            .unwrap()
            .root()
            .clone();
        root.set_known("EnableHardwareSensors", JsonNode::Bool(true))
            .unwrap();
        fixture
            .engine
            .repository
            .save("application", SettingsDocument::from_root(root).unwrap())
            .unwrap();
        for _ in 0..4 {
            host.poll(&mut fixture.engine).unwrap();
        }
        assert_eq!(created.load(Ordering::Relaxed), 0);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn dashboard_with_hidden_sensors_does_not_start_slow_auxiliary_sources() {
        let mut fixture = super::super::lifecycle_tests::Fixture::new();
        fixture.engine.repository.save("application", SettingsDocument::parse(
            r#"{"EnableHardwareSensors":true}"#).unwrap()).unwrap();
        fixture.engine.repository.save("dashboard", SettingsDocument::parse(
            r#"{"ShowSensors":false}"#).unwrap()).unwrap();
        fixture.engine.value("ui.main_visible", json!(true));
        fixture.engine.value("ui.page", json!("dashboard"));
        let (mut host, _, calls, created) = fake();
        host.poll(&mut fixture.engine).unwrap();
        assert!(!host.wanted && !host.is_busy());
        assert_eq!(created.load(Ordering::Relaxed), 0);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn cancel_waits_for_read_and_actual_worker_shutdown() {
        let mut fixture = super::super::lifecycle_tests::Fixture::new();
        fixture.engine.read_only = true;
        let (mut host, answers, calls, _) = fake();
        let read = Arc::new(Mutex::new(None));
        let close = Arc::new(Mutex::new(None));
        answers
            .lock()
            .unwrap()
            .extend([read.clone(), close.clone()]);
        host.battery_information(&mut fixture.engine, 42).unwrap();
        host.poll(&mut fixture.engine).unwrap();
        host.cancel();
        host.poll(&mut fixture.engine).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(host.is_busy());
        *read.lock().unwrap() = Some(Ok(json!({"batteries":[],"errors":[]})));
        host.poll(&mut fixture.engine).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert!(host.is_busy());
        *close.lock().unwrap() = Some(Ok(json!({"shutdown_confirmed":true,"owner_released":true})));
        host.poll(&mut fixture.engine).unwrap();
        assert!(!host.is_busy());
    }
    #[test]
    fn failed_cleanup_retains_same_port_until_explicit_retry_ack() {
        let mut fixture = super::super::lifecycle_tests::Fixture::new();
        fixture.engine.read_only = true;
        let (mut host, answers, _, created) = fake();
        let read = Arc::new(Mutex::new(Some(Ok(json!({"batteries":[]})))));
        let close = Arc::new(Mutex::new(Some(Err("MR11 restore pending".into()))));
        let retry = Arc::new(Mutex::new(None));
        answers.lock().unwrap().extend([read, close, retry.clone()]);
        host.battery_information(&mut fixture.engine, 42).unwrap();
        host.poll(&mut fixture.engine).unwrap();
        host.cancel();
        host.poll(&mut fixture.engine).unwrap();
        host.poll(&mut fixture.engine).unwrap();
        assert!(host.port.is_some() && host.cleanup_failed && host.is_busy());
        host.retry(43);
        host.poll(&mut fixture.engine).unwrap();
        assert_eq!(created.load(Ordering::Relaxed), 1);
        assert!(host.is_busy());
        *retry.lock().unwrap() = Some(Ok(json!({"shutdown_confirmed":true,"owner_released":true})));
        host.poll(&mut fixture.engine).unwrap();
        assert!(!host.is_busy());
        assert!(
            fixture
                .engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|r| r.request_id == 43 && r.ok)
        );
    }
}
