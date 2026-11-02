//! Legacy MacroLib documents and explicit, cancellable input playback.
//! Parsing/new constructors never hook, record, start threads, or send input.
use anyhow::{Context, Result, bail, ensure};
pub use jiks_core::settings::MacroIdentifier;
use jiks_core::settings::{JsonNode, SettingsDocument, parse_timespan_ticks};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub mod native;
pub use native::{NativePlayer, NativeRecorder};
#[cfg(test)]
mod tests;
pub const INPUT_MARKER: usize = 1337;
pub const MAX_EVENTS: usize = 100_000;
pub const MAX_PLAYBACK_EVENTS: usize = 1_000_000;
pub const ALLOWED_REPEAT_COUNTS: [i32; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
pub const TRIGGER_KEYS: [u32; 10] = [0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    Unknown,
    Keyboard,
    Mouse,
    Future(i32),
}
impl Source {
    fn parse(value: Option<&JsonNode>) -> Result<Self> {
        Ok(
            match enumeration(value, &["Unknown", "Keyboard", "Mouse"])? {
                0 => Self::Unknown,
                1 => Self::Keyboard,
                2 => Self::Mouse,
                n => Self::Future(n),
            },
        )
    }
    fn number(self) -> i32 {
        match self {
            Self::Unknown => 0,
            Self::Keyboard => 1,
            Self::Mouse => 2,
            Self::Future(n) => n,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Unknown,
    Down,
    Up,
    Wheel,
    HorizontalWheel,
    Move,
    Future(i32),
}
impl Direction {
    fn parse(value: Option<&JsonNode>) -> Result<Self> {
        Ok(
            match enumeration(
                value,
                &["Unknown", "Down", "Up", "Wheel", "HorizontalWheel", "Move"],
            )? {
                0 => Self::Unknown,
                1 => Self::Down,
                2 => Self::Up,
                3 => Self::Wheel,
                4 => Self::HorizontalWheel,
                5 => Self::Move,
                n => Self::Future(n),
            },
        )
    }
    fn number(self) -> i32 {
        match self {
            Self::Unknown => 0,
            Self::Down => 1,
            Self::Up => 2,
            Self::Wheel => 3,
            Self::HorizontalWheel => 4,
            Self::Move => 5,
            Self::Future(n) => n,
        }
    }
}
fn enumeration(value: Option<&JsonNode>, names: &[&str]) -> Result<i32> {
    let Some(value) = value else { return Ok(0) };
    if let Some(text) = value.as_str() {
        if let Some(index) = names.iter().position(|n| n.eq_ignore_ascii_case(text)) {
            return Ok(index as i32);
        }
        return text.parse().context("Unknown macro enum name");
    }
    i32::try_from(value.as_i64().context("Macro enum must be Int32/name")?)
        .context("Macro enum Int32 overflow")
}
fn integer(value: Option<&JsonNode>, default: i64) -> Result<i64> {
    value.map_or(Ok(default), |v| {
        v.as_i64().context("Macro field must be integer")
    })
}
fn boolean(value: Option<&JsonNode>, default: bool) -> Result<bool> {
    value.map_or(Ok(default), |v| {
        v.as_bool().context("Macro field must be boolean")
    })
}
fn num(value: impl ToString) -> JsonNode {
    JsonNode::Number(value.to_string())
}
fn object() -> JsonNode {
    JsonNode::Object(vec![])
}
fn set(value: &mut JsonNode, name: &str, entry: JsonNode) -> Result<()> {
    value.set_known(name, entry)?;
    Ok(())
}
fn timespan(ticks: i64) -> String {
    let negative = ticks < 0;
    let value = i128::from(ticks).abs();
    let seconds = value / 10_000_000;
    let fraction = value % 10_000_000;
    let days = seconds / 86400;
    let hour = seconds % 86400 / 3600;
    let minute = seconds % 3600 / 60;
    let second = seconds % 60;
    format!(
        "{}{}{:02}:{:02}:{:02}{}",
        if negative { "-" } else { "" },
        if days > 0 {
            format!("{days}.")
        } else {
            String::new()
        },
        hour,
        minute,
        second,
        if fraction > 0 {
            format!(".{fraction:07}")
        } else {
            String::new()
        }
    )
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}
#[derive(Debug, Clone, PartialEq)]
pub struct MacroEvent {
    pub source: Source,
    pub direction: Direction,
    pub key: u32,
    pub point: Point,
    /// Exact .NET TimeSpan ticks (100ns), including invalid negative values kept for editing.
    pub delay_ticks: i64,
    raw: JsonNode,
}
impl MacroEvent {
    pub fn new(
        source: Source,
        direction: Direction,
        key: u32,
        point: Point,
        delay_ticks: i64,
    ) -> Self {
        Self {
            source,
            direction,
            key,
            point,
            delay_ticks,
            raw: object(),
        }
    }
    pub fn from_json(value: &JsonNode) -> Result<Self> {
        ensure!(value.as_object().is_some(), "Macro event must be object");
        let point = match value.get_ci("Point") {
            None => Point::default(),
            Some(JsonNode::String(text)) => {
                let (x, y) = text.split_once(',').context("Legacy Point needs x, y")?;
                Point {
                    x: x.trim().parse().context("Point X")?,
                    y: y.trim().parse().context("Point Y")?,
                }
            }
            Some(point) => {
                ensure!(
                    point.as_object().is_some(),
                    "Point must be object or x, y string"
                );
                Point {
                    x: i32::try_from(integer(point.get_ci("X"), 0)?)?,
                    y: i32::try_from(integer(point.get_ci("Y"), 0)?)?,
                }
            }
        };
        let delay_ticks = value.get_ci("Delay").map_or(Ok(0), |n| {
            parse_timespan_ticks(n.as_str().context("Macro Delay must be TimeSpan string")?)
                .map_err(anyhow::Error::from)
        })?;
        Ok(Self {
            source: Source::parse(value.get_ci("Source"))?,
            direction: Direction::parse(value.get_ci("Direction"))?,
            key: u32::try_from(integer(value.get_ci("Key"), 0)?)?,
            point,
            delay_ticks,
            raw: value.clone(),
        })
    }
    pub fn to_json(&self) -> Result<JsonNode> {
        let mut value = self.raw.clone();
        set(&mut value, "Source", num(self.source.number()))?;
        set(&mut value, "Direction", num(self.direction.number()))?;
        set(&mut value, "Key", num(self.key))?;
        set(
            &mut value,
            "Delay",
            JsonNode::String(timespan(self.delay_ticks)),
        )?;
        // Json.NET System.Drawing.PointConverter emits "x, y". Keep future Point
        // object extensions when the input already used that representation.
        let point = if let Some(JsonNode::Object(_)) = value.get_ci("Point") {
            let mut point = value.get_ci("Point").unwrap().clone();
            set(&mut point, "X", num(self.point.x))?;
            set(&mut point, "Y", num(self.point.y))?;
            point
        } else {
            JsonNode::String(format!("{}, {}", self.point.x, self.point.y))
        };
        set(&mut value, "Point", point)?;
        Ok(value)
    }
    fn input(&self) -> Result<InputEvent> {
        ensure!(self.delay_ticks >= 0, "Negative macro delay cannot play");
        if self.raw.get_ci("JiksDelayOnly").and_then(JsonNode::as_bool) == Some(true) {
            ensure!(
                self.source == Source::Unknown
                    && self.direction == Direction::Unknown
                    && self.key == 0,
                "Delay-only macro marker cannot hide input"
            );
            return Ok(InputEvent::DelayOnly);
        }
        match (self.source, self.direction) {
            (Source::Keyboard, Direction::Down | Direction::Up) => {
                ensure!((1..=254).contains(&self.key), "Virtual key must be 1..254");
                Ok(InputEvent::Button {
                    button: InputButton::Keyboard(self.key as u16),
                    up: self.direction == Direction::Up,
                })
            }
            (Source::Mouse, Direction::Down | Direction::Up) => {
                let button = match self.key {
                    1 => MouseButton::Left,
                    2 => MouseButton::Right,
                    3 => MouseButton::Middle,
                    0x10000 => MouseButton::X1,
                    0x20000 => MouseButton::X2,
                    _ => bail!("Unknown mouse button code"),
                };
                Ok(InputEvent::Button {
                    button: InputButton::Mouse(button),
                    up: self.direction == Direction::Up,
                })
            }
            (Source::Mouse, Direction::Wheel | Direction::HorizontalWheel) => {
                Ok(InputEvent::Wheel {
                    delta: self.key as i32,
                    horizontal: self.direction == Direction::HorizontalWheel,
                })
            }
            (Source::Mouse, Direction::Move) => Ok(InputEvent::Move(self.point)),
            _ => bail!("Unsupported macro source/direction; preserved document is not executable"),
        }
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct MacroSequence {
    pub repeat_count: i32,
    pub ignore_delays: bool,
    pub interrupt_on_other_key: bool,
    pub events: Vec<MacroEvent>,
    raw: JsonNode,
    events_null: bool,
}
impl Default for MacroSequence {
    fn default() -> Self {
        Self {
            repeat_count: 0,
            ignore_delays: false,
            interrupt_on_other_key: false,
            events: vec![],
            raw: object(),
            events_null: false,
        }
    }
}
impl MacroSequence {
    pub fn new(events: Vec<MacroEvent>) -> Self {
        Self {
            repeat_count: 1,
            events,
            ..Self::default()
        }
    }
    pub fn from_json(value: &JsonNode) -> Result<Self> {
        ensure!(value.as_object().is_some(), "Macro sequence must be object");
        let events = match value.get_ci("Events") {
            None | Some(JsonNode::Null) => vec![],
            Some(value) => value
                .as_array()
                .context("Macro Events must be array/null")?
                .iter()
                .map(MacroEvent::from_json)
                .collect::<Result<Vec<_>>>()?,
        };
        ensure!(events.len() <= MAX_EVENTS, "Macro exceeds event capacity");
        Ok(Self {
            repeat_count: i32::try_from(integer(value.get_ci("RepeatCount"), 0)?)?,
            ignore_delays: boolean(value.get_ci("IgnoreDelays"), false)?,
            interrupt_on_other_key: boolean(value.get_ci("InterruptOnOtherKey"), false)?,
            events,
            raw: value.clone(),
            events_null: matches!(value.get_ci("Events"), Some(JsonNode::Null)),
        })
    }
    pub fn to_json(&self) -> Result<JsonNode> {
        let mut value = self.raw.clone();
        set(&mut value, "RepeatCount", num(self.repeat_count))?;
        set(
            &mut value,
            "IgnoreDelays",
            JsonNode::Bool(self.ignore_delays),
        )?;
        set(
            &mut value,
            "InterruptOnOtherKey",
            JsonNode::Bool(self.interrupt_on_other_key),
        )?;
        let events = if self.events_null && self.events.is_empty() {
            JsonNode::Null
        } else {
            JsonNode::Array(
                self.events
                    .iter()
                    .map(MacroEvent::to_json)
                    .collect::<Result<_>>()?,
            )
        };
        set(&mut value, "Events", events)?;
        Ok(value)
    }
    pub fn validate_playback(&self) -> Result<()> {
        ensure!(
            (0..=10).contains(&self.repeat_count),
            "Macro repeat count must be 0..10 (legacy UI 1..10)"
        );
        ensure!(
            self.events.len() <= MAX_EVENTS
                && self.events.len().saturating_mul(self.repeat_count as usize)
                    <= MAX_PLAYBACK_EVENTS,
            "Macro playback exceeds event budget"
        );
        for event in &self.events {
            event.input()?;
        }
        Ok(())
    }
    /// Preserve the old explicit-save cleanup: remove only downs with no later
    /// matching up; parsing never silently edits a user's original sequence.
    pub fn clean_legacy(&mut self) {
        let mut ups = BTreeSet::new();
        let mut retain = vec![true; self.events.len()];
        for (i, event) in self.events.iter().enumerate().rev() {
            let key = (event.source.number(), event.key);
            if event.direction == Direction::Up {
                ups.insert(key);
            } else if event.direction == Direction::Down && !ups.contains(&key) {
                retain[i] = false;
            }
        }
        let mut index = 0;
        self.events.retain(|_| {
            let keep = retain[index];
            index += 1;
            keep
        });
    }
}
#[derive(Debug, Clone)]
pub struct MacroEntry {
    pub identifier: MacroIdentifier,
    pub sequence: MacroSequence,
}
fn normalized_identifier(mut identifier: MacroIdentifier) -> MacroIdentifier {
    if let Ok(source) = Source::parse(Some(&JsonNode::String(identifier.source.clone()))) {
        identifier.source = match source {
            Source::Unknown => "Unknown".into(),
            Source::Keyboard => "Keyboard".into(),
            Source::Mouse => "Mouse".into(),
            Source::Future(n) => n.to_string(),
        };
    }
    identifier
}
#[derive(Debug, Clone)]
pub struct MacroDocument {
    raw: SettingsDocument,
    pub enabled: bool,
    pub sequences: Vec<MacroEntry>,
}
impl MacroDocument {
    pub fn parse(json: &str) -> Result<Self> {
        Self::from_settings(SettingsDocument::parse(json)?)
    }
    pub fn from_settings(raw: SettingsDocument) -> Result<Self> {
        ensure!(
            raw.root().as_object().is_some(),
            "Macro document must be object"
        );
        let enabled = boolean(raw.root().get_ci("IsEnabled"), false)?;
        let mut sequences = Vec::new();
        let mut seen = BTreeSet::new();
        if let Some(map) = raw.root().get_ci("Sequences") {
            if !matches!(map, JsonNode::Null) {
                for (key, value) in map
                    .as_object()
                    .context("Macro Sequences must be dictionary/null")?
                {
                    // Unknown future identifier spellings stay opaque in raw data.
                    let Ok(identifier) = MacroIdentifier::parse(key) else {
                        continue;
                    };
                    let source = Source::parse(Some(&JsonNode::String(identifier.source.clone())))?;
                    ensure!(
                        seen.insert((source.number(), identifier.key)),
                        "Duplicate macro identifier alias"
                    );
                    sequences.push(MacroEntry {
                        identifier: normalized_identifier(identifier),
                        sequence: MacroSequence::from_json(value)?,
                    });
                }
            }
        }
        Ok(Self {
            raw,
            enabled,
            sequences,
        })
    }
    pub fn get(&self, identifier: &MacroIdentifier) -> Option<&MacroSequence> {
        let identifier = normalized_identifier(identifier.clone());
        self.sequences
            .iter()
            .find(|entry| entry.identifier == identifier)
            .map(|entry| &entry.sequence)
    }
    pub fn upsert(&mut self, identifier: MacroIdentifier, sequence: MacroSequence) {
        let identifier = normalized_identifier(identifier);
        if let Some(entry) = self
            .sequences
            .iter_mut()
            .find(|e| e.identifier == identifier)
        {
            entry.sequence = sequence
        } else {
            self.sequences.push(MacroEntry {
                identifier,
                sequence,
            })
        }
    }
    pub fn remove(&mut self, identifier: &MacroIdentifier) {
        let canonical = normalized_identifier(identifier.clone());
        self.sequences.retain(|entry| entry.identifier != canonical);
        if let Some(JsonNode::Object(items)) = self.raw.root().get_ci("Sequences") {
            let items = items
                .iter()
                .filter(|(key, _)| {
                    MacroIdentifier::parse(key)
                        .map(normalized_identifier)
                        .as_ref()
                        .ok()
                        != Some(&canonical)
                })
                .cloned()
                .collect();
            let _ = self.raw.set_path(&["Sequences"], JsonNode::Object(items));
        }
    }
    pub fn serialize(&self) -> Result<String> {
        let mut root = self.raw.root().clone();
        set(&mut root, "IsEnabled", JsonNode::Bool(self.enabled))?;
        let original = self
            .raw
            .root()
            .get_ci("Sequences")
            .and_then(JsonNode::as_object)
            .unwrap_or(&[]);
        // Keep unknown dictionary entries and unknown fields inside each typed
        // sequence/event. Arrays remain current arrays and deleted entries stay gone.
        let mut map = JsonNode::Object(
            original
                .iter()
                .filter(|(key, _)| MacroIdentifier::parse(key).is_err())
                .cloned()
                .collect(),
        );
        for entry in &self.sequences {
            map.set(&entry.identifier.legacy_key(), entry.sequence.to_json()?)?;
        }
        set(&mut root, "Sequences", map)?;
        Ok(SettingsDocument::from_root(root)?.serialize()?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum InputButton {
    Keyboard(u16),
    Mouse(MouseButton),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    DelayOnly,
    Button { button: InputButton, up: bool },
    Move(Point),
    Wheel { delta: i32, horizontal: bool },
}
pub trait InputTransport {
    fn send(&mut self, event: InputEvent) -> Result<()>;
    /// Prevent releasing a key the user already physically held before the macro.
    fn is_down(&mut self, button: InputButton) -> Result<bool>;
}
pub trait Clock {
    fn wait(&mut self, delay_ticks: u64, control: &PlaybackControl) -> Result<()>;
}
#[derive(Default)]
struct ControlState {
    stopped: AtomicBool,
    interruptible: AtomicBool,
    lock: Mutex<()>,
    wake: Condvar,
}
#[derive(Clone, Default)]
pub struct PlaybackControl(Arc<ControlState>);
impl PlaybackControl {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn stop(&self) {
        self.0.stopped.store(true, Ordering::Release);
        let _guard = self.0.lock.lock().unwrap_or_else(|p| p.into_inner());
        self.0.wake.notify_all();
    }
    pub fn is_stopped(&self) -> bool {
        self.0.stopped.load(Ordering::Acquire)
    }
    /// Match the original guard: physical, non-extended key down only. Injected
    /// input or our own SendInput marker never recursively interrupts playback.
    pub fn interrupt_keyboard(&self, flags: u32, extra_info: usize) {
        if self.0.interruptible.load(Ordering::Acquire) && flags == 0 && extra_info != INPUT_MARKER
        {
            self.stop();
        }
    }
}
#[derive(Default)]
pub struct SystemClock;
impl Clock for SystemClock {
    fn wait(&mut self, delay_ticks: u64, control: &PlaybackControl) -> Result<()> {
        let duration = Duration::new(
            delay_ticks / 10_000_000,
            (delay_ticks % 10_000_000 * 100) as u32,
        );
        let guard = control
            .0
            .lock
            .lock()
            .map_err(|_| anyhow::anyhow!("Macro clock poisoned"))?;
        let _ = control
            .0
            .wake
            .wait_timeout_while(guard, duration, |_| !control.is_stopped())
            .map_err(|_| anyhow::anyhow!("Macro clock poisoned"))?;
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackStatus {
    Completed,
    Stopped,
    Failed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackReport {
    pub status: PlaybackStatus,
    pub events_sent: usize,
    pub repeats_completed: i32,
    pub release_attempts: usize,
    pub unreleased: Vec<InputButton>,
    pub errors: Vec<String>,
}
/// Blocking pure driver; callers execute it on an owned worker, never in a hook.
/// Completion includes cleanup. Errors/panics never skip release attempts.
pub fn play(
    sequence: &MacroSequence,
    clock: &mut impl Clock,
    input: &mut impl InputTransport,
    control: &PlaybackControl,
) -> Result<PlaybackReport> {
    sequence.validate_playback()?;
    control
        .0
        .interruptible
        .store(sequence.interrupt_on_other_key, Ordering::Release);
    let mut report = PlaybackReport {
        status: PlaybackStatus::Completed,
        events_sent: 0,
        repeats_completed: 0,
        release_attempts: 0,
        unreleased: vec![],
        errors: vec![],
    };
    let mut owned = Vec::<InputButton>::new();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        for _ in 0..sequence.repeat_count {
            for event in &sequence.events {
                if control.is_stopped() {
                    return Ok(());
                }
                if !sequence.ignore_delays && event.delay_ticks > 0 {
                    clock.wait(event.delay_ticks as u64, control)?;
                }
                if control.is_stopped() {
                    return Ok(());
                }
                let event = event.input()?;
                if event == InputEvent::DelayOnly {
                    continue;
                }
                if let InputEvent::Button { button, up: false } = event {
                    if !owned.contains(&button) {
                        ensure!(
                            !input.is_down(button)?,
                            "Input button already held before macro: {button:?}"
                        );
                        owned.push(button);
                    }
                }
                if let InputEvent::Button { button, up: true } = event {
                    if !owned.contains(&button) {
                        ensure!(
                            !input.is_down(button)?,
                            "Refusing to release a button held before macro: {button:?}"
                        );
                    }
                }
                // Track attempted down before SendInput: a failing transport can
                // have physically completed before reporting an error.
                input.send(event)?;
                report.events_sent += 1;
                if let InputEvent::Button { button, up: true } = event {
                    owned.retain(|b| *b != button);
                }
            }
            report.repeats_completed += 1;
        }
        Ok(())
    }));
    match outcome {
        Ok(Ok(())) => {
            if control.is_stopped() {
                report.status = PlaybackStatus::Stopped
            }
        }
        Ok(Err(error)) => {
            report.status = PlaybackStatus::Failed;
            report.errors.push(format!("{error:#}"));
        }
        Err(_) => {
            report.status = PlaybackStatus::Failed;
            report
                .errors
                .push("Input/clock panicked during playback".into());
        }
    }
    // Reverse order releases modifiers last. Cleanup deliberately ignores stop.
    for button in owned.into_iter().rev() {
        report.release_attempts += 1;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            input.send(InputEvent::Button { button, up: true })
        })) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                report.unreleased.push(button);
                report.errors.push(format!("Release {button:?}: {error:#}"));
            }
            Err(_) => {
                report.unreleased.push(button);
                report.errors.push(format!("Release {button:?} panicked"));
            }
        }
    }
    if !report.unreleased.is_empty() {
        report.status = PlaybackStatus::Failed;
    }
    control.0.interruptible.store(false, Ordering::Release);
    Ok(report)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecorderSettings {
    pub keyboard: bool,
    pub mouse: bool,
    pub movement: bool,
}
impl Default for RecorderSettings {
    fn default() -> Self {
        Self {
            keyboard: true,
            mouse: false,
            movement: false,
        }
    }
}
impl RecorderSettings {
    pub fn from_bits(bits: u32) -> Result<Self> {
        ensure!(bits > 0 && bits & !7 == 0, "Invalid macro recorder flags");
        ensure!(
            bits & 4 == 0 || bits & 2 != 0,
            "Movement recording requires mouse"
        );
        Ok(Self {
            keyboard: bits & 1 != 0,
            mouse: bits & 2 != 0,
            movement: bits & 4 != 0,
        })
    }
    pub fn validate(self) -> Result<()> {
        ensure!(
            self.keyboard || self.mouse,
            "Recorder must select keyboard/mouse"
        );
        ensure!(!self.movement || self.mouse, "Movement needs mouse");
        Ok(())
    }
}
#[derive(Debug, Clone, Copy)]
pub struct CapturedEvent {
    pub source: Source,
    pub direction: Direction,
    pub key: u32,
    pub point: Point,
    pub timestamp_ms: u32,
    pub injected: bool,
    pub extra_info: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordDisposition {
    Ignored,
    Recorded,
    Stopped,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingReport {
    pub interrupted: bool,
    pub reason: String,
    pub events_json: String,
}
/// Pure recorder state; callbacks feed owned event data without retaining pointers.
pub struct RecorderState {
    settings: RecorderSettings,
    events: Vec<MacroEvent>,
    last_tick: Option<u32>,
    pressed: BTreeSet<(i32, u32)>,
    stopped: bool,
    interrupted: bool,
    reason: String,
}
impl RecorderState {
    pub fn new(settings: RecorderSettings) -> Result<Self> {
        settings.validate()?;
        Ok(Self {
            settings,
            events: vec![],
            last_tick: None,
            pressed: BTreeSet::new(),
            stopped: false,
            interrupted: false,
            reason: String::new(),
        })
    }
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }
    pub fn events(&self) -> &[MacroEvent] {
        &self.events
    }
    pub fn stop(&mut self, interrupted: bool, reason: &str) {
        self.stopped = true;
        self.interrupted |= interrupted;
        self.reason = reason.into();
        if self.interrupted {
            self.events.clear();
        }
        self.pressed.clear();
    }
    pub fn feed(&mut self, event: CapturedEvent) -> Result<RecordDisposition> {
        if self.stopped || event.injected || event.extra_info == INPUT_MARKER {
            return Ok(RecordDisposition::Ignored);
        }
        if event.source == Source::Keyboard
            && event.direction == Direction::Down
            && event.key == 0x1b
        {
            self.stop(false, "Escape");
            return Ok(RecordDisposition::Stopped);
        }
        let selected = match event.source {
            Source::Keyboard => self.settings.keyboard,
            Source::Mouse => {
                self.settings.mouse
                    && (event.direction != Direction::Move || self.settings.movement)
            }
            _ => false,
        };
        if !selected {
            return Ok(RecordDisposition::Ignored);
        }
        let key = (event.source.number(), event.key);
        if event.direction == Direction::Down && self.pressed.contains(&key) {
            return Ok(RecordDisposition::Ignored);
        }
        let delay = u64::from(
            self.last_tick
                .map_or(0, |last| event.timestamp_ms.wrapping_sub(last)),
        ) * 10_000;
        let converted = MacroEvent::new(
            event.source,
            event.direction,
            event.key,
            event.point,
            delay as i64,
        );
        converted.input()?;
        if self.events.len() >= MAX_EVENTS {
            self.stop(true, "Recorder event budget exceeded");
            return Ok(RecordDisposition::Stopped);
        }
        self.events.push(converted);
        self.last_tick = Some(event.timestamp_ms);
        if event.direction == Direction::Down {
            self.pressed.insert(key);
        } else if event.direction == Direction::Up {
            self.pressed.remove(&key);
        }
        Ok(RecordDisposition::Recorded)
    }
    pub fn sequence(&self) -> MacroSequence {
        let mut result = MacroSequence::new(self.events.clone());
        result.clean_legacy();
        result
    }
    pub fn report(&self) -> Result<RecordingReport> {
        Ok(RecordingReport {
            interrupted: self.interrupted,
            reason: self.reason.clone(),
            events_json: self.sequence().to_json()?.to_json(),
        })
    }
}
