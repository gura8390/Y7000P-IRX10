use jiks_core::{
    automation::{AutomationEvent, AutomationState, TriggerDto, TriggerKind},
    settings::JsonNode,
};
use serde_json::json;
fn trigger(kind: TriggerKind, name: &str) -> TriggerDto {
    TriggerDto::new(
        kind,
        JsonNode::parse(
            &json!({"Processes":[{"Name":name,"ExecutablePath":null}],"Future":-0}).to_string(),
        )
        .unwrap(),
    )
    .unwrap()
}
#[test]
fn process_state_and_runtime_start_stop_match_mixed_case_without_rewriting_configuration() {
    let raw = JsonNode::parse(
        r#"{"Processes":[{"Name":"GaMe","ExecutablePath":null}],"Future":1e9999,"future":-0}"#,
    )
    .unwrap();
    let configured = TriggerDto::new(TriggerKind::ProcessesAreRunning, raw).unwrap();
    let original = configured.raw().to_json();
    let mut state = AutomationState::default();
    state.process_names = vec!["gAmE".into()];
    assert!(configured.matches_state(&state).unwrap());
    assert!(
        configured
            .matches_event(
                &AutomationEvent::Process {
                    started: true,
                    name: "GAME".into(),
                    executable_path: Some("D:\\fixture\\GAME.exe".into())
                },
                &state
            )
            .unwrap()
    );
    assert_eq!(configured.raw().to_json(), original);
    assert!(original.contains("1e9999") && original.contains("-0"));
    let stopped = trigger(TriggerKind::ProcessesStopRunning, "GAME");
    state.process_names.clear();
    assert!(
        stopped
            .matches_event(
                &AutomationEvent::Process {
                    started: false,
                    name: "game".into(),
                    executable_path: None
                },
                &state
            )
            .unwrap()
    );
    assert!(
        !trigger(TriggerKind::ProcessesAreRunning, "GAME.exe")
            .matches_state(&AutomationState {
                process_names: vec!["game".into()],
                ..Default::default()
            })
            .unwrap()
    );
}
#[test]
fn process_name_ordinal_matching_agrees_with_independent_dotnet_unicode_fixture() {
    let cases: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/ordinal-csharp.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let left = case["left"].as_str().unwrap();
        let right = case["right"].as_str().unwrap();
        let expected = case["equal"].as_bool().unwrap();
        let state = AutomationState {
            process_names: vec![right.into()],
            ..Default::default()
        };
        let trigger = trigger(TriggerKind::ProcessesAreRunning, left);
        assert_eq!(
            trigger.matches_state(&state).unwrap(),
            expected,
            "{left:?} {right:?}"
        );
        assert_eq!(
            trigger
                .matches_event(
                    &AutomationEvent::Process {
                        started: true,
                        name: right.into(),
                        executable_path: None
                    },
                    &state
                )
                .unwrap(),
            expected,
            "event {left:?} {right:?}"
        );
    }
}
