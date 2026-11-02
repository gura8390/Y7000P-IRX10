use super::{
    AutomationEnvironment, AutomationEvent, AutomationState, PipelineDto, StepDto, StepKind,
    TriggerKind,
};
use crate::{CoreError, CoreResult};
use std::collections::{HashMap, HashSet, VecDeque};

pub const MAX_EXPANDED_STEPS: usize = 10_000;
pub const MAX_STARTUP_EVENTS: usize = 128;

/// Iterative expansion validates the entire reachable quick-action graph before any executor call.
pub fn expand_pipeline(
    root: &PipelineDto,
    other_pipelines: &[PipelineDto],
) -> CoreResult<Vec<StepDto>> {
    let mut pipelines = vec![root];
    pipelines.extend(other_pipelines.iter());
    let mut by_id = HashMap::new();
    for (index, pipeline) in pipelines.iter().enumerate().skip(1) {
        by_id.entry(pipeline.id.clone()).or_insert(index);
    }
    by_id.insert(root.id.clone(), 0);
    struct Frame {
        pipeline: usize,
        index: usize,
        reference: Option<StepDto>,
    }
    let mut frames = vec![Frame {
        pipeline: 0,
        index: 0,
        reference: None,
    }];
    let mut path = HashSet::from([root.id.clone()]);
    let mut result = Vec::new();
    let mut visited = 0;
    while let Some(mut frame) = frames.pop() {
        let pipeline = pipelines[frame.pipeline];
        if frame.index >= pipeline.steps.len() {
            path.remove(&pipeline.id);
            if let Some(reference) = frame.reference {
                result.push(reference);
            }
            continue;
        }
        visited += 1;
        if visited > MAX_EXPANDED_STEPS {
            return Err(CoreError::Capacity(format!(
                "quick-action expansion exceeds {MAX_EXPANDED_STEPS}"
            )));
        }
        let step = pipeline.steps[frame.index].clone();
        frame.index += 1;
        frames.push(frame);
        if step.kind != StepKind::QuickAction {
            result.push(step);
            continue;
        }
        let Some(id) = step.referenced_pipeline()? else {
            continue;
        };
        let Some(&target) = by_id.get(&id) else {
            continue;
        };
        let pipeline = pipelines[target];
        if pipeline
            .trigger
            .as_ref()
            .is_some_and(|t| t.has_all_triggers())
        {
            continue;
        }
        if !path.insert(pipeline.id.clone()) {
            return Err(CoreError::Cycle(pipeline.id.clone()));
        }
        frames.push(Frame {
            pipeline: target,
            index: 0,
            reference: Some(step),
        });
    }
    for step in &result {
        step.validate()?;
    }
    Ok(result)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutomationContext {
    pub last_run_output: Option<String>,
}
impl AutomationContext {
    pub fn set_run_output(&mut self, output: &str) {
        self.last_run_output = Some(output.trim_end().to_owned());
    }
    pub fn notification_text(&self, template: &str) -> String {
        template.replace(
            "$RUN_OUTPUT$",
            self.last_run_output.as_deref().unwrap_or(""),
        )
    }
}
/// The platform supplies execution. Parsing/planning never constructs a platform service.
/// Synchronously emitted descendants are appended before returning, including error returns.
pub trait ActionExecutor {
    fn execute(
        &mut self,
        step: &StepDto,
        context: &mut AutomationContext,
        environment: &AutomationEnvironment,
        state: &mut AutomationState,
        emitted: &mut Vec<AutomationEvent>,
    ) -> CoreResult<()>;
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunReport {
    pub pipelines: Vec<String>,
    pub attempted_steps: usize,
    pub skipped_dangerous_steps: usize,
    pub step_failures: Vec<String>,
}
impl RunReport {
    fn append(&mut self, other: Self) {
        self.pipelines.extend(other.pipelines);
        self.attempted_steps += other.attempted_steps;
        self.skipped_dangerous_steps += other.skipped_dangerous_steps;
        self.step_failures.extend(other.step_failures);
    }
}
pub fn pipeline_environment(pipeline: &PipelineDto) -> CoreResult<AutomationEnvironment> {
    let mut environment = AutomationEnvironment::default();
    if let Some(trigger) = &pipeline.trigger {
        if matches!(trigger.kind, TriggerKind::And | TriggerKind::Or) {
            for child in &trigger.children {
                child.update_environment(&mut environment)?;
            }
        } else {
            trigger.update_environment(&mut environment)?;
        }
    }
    Ok(environment)
}
pub fn run_pipeline(
    pipeline: &PipelineDto,
    others: &[PipelineDto],
    state: &mut AutomationState,
    executor: &mut impl ActionExecutor,
    emitted: &mut Vec<AutomationEvent>,
) -> CoreResult<RunReport> {
    let plan = expand_pipeline(pipeline, others)?;
    let environment = pipeline_environment(pipeline)?;
    let mut context = AutomationContext::default();
    let mut report = RunReport {
        pipelines: vec![pipeline.id.clone()],
        ..RunReport::default()
    };
    for step in &plan {
        if environment.startup && step.is_dangerous_on_startup() {
            report.skipped_dangerous_steps += 1;
            continue;
        }
        // The reference marker is a no-op after its target's expansion, as in the legacy engine.
        if step.kind == StepKind::QuickAction {
            continue;
        }
        report.attempted_steps += 1;
        if let Err(error) = executor.execute(step, &mut context, &environment, state, emitted) {
            report.step_failures.push(error.to_string());
        }
    }
    Ok(report)
}
pub fn run_event(
    pipelines: &[PipelineDto],
    event: &AutomationEvent,
    state: &mut AutomationState,
    executor: &mut impl ActionExecutor,
    emitted: &mut Vec<AutomationEvent>,
    enabled: bool,
    fail_on_error: bool,
) -> CoreResult<RunReport> {
    let mut report = RunReport::default();
    if !enabled {
        return Ok(report);
    }
    for pipeline in pipelines {
        if matches!(event, AutomationEvent::Startup)
            && !pipeline.run_on_startup
            && !pipeline
                .trigger
                .as_ref()
                .is_some_and(|t| t.kind == TriggerKind::OnStartup)
        {
            continue;
        }
        let Some(trigger) = &pipeline.trigger else {
            continue;
        };
        let matched = match trigger.matches_event(event, state) {
            Ok(value) => value,
            Err(error) => {
                if fail_on_error {
                    return Err(error);
                }
                report.step_failures.push(error.to_string());
                continue;
            }
        };
        if !matched {
            continue;
        }
        match run_pipeline(pipeline, pipelines, state, executor, emitted) {
            Ok(result) => {
                let failures = !result.step_failures.is_empty();
                report.append(result);
                if failures && fail_on_error {
                    return Err(CoreError::invalid(report.step_failures.join("; ")));
                }
            }
            Err(error) => {
                if fail_on_error {
                    return Err(error);
                }
                report.step_failures.push(error.to_string());
            }
        }
        if pipeline.is_exclusive {
            break;
        }
    }
    Ok(report)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueResult<E> {
    Accepted,
    LateIgnored,
    Foreign(E),
}
/// A batch is owned by one executor; share it behind one mutex if callbacks arrive concurrently.
/// Limits are recorded here and raised by `next_event`, never through an async-void/native callback.
#[derive(Debug)]
pub struct StartupBatch<E> {
    id: u64,
    queue: VecDeque<E>,
    accepted: usize,
    limit: usize,
    closed: bool,
    failure: Option<CoreError>,
}
impl<E> StartupBatch<E> {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            queue: VecDeque::new(),
            accepted: 0,
            limit: MAX_STARTUP_EVENTS,
            closed: false,
            failure: None,
        }
    }
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn is_closed(&self) -> bool {
        self.closed
    }
    pub fn accepted_events(&self) -> usize {
        self.accepted
    }
    pub fn pending_events(&self) -> usize {
        self.queue.len()
    }
    pub fn enqueue_from(&mut self, origin: Option<u64>, event: E) -> EnqueueResult<E> {
        if origin != Some(self.id) {
            return EnqueueResult::Foreign(event);
        }
        if self.closed {
            return EnqueueResult::LateIgnored;
        }
        if self.failure.is_some() {
            return EnqueueResult::Accepted;
        }
        self.accepted += 1;
        if self.accepted > self.limit {
            self.failure = Some(CoreError::Capacity(format!(
                "startup event limit {} exceeded",
                self.limit
            )));
        } else {
            self.queue.push_back(event);
        }
        EnqueueResult::Accepted
    }
    pub fn next_event(&mut self) -> CoreResult<Option<E>> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        let value = self.queue.pop_front();
        if value.is_none() {
            self.closed = true;
        }
        Ok(value)
    }
    pub fn close(&mut self) {
        self.closed = true;
        self.queue.clear();
    }
}
pub fn run_startup(
    pipelines: &[PipelineDto],
    state: &mut AutomationState,
    executor: &mut impl ActionExecutor,
    batch_id: u64,
    enabled: bool,
) -> CoreResult<RunReport> {
    if !enabled {
        return Ok(RunReport::default());
    }
    let mut batch = StartupBatch::new(batch_id);
    batch.enqueue_from(Some(batch_id), AutomationEvent::Startup);
    let mut report = RunReport::default();
    let result = (|| {
        while let Some(event) = batch.next_event()? {
            let mut emitted = Vec::new();
            let outcome = run_event(pipelines, &event, state, executor, &mut emitted, true, true);
            match outcome {
                Ok(outcome) => report.append(outcome),
                Err(error) => return Err(error),
            }
            for event in emitted {
                batch.enqueue_from(Some(batch_id), event);
            }
        }
        Ok(report)
    })();
    batch.close();
    result
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StartupState {
    #[default]
    Uninitialized,
    Initialized,
    Running,
    Complete,
    Failed,
}
#[derive(Debug, Default)]
pub struct StartupGate {
    state: StartupState,
}
impl StartupGate {
    pub fn state(&self) -> StartupState {
        self.state
    }
    pub fn initialization_finished(&mut self, succeeded: bool) {
        self.state = if succeeded {
            StartupState::Initialized
        } else {
            StartupState::Failed
        };
    }
    pub fn begin(&mut self) -> CoreResult<()> {
        if self.state != StartupState::Initialized {
            return Err(CoreError::invalid(
                "automation initialization must succeed before startup",
            ));
        }
        self.state = StartupState::Running;
        Ok(())
    }
    pub fn finish(&mut self, succeeded: bool) -> CoreResult<()> {
        if self.state != StartupState::Running {
            return Err(CoreError::invalid("no running startup batch"));
        }
        self.state = if succeeded {
            StartupState::Complete
        } else {
            StartupState::Failed
        };
        Ok(())
    }
    pub fn thermal_autostart_allowed(&self, requested: bool) -> bool {
        requested && self.state == StartupState::Complete
    }
}
