//! Opt-in OSD FPS lifecycle. Admission is not Started, and a Stopped message is
//! not the native worker join. This host owns its source until actual cleanup.
use super::*;
use jiks_platform::fps::{
    self, FpsMessage, FpsOptions, FpsPhase, FpsSample, FpsSampler, FpsStatus, FpsStopReport,
};

trait SourcePort {
    fn start(&mut self, options: FpsOptions) -> Result<()>;
    fn poll(&mut self) -> Option<FpsMessage>;
    fn stop(&mut self) -> Option<u64>;
    fn finish(&mut self) -> Option<Result<FpsStopReport>>;
    fn status(&self) -> Option<FpsStatus>;
}
impl SourcePort for FpsSampler {
    fn start(&mut self, options: FpsOptions) -> Result<()> {
        self.start(options)
    }
    fn poll(&mut self) -> Option<FpsMessage> {
        self.poll()
    }
    fn stop(&mut self) -> Option<u64> {
        self.stop_ticket()
    }
    fn finish(&mut self) -> Option<Result<FpsStopReport>> {
        self.try_finish()
    }
    fn status(&self) -> Option<FpsStatus> {
        self.status()
    }
}
const KEYS: [&str; 3] = ["Fps", "LowFps", "FrameTime"];
const STALE_MS: u64 = 2000;
fn wants_fps(items: Option<&JsonNode>) -> Result<bool> {
    // The original OsdSettings DTO initializes absent Items to all enum values.
    let Some(items) = items else { return Ok(true) };
    let JsonNode::Array(items) = items else {
        bail!("OSD Items 必须是列表")
    };
    ensure!(items.len() <= 128, "OSD Items 过长");
    Ok(items.iter().any(|item| {
        item.as_i64().is_some_and(|n| (0..=2).contains(&n))
            || item
                .as_str()
                .is_some_and(|s| KEYS.iter().any(|k| k.eq_ignore_ascii_case(s)))
    }))
}
fn enabled(root: &JsonNode, key: &str) -> Result<bool> {
    match root.get_ci(key) {
        None | Some(JsonNode::Null) => Ok(false),
        Some(value) => value.as_bool().with_context(|| format!("{key} 必须是开关")),
    }
}
fn desired(application: &JsonNode, osd: &JsonNode, visible: bool, blocked: bool) -> Result<bool> {
    if blocked || !visible {
        return Ok(false);
    }
    Ok(enabled(application, "EnableHardwareSensors")?
        && enabled(osd, "ShowOsd")?
        && wants_fps(osd.get_ci("Items"))?)
}
fn complete(report: &FpsStopReport) -> bool {
    report.session_stopped && report.consumer_joined
}
fn valid(sample: &FpsSample, now: u64) -> bool {
    sample.process.pid > 4
        && sample.process.creation_filetime != 0
        && !sample.process.name.is_empty()
        && sample.process.name.len() <= 1040
        && !sample.process.name.contains(['\\', '/', '\0'])
        && sample.generation > 0
        && sample.window_capacity == 500
        && sample.intervals <= 500
        && sample.process_aggregate
        && !sample.displayed_fps_verified
        && sample.source == fps::SOURCE
        && now
            .checked_sub(sample.sampled_tick_ms)
            .is_some_and(|age| age <= STALE_MS)
        && sample.sampled_tick_ms >= sample.last_present_tick_ms
        && now
            .checked_sub(sample.last_present_tick_ms)
            .is_some_and(|age| age <= STALE_MS)
        && [sample.fps, sample.low_fps, sample.frame_time_ms]
            .into_iter()
            .flatten()
            .all(|v| v.is_finite() && v > 0.)
        && (sample.intervals > 0
            || [sample.fps, sample.low_fps, sample.frame_time_ms]
                .iter()
                .all(Option::is_none))
        && (sample.low_fps.is_none() || sample.intervals == 500)
}

pub(super) struct FpsHost {
    source: Box<dyn SourcePort>,
    owned: bool,
    active: bool,
    stopping: bool,
    cancelled: bool,
    failed: bool,
    cleanup_uncertain: bool,
    sample: Option<FpsSample>,
    losses: (u64, u64),
    status: String,
    error: Option<String>,
    rows: Vec<Metric>,
    dirty: bool,
    next_status: u64,
    retry_waiters: Vec<(u64, u64)>,
    failed_attempt: Option<(u64, String)>,
    finished: Option<std::result::Result<(), String>>,
    last_stop_ticket: Option<u64>,
}
impl Default for FpsHost {
    fn default() -> Self {
        Self::new(Box::new(FpsSampler::new()))
    }
}
impl FpsHost {
    fn new(source: Box<dyn SourcePort>) -> Self {
        let mut host = Self {
            source,
            owned: false,
            active: false,
            stopping: false,
            cancelled: false,
            failed: false,
            cleanup_uncertain: false,
            sample: None,
            losses: (0, 0),
            status: "FPS采集关闭".into(),
            error: None,
            rows: vec![],
            dirty: true,
            next_status: 0,
            retry_waiters: vec![],
            failed_attempt: None,
            finished: None,
            last_stop_ticket: None,
        };
        host.rebuild();
        host
    }
    pub(super) fn is_busy(&self) -> bool {
        self.owned || self.cleanup_uncertain || !self.retry_waiters.is_empty()
    }
    pub(super) fn is_active(&self) -> bool {
        self.active && !self.stopping && !self.failed
    }
    /// A failed actual cleanup can reject Exit while retaining this source.
    /// An old failure does not reject a newer explicit retry still in flight.
    pub(super) fn cleanup_failure(&self) -> Option<String> {
        (self.cleanup_uncertain
            && self.failed
            && self.failed_attempt.as_ref().is_none_or(|(attempt, _)| {
                self.last_stop_ticket
                    .is_none_or(|ticket| *attempt >= ticket)
            }))
        .then(|| self.error.clone().unwrap_or_else(|| self.status.clone()))
    }
    pub(super) fn metrics(&self) -> Vec<Metric> {
        let mut rows = self.rows.clone();
        if !self.is_active() || self.sample.is_none() {
            for row in &mut rows {
                row.value = "—".into();
                row.detail = format!("{}；{}", fps::SOURCE, self.status);
            }
        }
        rows
    }
    fn status(&mut self, text: impl Into<String>) {
        let text = text.into();
        if self.status != text {
            self.status = text;
            self.dirty = true;
        }
    }
    fn clear(&mut self) {
        if self.sample.take().is_some() {
            self.dirty = true;
        }
    }
    fn failure(&mut self, text: String) {
        self.failed = true;
        self.active = false;
        self.clear();
        self.status(text.clone());
        if self.error.as_ref() != Some(&text) {
            self.error = Some(text);
            self.dirty = true;
        }
    }
    fn request_stop(&mut self) {
        self.active = false;
        self.clear();
        if self.owned && !self.stopping {
            self.stopping = true;
            self.dirty = true;
            // A source fault may have begun cleanup before its control message
            // reaches us. Never turn an ordinary hide/cancel tick into a retry.
            if let Some(status) = self.source.status() {
                match status.phase {
                    FpsPhase::CleanupFailed => {
                        self.note_failed_status(status);
                        return;
                    }
                    FpsPhase::Stopping | FpsPhase::Stopped => return,
                    _ => {}
                }
            }
            let ticket = self.source.stop();
            if let Some(ticket) = ticket {
                self.last_stop_ticket = Some(ticket);
            }
            if ticket.is_none() {
                self.cleanup_uncertain = true;
                self.failure("FPS停止请求未被来源确认；所有权保留".into());
            }
        }
    }
    /// Terminal host gate for shutdown/fault/restore. Poll must still run to
    /// receive the actual join; repeatedly calling cancel never retries cleanup.
    pub(super) fn cancel(&mut self) {
        self.cancelled = true;
        self.request_stop();
        self.status("FPS正在停止；等待自有会话与消费线程实际结束");
    }
    /// Explicit retry only; the normal tick never advances the native retry epoch.
    pub(super) fn retry(&mut self) {
        self.begin_retry();
    }
    fn begin_retry(&mut self) -> Option<u64> {
        self.cancelled = false;
        if self.owned || self.cleanup_uncertain {
            self.stopping = true;
            let ticket = self.source.stop();
            if let Some(ticket) = ticket {
                self.last_stop_ticket = Some(ticket);
            }
            if ticket.is_none() {
                self.failure("FPS清理重试未被来源确认；所有权仍保留".into());
            } else {
                self.status("FPS已请求重试清理；等待实际完成");
            }
            ticket
        } else {
            self.failed = false;
            self.error = None;
            self.dirty = true;
            self.status("FPS已允许显式重试，等待可见OSD消费者");
            None
        }
    }
    pub(super) fn request_retry(&mut self, engine: &mut Engine, id: u64) -> Result<()> {
        ensure!(self.retry_waiters.len() < 16, "FPS清理回执队列已满");
        if self.owned || self.cleanup_uncertain {
            if let Some(ticket) = self.begin_retry() {
                self.retry_waiters.push((id, ticket));
            } else {
                engine.finish(id, false, "FPS清理重试未受理；资源所有权保留");
            }
        } else {
            self.retry();
            engine.finish(
                id,
                true,
                "没有活动FPS原生资源；已允许显式重试，下一次实际Started另行核对",
            );
        }
        Ok(())
    }
    fn note_failed_status(&mut self, status: FpsStatus) {
        if let Some(attempt) = status.cleanup_attempt {
            self.failed_attempt = Some((attempt, status.reason.clone()));
        }
        self.stopping = true;
        self.cleanup_uncertain = true;
        self.failure(format!(
            "FPS清理失败，所有权保留；请显式重试：{}",
            status.reason
        ));
    }
    fn take_retry_results(&mut self) -> Vec<(u64, bool, String)> {
        let mut results = vec![];
        if let Some(result) = self.finished.take() {
            for (id, _) in self.retry_waiters.drain(..) {
                results.push((
                    id,
                    result.is_ok(),
                    match &result {
                        Ok(()) => "FPS自有会话停止、消费线程join和控制worker结束已实际确认".into(),
                        Err(error) => format!("FPS清理回执错误：{error}"),
                    },
                ));
            }
        } else if let Some((attempt, error)) = &self.failed_attempt {
            self.retry_waiters.retain(|(id, ticket)| {
                if ticket <= attempt {
                    results.push((
                        *id,
                        false,
                        format!("FPS本次清理失败，所有权仍保留：{error}"),
                    ));
                    false
                } else {
                    true
                }
            });
        }
        if !results.is_empty() {
            self.dirty = true;
        }
        results
    }
    pub(super) fn poll(&mut self, engine: &mut Engine) -> Result<()> {
        let blocked = engine.read_only
            || engine.initializing
            || !engine.identity_supported
            || engine.shutdown.is_some()
            || engine.faulted
            || engine.maintenance.as_ref().is_some_and(|job| job.restoring);
        let visible = engine
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values
            .get("osd.actual_visible")
            .and_then(Value::as_bool)
            == Some(true);
        let wanted = (|| -> Result<bool> {
            desired(
                engine.repository.document("application")?.root(),
                engine.repository.document("osd")?.root(),
                visible,
                blocked,
            )
        })();
        let wanted = match wanted {
            Ok(wanted) => wanted,
            Err(error) => {
                self.failure(format!("FPS设置无效，未启动采集：{error:#}"));
                false
            }
        };
        self.advance(wanted, monotonic_ms());
        for (id, ok, message) in self.take_retry_results() {
            engine.finish(id, ok, message);
        }
        if self.dirty {
            self.rebuild();
            let mut state = engine.state.lock().unwrap_or_else(|p| p.into_inner());
            state
                .values
                .insert("fps.active".into(), json!(self.is_active()));
            state
                .values
                .insert("fps.busy".into(), json!(self.is_busy()));
            state
                .values
                .insert("fps.status".into(), json!(&self.status));
            state.values.insert("fps.error".into(), json!(&self.error));
            state.values.insert("fps.source".into(), json!(fps::SOURCE));
            state.values.insert(
                "fps.process".into(),
                json!(self.sample.as_ref().map(|sample| &sample.process)),
            );
            state
                .values
                .insert("fps.events_dropped".into(), json!(self.losses.0));
            state
                .values
                .insert("fps.etw_buffers_lost".into(), json!(self.losses.1));
            state
                .metrics
                .retain(|metric| !KEYS.contains(&metric.key.as_str()));
            state.metrics.extend(self.rows.iter().cloned());
            state.revision = state.revision.wrapping_add(1);
            self.dirty = false;
        }
        Ok(())
    }
    fn advance(&mut self, wanted: bool, now: u64) {
        let allowed = wanted && !self.cancelled;
        if !allowed {
            self.request_stop();
            if !self.owned && !self.failed {
                self.status("FPS采集关闭；没有实际可见且已启用的OSD消费者");
            }
        }
        // Native control messages are bounded/coalesced; keep the coordinator
        // bounded even if a future port violates that admission contract.
        for _ in 0..32 {
            let Some(message) = self.source.poll() else {
                break;
            };
            match message {
                FpsMessage::Started { .. }
                    if self.owned && allowed && !self.stopping && !self.failed =>
                {
                    self.active = true;
                    self.dirty = true;
                    self.status("FPS自有会话已启动；等待前台进程的有效Present间隔");
                }
                FpsMessage::Sample(sample) if self.is_active() && allowed => {
                    let losses = (sample.events_dropped, sample.etw_buffers_lost);
                    if losses != self.losses {
                        self.losses = losses;
                        self.dirty = true;
                        self.clear();
                        self.status("FPS检测到事件/缓冲丢失；丢弃历史，等待新间隔");
                    } else if valid(&sample, now) {
                        self.sample = Some(sample);
                        self.dirty = true;
                        self.status("前台进程聚合的内核Present节奏；不是显示帧率");
                    } else {
                        self.clear();
                        self.status("FPS样本无效或已过期；不显示估计值");
                    }
                }
                FpsMessage::Unavailable { reason } => {
                    self.clear();
                    if !self.failed {
                        self.status(reason);
                    }
                }
                FpsMessage::Fault {
                    message,
                    cleanup_pending,
                } => {
                    self.failure(format!("FPS来源故障：{message}"));
                    // The source is already performing its cleanup. Another
                    // Stop here could accidentally retry CleanupFailed at40Hz.
                    if self.owned {
                        self.stopping = true;
                    }
                    if cleanup_pending {
                        self.status(format!("FPS故障，清理尚未完成；请显式重试：{message}"));
                    }
                }
                FpsMessage::Stopped(report) => {
                    self.active = false;
                    self.clear();
                    self.stopping = self.owned;
                    if !complete(&report) {
                        self.cleanup_uncertain = true;
                        self.failure("FPS结束消息未确认全部原生资源已释放".into());
                    } else if !self.failed {
                        self.status("FPS会话已停止；仍等待控制线程实际join");
                    }
                }
                _ => {} // late Started/Sample cannot revive a stopped consumer
            }
        }
        if self.owned && now >= self.next_status {
            self.next_status = now.saturating_add(100);
            if let Some(status) = self
                .source
                .status()
                .filter(|s| s.phase == FpsPhase::CleanupFailed)
            {
                self.note_failed_status(status);
            }
        }
        if self.owned {
            if let Some(result) = self.source.finish() {
                self.active = false;
                self.clear();
                self.dirty = true;
                match result {
                    Ok(report) if complete(&report) => {
                        self.owned = false;
                        self.stopping = false;
                        self.cleanup_uncertain = false;
                        self.finished = Some(Ok(()));
                        if !self.failed {
                            self.status("FPS自有会话与消费线程已实际结束");
                        }
                    }
                    Ok(_) => {
                        self.cleanup_uncertain = true;
                        self.stopping = true;
                        self.failure("FPS worker结束但未证明资源已释放；退出屏障保留".into());
                        self.finished = Some(Err("worker结束但未证明资源已释放".into()));
                    }
                    Err(error) => {
                        // FpsSampler's Some(Err) contract is post-cleanup; a
                        // still-owned cleanup failure never returns Some.
                        self.owned = false;
                        self.stopping = false;
                        self.cleanup_uncertain = false;
                        self.failure(format!("FPS来源已清理，但发生错误；请显式重试：{error:#}"));
                        self.finished = Some(Err(format!("{error:#}")));
                    }
                }
            }
        }
        if self
            .sample
            .as_ref()
            .is_some_and(|sample| !valid(sample, now))
        {
            self.clear();
            self.status("FPS已过期；等待新Present间隔");
        }
        if allowed
            && !self.owned
            && !self.cleanup_uncertain
            && !self.failed
            && !self.stopping
            && self.finished.is_none()
        {
            self.active = false;
            self.clear();
            self.losses = (0, 0);
            self.failed_attempt = None;
            self.finished = None;
            self.last_stop_ticket = None;
            self.next_status = now;
            self.dirty = true;
            let options = FpsOptions {
                enabled: true,
                ..FpsOptions::default()
            };
            match self.source.start(options) {
                Ok(()) => {
                    self.owned = true;
                    self.status("FPS已提交启动，尚未收到实际Started");
                }
                Err(error) => self.failure(format!("FPS未能启动；不自动重试：{error:#}")),
            }
        }
    }
    fn rebuild(&mut self) {
        let sample = self.sample.as_ref().filter(|_| self.is_active());
        let process = sample
            .map(|s| {
                format!(
                    "；{} · PID{}；{}/500间隔",
                    s.process.name, s.process.pid, s.intervals
                )
            })
            .unwrap_or_default();
        let detail = format!("{}；{}{}", fps::SOURCE, self.status, process);
        self.rows = KEYS
            .into_iter()
            .enumerate()
            .map(|(i, key)| {
                let value = sample.and_then(|s| match i {
                    0 => s.fps,
                    1 => s.low_fps,
                    _ => s.frame_time_ms,
                });
                Metric {
                    key: key.into(),
                    label: match i {
                        0 => "提交帧率",
                        1 => "提交帧率1% Low",
                        _ => "提交间隔",
                    }
                    .into(),
                    value: value
                        .map(|v| {
                            if i == 2 {
                                format!("{v:.2} ms")
                            } else {
                                format!("{v:.1} FPS")
                            }
                        })
                        .unwrap_or_else(|| "—".into()),
                    detail: if i == 1 {
                        format!("{detail}；满500间隔后最慢5个瞬时帧率的均值")
                    } else {
                        detail.clone()
                    },
                }
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Trace {
        starts: usize,
        stops: usize,
        messages: VecDeque<FpsMessage>,
        finish: Option<Result<FpsStopReport>>,
        status: Option<FpsStatus>,
        start_error: bool,
        enabled: bool,
    }
    struct Fake(Arc<Mutex<Trace>>);
    impl SourcePort for Fake {
        fn start(&mut self, options: FpsOptions) -> Result<()> {
            let mut t = self.0.lock().unwrap();
            t.starts += 1;
            t.enabled = options.enabled;
            if t.start_error {
                bail!("fake admission failure")
            }
            Ok(())
        }
        fn poll(&mut self) -> Option<FpsMessage> {
            self.0.lock().unwrap().messages.pop_front()
        }
        fn stop(&mut self) -> Option<u64> {
            let mut t = self.0.lock().unwrap();
            t.stops += 1;
            Some(t.stops as u64)
        }
        fn finish(&mut self) -> Option<Result<FpsStopReport>> {
            self.0.lock().unwrap().finish.take()
        }
        fn status(&self) -> Option<FpsStatus> {
            self.0.lock().unwrap().status.clone()
        }
    }
    fn fixture() -> (FpsHost, Arc<Mutex<Trace>>) {
        let t = Arc::new(Mutex::new(Trace::default()));
        (FpsHost::new(Box::new(Fake(t.clone()))), t)
    }
    fn report() -> FpsStopReport {
        FpsStopReport {
            session_name: "Jiks-Fps-fixture-owned".into(),
            session_stopped: true,
            consumer_joined: true,
            events_dropped: 0,
        }
    }
    fn sample(now: u64) -> FpsSample {
        FpsSample {
            process: fps::FpsProcess {
                pid: 1234,
                creation_filetime: 99,
                name: "fixture.exe".into(),
            },
            generation: 1,
            fps: Some(60.),
            low_fps: None,
            frame_time_ms: Some(1000. / 60.),
            intervals: 6,
            window_capacity: 500,
            sampled_tick_ms: now,
            last_present_tick_ms: now,
            events_dropped: 0,
            etw_buffers_lost: 0,
            source: fps::SOURCE.into(),
            process_aggregate: true,
            displayed_fps_verified: false,
        }
    }
    fn start(host: &mut FpsHost, t: &Arc<Mutex<Trace>>, now: u64) {
        host.advance(true, now);
        t.lock().unwrap().messages.push_back(FpsMessage::Started {
            session_name: "Jiks-Fps-fixture-owned".into(),
        });
        host.advance(true, now + 1);
        assert!(host.is_active());
    }
    fn emit(t: &Arc<Mutex<Trace>>, message: FpsMessage) {
        t.lock().unwrap().messages.push_back(message)
    }
    #[test]
    fn default_host_is_inert() {
        let h = FpsHost::default();
        assert!(!h.is_busy());
        assert!(!h.is_active());
        assert!(h.metrics().iter().all(|m| m.value == "—"));
    }
    #[test]
    fn exact_visibility_sensor_osd_and_readonly_gates() {
        let app = JsonNode::parse(r#"{"EnableHardwareSensors":true}"#).unwrap();
        let osd = JsonNode::parse(r#"{"ShowOsd":true,"Items":[0]}"#).unwrap();
        assert!(desired(&app, &osd, true, false).unwrap());
        assert!(!desired(&app, &osd, true, true).unwrap());
        assert!(!desired(&app, &osd, false, false).unwrap());
        assert!(!desired(&JsonNode::object(), &osd, true, false).unwrap());
        assert!(!desired(&app, &JsonNode::object(), true, false).unwrap());
        for items in [r#"[]"#, r#"[3,4,5]"#] {
            let osd = JsonNode::parse(&format!(r#"{{"ShowOsd":true,"Items":{items}}}"#)).unwrap();
            assert!(!desired(&app, &osd, true, false).unwrap());
        }
        for items in [
            r#"[0]"#,
            r#"[1]"#,
            r#"[2]"#,
            r#"["Fps"]"#,
            r#"["LowFps"]"#,
            r#"["FrameTime"]"#,
        ] {
            assert!(wants_fps(Some(&JsonNode::parse(items).unwrap())).unwrap());
        }
        assert!(wants_fps(None).unwrap());
        assert!(wants_fps(Some(&JsonNode::Null)).is_err());
    }
    #[test]
    fn admission_and_stopped_message_do_not_fake_started_or_join() {
        let (mut h, t) = fixture();
        h.advance(true, 100);
        assert_eq!(t.lock().unwrap().starts, 1);
        assert!(t.lock().unwrap().enabled);
        assert!(h.is_busy());
        assert!(!h.is_active());
        emit(
            &t,
            FpsMessage::Started {
                session_name: "owned".into(),
            },
        );
        h.advance(true, 101);
        assert!(h.is_active());
        h.advance(false, 102);
        h.advance(false, 103);
        assert_eq!(t.lock().unwrap().stops, 1);
        emit(&t, FpsMessage::Stopped(report()));
        h.advance(false, 104);
        assert!(h.is_busy());
        t.lock().unwrap().finish = Some(Ok(report()));
        h.advance(false, 105);
        assert!(!h.is_busy());
        assert!(!h.is_active());
    }
    #[test]
    fn read_only_or_hidden_never_submits_native_start() {
        let (mut h, t) = fixture();
        for now in 0..100 {
            h.advance(false, now);
        }
        assert_eq!(t.lock().unwrap().starts, 0);
        assert_eq!(t.lock().unwrap().stops, 0);
    }
    #[test]
    fn late_started_and_sample_cannot_revive_cancelled_host() {
        let (mut h, t) = fixture();
        h.advance(true, 100);
        h.cancel();
        emit(
            &t,
            FpsMessage::Started {
                session_name: "owned".into(),
            },
        );
        emit(&t, FpsMessage::Sample(sample(101)));
        h.advance(true, 102);
        assert!(!h.is_active());
        assert!(h.is_busy());
        assert!(h.sample.is_none());
        assert_eq!(t.lock().unwrap().stops, 1);
        h.cancel();
        assert_eq!(t.lock().unwrap().stops, 1);
    }
    #[test]
    fn cleanup_failure_keeps_owner_and_does_not_retry_each_tick() {
        let (mut h, t) = fixture();
        start(&mut h, &t, 100);
        t.lock().unwrap().status = Some(FpsStatus {
            phase: FpsPhase::CleanupFailed,
            reason: "fake stop failure".into(),
            cleanup_attempt: Some(0),
        });
        emit(
            &t,
            FpsMessage::Fault {
                message: "fake stop failure".into(),
                cleanup_pending: true,
            },
        );
        for now in 102..202 {
            h.advance(false, now);
        }
        assert!(h.is_busy());
        assert!(!h.is_active());
        assert_eq!(t.lock().unwrap().stops, 0);
        h.retry();
        assert_eq!(t.lock().unwrap().stops, 1);
        t.lock().unwrap().status = None;
        t.lock().unwrap().finish = Some(Ok(report()));
        h.advance(false, 203);
        assert!(!h.is_busy());
    }
    #[test]
    fn failed_start_is_latched_until_explicit_retry() {
        let (mut h, t) = fixture();
        t.lock().unwrap().start_error = true;
        for now in 100..200 {
            h.advance(true, now);
        }
        assert_eq!(t.lock().unwrap().starts, 1);
        assert!(!h.is_active());
        assert!(!h.is_busy());
        h.retry();
        t.lock().unwrap().start_error = false;
        h.advance(true, 201);
        assert_eq!(t.lock().unwrap().starts, 2);
        assert!(h.is_busy());
        assert!(!h.is_active());
    }
    #[test]
    fn unavailable_loss_and_stale_data_clear_values_instead_of_zero() {
        let (mut h, t) = fixture();
        start(&mut h, &t, 100);
        emit(&t, FpsMessage::Sample(sample(102)));
        h.advance(true, 102);
        h.rebuild();
        assert_eq!(h.metrics()[0].value, "60.0 FPS");
        assert_eq!(h.metrics()[1].value, "—");
        emit(
            &t,
            FpsMessage::Unavailable {
                reason: "no foreground".into(),
            },
        );
        h.advance(true, 103);
        h.rebuild();
        assert!(h.metrics().iter().all(|m| m.value == "—"));
        let mut s = sample(104);
        s.events_dropped = 1;
        emit(&t, FpsMessage::Sample(s.clone()));
        h.advance(true, 104);
        assert!(h.sample.is_none());
        s.sampled_tick_ms = 105;
        s.last_present_tick_ms = 105;
        emit(&t, FpsMessage::Sample(s));
        h.advance(true, 105);
        assert!(h.sample.is_some());
        h.advance(true, 2106);
        assert!(h.sample.is_none());
    }
    #[test]
    fn invalid_or_future_samples_are_not_displayed_as_valid() {
        for mutate in 0..6 {
            let (mut h, t) = fixture();
            start(&mut h, &t, 100);
            let mut s = sample(102);
            match mutate {
                0 => s.fps = Some(f64::NAN),
                1 => s.fps = Some(0.),
                2 => s.low_fps = Some(30.),
                3 => s.sampled_tick_ms = 200,
                4 => s.process.name = "C:\\private\\fixture.exe".into(),
                _ => s.displayed_fps_verified = true,
            };
            emit(&t, FpsMessage::Sample(s));
            h.advance(true, 102);
            assert!(h.sample.is_none());
        }
    }
    #[test]
    fn invalid_cleanup_report_retains_exit_barrier() {
        let (mut h, t) = fixture();
        start(&mut h, &t, 100);
        h.cancel();
        let mut bad = report();
        bad.consumer_joined = false;
        t.lock().unwrap().finish = Some(Ok(bad));
        h.advance(false, 102);
        assert!(h.is_busy());
        assert!(!h.is_active());
        assert!(h.cleanup_uncertain);
    }
    #[test]
    fn post_cleanup_worker_error_is_reported_without_leaking_busy() {
        let (mut h, t) = fixture();
        start(&mut h, &t, 100);
        h.cancel();
        t.lock().unwrap().finish = Some(Err(anyhow::anyhow!("fake source failure after cleanup")));
        h.advance(false, 102);
        assert!(!h.is_busy());
        assert!(h.failed);
        assert!(h.error.is_some());
    }
    #[test]
    fn cleanup_retry_ack_ignores_old_failure_and_waits_for_matching_attempt() {
        let (mut h, t) = fixture();
        start(&mut h, &t, 100);
        let first = h.begin_retry().unwrap();
        h.retry_waiters.push((11, first));
        t.lock().unwrap().status = Some(FpsStatus {
            phase: FpsPhase::CleanupFailed,
            reason: "old failure".into(),
            cleanup_attempt: Some(first - 1),
        });
        emit(
            &t,
            FpsMessage::Fault {
                message: "old queued fault".into(),
                cleanup_pending: true,
            },
        );
        h.advance(false, 202);
        assert!(h.take_retry_results().is_empty());
        assert!(h.is_busy());
        t.lock().unwrap().status = Some(FpsStatus {
            phase: FpsPhase::CleanupFailed,
            reason: "actual retry failed".into(),
            cleanup_attempt: Some(first),
        });
        h.advance(false, 303);
        let replies = h.take_retry_results();
        assert_eq!(replies.len(), 1);
        assert_eq!((replies[0].0, replies[0].1), (11, false));
        assert!(h.is_busy());
        assert_eq!(t.lock().unwrap().stops, 1);
    }
    #[test]
    fn retry_stopped_message_is_not_success_until_actual_join() {
        let (mut h, t) = fixture();
        start(&mut h, &t, 100);
        let ticket = h.begin_retry().unwrap();
        h.retry_waiters.push((22, ticket));
        emit(&t, FpsMessage::Stopped(report()));
        h.advance(false, 102);
        assert!(h.take_retry_results().is_empty());
        assert!(h.is_busy());
        t.lock().unwrap().finish = Some(Ok(report()));
        h.advance(true, 103);
        assert_eq!(t.lock().unwrap().starts, 1); // no new start before receipt is consumed
        let replies = h.take_retry_results();
        assert_eq!(replies.len(), 1);
        assert_eq!((replies[0].0, replies[0].1), (22, true));
        assert!(!h.is_busy());
    }
    #[test]
    fn cancel_clears_public_metrics_before_the_next_poll() {
        let (mut h, t) = fixture();
        start(&mut h, &t, 100);
        emit(&t, FpsMessage::Sample(sample(102)));
        h.advance(true, 102);
        h.rebuild();
        assert_eq!(h.metrics()[0].value, "60.0 FPS");
        h.cancel();
        assert!(h.metrics().iter().all(|m| m.value == "—"));
    }
    #[test]
    fn engine_saved_gates_actual_visibility_and_retry_result_use_real_host_paths() {
        let mut fixture = crate::service_engine::lifecycle_tests::Fixture::new();
        let engine = &mut fixture.engine;
        for (key, field) in [("application", "EnableHardwareSensors"), ("osd", "ShowOsd")] {
            let mut root = engine.repository.document(key).unwrap().root().clone();
            root.set_known(field, JsonNode::Bool(true)).unwrap();
            if key == "osd" {
                root.set_known("Items", JsonNode::parse("[0]").unwrap())
                    .unwrap();
            }
            engine
                .repository
                .save(key, SettingsDocument::from_root(root).unwrap())
                .unwrap();
        }
        let (mut host, trace) = self::fixture();
        engine.value("osd.actual_visible", json!(true));
        engine.read_only = true;
        host.poll(engine).unwrap();
        assert_eq!(trace.lock().unwrap().starts, 0);
        engine.read_only = false;
        engine.value("osd.actual_visible", json!(false));
        host.poll(engine).unwrap();
        assert_eq!(trace.lock().unwrap().starts, 0);
        engine.value("osd.actual_visible", json!(true));
        host.poll(engine).unwrap();
        assert_eq!(trace.lock().unwrap().starts, 1);
        assert!(!host.is_active());
        emit(
            &trace,
            FpsMessage::Started {
                session_name: "owned-fixture".into(),
            },
        );
        host.poll(engine).unwrap();
        assert!(host.is_active());
        host.request_retry(engine, 955).unwrap();
        assert!(
            !engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|r| r.request_id == 955)
        );
        emit(&trace, FpsMessage::Stopped(report()));
        host.poll(engine).unwrap();
        assert!(
            !engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|r| r.request_id == 955)
        );
        trace.lock().unwrap().finish = Some(Ok(report()));
        host.poll(engine).unwrap();
        let state = engine.state.lock().unwrap();
        assert!(state.results.iter().any(|r| r.request_id == 955 && r.ok));
        assert_eq!(state.values.get("fps.busy"), Some(&json!(false)));
    }
    #[test]
    fn actual_cleanup_failure_rejects_exit_but_retains_source_and_allows_retry() {
        let (mut host, trace) = self::fixture();
        start(&mut host, &trace, 100);
        trace.lock().unwrap().status = Some(FpsStatus {
            phase: FpsPhase::CleanupFailed,
            reason: "fake actual cleanup error".into(),
            cleanup_attempt: Some(0),
        });
        host.advance(true, 202);
        assert!(host.cleanup_failure().is_some());
        let mut fixture = crate::service_engine::lifecycle_tests::Fixture::new();
        fixture.engine.fps = host;
        fixture
            .engine
            .process(Command::Shutdown { request_id: 987 })
            .unwrap();
        assert!(!fixture.engine.poll_shutdown().unwrap());
        assert!(fixture.engine.shutdown.is_none());
        assert!(fixture.engine.fps.is_busy());
        assert!(
            fixture
                .engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|r| r.request_id == 987 && !r.ok)
        );
        assert!(fixture.log.lock().unwrap().is_empty());
        fixture.engine.fps.retry();
        assert!(fixture.engine.fps.is_busy());
        assert!(fixture.engine.fps.cleanup_failure().is_none()); // old epoch0 is not retry1
    }
}
