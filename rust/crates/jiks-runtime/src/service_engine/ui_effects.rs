//! Queueing a UI action is not a successful acknowledgement.
use super::*;
struct Effect {
    id: u64,
    kind: String,
    payload: Value,
    deadline: u64,
    detached: bool,
    external: Option<u64>,
}
#[derive(Default)]
pub(super) struct UiEffects {
    queue: VecDeque<Effect>,
    pending: Option<Effect>,
}
impl UiEffects {
    pub(super) fn is_busy(&self) -> bool {
        self.pending.is_some() || !self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_launch_show_finishes_external_request_only_after_native_ack() {
        let mut fixture = super::super::lifecycle_tests::Fixture::new();
        let engine = &mut fixture.engine;
        engine
            .process(Command::Execute {
                request_id: 991,
                command: "tools.show".into(),
                payload: json!({}),
            })
            .unwrap();
        engine.poll_ui_effects();
        assert!(
            !engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|result| result.request_id == 991)
        );
        let id = engine.ui_effects.pending.as_ref().unwrap().id;
        engine
            .complete_native_ui(&json!({"effect_id":id,"ok":true}))
            .unwrap();
        assert!(
            engine
                .state
                .lock()
                .unwrap()
                .results
                .iter()
                .any(|result| result.request_id == 991 && result.ok)
        );
        assert!(!engine.internal_requests.contains_key(&id));
    }
    #[test]
    fn opaque_descending_ids_require_exact_actual_ack_and_detached_receipts_do_not_leak() {
        let mut fixture = super::super::lifecycle_tests::Fixture::new();
        let engine = &mut fixture.engine;
        let first = engine.request_ui_effect("show", json!({})).unwrap();
        let second = engine.request_ui_effect("show", json!({})).unwrap();
        assert!(second < first);
        engine.poll_ui_effects();
        assert!(engine.take_internal_result(first).is_none());
        assert!(
            engine
                .complete_native_ui(&json!({"effect_id":second,"ok":true}))
                .is_err()
        );
        assert!(engine.ui_effects.pending.is_some());
        engine
            .complete_native_ui(&json!({"effect_id":first,"ok":true}))
            .unwrap();
        assert!(engine.take_internal_result(first).unwrap().is_ok());
        engine.poll_ui_effects();
        engine
            .complete_native_ui(&json!({"effect_id":second,"ok":false,"message":"failed"}))
            .unwrap();
        assert!(engine.take_internal_result(second).unwrap().is_err());
        engine.queue_notification(json!({"text":"test"})).unwrap();
        engine.poll_ui_effects();
        let id = engine.ui_effects.pending.as_ref().unwrap().id;
        engine
            .complete_native_ui(&json!({"effect_id":id,"ok":true}))
            .unwrap();
        assert!(!engine.internal_requests.contains_key(&id));
    }
    #[test]
    fn pending_ui_timeout_cannot_block_pipe_eof_shutdown_and_queued_effects_never_start() {
        let mut fixture = super::super::lifecycle_tests::Fixture::new();
        let engine = &mut fixture.engine;
        let first = engine.request_ui_effect("notification", json!({})).unwrap();
        let second = engine.request_ui_effect("show", json!({})).unwrap();
        engine.poll_ui_effects();
        engine.shutdown = Some(77);
        engine.ui_effects.pending.as_mut().unwrap().deadline = 0;
        engine.poll_ui_effects();
        assert!(!engine.ui_effects.is_busy());
        assert!(engine.take_internal_result(first).unwrap().is_err());
        assert!(engine.take_internal_result(second).unwrap().is_err());
        assert_eq!(
            engine.state.lock().unwrap().values.get("native.ui_effect"),
            Some(&Value::Null)
        );
    }
}
impl Engine {
    pub(super) fn notification_queued(&self, payload: &Value) -> bool {
        self.ui_effects
            .queue
            .back()
            .or(self.ui_effects.pending.as_ref())
            .is_some_and(|effect| {
                effect.kind == "notification"
                    && effect.payload.get("type") == payload.get("type")
                    && effect.payload.get("text") == payload.get("text")
            })
    }
    pub(super) fn request_ui_effect(&mut self, kind: &str, payload: Value) -> Result<u64> {
        ensure!(self.shutdown.is_none() && !self.faulted, "界面效果正在停止");
        ensure!(self.ui_effects.queue.len() < 32, "界面效果队列已满");
        let id = self.new_internal_request()?;
        self.ui_effects.queue.push_back(Effect {
            id,
            kind: kind.into(),
            payload,
            deadline: 0,
            detached: false,
            external: None,
        });
        Ok(id)
    }
    pub(super) fn queue_notification(&mut self, payload: Value) -> Result<()> {
        let id = self.request_ui_effect("notification", payload)?;
        if let Some(effect) = self
            .ui_effects
            .queue
            .iter_mut()
            .find(|effect| effect.id == id)
        {
            effect.detached = true;
        }
        Ok(())
    }
    pub(super) fn request_external_show(&mut self, id: u64) -> Result<()> {
        let effect = self.request_ui_effect("show", json!({}))?;
        let effect = self
            .ui_effects
            .queue
            .iter_mut()
            .find(|pending| pending.id == effect)
            .context("显示请求未排入队列")?;
        effect.external = Some(id);
        effect.detached = true;
        Ok(())
    }
    fn finish_ui_effect(&mut self, effect: Effect, ok: bool, message: impl Into<String>) {
        let message = message.into();
        if let Some(external) = effect.external {
            self.finish(external, ok, &message);
        }
        self.finish(effect.id, ok, message);
        if effect.detached {
            let _ = self.take_internal_result(effect.id);
        }
    }
    pub(super) fn complete_native_ui(&mut self, payload: &Value) -> Result<()> {
        let id = payload
            .get("effect_id")
            .and_then(Value::as_u64)
            .context("缺少界面效果回执ID")?;
        let pending = self
            .ui_effects
            .pending
            .as_ref()
            .context("没有待完成的界面效果")?;
        ensure!(pending.id == id, "过期或错误的界面效果回执");
        let ok = payload
            .get("ok")
            .and_then(Value::as_bool)
            .context("界面效果回执结果无效")?;
        let message = payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("界面效果已返回")
            .chars()
            .take(1024)
            .collect::<String>();
        let effect = self.ui_effects.pending.take().unwrap();
        self.value("native.ui_effect", Value::Null);
        self.finish_ui_effect(effect, ok, message);
        Ok(())
    }
    pub(super) fn poll_ui_effects(&mut self) {
        if self.shutdown.is_some() || self.faulted {
            while let Some(effect) = self.ui_effects.queue.pop_front() {
                self.finish_ui_effect(effect, false, "退出前取消尚未开始的界面效果");
            }
        }
        if self
            .ui_effects
            .pending
            .as_ref()
            .is_some_and(|effect| monotonic_ms() >= effect.deadline)
        {
            let effect = self.ui_effects.pending.take().unwrap();
            self.value("native.ui_effect", Value::Null);
            self.finish_ui_effect(effect, false, "界面未在期限内实际确认效果；没有假称成功");
        }
        if self.ui_effects.pending.is_none() && self.shutdown.is_none() && !self.faulted {
            if let Some(mut effect) = self.ui_effects.queue.pop_front() {
                effect.deadline = monotonic_ms().saturating_add(5000);
                self.value(
                    "native.ui_effect",
                    json!({"effect_id":effect.id,"kind":effect.kind,"payload":effect.payload}),
                );
                self.ui_effects.pending = Some(effect);
            }
        }
    }
}
