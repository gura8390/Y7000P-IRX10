use super::*;
#[derive(Default)]
struct FakeClock {
    waits: Vec<u64>,
    stop_on_wait: bool,
}
impl Clock for FakeClock {
    fn wait(&mut self, ticks: u64, control: &PlaybackControl) -> Result<()> {
        self.waits.push(ticks);
        if self.stop_on_wait {
            control.stop();
        }
        Ok(())
    }
}
#[derive(Default)]
struct FakeInput {
    sent: Vec<InputEvent>,
    down: BTreeSet<InputButton>,
    calls: usize,
    fail_at: Option<usize>,
    panic_at: Option<usize>,
    release_fails: bool,
    stop_at: Option<(usize, PlaybackControl)>,
}
impl InputTransport for FakeInput {
    fn is_down(&mut self, button: InputButton) -> Result<bool> {
        Ok(self.down.contains(&button))
    }
    fn send(&mut self, event: InputEvent) -> Result<()> {
        self.calls += 1;
        self.sent.push(event);
        if let InputEvent::Button { button, up } = event {
            if up {
                if self.release_fails {
                    bail!("injected release failed")
                }
                self.down.remove(&button);
            } else {
                self.down.insert(button);
            }
        }
        if let Some((index, control)) = &self.stop_at {
            if self.calls == *index {
                control.stop();
            }
        }
        if self.panic_at == Some(self.calls) {
            panic!("fake after physical input")
        }
        if self.fail_at == Some(self.calls) {
            bail!("fake after physical input")
        }
        Ok(())
    }
}
fn key(direction: Direction, key: u32, ticks: i64) -> MacroEvent {
    MacroEvent::new(Source::Keyboard, direction, key, Point::default(), ticks)
}
fn captured(source: Source, direction: Direction, key: u32, time: u32) -> CapturedEvent {
    CapturedEvent {
        source,
        direction,
        key,
        point: Point::default(),
        timestamp_ms: time,
        injected: false,
        extra_info: 0,
    }
}

#[test]
fn legacy_dictionary_u64_timespan_point_and_raw_extensions_roundtrip() {
    let text = r#"{"IsEnabled":true,"Future":1.2345678901234567890123456789,"future":1e9999,"$type":"Never.Instantiate,Unknown","Sequences":{"Keyboard,18446744073709551615":{"RepeatCount":2,"IgnoreDelays":false,"InterruptOnOtherKey":true,"FutureNull":null,"Events":[{"Source":1,"Direction":1,"Key":65,"Point":"-100, 250","Delay":"1.02:03:04.0000001","Nested":{"Future":1,"future":2}},{"Source":"Keyboard","Direction":"Up","Key":65,"Delay":"00:00:00.0000002"}]}}}"#;
    let mut doc = MacroDocument::parse(text).unwrap();
    assert!(doc.enabled);
    assert_eq!(doc.sequences[0].identifier.key, u64::MAX);
    assert_eq!(
        doc.sequences[0].sequence.events[0].point,
        Point { x: -100, y: 250 }
    );
    assert_eq!(doc.sequences[0].sequence.events[1].delay_ticks, 2);
    doc.sequences[0].sequence.repeat_count = 3;
    for _ in 0..3 {
        let json = doc.serialize().unwrap();
        assert!(json.contains("1.2345678901234567890123456789"));
        assert!(json.contains("1e9999"));
        assert!(json.contains("Never.Instantiate"));
        doc = MacroDocument::parse(&json).unwrap();
    }
    let sequence = &doc.sequences[0].sequence;
    assert_eq!(sequence.repeat_count, 3);
    assert_eq!(sequence.raw.get("FutureNull"), Some(&JsonNode::Null));
    assert_eq!(
        sequence.events[0]
            .raw
            .get_path(&["Nested", "Future"])
            .unwrap()
            .as_i64(),
        Some(1)
    );
    assert_eq!(
        sequence.events[0]
            .raw
            .get_path(&["Nested", "future"])
            .unwrap()
            .as_i64(),
        Some(2)
    );
}
#[test]
fn exact_timespan_extremes_and_future_enum_are_preserved_but_not_played() {
    for ticks in [i64::MIN, -1, 0, 1, i64::MAX] {
        assert_eq!(parse_timespan_ticks(&timespan(ticks)).unwrap(), ticks);
    }
    let value = JsonNode::parse(
        r#"{"RepeatCount":1,"Events":[{"Source":99,"Direction":98,"Delay":"00:00:00"}]}"#,
    )
    .unwrap();
    let sequence = MacroSequence::from_json(&value).unwrap();
    assert!(sequence.validate_playback().is_err());
    assert_eq!(
        sequence
            .to_json()
            .unwrap()
            .get("Events")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .get("Source")
            .unwrap()
            .as_i64(),
        Some(99)
    );
}
#[test]
fn null_events_defaults_and_opaque_future_identifiers_do_not_instantiate_metadata() {
    let doc=MacroDocument::parse(r#"{"Sequences":{"Keyboard,96":{"Events":null},"FutureSource,1":{"$type":"Process.Start","Raw":true}}}"#).unwrap();
    assert!(!doc.enabled);
    assert_eq!(doc.sequences.len(), 1);
    assert_eq!(doc.sequences[0].sequence.repeat_count, 0);
    let json = JsonNode::parse(&doc.serialize().unwrap()).unwrap();
    assert_eq!(
        json.get_path(&["Sequences", "Keyboard,96", "Events"]),
        Some(&JsonNode::Null)
    );
    assert!(
        json.get_path(&["Sequences", "FutureSource,1", "Raw"])
            .unwrap()
            .as_bool()
            .unwrap()
    );
}
#[test]
fn edits_delete_known_array_and_dictionary_without_resurrection() {
    let mut doc=MacroDocument::parse(r#"{"Sequences":{"Keyboard,96":{"RepeatCount":1,"Events":[{"Source":1,"Direction":1,"Key":65,"Delay":"00:00:00"},{"Source":1,"Direction":2,"Key":65,"Delay":"00:00:00"}]},"Keyboard,97":{"Events":[]}}}"#).unwrap();
    doc.sequences[0].sequence.events.clear();
    doc.remove(&MacroIdentifier::parse("Keyboard,97").unwrap());
    let reloaded = MacroDocument::parse(&doc.serialize().unwrap()).unwrap();
    assert_eq!(reloaded.sequences.len(), 1);
    assert!(reloaded.sequences[0].sequence.events.is_empty());
}
#[test]
fn malformed_known_fields_and_alias_collision_are_rejected() {
    for text in [
        r#"{"IsEnabled":1}"#,
        r#"{"Sequences":[]}"#,
        r#"{"Sequences":{"Keyboard,1":{"RepeatCount":2147483648}}}"#,
        r#"{"Sequences":{"Keyboard,1":{"Events":[{"Key":4294967296}]}}}"#,
        r#"{"Sequences":{"Keyboard,1":{"Events":[{"Delay":"00:00:00.12345678"}]}}}"#,
        r#"{"Sequences":{"Keyboard,96":{},"1,96":{}}}"#,
    ] {
        assert!(MacroDocument::parse(text).is_err(), "{text}");
    }
}
#[test]
fn numeric_source_identifier_and_named_source_are_the_same_typed_key() {
    let mut doc = MacroDocument::parse(
        r#"{"Sequences":{"1,18446744073709551615":{"RepeatCount":1,"Events":[]}}}"#,
    )
    .unwrap();
    let named = MacroIdentifier::parse("Keyboard,18446744073709551615").unwrap();
    assert!(doc.get(&named).is_some());
    doc.upsert(named.clone(), MacroSequence::new(vec![]));
    assert_eq!(doc.sequences.len(), 1);
    assert!(
        doc.serialize()
            .unwrap()
            .contains("Keyboard,18446744073709551615")
    );
    doc.remove(&named);
    assert!(
        MacroDocument::parse(&doc.serialize().unwrap())
            .unwrap()
            .sequences
            .is_empty()
    );
}
#[test]
fn old_cleanup_removes_only_downs_without_later_matching_source_up() {
    let mut sequence = MacroSequence::new(vec![
        key(Direction::Down, 65, 1),
        key(Direction::Down, 66, 2),
        key(Direction::Up, 65, 3),
        MacroEvent::new(Source::Mouse, Direction::Down, 1, Point::default(), 4),
    ]);
    sequence.clean_legacy();
    assert_eq!(sequence.events.len(), 2);
    assert_eq!(sequence.events[0].key, 65);
    assert_eq!(sequence.events[1].direction, Direction::Up);
}
#[test]
fn repeats_and_exact_delays_match_legacy_order() {
    let mut sequence =
        MacroSequence::new(vec![key(Direction::Down, 65, 7), key(Direction::Up, 65, 9)]);
    sequence.repeat_count = 3;
    let (mut clock, mut input) = (FakeClock::default(), FakeInput::default());
    let report = play(&sequence, &mut clock, &mut input, &PlaybackControl::new()).unwrap();
    assert_eq!(report.status, PlaybackStatus::Completed);
    assert_eq!(report.events_sent, 6);
    assert_eq!(report.repeats_completed, 3);
    assert_eq!(clock.waits, vec![7, 9, 7, 9, 7, 9]);
    assert!(input.down.is_empty());
    assert_eq!(report.release_attempts, 0);
    sequence.ignore_delays = true;
    clock.waits.clear();
    assert!(play(&sequence, &mut clock, &mut input, &PlaybackControl::new()).is_ok());
    assert!(clock.waits.is_empty());
}
#[test]
fn stop_during_delay_sends_no_next_key_and_releases_prior_key() {
    let sequence = MacroSequence::new(vec![
        key(Direction::Down, 0x11, 0),
        key(Direction::Down, 65, 10),
        key(Direction::Up, 65, 0),
    ]);
    let mut clock = FakeClock {
        stop_on_wait: true,
        ..Default::default()
    };
    let mut input = FakeInput::default();
    let report = play(&sequence, &mut clock, &mut input, &PlaybackControl::new()).unwrap();
    assert_eq!(report.status, PlaybackStatus::Stopped);
    assert_eq!(report.events_sent, 1);
    assert_eq!(report.release_attempts, 1);
    assert!(input.down.is_empty());
}
#[test]
fn normal_completion_balances_unmatched_downs_in_reverse_order() {
    let sequence = MacroSequence::new(vec![
        key(Direction::Down, 0x11, 0),
        key(Direction::Down, 65, 0),
    ]);
    let mut input = FakeInput::default();
    let report = play(
        &sequence,
        &mut FakeClock::default(),
        &mut input,
        &PlaybackControl::new(),
    )
    .unwrap();
    assert_eq!(report.status, PlaybackStatus::Completed);
    assert_eq!(report.release_attempts, 2);
    assert_eq!(
        &input.sent[2..],
        &[
            InputEvent::Button {
                button: InputButton::Keyboard(65),
                up: true
            },
            InputEvent::Button {
                button: InputButton::Keyboard(0x11),
                up: true
            }
        ]
    );
    assert!(input.down.is_empty());
}
#[test]
fn error_and_panic_after_physical_down_still_release_attempted_keys() {
    let sequence = MacroSequence::new(vec![
        key(Direction::Down, 65, 0),
        key(Direction::Down, 66, 0),
    ]);
    for panic in [false, true] {
        let mut input = FakeInput {
            fail_at: (!panic).then_some(2),
            panic_at: panic.then_some(2),
            ..Default::default()
        };
        let report = play(
            &sequence,
            &mut FakeClock::default(),
            &mut input,
            &PlaybackControl::new(),
        )
        .unwrap();
        assert_eq!(report.status, PlaybackStatus::Failed);
        assert_eq!(report.release_attempts, 2);
        assert!(input.down.is_empty());
        assert!(report.unreleased.is_empty());
    }
}
#[test]
fn failed_release_is_reported_without_false_cleanup_success() {
    let sequence = MacroSequence::new(vec![key(Direction::Down, 65, 0)]);
    let mut input = FakeInput {
        release_fails: true,
        ..Default::default()
    };
    let report = play(
        &sequence,
        &mut FakeClock::default(),
        &mut input,
        &PlaybackControl::new(),
    )
    .unwrap();
    assert_eq!(report.status, PlaybackStatus::Failed);
    assert_eq!(report.unreleased, vec![InputButton::Keyboard(65)]);
    assert_eq!(report.release_attempts, 1);
}
#[test]
fn preexisting_physical_keys_are_neither_pressed_nor_released() {
    for direction in [Direction::Down, Direction::Up] {
        let sequence = MacroSequence::new(vec![key(direction, 65, 0)]);
        let mut input = FakeInput::default();
        input.down.insert(InputButton::Keyboard(65));
        let report = play(
            &sequence,
            &mut FakeClock::default(),
            &mut input,
            &PlaybackControl::new(),
        )
        .unwrap();
        assert_eq!(report.status, PlaybackStatus::Failed);
        assert!(input.sent.is_empty());
        assert!(input.down.contains(&InputButton::Keyboard(65)));
    }
}
#[test]
fn whole_sequence_validation_precedes_any_injected_action() {
    let mut sequence = MacroSequence::new(vec![
        key(Direction::Down, 65, 0),
        key(Direction::Down, 999, 0),
    ]);
    let mut input = FakeInput::default();
    assert!(
        play(
            &sequence,
            &mut FakeClock::default(),
            &mut input,
            &PlaybackControl::new()
        )
        .is_err()
    );
    assert!(input.sent.is_empty());
    sequence.events.pop();
    sequence.events[0].delay_ticks = -1;
    assert!(sequence.validate_playback().is_err());
    sequence.events[0].delay_ticks = 0;
    sequence.repeat_count = 11;
    assert!(sequence.validate_playback().is_err());
    sequence.repeat_count = 0;
    assert_eq!(
        play(
            &sequence,
            &mut FakeClock::default(),
            &mut input,
            &PlaybackControl::new()
        )
        .unwrap()
        .events_sent,
        0
    );
}
#[test]
fn mouse_buttons_move_and_signed_both_wheels_have_complete_mapping() {
    for (key, button) in [
        (1, MouseButton::Left),
        (2, MouseButton::Right),
        (3, MouseButton::Middle),
        (0x10000, MouseButton::X1),
        (0x20000, MouseButton::X2),
    ] {
        assert_eq!(
            MacroEvent::new(Source::Mouse, Direction::Down, key, Point::default(), 0)
                .input()
                .unwrap(),
            InputEvent::Button {
                button: InputButton::Mouse(button),
                up: false
            }
        );
    }
    for direction in [Direction::Wheel, Direction::HorizontalWheel] {
        assert_eq!(
            MacroEvent::new(
                Source::Mouse,
                direction,
                (-120i32) as u32,
                Point::default(),
                0
            )
            .input()
            .unwrap(),
            InputEvent::Wheel {
                delta: -120,
                horizontal: direction == Direction::HorizontalWheel
            }
        );
    }
    assert_eq!(
        MacroEvent::new(
            Source::Mouse,
            Direction::Move,
            0,
            Point { x: -500, y: 700 },
            0
        )
        .input()
        .unwrap(),
        InputEvent::Move(Point { x: -500, y: 700 })
    );
}
#[test]
fn interrupt_guard_matches_physical_flags_and_ignores_own_injection() {
    let control = PlaybackControl::new();
    control.interrupt_keyboard(0, 0);
    assert!(!control.is_stopped());
    control.0.interruptible.store(true, Ordering::Release);
    for (flags, extra) in [(0x10, 0), (1, 0), (0x80, 0), (0, INPUT_MARKER)] {
        control.interrupt_keyboard(flags, extra);
        assert!(!control.is_stopped());
    }
    control.interrupt_keyboard(0, 0);
    assert!(control.is_stopped());
}
#[test]
fn recorder_first_delay_rollover_and_autorepeat_are_exact() {
    let mut state = RecorderState::new(RecorderSettings::default()).unwrap();
    assert_eq!(
        state
            .feed(captured(
                Source::Keyboard,
                Direction::Down,
                65,
                u32::MAX - 4
            ))
            .unwrap(),
        RecordDisposition::Recorded
    );
    assert_eq!(
        state
            .feed(captured(
                Source::Keyboard,
                Direction::Down,
                65,
                u32::MAX - 1
            ))
            .unwrap(),
        RecordDisposition::Ignored
    );
    state
        .feed(captured(Source::Keyboard, Direction::Up, 65, 5))
        .unwrap();
    assert_eq!(state.events().len(), 2);
    assert_eq!(state.events()[0].delay_ticks, 0);
    assert_eq!(state.events()[1].delay_ticks, 100_000);
}
#[test]
fn recorder_filters_injected_and_disabled_sources_without_advancing_delay() {
    let mut state = RecorderState::new(RecorderSettings::default()).unwrap();
    state
        .feed(captured(Source::Keyboard, Direction::Down, 65, 0))
        .unwrap();
    let mut own = captured(Source::Keyboard, Direction::Down, 66, 10);
    own.extra_info = INPUT_MARKER;
    assert_eq!(state.feed(own).unwrap(), RecordDisposition::Ignored);
    own.extra_info = 0;
    own.injected = true;
    assert_eq!(state.feed(own).unwrap(), RecordDisposition::Ignored);
    assert_eq!(
        state
            .feed(captured(Source::Mouse, Direction::Move, 0, 15))
            .unwrap(),
        RecordDisposition::Ignored
    );
    state
        .feed(captured(Source::Keyboard, Direction::Up, 65, 20))
        .unwrap();
    assert_eq!(state.events()[1].delay_ticks, 200_000);
}
#[test]
fn escape_saves_balanced_events_and_session_abort_discards_them() {
    let mut state = RecorderState::new(RecorderSettings::default()).unwrap();
    state
        .feed(captured(Source::Keyboard, Direction::Down, 65, 10))
        .unwrap();
    state
        .feed(captured(Source::Keyboard, Direction::Up, 65, 20))
        .unwrap();
    assert_eq!(
        state
            .feed(captured(Source::Keyboard, Direction::Down, 27, 30))
            .unwrap(),
        RecordDisposition::Stopped
    );
    assert!(!state.report().unwrap().interrupted);
    assert_eq!(state.sequence().events.len(), 2);
    state.stop(true, "Session locked");
    assert!(state.report().unwrap().interrupted);
    assert!(state.sequence().events.is_empty());
}
#[test]
fn recorder_flags_and_mouse_movement_are_explicit() {
    for bits in [0, 4, 5, 8, u32::MAX] {
        assert!(RecorderSettings::from_bits(bits).is_err());
    }
    for bits in [1, 2, 3, 6, 7] {
        assert!(RecorderSettings::from_bits(bits).is_ok());
    }
    let mut state = RecorderState::new(RecorderSettings::from_bits(3).unwrap()).unwrap();
    assert_eq!(
        state
            .feed(captured(Source::Mouse, Direction::Move, 0, 10))
            .unwrap(),
        RecordDisposition::Ignored
    );
    let mut state = RecorderState::new(RecorderSettings::from_bits(7).unwrap()).unwrap();
    assert_eq!(
        state
            .feed(captured(Source::Mouse, Direction::Move, 0, 10))
            .unwrap(),
        RecordDisposition::Recorded
    );
}
