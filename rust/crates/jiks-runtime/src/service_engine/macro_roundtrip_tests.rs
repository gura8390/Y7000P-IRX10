//! Full recording/persistence/playback regression with a fake event source.
//! No NativeRecorder.start reaches a hook, lease, thread or input adapter.
use super::*;
use crate::macros::{
    CapturedEvent, Clock, Direction, InputButton, InputEvent, InputTransport, MouseButton,
    PlaybackControl, PlaybackStatus, Point, RecordDisposition, Source,
};
use std::collections::BTreeSet;

#[derive(Default)]
struct FakeClock(Vec<u64>);
impl Clock for FakeClock {
    fn wait(&mut self, ticks: u64, _: &PlaybackControl) -> Result<()> {
        self.0.push(ticks);
        Ok(())
    }
}
#[derive(Default)]
struct FakeInput {
    events: Vec<InputEvent>,
    down: BTreeSet<InputButton>,
}
impl InputTransport for FakeInput {
    fn is_down(&mut self, button: InputButton) -> Result<bool> {
        Ok(self.down.contains(&button))
    }
    fn send(&mut self, event: InputEvent) -> Result<()> {
        if let InputEvent::Button { button, up } = event {
            if up {
                self.down.remove(&button);
            } else {
                self.down.insert(button);
            }
        }
        self.events.push(event);
        Ok(())
    }
}
fn event(
    source: Source,
    direction: Direction,
    key: u32,
    timestamp_ms: u32,
    point: Point,
) -> CapturedEvent {
    CapturedEvent {
        source,
        direction,
        key,
        point,
        timestamp_ms,
        injected: false,
        extra_info: 0,
    }
}
fn execute(engine: &mut Engine, id: u64, command: &str, payload: Value) {
    engine
        .process(Command::Execute {
            request_id: id,
            command: command.into(),
            payload,
        })
        .unwrap();
}
fn completed(engine: &Engine, id: u64) -> Option<bool> {
    engine
        .state
        .lock()
        .unwrap()
        .results
        .iter()
        .find(|result| result.request_id == id)
        .map(|result| result.ok)
}
fn recording(engine: &Engine) -> (u64, String, Value) {
    let state = engine.state.lock().unwrap();
    let revision = state.values["macro.recording.revision"].as_u64().unwrap();
    let items = &state.collections["macro.recording"];
    assert_eq!(items.len(), 1);
    (revision, items[0].id.clone(), items[0].value.clone())
}

#[test]
fn fake_source_actual_stop_engine_normalization_save_reload_and_playback_preserve_every_event() {
    let mut fixture = lifecycle_tests::Fixture::new();
    let (recorder, source) = NativeRecorder::fixture();
    fixture.engine.recorder = recorder;
    execute(
        &mut fixture.engine,
        800,
        "macro.record.start",
        json!({"mode":"keyboard_mouse_movement"}),
    );
    assert!(fixture.engine.record_preparation.is_some());
    assert_eq!(source.starts(), 0);
    assert_eq!(completed(&fixture.engine, 800), None);
    // Advance only this owned preparation deadline; no real three-second wait.
    fixture.engine.record_preparation.as_mut().unwrap().deadline = 0;
    fixture.engine.poll_macros().unwrap();
    assert_eq!(source.starts(), 1);
    assert_eq!(completed(&fixture.engine, 800), Some(true));
    let origin = Point::default();
    let events = [
        event(Source::Keyboard, Direction::Down, 65, 1000, origin),
        event(
            Source::Mouse,
            Direction::Move,
            0,
            1010,
            Point { x: -20, y: 30 },
        ),
        event(Source::Mouse, Direction::Down, 1, 1025, origin),
        event(Source::Mouse, Direction::Wheel, 120, 1040, origin),
        event(
            Source::Mouse,
            Direction::HorizontalWheel,
            (-240i32) as u32,
            1050,
            origin,
        ),
        event(
            Source::Mouse,
            Direction::Move,
            0,
            1060,
            Point { x: 300, y: -120 },
        ),
        event(Source::Mouse, Direction::Up, 1, 1075, origin),
        event(Source::Keyboard, Direction::Up, 65, 1100, origin),
    ];
    for event in events {
        assert_eq!(source.feed(event).unwrap(), RecordDisposition::Recorded);
    }
    fixture.engine.poll_macros().unwrap();
    assert_eq!(
        fixture.engine.state.lock().unwrap().values["macro.recording.count"],
        8
    );
    assert!(
        !fixture
            .engine
            .state
            .lock()
            .unwrap()
            .collections
            .contains_key("macro.recording")
    );
    execute(&mut fixture.engine, 801, "macro.record.stop", json!({}));
    assert!(source.stop_requested() && fixture.engine.recorder.is_active());
    fixture.engine.poll_macros().unwrap();
    assert_eq!(completed(&fixture.engine, 801), None);
    assert!(
        !fixture
            .engine
            .state
            .lock()
            .unwrap()
            .collections
            .contains_key("macro.recording")
    );
    source.acknowledge_cleanup().unwrap();
    // Uses RecorderState::report itself (a legacy sequence object with Events),
    // not a fabricated UI array. This is the regression boundary.
    fixture.engine.poll_macros().unwrap();
    assert!(!fixture.engine.recorder.is_active());
    assert_eq!(completed(&fixture.engine, 801), Some(true));
    let (first_revision, first_id, ui_events) = recording(&fixture.engine);
    assert_eq!(first_revision, 1);
    assert_eq!(first_id, "recording-1");
    let rows = ui_events.as_array().unwrap();
    assert_eq!(rows.len(), 8);
    let kinds = rows
        .iter()
        .map(|row| row["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        [
            "key_down",
            "mouse_move",
            "mouse_down",
            "wheel",
            "horizontal_wheel",
            "mouse_move",
            "mouse_up",
            "key_up"
        ]
    );
    assert_eq!(rows[4]["key"], -240);
    assert_eq!(rows[1]["x"], -20);
    assert_eq!(rows[5]["y"], -120);
    assert_eq!(
        rows.iter()
            .map(|row| row["delay_ms"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [0, 10, 15, 15, 10, 10, 15, 25]
    );
    let before_metadata = fixture.engine.state.lock().unwrap().metadata_revision;
    fixture.engine.poll_macros().unwrap();
    assert_eq!(
        recording(&fixture.engine),
        (first_revision, first_id.clone(), ui_events.clone())
    );
    assert_eq!(
        fixture.engine.state.lock().unwrap().metadata_revision,
        before_metadata
    );

    let payload=JsonNode::parse(&json!({"name":"Recorded fixture","trigger_key":96,"repeat":1,"ignore_delays":false,"interrupt_on_other_key":true,"events":ui_events}).to_string()).unwrap();
    let change =
        config_actions::apply_action("macro.save", &payload, &fixture.engine.documents()).unwrap();
    assert_eq!(change.documents.len(), 1);
    assert_eq!(change.documents[0].0, "macros");
    for (key, document) in change.documents {
        fixture.engine.repository.save(&key, document).unwrap();
    }
    let reloaded = ConfigRepository::load(fixture.engine.repository.root()).unwrap();
    let document =
        macros::MacroDocument::from_settings(reloaded.document("macros").unwrap().clone()).unwrap();
    assert!(
        !document.enabled,
        "Recording/saving must not enable global shortcuts"
    );
    let sequence = document
        .get(&MacroIdentifier {
            source: "Keyboard".into(),
            key: 96,
        })
        .unwrap();
    assert_eq!(sequence.events.len(), 8);
    assert_eq!(
        sequence
            .events
            .iter()
            .map(|event| event.delay_ticks)
            .collect::<Vec<_>>(),
        [0, 100000, 150000, 150000, 100000, 100000, 150000, 250000]
    );
    let mut clock = FakeClock::default();
    let mut input = FakeInput::default();
    let played = macros::play(sequence, &mut clock, &mut input, &PlaybackControl::new()).unwrap();
    assert_eq!(played.status, PlaybackStatus::Completed);
    assert_eq!(played.events_sent, 8);
    assert_eq!(played.release_attempts, 0);
    assert!(played.unreleased.is_empty() && played.errors.is_empty() && input.down.is_empty());
    assert_eq!(
        clock.0,
        [100000, 150000, 150000, 100000, 100000, 150000, 250000]
    );
    assert_eq!(
        input.events,
        [
            InputEvent::Button {
                button: InputButton::Keyboard(65),
                up: false
            },
            InputEvent::Move(Point { x: -20, y: 30 }),
            InputEvent::Button {
                button: InputButton::Mouse(MouseButton::Left),
                up: false
            },
            InputEvent::Wheel {
                delta: 120,
                horizontal: false
            },
            InputEvent::Wheel {
                delta: -240,
                horizontal: true
            },
            InputEvent::Move(Point { x: 300, y: -120 }),
            InputEvent::Button {
                button: InputButton::Mouse(MouseButton::Left),
                up: true
            },
            InputEvent::Button {
                button: InputButton::Keyboard(65),
                up: true
            },
        ]
    );
    let saved = std::fs::read(reloaded.root().join("macro.json")).unwrap();
    // A second recording has a unique revision/item even if no manual Save is
    // performed. Polling the same completion cannot republish it repeatedly.
    execute(
        &mut fixture.engine,
        802,
        "macro.record.start",
        json!({"mode":"keyboard"}),
    );
    for event in [
        event(Source::Keyboard, Direction::Down, 66, 2000, origin),
        event(Source::Keyboard, Direction::Up, 66, 2020, origin),
    ] {
        source.feed(event).unwrap();
    }
    execute(&mut fixture.engine, 803, "macro.record.stop", json!({}));
    source.acknowledge_cleanup().unwrap();
    fixture.engine.poll_macros().unwrap();
    let (second_revision, second_id, second_events) = recording(&fixture.engine);
    assert_eq!(second_revision, first_revision + 1);
    assert_ne!(second_id, first_id);
    assert_eq!(second_events.as_array().unwrap().len(), 2);
    assert_eq!(
        std::fs::read(reloaded.root().join("macro.json")).unwrap(),
        saved,
        "Recording is a draft, not automatic persistence"
    );
    fixture.engine.poll_macros().unwrap();
    assert_eq!(recording(&fixture.engine).0, second_revision);
    assert!(fixture.log.lock().unwrap().is_empty());
}

#[test]
fn cancelling_three_second_movement_preparation_never_starts_a_source() {
    for cancel_readonly in [false, true] {
        let mut fixture = lifecycle_tests::Fixture::new();
        let (recorder, source) = NativeRecorder::fixture();
        fixture.engine.recorder = recorder;
        let before = monotonic_ms();
        execute(
            &mut fixture.engine,
            810,
            "macro.record.start",
            json!({"mode":"keyboard_mouse_movement"}),
        );
        assert!(
            fixture.engine.record_preparation.as_ref().unwrap().deadline
                >= before.saturating_add(3000)
        );
        assert_eq!(
            fixture.engine.state.lock().unwrap().values["macro.prepare_remaining_seconds"],
            3
        );
        assert!(!fixture.engine.recorder.is_active());
        fixture.engine.poll_macros().unwrap();
        assert_eq!(source.starts(), 0);
        if cancel_readonly {
            fixture.engine.read_only = true;
        } else {
            execute(&mut fixture.engine, 811, "macro.record.stop", json!({}));
        }
        fixture.engine.poll_macros().unwrap();
        assert!(fixture.engine.record_preparation.is_none());
        assert_eq!(completed(&fixture.engine, 810), Some(false));
        if !cancel_readonly {
            assert_eq!(completed(&fixture.engine, 811), Some(true));
        }
        for _ in 0..3 {
            fixture.engine.poll_macros().unwrap();
        }
        assert_eq!(source.starts(), 0);
        assert!(!source.stop_requested() && !fixture.engine.recorder.is_active());
        assert_eq!(
            fixture.engine.state.lock().unwrap().values["macro.preparing"],
            false
        );
        assert!(
            !fixture
                .engine
                .state
                .lock()
                .unwrap()
                .collections
                .contains_key("macro.recording")
        );
        assert!(fixture.log.lock().unwrap().is_empty());
    }
}
