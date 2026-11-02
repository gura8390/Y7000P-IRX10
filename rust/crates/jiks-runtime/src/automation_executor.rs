//! Single-consumer automation scheduler. This module owns plans, never devices,
//! processes or threads. Admission/try_send is not an acknowledgement: the host
//! must report the actual worker result, including physical readback, before a
//! later step can be issued or the Engine can complete its pipeline.
use crate::automation_runtime::{
    ExecutionPlan, PlannedAction, StateSource, toggle_state_key, translate_step,
};
use anyhow::{Context, Result, ensure};
use jiks_core::automation::{AutomationContext, AutomationEnvironment, AutomationEvent};
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_EMITTED_EVENTS: usize = 128;
pub const STATE_FRESHNESS_MS: u64 = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRequirement {
    pub token: u64,
    pub step_index: usize,
    pub kind: StateSource,
    pub keys: Vec<String>,
    pub not_before_tick_ms: u64,
}
struct ConfirmedState {
    values: BTreeMap<String, Value>,
    completed_at: u64,
}

#[derive(Debug, Clone)]
pub struct StepEffect {
    pub plan_token: u64,
    pub step_token: u64,
    pub step_index: usize,
    pub origin: Option<u64>,
    pub environment: AutomationEnvironment,
    pub action: PlannedAction,
}

/// Only construct this once the admitted effect's actual operation is done.
/// A timed-out native ticket which still owns IO is not done; use
/// `note_inflight_failure` and keep waiting instead of acknowledging it.
#[derive(Debug, Clone, Default)]
pub struct StepCompletion {
    pub failure: Option<String>,
    /// Some("") clears the previous output; None leaves it unchanged (e.g. the
    /// legacy CheckInstance no-op). Only a Run effect may supply output.
    pub run_output: Option<String>,
    pub emitted: Vec<AutomationEvent>,
}

#[derive(Debug, Clone)]
pub struct PipelineCompletion {
    pub plan_token: u64,
    pub failures: Vec<String>,
    pub emitted: Vec<AutomationEvent>,
    pub attempted_steps: usize,
    pub cancelled: bool,
}

struct Running {
    plan: ExecutionPlan,
    cursor: usize,
    context: AutomationContext,
    failures: Vec<String>,
    emitted: Vec<AutomationEvent>,
    event_overflow: bool,
    pending: Option<StepEffect>,
    delay_until: Option<u64>,
    cancelled: bool,
    attempted_steps: usize,
    required: Option<StateRequirement>,
    confirmed: Option<ConfirmedState>,
}

#[derive(Default)]
pub struct Executor {
    running: Option<Running>,
    completion: Option<PipelineCompletion>,
    next_step_token: u64,
    last_tick_ms: Option<u64>,
    next_state_token: u64,
}

impl Executor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start(&mut self, plan: ExecutionPlan) -> Result<()> {
        ensure!(self.running.is_none(), "Automation executor is busy");
        ensure!(
            self.completion.is_none(),
            "Previous automation completion has not been consumed"
        );
        ensure!(
            plan.pipeline.source_steps.len() == plan.pipeline.actions.len(),
            "Automation source/action plan length mismatch"
        );
        for step in &plan.pipeline.source_steps {
            step.validate()?;
        }
        self.running = Some(Running {
            plan,
            cursor: 0,
            context: AutomationContext::default(),
            failures: Vec::new(),
            emitted: Vec::new(),
            event_overflow: false,
            pending: None,
            delay_until: None,
            cancelled: false,
            attempted_steps: 0,
            required: None,
            confirmed: None,
        });
        self.finish_if_ready();
        Ok(())
    }

    pub fn is_busy(&self) -> bool {
        self.running.is_some()
    }
    pub fn is_cancelled(&self) -> bool {
        self.running.as_ref().is_some_and(|r| r.cancelled)
    }

    pub fn pending_step(&self) -> Option<&StepEffect> {
        self.running.as_ref()?.pending.as_ref()
    }

    /// A returned nonce is a read intent owned until confirm_state/fail_state.
    /// Failed transport admission must call fail_state. Once admitted, even
    /// cancellation waits for the reader's actual completion acknowledgement.
    pub fn required_state(&mut self, now_ms: u64) -> Result<Option<StateRequirement>> {
        self.check_clock(now_ms)?;
        let Some(running) = self.running.as_mut() else {
            return Ok(None);
        };
        if running.cancelled
            || running.pending.is_some()
            || running.delay_until.is_some()
            || running.confirmed.is_some()
        {
            return Ok(None);
        }
        if let Some(required) = &running.required {
            return Ok(Some(required.clone()));
        }
        let Some((kind, key)) =
            toggle_state_key(&running.plan.pipeline.source_steps[running.cursor])?
        else {
            return Ok(None);
        };
        self.next_state_token = self
            .next_state_token
            .checked_add(1)
            .context("State verification token exhausted")?;
        let required = StateRequirement {
            token: self.next_state_token,
            step_index: running.cursor,
            kind,
            keys: vec![key.into()],
            not_before_tick_ms: now_ms,
        };
        running.required = Some(required.clone());
        Ok(Some(required))
    }
    pub fn confirm_state(
        &mut self,
        token: u64,
        values: &BTreeMap<String, Value>,
        requested_at_ms: u64,
        completed_at_ms: u64,
    ) -> Result<()> {
        let running = self.running.as_mut().context("No automation plan active")?;
        let required = running
            .required
            .as_ref()
            .context("No state verification pending")?;
        ensure!(
            running.confirmed.is_none(),
            "State verification was already acknowledged"
        );
        ensure!(
            required.token == token && required.step_index == running.cursor,
            "State verification acknowledgement mismatch"
        );
        ensure!(
            requested_at_ms >= required.not_before_tick_ms && completed_at_ms >= requested_at_ms,
            "State verification predates this step/read intent"
        );
        let mut confirmed = BTreeMap::new();
        for key in &required.keys {
            let value = values
                .get(key)
                .and_then(Value::as_bool)
                .with_context(|| format!("Verified state missing boolean {key}"))?;
            confirmed.insert(key.clone(), Value::Bool(value));
        }
        if running.cancelled {
            running.required = None;
            running.confirmed = None;
        } else {
            running.confirmed = Some(ConfirmedState {
                values: confirmed,
                completed_at: completed_at_ms,
            });
        }
        self.finish_if_ready();
        Ok(())
    }
    pub fn fail_state(&mut self, token: u64, error: impl Into<String>) -> Result<()> {
        let running = self.running.as_mut().context("No automation plan active")?;
        ensure!(
            running
                .required
                .as_ref()
                .is_some_and(|r| r.token == token && r.step_index == running.cursor),
            "State verification acknowledgement mismatch"
        );
        ensure!(
            running.confirmed.is_none(),
            "State verification was already acknowledged"
        );
        running.failures.push(format!(
            "Step {} state verification: {}",
            running.cursor,
            error.into()
        ));
        running.required = None;
        running.confirmed = None;
        running.attempted_steps += 1;
        running.cursor += 1;
        self.finish_if_ready();
        Ok(())
    }
    fn check_clock(&mut self, now_ms: u64) -> Result<()> {
        ensure!(
            self.last_tick_ms.is_none_or(|old| now_ms >= old),
            "Automation monotonic clock moved backwards"
        );
        self.last_tick_ms = Some(now_ms);
        Ok(())
    }

    /// Polling emits an effect at most once. Delays are monotonic deadlines and
    /// never sleep the consumer thread. Translation uses current confirmed
    /// values, rather than the plan's initial preview snapshot.
    pub fn poll(
        &mut self,
        now_ms: u64,
        values: &BTreeMap<String, Value>,
    ) -> Result<Option<StepEffect>> {
        self.check_clock(now_ms)?;
        loop {
            self.finish_if_ready();
            let Some(running) = self.running.as_mut() else {
                return Ok(None);
            };
            if running.pending.is_some() {
                return Ok(None);
            }
            if let Some(deadline) = running.delay_until {
                if now_ms < deadline {
                    return Ok(None);
                }
                running.delay_until = None;
                running.cursor += 1;
                continue;
            }
            let index = running.cursor;
            let source = &running.plan.pipeline.source_steps[index];
            if toggle_state_key(source)?.is_some() {
                let Some(confirmed) = &running.confirmed else {
                    return Ok(None);
                };
                ensure!(
                    confirmed.completed_at <= now_ms,
                    "Verified state is from a future clock tick"
                );
                if now_ms - confirmed.completed_at > STATE_FRESHNESS_MS {
                    running.required = None;
                    running.confirmed = None;
                    return Ok(None);
                }
            }
            let actual_values = running
                .confirmed
                .as_ref()
                .map_or(values, |state| &state.values);
            let mut action = match translate_step(source, actual_values) {
                Ok(action) => action,
                Err(error) => {
                    running.failures.push(format!("Step {index}: {error:#}"));
                    running.attempted_steps += 1;
                    running.cursor += 1;
                    running.required = None;
                    running.confirmed = None;
                    continue;
                }
            };
            running.attempted_steps += 1;
            running.required = None;
            running.confirmed = None;
            if let PlannedAction::Delay { milliseconds } = action {
                match now_ms.checked_add(milliseconds) {
                    Some(deadline) if milliseconds != 0 => {
                        running.delay_until = Some(deadline);
                        return Ok(None);
                    }
                    Some(_) => running.cursor += 1,
                    None => {
                        running
                            .failures
                            .push(format!("Step {index}: Delay deadline overflow"));
                        running.cursor += 1;
                    }
                }
                continue;
            }
            if let PlannedAction::Notification {
                text: Some(text), ..
            } = &mut action
            {
                *text = running.context.notification_text(text);
            }
            let token = self
                .next_step_token
                .checked_add(1)
                .context("Automation step token exhausted")?;
            self.next_step_token = token;
            let effect = StepEffect {
                plan_token: running.plan.token,
                step_token: token,
                step_index: index,
                origin: running.plan.origin,
                environment: running.plan.pipeline.environment.clone(),
                action,
            };
            running.pending = Some(effect.clone());
            return Ok(Some(effect));
        }
    }

    pub fn complete_step(&mut self, token: u64, result: StepCompletion) -> Result<()> {
        let running = self
            .running
            .as_mut()
            .context("No automation plan is active")?;
        let pending = running
            .pending
            .as_ref()
            .context("No automation step is pending")?;
        ensure!(
            pending.step_token == token,
            "Automation step acknowledgement mismatch"
        );
        ensure!(
            result.run_output.is_none()
                || matches!(pending.action, PlannedAction::Run { .. })
                || matches!(&pending.action,PlannedAction::Command(c) if c.command=="preset.cycle"),
            "Only a Run or CyclePreset step may supply contextual output"
        );
        // Validate before taking ownership: a malformed/stale acknowledgement
        // cannot erase the only record of the still-pending physical operation.
        if let Some(error) = result.failure {
            running
                .failures
                .push(format!("Step {}: {error}", running.cursor));
        }
        if let Some(output) = result.run_output {
            running.context.set_run_output(&output);
        }
        for event in result.emitted {
            if running.emitted.len() < MAX_EMITTED_EVENTS {
                running.emitted.push(event);
            } else if !running.event_overflow {
                running.event_overflow = true;
                running
                    .failures
                    .push("Automation emitted event capacity exceeded".into());
            }
        }
        running.pending = None;
        running.cursor += 1;
        self.finish_if_ready();
        Ok(())
    }

    /// Records a timeout/error without releasing the in-flight operation. Its
    /// eventual success cannot erase this failure or open the startup gate.
    pub fn note_inflight_failure(&mut self, token: u64, error: impl Into<String>) -> Result<()> {
        let running = self
            .running
            .as_mut()
            .context("No automation plan is active")?;
        ensure!(
            running
                .pending
                .as_ref()
                .is_some_and(|p| p.step_token == token),
            "Automation step acknowledgement mismatch"
        );
        running
            .failures
            .push(format!("Step {}: {}", running.cursor, error.into()));
        Ok(())
    }

    /// Stops admitting later steps. An already-admitted effect remains owned
    /// until complete_step reports actual completion, even during shutdown.
    pub fn cancel(&mut self, reason: impl Into<String>) -> bool {
        let Some(running) = self.running.as_mut() else {
            return false;
        };
        if !running.cancelled {
            running.cancelled = true;
            running
                .failures
                .push(format!("Automation cancelled: {}", reason.into()));
            running.delay_until = None;
        }
        self.finish_if_ready();
        true
    }

    pub fn take_completion(&mut self) -> Option<PipelineCompletion> {
        self.completion.take()
    }

    fn finish_if_ready(&mut self) {
        let ready = self.running.as_ref().is_some_and(|r| {
            r.pending.is_none()
                && (r.required.is_none() || r.confirmed.is_some())
                && (r.cancelled || r.cursor == r.plan.pipeline.source_steps.len())
        });
        if ready {
            let running = self.running.take().expect("ready plan exists");
            self.completion = Some(PipelineCompletion {
                plan_token: running.plan.token,
                failures: running.failures,
                emitted: running.emitted,
                attempted_steps: running.attempted_steps,
                cancelled: running.cancelled,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation_runtime::{Engine, plan_pipeline, step_from_ui};
    use jiks_core::{
        automation::{AutomationDocument, AutomationState, PipelineDto, TriggerDto, TriggerKind},
        settings::JsonNode,
    };
    use serde_json::json;

    fn node(value: Value) -> JsonNode {
        JsonNode::parse(&value.to_string()).unwrap()
    }
    fn plan(steps: Vec<Value>) -> ExecutionPlan {
        let steps = steps
            .into_iter()
            .map(|v| step_from_ui(&node(v)).unwrap().raw().clone())
            .collect();
        let mut raw = node(json!({"Id":"11111111-1111-1111-1111-111111111111","Steps":[]}));
        raw.set("Steps", JsonNode::Array(steps)).unwrap();
        let pipeline = PipelineDto::parse(raw, &mut || "unused".into()).unwrap();
        ExecutionPlan {
            token: 1,
            pipeline: plan_pipeline(&pipeline, &[], &BTreeMap::new()).unwrap(),
            origin: Some(42),
            request_id: None,
        }
    }

    #[test]
    fn admission_is_not_completion_and_rejected_ack_preserves_pending() {
        let mut e = Executor::new();
        e.start(plan(vec![
            json!({"type":"fn_lock","parameters":{"value":"on"}}),
            json!({"type":"fn_lock","parameters":{"value":"off"}}),
        ]))
        .unwrap();
        let first = e.poll(0, &BTreeMap::new()).unwrap().unwrap();
        assert!(e.poll(1, &BTreeMap::new()).unwrap().is_none());
        assert!(e.take_completion().is_none());
        assert!(
            e.complete_step(first.step_token + 1, StepCompletion::default())
                .is_err()
        );
        assert!(
            e.complete_step(
                first.step_token,
                StepCompletion {
                    run_output: Some("invalid".into()),
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert_eq!(e.pending_step().unwrap().step_token, first.step_token);
        e.complete_step(first.step_token, StepCompletion::default())
            .unwrap();
        let second = e.poll(2, &BTreeMap::new()).unwrap().unwrap();
        assert_ne!(first.step_token, second.step_token);
        e.complete_step(second.step_token, StepCompletion::default())
            .unwrap();
        let done = e.take_completion().unwrap();
        assert!(done.failures.is_empty());
        assert_eq!(done.attempted_steps, 2);
    }

    #[test]
    fn delays_are_deadlines_and_cancel_during_delay_never_emits_next() {
        let mut e = Executor::new();
        e.start(plan(vec![
            json!({"type":"delay","parameters":{"milliseconds":25}}),
            json!({"type":"show_app","parameters":{}}),
        ]))
        .unwrap();
        assert!(e.poll(100, &BTreeMap::new()).unwrap().is_none());
        assert!(e.poll(124, &BTreeMap::new()).unwrap().is_none());
        assert!(e.poll(123, &BTreeMap::new()).is_err());
        assert!(e.poll(125, &BTreeMap::new()).unwrap().is_some());
        let pending = e.pending_step().unwrap().step_token;
        e.complete_step(pending, StepCompletion::default()).unwrap();
        assert!(e.take_completion().unwrap().failures.is_empty());
        e.start(plan(vec![
            json!({"type":"delay","parameters":{"milliseconds":25}}),
            json!({"type":"show_app","parameters":{}}),
        ]))
        .unwrap();
        assert!(e.poll(200, &BTreeMap::new()).unwrap().is_none());
        assert!(e.cancel("shutdown"));
        assert!(e.poll(999, &BTreeMap::new()).unwrap().is_none());
        let done = e.take_completion().unwrap();
        assert!(done.cancelled);
        assert_eq!(done.attempted_steps, 1);
    }

    #[test]
    fn cancellation_timeout_and_late_success_keep_actual_ownership_and_failure() {
        let mut e = Executor::new();
        e.start(plan(vec![
            json!({"type":"show_app","parameters":{}}),
            json!({"type":"close_app","parameters":{}}),
        ]))
        .unwrap();
        let effect = e.poll(0, &BTreeMap::new()).unwrap().unwrap();
        e.note_inflight_failure(effect.step_token, "native deadline exceeded, still pending")
            .unwrap();
        assert!(e.cancel("shutdown"));
        assert!(e.is_busy());
        assert!(e.take_completion().is_none());
        assert!(e.poll(10000, &BTreeMap::new()).unwrap().is_none());
        assert!(e.start(plan(vec![])).is_err());
        e.complete_step(effect.step_token, StepCompletion::default())
            .unwrap();
        let done = e.take_completion().unwrap();
        assert_eq!(done.failures.len(), 2);
        assert_eq!(done.attempted_steps, 1);
        assert!(!e.is_busy());
    }

    #[test]
    fn toggles_use_each_confirmed_readback_and_failure_continues_remaining_steps() {
        let mut e = Executor::new();
        e.start(plan(vec![
            json!({"type":"macro","parameters":{"value":"toggle"}}),
            json!({"type":"macro","parameters":{"value":"toggle"}}),
            json!({"type":"show_app","parameters":{}}),
        ]))
        .unwrap();
        let mut values = BTreeMap::from([("macro.enabled".into(), json!(false))]);
        let required = e.required_state(0).unwrap().unwrap();
        e.confirm_state(required.token, &values, 0, 0).unwrap();
        let first = e.poll(0, &values).unwrap().unwrap();
        let PlannedAction::Command(command) = first.action else {
            panic!()
        };
        assert_eq!(
            command
                .payload
                .get("macro.enabled")
                .and_then(JsonNode::as_bool),
            Some(true)
        );
        e.complete_step(first.step_token, StepCompletion::default())
            .unwrap();
        values.insert("macro.enabled".into(), json!(true));
        let required = e.required_state(1).unwrap().unwrap();
        e.confirm_state(required.token, &values, 1, 1).unwrap();
        let second = e.poll(1, &values).unwrap().unwrap();
        let PlannedAction::Command(command) = second.action else {
            panic!()
        };
        assert_eq!(
            command
                .payload
                .get("macro.enabled")
                .and_then(JsonNode::as_bool),
            Some(false)
        );
        e.complete_step(
            second.step_token,
            StepCompletion {
                failure: Some("save failed".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let third = e.poll(2, &values).unwrap().unwrap();
        e.complete_step(third.step_token, StepCompletion::default())
            .unwrap();
        assert_eq!(e.take_completion().unwrap().failures.len(), 1);
    }

    #[test]
    fn run_output_is_trimmed_substituted_and_context_is_per_pipeline() {
        let mut e = Executor::new();
        let make = || {
            plan(vec![
                json!({"type":"run","parameters":{"path":"fixture-only.exe","wait":true}}),
                json!({"type":"notification","parameters":{"message":"Result: $RUN_OUTPUT$"}}),
            ])
        };
        e.start(make()).unwrap();
        let run = e.poll(0, &BTreeMap::new()).unwrap().unwrap();
        e.complete_step(
            run.step_token,
            StepCompletion {
                run_output: Some("test\r\n  ".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let notice = e.poll(1, &BTreeMap::new()).unwrap().unwrap();
        let PlannedAction::Notification { text, .. } = notice.action else {
            panic!()
        };
        assert_eq!(text.as_deref(), Some("Result: test"));
        e.complete_step(notice.step_token, StepCompletion::default())
            .unwrap();
        e.take_completion().unwrap();
        e.start(make()).unwrap();
        let run = e.poll(2, &BTreeMap::new()).unwrap().unwrap();
        e.complete_step(run.step_token, StepCompletion::default())
            .unwrap();
        let notice = e.poll(3, &BTreeMap::new()).unwrap().unwrap();
        let PlannedAction::Notification { text, .. } = notice.action else {
            panic!()
        };
        assert_eq!(text.as_deref(), Some("Result: "));
    }

    #[test]
    fn failed_translation_deadline_overflow_and_event_overflow_are_reported() {
        let mut e = Executor::new();
        e.start(plan(vec![
            json!({"type":"macro","parameters":{"value":"toggle"}}),
            json!({"type":"delay","parameters":{"milliseconds":1}}),
            json!({"type":"show_app","parameters":{}}),
        ]))
        .unwrap();
        let required = e.required_state(u64::MAX).unwrap().unwrap();
        e.fail_state(required.token, "actual state read failed")
            .unwrap();
        let action = e.poll(u64::MAX, &BTreeMap::new()).unwrap().unwrap();
        e.complete_step(
            action.step_token,
            StepCompletion {
                emitted: vec![AutomationEvent::Startup; 129],
                ..Default::default()
            },
        )
        .unwrap();
        let done = e.take_completion().unwrap();
        assert_eq!(done.failures.len(), 3);
        assert_eq!(done.emitted.len(), 128);
    }

    #[test]
    fn executor_completion_gates_startup_and_propagates_real_step_failure() {
        let pipeline = PipelineDto::new(
            "11111111-1111-1111-1111-111111111111",
            Some(TriggerDto::new(TriggerKind::OnStartup, JsonNode::object()).unwrap()),
            vec![step_from_ui(&node(json!({"type":"show_app","parameters":{}}))).unwrap()],
        )
        .unwrap();
        let raw = node(
            json!({"IsEnabled":true,"Pipelines":[pipeline.to_json_node().to_value().unwrap()]}),
        );
        let mut engine =
            Engine::new(AutomationDocument::parse(&raw.to_json(), || "unused".into()).unwrap());
        engine.initialize().unwrap();
        engine
            .start_startup(42, AutomationState::default())
            .unwrap();
        let mut e = Executor::new();
        e.start(engine.next_plan(&BTreeMap::new()).unwrap().unwrap())
            .unwrap();
        let action = e.poll(0, &BTreeMap::new()).unwrap().unwrap();
        assert!(!engine.startup_autostart_allowed());
        assert!(e.take_completion().is_none());
        e.complete_step(
            action.step_token,
            StepCompletion {
                failure: Some("actual dispatch rejected".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let done = e.take_completion().unwrap();
        assert!(
            engine
                .complete_plan(done.plan_token, done.failures, done.emitted)
                .is_err()
        );
        assert!(!engine.startup_autostart_allowed());
    }
    #[test]
    fn toggle_after_delay_waits_for_step_scoped_actual_read_and_observes_external_change() {
        let mut e = Executor::new();
        let plan = plan(vec![
            json!({"type":"macro","parameters":{"value":"toggle"}}),
            json!({"type":"delay","parameters":{"milliseconds":10}}),
            json!({"type":"macro","parameters":{"value":"toggle"}}),
        ]);
        assert!(matches!(
            plan.pipeline.actions[0],
            PlannedAction::Deferred { .. }
        ));
        e.start(plan).unwrap();
        let false_values = BTreeMap::from([("macro.enabled".into(), json!(false))]);
        let stale_true_values = BTreeMap::from([("macro.enabled".into(), json!(true))]);
        assert!(e.poll(10, &stale_true_values).unwrap().is_none());
        let first_read = e.required_state(10).unwrap().unwrap();
        assert_eq!(first_read.kind, StateSource::Configuration);
        assert!(
            e.confirm_state(first_read.token, &false_values, 9, 10)
                .is_err()
        );
        e.confirm_state(first_read.token, &false_values, 10, 11)
            .unwrap();
        let first = e.poll(11, &stale_true_values).unwrap().unwrap();
        let PlannedAction::Command(command) = first.action else {
            panic!()
        };
        assert_eq!(
            command
                .payload
                .get("macro.enabled")
                .and_then(JsonNode::as_bool),
            Some(true)
        );
        assert!(e.poll(49, &false_values).unwrap().is_none());
        e.complete_step(first.step_token, StepCompletion::default())
            .unwrap();
        assert!(e.poll(50, &stale_true_values).unwrap().is_none());
        assert!(e.poll(60, &stale_true_values).unwrap().is_none());
        let second_read = e.required_state(60).unwrap().unwrap();
        assert_ne!(first_read.token, second_read.token);
        assert!(
            e.confirm_state(first_read.token, &stale_true_values, 60, 60)
                .is_err()
        );
        // An external actor returned it to false during Delay; metadata still
        // says true. Only this actual step read may decide the target.
        e.confirm_state(second_read.token, &false_values, 60, 62)
            .unwrap();
        let second = e.poll(62, &stale_true_values).unwrap().unwrap();
        let PlannedAction::Command(command) = second.action else {
            panic!()
        };
        assert_eq!(
            command
                .payload
                .get("macro.enabled")
                .and_then(JsonNode::as_bool),
            Some(true)
        );
    }
    #[test]
    fn stale_confirmation_reissues_nonce_and_cancel_keeps_admitted_reader_owned() {
        let mut e = Executor::new();
        e.start(plan(vec![
            json!({"type":"macro","parameters":{"value":"toggle"}}),
        ]))
        .unwrap();
        let values = BTreeMap::from([("macro.enabled".into(), json!(false))]);
        let first = e.required_state(0).unwrap().unwrap();
        e.confirm_state(first.token, &values, 0, 1).unwrap();
        assert!(e.poll(1002, &values).unwrap().is_none());
        let fresh = e.required_state(1002).unwrap().unwrap();
        assert_ne!(first.token, fresh.token);
        assert!(e.cancel("shutdown while reader pending"));
        assert!(e.is_busy());
        assert!(e.take_completion().is_none());
        assert!(e.confirm_state(first.token, &values, 1002, 1003).is_err());
        e.confirm_state(fresh.token, &values, 1002, 1003).unwrap();
        let done = e.take_completion().unwrap();
        assert!(done.cancelled);
        assert_eq!(done.failures.len(), 1);
        assert!(!e.is_busy());
    }
}
