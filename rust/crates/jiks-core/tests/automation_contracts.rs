use jiks_core::automation::*;
use jiks_core::settings::JsonNode;
use jiks_core::{CoreError, CoreResult};

fn id(n: usize) -> String {
    normalize_guid(&format!("{n:032x}")).unwrap()
}
fn object(text: &str) -> JsonNode {
    JsonNode::parse(text).unwrap()
}
fn show(marker: &str) -> StepDto {
    let mut node = JsonNode::object();
    node.set("Marker", marker.into()).unwrap();
    StepDto::new(StepKind::ShowApp, node).unwrap()
}
fn emitting(marker: &str, preset: &str, fail: bool) -> StepDto {
    let mut node = show(marker).raw().clone();
    node.set("EmitPreset", preset.into()).unwrap();
    node.set("Fail", fail.into()).unwrap();
    StepDto::parse(node).unwrap()
}
fn reference(target: &str) -> StepDto {
    let mut node = JsonNode::object();
    node.set("PipelineId", target.into()).unwrap();
    StepDto::new(StepKind::QuickAction, node).unwrap()
}
fn startup() -> TriggerDto {
    TriggerDto::new(TriggerKind::OnStartup, JsonNode::object()).unwrap()
}
fn preset(target: &str) -> TriggerDto {
    let mut node = JsonNode::object();
    node.set("PresetId", target.into()).unwrap();
    TriggerDto::new(TriggerKind::GodModePresetChanged, node).unwrap()
}
fn pipeline(n: usize, trigger: Option<TriggerDto>, steps: Vec<StepDto>) -> PipelineDto {
    let mut pipeline = PipelineDto::new(&id(n), trigger, steps).unwrap();
    pipeline.is_exclusive = false;
    pipeline
}

#[derive(Default)]
struct FakeExecutor {
    calls: Vec<String>,
}
impl ActionExecutor for FakeExecutor {
    fn execute(
        &mut self,
        step: &StepDto,
        context: &mut AutomationContext,
        _environment: &AutomationEnvironment,
        _state: &mut AutomationState,
        emitted: &mut Vec<AutomationEvent>,
    ) -> CoreResult<()> {
        self.calls.push(
            step.field("Marker")
                .and_then(JsonNode::as_str)
                .unwrap_or("unnamed")
                .into(),
        );
        if let Some(preset) = step.field("EmitPreset").and_then(JsonNode::as_str) {
            emitted.push(AutomationEvent::PresetChanged(preset.into()));
        }
        if let Some(output) = step.field("Output").and_then(JsonNode::as_str) {
            context.set_run_output(output);
        }
        if step.field("Fail").and_then(JsonNode::as_bool) == Some(true) {
            return Err(CoreError::invalid("fixture action failed"));
        }
        Ok(())
    }
}

#[test]
fn complete_step_and_trigger_type_tables_roundtrip_without_reflection() {
    assert_eq!(StepKind::ALL.len(), 49);
    assert_eq!(TriggerKind::ALL.len(), 34);
    for &kind in StepKind::ALL {
        let node =
            object("{'FutureOption':1,'futureoption':2,'Future':1.2345678901234567890123456789}");
        let dto = StepDto::new(kind, node).unwrap();
        let parsed = StepDto::parse(JsonNode::parse(&dto.raw().to_json()).unwrap()).unwrap();
        assert_eq!(parsed.kind, kind);
        assert_eq!(parsed.raw(), dto.raw());
    }
    for &kind in TriggerKind::ALL {
        let mut node = object("{'Future':[null,1e9999]}");
        if matches!(kind, TriggerKind::And | TriggerKind::Or) {
            node.set("Triggers", JsonNode::Array(Vec::new())).unwrap();
        }
        let dto = TriggerDto::new(kind, node).unwrap();
        assert_eq!(TriggerDto::parse(dto.raw().clone()).unwrap(), dto);
    }
    let unknown = StepDto::parse(object(
        "{'$type':'System.Diagnostics.Process, System','Future':1e9999}",
    ))
    .unwrap();
    assert_eq!(unknown.kind, StepKind::Unknown);
    assert!(unknown.validate().is_err());
    assert!(unknown.raw().to_json().contains("1e9999"));
    let forged = StepDto::parse(object("{'$type':'LenovoLegionToolkit.Lib.Automation.Steps.PowerModeAutomationStep, Untrusted.Assembly','State':'GodMode'}")).unwrap();
    assert_eq!(forged.kind, StepKind::Unknown);
    assert!(forged.action_request().is_err());
}

#[test]
fn automation_document_preserves_unknown_types_and_canonicalizes_only_known_names() {
    let json = format!(
        r#"{{"isenabled":true,"FutureOption":1,"futureoption":2,"pipelines":[{{"id":"{}","name":"old","Steps":[{{"$type":"Future.Unknown, Missing","Raw":1e9999}}],"Future":null}}]}}"#,
        id(1)
    );
    let mut document = AutomationDocument::parse(&json, || {
        panic!("existing Guid must not generate a new identity")
    })
    .unwrap();
    assert!(document.enabled);
    assert_eq!(document.pipelines[0].steps[0].kind, StepKind::Unknown);
    document.pipelines[0].name = Some("new".into());
    let exported = JsonNode::parse(&document.to_json()).unwrap();
    assert!(exported.get("isenabled").is_none());
    assert!(exported.get("pipelines").is_none());
    assert_eq!(exported.get("FutureOption").unwrap().as_i64(), Some(1));
    assert_eq!(exported.get("futureoption").unwrap().as_i64(), Some(2));
    let p = &exported.get("Pipelines").unwrap().as_array().unwrap()[0];
    assert!(p.get("name").is_none());
    assert_eq!(p.get("Name").unwrap().as_str(), Some("new"));
    assert!(p.to_json().contains("1e9999"));
    let invalid = "{\"IsEnabled\":null,\"Pipelines\":[]}";
    assert!(AutomationDocument::parse(invalid, || id(2)).is_err());
}

#[test]
fn missing_pipeline_id_uses_injected_source_and_run_defaults_match_legacy() {
    let document =
        AutomationDocument::parse("{\"Pipelines\":[{\"Steps\":[]}]}", || id(77)).unwrap();
    assert!(!document.enabled);
    assert_eq!(document.pipelines[0].id, id(77));
    assert!(document.pipelines[0].is_exclusive);
    let run = StepDto::new(
        StepKind::Run,
        object("{'ScriptPath':'fixture.cmd','ScriptArguments':'a = b','RunSilently':null}"),
    )
    .unwrap();
    let options = run.run_options().unwrap();
    assert!(options.run_silently);
    assert!(!options.wait_until_finished);
    assert!(!options.check_instance);
    assert_eq!(options.script_arguments, "a = b");
    let mut context = AutomationContext::default();
    context.set_run_output("hello \r\n");
    assert_eq!(context.notification_text("out=$RUN_OUTPUT$"), "out=hello");
}

#[test]
fn root_and_indirect_reference_cycles_fail_before_first_action() {
    let root = pipeline(1, None, vec![show("must not run"), reference(&id(1))]);
    let mut executor = FakeExecutor::default();
    let mut state = AutomationState::default();
    assert!(run_pipeline(&root, &[], &mut state, &mut executor, &mut Vec::new()).is_err());
    assert!(executor.calls.is_empty());
    let a = pipeline(1, None, vec![show("must not run"), reference(&id(2))]);
    let b = pipeline(2, None, vec![reference(&id(3))]);
    let c = pipeline(3, None, vec![reference(&id(2))]);
    assert!(expand_pipeline(&a, &[b, c]).is_err());
    assert!(executor.calls.is_empty());
}

#[test]
fn deep_chain_is_iterative_dag_reuse_is_allowed_and_budget_is_finite() {
    let mut chain: Vec<_> = (0..2000)
        .map(|n| pipeline(n + 1, None, vec![reference(&id(n + 2))]))
        .collect();
    chain[1999].steps = vec![show("leaf")];
    let expanded = expand_pipeline(&chain[0], &chain[1..]).unwrap();
    assert_eq!(
        expanded
            .iter()
            .filter(|s| s.kind == StepKind::ShowApp)
            .count(),
        1
    );
    let leaf = pipeline(10_001, None, vec![show("reusable")]);
    let root = pipeline(10_002, None, vec![reference(&leaf.id), reference(&leaf.id)]);
    assert_eq!(
        expand_pipeline(&root, &[leaf])
            .unwrap()
            .iter()
            .filter(|s| s.kind == StepKind::ShowApp)
            .count(),
        2
    );
    let too_large = pipeline(11_000, None, vec![show("budget"); MAX_EXPANDED_STEPS + 1]);
    assert!(expand_pipeline(&too_large, &[]).is_err());
}

#[test]
fn dangling_or_triggered_quick_actions_skip_but_unknown_steps_disable_the_whole_plan() {
    let trigger_target = pipeline(2, Some(startup()), vec![show("not a quick action")]);
    let root = pipeline(
        1,
        None,
        vec![reference(&id(2)), reference(&id(999)), show("one")],
    );
    assert_eq!(expand_pipeline(&root, &[trigger_target]).unwrap().len(), 1);
    let unknown = StepDto::parse(object(
        "{'$type':'Untrusted.Future, Missing','Payload':1e9999}",
    ))
    .unwrap();
    let root = pipeline(1, None, vec![show("first"), unknown]);
    let mut fake = FakeExecutor::default();
    assert!(
        run_pipeline(
            &root,
            &[],
            &mut AutomationState::default(),
            &mut fake,
            &mut Vec::new()
        )
        .is_err()
    );
    assert!(fake.calls.is_empty());
}

#[test]
fn startup_drains_multiple_generations_before_success() {
    let first = id(20);
    let second = id(21);
    let pipelines = [
        pipeline(1, Some(startup()), vec![emitting("1", &first, false)]),
        pipeline(2, Some(preset(&first)), vec![emitting("2", &second, false)]),
        pipeline(3, Some(preset(&second)), vec![show("3")]),
    ];
    let mut fake = FakeExecutor::default();
    let report = run_startup(
        &pipelines,
        &mut AutomationState::default(),
        &mut fake,
        1,
        true,
    )
    .unwrap();
    assert_eq!(fake.calls, ["1", "2", "3"]);
    assert_eq!(report.attempted_steps, 3);
}

#[test]
fn startup_step_errors_finish_the_current_pipeline_but_skip_later_and_descendants() {
    let derived = id(20);
    let pipelines = [
        pipeline(
            1,
            Some(startup()),
            vec![
                emitting("failed", &derived, true),
                show("same-pipeline-continues"),
            ],
        ),
        pipeline(2, Some(startup()), vec![show("later-startup-must-not-run")]),
        pipeline(
            3,
            Some(preset(&derived)),
            vec![show("derived-must-not-run")],
        ),
    ];
    let mut fake = FakeExecutor::default();
    assert!(
        run_startup(
            &pipelines,
            &mut AutomationState::default(),
            &mut fake,
            1,
            true
        )
        .is_err()
    );
    assert_eq!(fake.calls, ["failed", "same-pipeline-continues"]);
}

#[test]
fn derived_failure_reaches_startup_owner_and_runtime_retains_continuation() {
    let derived = id(20);
    let pipelines = [
        pipeline(
            1,
            Some(startup()),
            vec![emitting("startup", &derived, false)],
        ),
        pipeline(
            2,
            Some(preset(&derived)),
            vec![emitting("derived-failed", &derived, true)],
        ),
        pipeline(3, Some(preset(&derived)), vec![show("runtime-later")]),
    ];
    let mut fake = FakeExecutor::default();
    assert!(
        run_startup(
            &pipelines,
            &mut AutomationState::default(),
            &mut fake,
            1,
            true
        )
        .is_err()
    );
    assert_eq!(fake.calls, ["startup", "derived-failed"]);
    let mut fake = FakeExecutor::default();
    let report = run_event(
        &pipelines,
        &AutomationEvent::PresetChanged(derived),
        &mut AutomationState::default(),
        &mut fake,
        &mut Vec::new(),
        true,
        false,
    )
    .unwrap();
    assert_eq!(fake.calls, ["derived-failed", "runtime-later"]);
    assert_eq!(report.step_failures.len(), 1);
}

#[test]
fn startup_budget_is_total_not_queue_length_and_closed_origins_are_ignored() {
    let loop_id = id(20);
    let pipelines = [
        pipeline(
            1,
            Some(startup()),
            vec![emitting("startup", &loop_id, false)],
        ),
        pipeline(
            2,
            Some(preset(&loop_id)),
            vec![emitting("loop", &loop_id, false)],
        ),
    ];
    let mut fake = FakeExecutor::default();
    assert!(
        run_startup(
            &pipelines,
            &mut AutomationState::default(),
            &mut fake,
            7,
            true
        )
        .is_err()
    );
    assert_eq!(fake.calls.len(), MAX_STARTUP_EVENTS);
    let mut batch = StartupBatch::new(7);
    assert_eq!(batch.enqueue_from(None, 1), EnqueueResult::Foreign(1));
    assert_eq!(batch.enqueue_from(Some(7), 2), EnqueueResult::Accepted);
    assert_eq!(batch.next_event().unwrap(), Some(2));
    assert_eq!(batch.next_event().unwrap(), None);
    assert!(batch.is_closed());
    assert_eq!(batch.enqueue_from(Some(7), 3), EnqueueResult::LateIgnored);
    assert_eq!(batch.enqueue_from(Some(8), 4), EnqueueResult::Foreign(4));
}

#[test]
fn initialization_and_startup_must_both_succeed_for_thermal_autostart() {
    let mut gate = StartupGate::default();
    assert!(!gate.thermal_autostart_allowed(true));
    assert!(gate.begin().is_err());
    gate.initialization_finished(false);
    assert!(gate.begin().is_err());
    gate.initialization_finished(true);
    gate.begin().unwrap();
    gate.finish(false).unwrap();
    assert!(!gate.thermal_autostart_allowed(true));
    gate.initialization_finished(true);
    gate.begin().unwrap();
    gate.finish(true).unwrap();
    assert!(gate.thermal_autostart_allowed(true));
    assert!(!gate.thermal_autostart_allowed(false));
}

#[test]
fn dangerous_startup_actions_and_disabled_automation_do_not_execute() {
    let dangerous = StepDto::new(StepKind::CloseApp, JsonNode::object()).unwrap();
    let pipelines = [pipeline(1, Some(startup()), vec![dangerous, show("safe")])];
    let mut fake = FakeExecutor::default();
    let report = run_startup(
        &pipelines,
        &mut AutomationState::default(),
        &mut fake,
        1,
        true,
    )
    .unwrap();
    assert_eq!(fake.calls, ["safe"]);
    assert_eq!(report.skipped_dangerous_steps, 1);
    let mut fake = FakeExecutor::default();
    run_startup(
        &pipelines,
        &mut AutomationState::default(),
        &mut fake,
        2,
        false,
    )
    .unwrap();
    assert!(fake.calls.is_empty());
}

#[test]
fn and_uses_other_current_states_and_or_environment_overwrites_in_order() {
    let hdr = TriggerDto::new(TriggerKind::HdrOn, JsonNode::object()).unwrap();
    let ac = TriggerDto::new(TriggerKind::AcConnected, JsonNode::object()).unwrap();
    let and = TriggerDto::composite(TriggerKind::And, vec![hdr.clone(), ac]).unwrap();
    let mut state = AutomationState {
        adapter: Some(AdapterState::Connected),
        hdr_on: Some(true),
        ..AutomationState::default()
    };
    assert!(
        and.matches_event(&AutomationEvent::Hdr(Some(true)), &state)
            .unwrap()
    );
    state.adapter = Some(AdapterState::Disconnected);
    assert!(
        !and.matches_event(&AutomationEvent::Hdr(Some(true)), &state)
            .unwrap()
    );
    assert!(!and.matches_state(&state).unwrap());
    let or = TriggerDto::composite(
        TriggerKind::Or,
        vec![
            hdr,
            TriggerDto::new(TriggerKind::HdrOff, JsonNode::object()).unwrap(),
        ],
    )
    .unwrap();
    assert!(
        or.matches_event(&AutomationEvent::Hdr(Some(true)), &state)
            .unwrap()
    );
    let mut env = AutomationEnvironment::default();
    or.update_environment(&mut env).unwrap();
    assert_eq!(env.variables["LLT_IS_HDR_ON"].as_deref(), Some("FALSE"));
}

#[test]
fn time_range_uses_utc_previous_day_after_midnight_and_inclusive_endpoints() {
    let range = TriggerDto::new(TriggerKind::TimeRange, object("{'StartTime':{'Hour':23,'Minute':0,'Second':0},'EndTime':{'Hour':2,'Minute':0,'Second':0},'Days':['Monday']}")).unwrap();
    let mut state = AutomationState {
        utc_time: TimeOfDay::new(1, 0, 0).unwrap(),
        utc_day: 2,
        ..AutomationState::default()
    };
    assert!(range.matches_state(&state).unwrap());
    state.utc_time = TimeOfDay::new(2, 0, 0).unwrap();
    assert!(range.matches_state(&state).unwrap());
    state.utc_time = TimeOfDay::new(3, 0, 0).unwrap();
    assert!(!range.matches_state(&state).unwrap());
    assert!(
        range
            .matches_event(
                &AutomationEvent::Time {
                    time: TimeOfDay::new(23, 0, 0).unwrap(),
                    day: 1
                },
                &state
            )
            .unwrap()
    );
    assert!(
        !range
            .matches_event(
                &AutomationEvent::Time {
                    time: TimeOfDay::new(1, 0, 0).unwrap(),
                    day: 2
                },
                &state
            )
            .unwrap()
    );
    let fallback = TriggerDto::new(TriggerKind::TimeRange, object("{'IsSunriseToSunset':true,'StartTime':{'Hour':1,'Minute':0,'Second':0},'EndTime':{'Hour':2,'Minute':0,'Second':0}}")).unwrap();
    assert!(
        fallback
            .matches_event(
                &AutomationEvent::Time {
                    time: TimeOfDay::new(1, 0, 0).unwrap(),
                    day: 0
                },
                &state
            )
            .unwrap()
    );
    assert!(!fallback.matches_state(&state).unwrap());
}

#[test]
fn periodic_uses_local_clock_and_legacy_environment_encodings_are_preserved() {
    let periodic = TriggerDto::new(TriggerKind::Periodic, object("{'Period':'00:00:05'}")).unwrap();
    let mut state = AutomationState {
        local_day_seconds: 10.,
        utc_time: TimeOfDay::new(1, 2, 3).unwrap(),
        ..AutomationState::default()
    };
    assert!(periodic.matches_state(&state).unwrap());
    state.local_day_seconds = 11.;
    assert!(!periodic.matches_state(&state).unwrap());
    let custom = TriggerDto::new(
        TriggerKind::PowerMode,
        object("{'PowerModeState':'GodMode'}"),
    )
    .unwrap();
    state.power_mode = Some(254);
    assert!(custom.matches_state(&state).unwrap());
    let mut env = AutomationEnvironment::default();
    custom.update_environment(&mut env).unwrap();
    assert_eq!(env.variables["LLT_POWER_MODE"].as_deref(), Some("255"));
    let time = TriggerDto::new(
        TriggerKind::Time,
        object("{'Time':{'Hour':1,'Minute':2,'Second':3},'Days':['Sunday','Monday']}"),
    )
    .unwrap();
    time.update_environment(&mut env).unwrap();
    assert_eq!(env.variables["LLT_TIME"].as_deref(), Some("1:2.3"));
    assert_eq!(env.variables["LLT_DAYS"].as_deref(), Some("SUNDAY,MONDAY"));
}

#[test]
fn battery_process_wifi_device_and_session_boundaries_match_legacy() {
    let below = TriggerDto::new(
        TriggerKind::BatteryPercentage,
        object("{'Percentage':50,'IsBelow':true}"),
    )
    .unwrap();
    let mut state = AutomationState {
        battery_percentage: Some(50),
        process_names: vec!["game".into()],
        wifi_ssid: Some("fixture".into()),
        connected_device_ids: vec!["ID1".into()],
        session_locked: Some(false),
        ..AutomationState::default()
    };
    assert!(!below.matches_state(&state).unwrap());
    state.battery_percentage = Some(49);
    assert!(below.matches_state(&state).unwrap());
    let processes = TriggerDto::new(
        TriggerKind::ProcessesAreRunning,
        object("{'Processes':[{'Name':'game','ExecutablePath':null},{'Name':'other'}]}"),
    )
    .unwrap();
    assert!(processes.matches_state(&state).unwrap());
    assert!(
        !processes
            .matches_event(
                &AutomationEvent::Process {
                    started: true,
                    name: "unrelated".into(),
                    executable_path: None
                },
                &state
            )
            .unwrap()
    );
    let wifi = TriggerDto::new(TriggerKind::WifiConnected, object("{'Ssids':[]}")).unwrap();
    assert!(wifi.matches_state(&state).unwrap());
    let device = TriggerDto::new(
        TriggerKind::DeviceConnected,
        object("{'InstanceIds':['ID1']}"),
    )
    .unwrap();
    assert!(device.matches_state(&state).unwrap());
    assert!(
        !device
            .matches_event(
                &AutomationEvent::Native {
                    kind: NativeEvent::DeviceConnected,
                    instance_id: Some("id1".into())
                },
                &state
            )
            .unwrap()
    );
    let unlock = TriggerDto::new(TriggerKind::SessionUnlock, JsonNode::object()).unwrap();
    let lock = TriggerDto::new(TriggerKind::SessionLock, JsonNode::object()).unwrap();
    assert!(
        unlock
            .matches_event(&AutomationEvent::Startup, &state)
            .unwrap()
    );
    assert!(
        !lock
            .matches_event(&AutomationEvent::Startup, &state)
            .unwrap()
    );
}

#[test]
fn all_49_steps_have_validated_inert_action_request_mapping() {
    for &kind in StepKind::ALL {
        let mut fields = JsonNode::object();
        match kind {
            StepKind::Delay => fields.set("State", object("{'DelaySeconds':1}")).unwrap(),
            StepKind::DpiScale => fields.set("State", object("{'Scale':150}")).unwrap(),
            StepKind::Resolution => fields
                .set("State", object("{'Width':1920,'Height':1080}"))
                .unwrap(),
            StepKind::RefreshRate => fields
                .set(
                    "State",
                    object("{'Frequency':60,'IsDynamic':false,'BaseFrequency':0}"),
                )
                .unwrap(),
            StepKind::DisplayBrightness => fields
                .set("Brightness", JsonNode::Number("50".into()))
                .unwrap(),
            StepKind::SpeakerVolume => fields.set("Volume", JsonNode::Number("50".into())).unwrap(),
            StepKind::SpectrumKeyboardProfile | StepKind::SpectrumKeyboardBrightness => {
                fields.set("State", JsonNode::Number("1".into())).unwrap()
            }
            StepKind::GodModePreset => fields.set("PresetId", id(77).into()).unwrap(),
            _ => {}
        }
        let dto = StepDto::new(kind, fields).unwrap();
        assert!(dto.action_request().is_ok(), "{kind:?}");
    }
    let power = StepDto::new(StepKind::PowerMode, object("{'State':'GodMode'}")).unwrap();
    assert_eq!(
        power.action_request().unwrap(),
        ActionRequest::Feature {
            kind: StepKind::PowerMode,
            state: FeatureState::Enum(254)
        }
    );
    let future = StepDto::new(
        StepKind::PowerMode,
        object("{'State':'FutureMode','Future':1e9999}"),
    )
    .unwrap();
    assert!(future.action_request().is_err());
    assert!(future.raw().to_json().contains("FutureMode"));
    let numeric_future = StepDto::new(StepKind::FnLock, object("{'State':999}")).unwrap();
    assert!(numeric_future.action_request().is_err());
}

#[test]
fn malformed_known_trigger_fields_fail_validation_without_executing_a_partial_plan() {
    let trigger = TriggerDto::new(
        TriggerKind::WifiConnected,
        object("{'Ssids':null,'Future':1e9999}"),
    )
    .unwrap();
    assert!(trigger.validate().is_err());
    assert!(trigger.raw().to_json().contains("1e9999"));
    let document = AutomationDocument::parse(
        &format!(
            r#"{{"Pipelines":[{{"Id":"{}","Trigger":{},"Steps":[]}}]}}"#,
            id(1),
            trigger.raw().to_json()
        ),
        || id(9),
    )
    .unwrap();
    assert!(document.validate_execution().is_err());
    let bad_time = TriggerDto::new(
        TriggerKind::Time,
        object("{'Time':{'Hour':99,'Minute':0,'Second':0}}"),
    )
    .unwrap();
    assert!(bad_time.validate().is_err());
}
