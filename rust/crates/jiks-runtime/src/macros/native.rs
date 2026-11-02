//! Explicit-only native input adapters. No Win32 API is called by new()/Default.
use super::*;
use std::{
    cell::RefCell,
    mem::size_of,
    ptr,
    sync::{
        OnceLock,
        atomic::AtomicU32,
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
};
use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, LPARAM, LRESULT, WPARAM},
    System::{
        LibraryLoader::GetModuleHandleW,
        RemoteDesktop::{WTSRegisterSessionNotification, WTSUnRegisterSessionNotification},
    },
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

static NATIVE_INPUT_BUSY: AtomicBool = AtomicBool::new(false);
static UNRELEASED: OnceLock<Mutex<Vec<InputButton>>> = OnceLock::new();
fn unreleased() -> &'static Mutex<Vec<InputButton>> {
    UNRELEASED.get_or_init(|| Mutex::new(vec![]))
}
/// Read only: no input, hook, thread, filesystem or device calls.
pub fn has_unreleased_input() -> bool {
    unreleased()
        .lock()
        .map(|ledger| !ledger.is_empty())
        .unwrap_or(true)
}
struct NativeLease;
impl NativeLease {
    fn acquire() -> Result<Self> {
        ensure!(
            NATIVE_INPUT_BUSY
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "A native macro playback/recording session is still active"
        );
        let lease = Self;
        ensure!(
            unreleased()
                .lock()
                .map_err(|_| anyhow::anyhow!("Release ledger poisoned"))?
                .is_empty(),
            "Earlier injected input has unverified releases; retry cleanup first"
        );
        Ok(lease)
    }
}
impl Drop for NativeLease {
    fn drop(&mut self) {
        NATIVE_INPUT_BUSY.store(false, Ordering::Release);
    }
}
#[derive(Debug, Clone, Copy)]
pub struct DesktopRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}
/// Pixel endpoints map to 0..65535 across the entire virtual desktop, including
/// negative monitor coordinates. No taskbar/working-area distortion.
pub fn normalize_position(point: Point, desktop: DesktopRect) -> Result<(i32, i32)> {
    ensure!(
        desktop.width > 1 && desktop.height > 1,
        "Virtual desktop dimensions unavailable"
    );
    let axis = |position: i32, origin: i32, size: i32| -> i32 {
        let relative = (i64::from(position) - i64::from(origin)).clamp(0, i64::from(size) - 1);
        ((relative * 65535 + (i64::from(size) - 1) / 2) / (i64::from(size) - 1)) as i32
    };
    Ok((
        axis(point.x, desktop.left, desktop.width),
        axis(point.y, desktop.top, desktop.height),
    ))
}
#[derive(Default)]
pub struct NativeInputTransport;
fn virtual_key(button: InputButton) -> u16 {
    match button {
        InputButton::Keyboard(key) => key,
        InputButton::Mouse(button) => match button {
            MouseButton::Left => 1,
            MouseButton::Right => 2,
            MouseButton::Middle => 4,
            MouseButton::X1 => 5,
            MouseButton::X2 => 6,
        },
    }
}
fn extended(key: u16) -> bool {
    matches!(key,0x21..=0x28|0x2c..=0x2e|0x5b..=0x5d|0x6f|0x90|0xa3|0xa5)
}
impl InputTransport for NativeInputTransport {
    fn is_down(&mut self, button: InputButton) -> Result<bool> {
        Ok(unsafe { GetAsyncKeyState(i32::from(virtual_key(button))) } < 0)
    }
    fn send(&mut self, event: InputEvent) -> Result<()> {
        let input = match event {
            InputEvent::DelayOnly => return Ok(()),
            InputEvent::Button {
                button: InputButton::Keyboard(key),
                up,
            } => {
                ensure!((1..=254).contains(&key), "Invalid native virtual key");
                INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: key,
                            wScan: 0,
                            dwFlags: (if up { KEYEVENTF_KEYUP } else { 0 })
                                | (if extended(key) {
                                    KEYEVENTF_EXTENDEDKEY
                                } else {
                                    0
                                }),
                            time: 0,
                            dwExtraInfo: INPUT_MARKER,
                        },
                    },
                }
            }
            InputEvent::Button {
                button: InputButton::Mouse(button),
                up,
            } => {
                let (flag, data) = match (button, up) {
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::X1, false) => (MOUSEEVENTF_XDOWN, 1),
                    (MouseButton::X1, true) => (MOUSEEVENTF_XUP, 1),
                    (MouseButton::X2, false) => (MOUSEEVENTF_XDOWN, 2),
                    (MouseButton::X2, true) => (MOUSEEVENTF_XUP, 2),
                };
                mouse_input(0, 0, data, flag)
            }
            InputEvent::Wheel { delta, horizontal } => mouse_input(
                0,
                0,
                delta as u32,
                if horizontal {
                    MOUSEEVENTF_HWHEEL
                } else {
                    MOUSEEVENTF_WHEEL
                },
            ),
            InputEvent::Move(point) => {
                let desktop = DesktopRect {
                    left: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
                    top: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
                    width: unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) },
                    height: unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) },
                };
                let (x, y) = normalize_position(point, desktop)?;
                mouse_input(
                    x,
                    y,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                )
            }
        };
        let sent = unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) };
        ensure!(
            sent == 1,
            "SendInput accepted {sent}/1 input (Win32 {}); UIPI failures may not set last error",
            unsafe { GetLastError() }
        );
        Ok(())
    }
}
fn mouse_input(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INPUT_MARKER,
            },
        },
    }
}
/// Explicit cleanup command after a report retained unverified releases. No
/// constructor or background retry synthesizes input on the user's behalf.
pub fn retry_unreleased_input() -> Result<Vec<InputButton>> {
    ensure!(
        NATIVE_INPUT_BUSY
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok(),
        "Native macro still active"
    );
    let _lease = NativeLease;
    let mut ledger = unreleased()
        .lock()
        .map_err(|_| anyhow::anyhow!("Release ledger poisoned"))?;
    let mut input = NativeInputTransport;
    ledger.retain(|button| {
        input
            .send(InputEvent::Button {
                button: *button,
                up: true,
            })
            .is_err()
    });
    Ok(ledger.clone())
}

const STOP: u32 = WM_APP + 0x130;
const EVENT: u32 = WM_APP + 0x131;
#[derive(Clone)]
enum Sink {
    Record {
        sender: SyncSender<CapturedEvent>,
        overflow: Arc<AtomicBool>,
        thread_id: u32,
    },
    Interrupt(PlaybackControl),
}
thread_local! {static SINK:RefCell<Option<Sink>>=const{RefCell::new(None)};}
fn next(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { CallNextHookEx(ptr::null_mut(), code, w, l) }
}
fn capture(event: CapturedEvent) -> bool {
    SINK.with(|slot| match slot.borrow().as_ref() {
        Some(Sink::Record {
            sender,
            overflow,
            thread_id,
        }) => {
            if event.injected || event.extra_info == INPUT_MARKER {
                return false;
            }
            if sender.try_send(event).is_err() {
                overflow.store(true, Ordering::Release);
                unsafe { PostThreadMessageW(*thread_id, STOP, 0, 0) };
            } else {
                unsafe { PostThreadMessageW(*thread_id, EVENT, 0, 0) };
            }
            true
        }
        _ => false,
    })
}
unsafe extern "system" fn keyboard_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 || l == 0 {
        return next(code, w, l);
    }
    let result = std::panic::catch_unwind(|| {
        let raw = unsafe { &*(l as *const KBDLLHOOKSTRUCT) };
        let direction = match w as u32 {
            WM_KEYDOWN | WM_SYSKEYDOWN => Direction::Down,
            WM_KEYUP | WM_SYSKEYUP => Direction::Up,
            _ => return false,
        };
        SINK.with(|slot| {
            if let Some(Sink::Interrupt(control)) = slot.borrow().as_ref() {
                control.interrupt_keyboard(raw.flags, raw.dwExtraInfo);
            }
        });
        capture(CapturedEvent {
            source: Source::Keyboard,
            direction,
            key: raw.vkCode,
            point: Point::default(),
            timestamp_ms: raw.time,
            injected: raw.flags & 0x12 != 0,
            extra_info: raw.dwExtraInfo,
        })
    });
    if result.unwrap_or(false) {
        1
    } else {
        next(code, w, l)
    }
}
unsafe extern "system" fn mouse_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 || l == 0 {
        return next(code, w, l);
    }
    let result = std::panic::catch_unwind(|| {
        let raw = unsafe { &*(l as *const MSLLHOOKSTRUCT) };
        let (direction, key) = match w as u32 {
            WM_LBUTTONDOWN => (Direction::Down, 1),
            WM_LBUTTONUP => (Direction::Up, 1),
            WM_RBUTTONDOWN => (Direction::Down, 2),
            WM_RBUTTONUP => (Direction::Up, 2),
            WM_MBUTTONDOWN => (Direction::Down, 3),
            WM_MBUTTONUP => (Direction::Up, 3),
            WM_XBUTTONDOWN => (Direction::Down, raw.mouseData & 0xffff0000),
            WM_XBUTTONUP => (Direction::Up, raw.mouseData & 0xffff0000),
            WM_MOUSEWHEEL => (Direction::Wheel, ((raw.mouseData as i32) >> 16) as u32),
            WM_MOUSEHWHEEL => (
                Direction::HorizontalWheel,
                ((raw.mouseData as i32) >> 16) as u32,
            ),
            WM_MOUSEMOVE => (Direction::Move, 0),
            _ => return false,
        };
        let captured = capture(CapturedEvent {
            source: Source::Mouse,
            direction,
            key,
            point: Point {
                x: raw.pt.x,
                y: raw.pt.y,
            },
            timestamp_ms: raw.time,
            injected: raw.flags & 3 != 0,
            extra_info: raw.dwExtraInfo,
        });
        captured && direction != Direction::Move
    });
    if result.unwrap_or(false) {
        1
    } else {
        next(code, w, l)
    }
}
unsafe extern "system" fn session_window(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
) -> LRESULT {
    if message == WM_WTSSESSION_CHANGE && matches!(w as u32, 2 | 4 | 6 | 7 | 9) {
        SINK.with(|slot| match slot.borrow().as_ref() {
            Some(Sink::Record {
                overflow,
                thread_id,
                ..
            }) => {
                overflow.store(true, Ordering::Release);
                unsafe { PostThreadMessageW(*thread_id, STOP, 0, 0) };
            }
            Some(Sink::Interrupt(control)) => control.stop(),
            None => {}
        });
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, message, w, l) }
}
struct Hooks {
    keyboard: HHOOK,
    mouse: HHOOK,
    window: HWND,
    session_registered: bool,
}
impl Drop for Hooks {
    fn drop(&mut self) {
        unsafe {
            if !self.keyboard.is_null() {
                UnhookWindowsHookEx(self.keyboard);
            }
            if !self.mouse.is_null() {
                UnhookWindowsHookEx(self.mouse);
            }
            if self.session_registered {
                WTSUnRegisterSessionNotification(self.window);
            }
            if !self.window.is_null() {
                DestroyWindow(self.window);
            }
        };
        SINK.with(|slot| *slot.borrow_mut() = None);
    }
}
fn install_hooks(record: bool) -> Result<Hooks> {
    let mut hooks = Hooks {
        keyboard: ptr::null_mut(),
        mouse: ptr::null_mut(),
        window: ptr::null_mut(),
        session_registered: false,
    };
    let module = unsafe { GetModuleHandleW(ptr::null()) };
    ensure!(!module.is_null(), "Macro module handle unavailable");
    let class: Vec<u16> = "JiksExplicitMacroSession\0".encode_utf16().collect();
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(session_window),
        hInstance: module,
        lpszClassName: class.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    let registered = unsafe { RegisterClassW(&window_class) };
    ensure!(
        registered != 0 || unsafe { GetLastError() } == 1410,
        "Macro session window class registration failed"
    );
    hooks.window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            ptr::null_mut(),
            module,
            ptr::null(),
        )
    };
    ensure!(
        !hooks.window.is_null(),
        "Macro session message window creation failed"
    );
    ensure!(
        unsafe { WTSRegisterSessionNotification(hooks.window, 0) } != 0,
        "Macro session notifications unavailable"
    );
    hooks.session_registered = true;
    hooks.keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), module, 0) };
    ensure!(
        !hooks.keyboard.is_null(),
        "Keyboard hook installation failed"
    );
    if record {
        hooks.mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), module, 0) };
        ensure!(!hooks.mouse.is_null(), "Mouse hook installation failed");
    }
    Ok(hooks)
}
struct HookSession {
    thread_id: Arc<AtomicU32>,
    stop_requested: Arc<AtomicBool>,
    join: Option<JoinHandle<Result<()>>>,
}
impl HookSession {
    fn stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
        let id = self.thread_id.load(Ordering::Acquire);
        if id != 0 {
            unsafe { PostThreadMessageW(id, STOP, 0, 0) };
        }
    }
    fn start(
        record: Option<Arc<Mutex<RecorderState>>>,
        control: Option<PlaybackControl>,
        lease: Option<NativeLease>,
    ) -> Result<Self> {
        let thread_id = Arc::new(AtomicU32::new(0));
        let id = thread_id.clone();
        let stop_requested = Arc::new(AtomicBool::new(false));
        let worker_stop = stop_requested.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("jiks-explicit-input-hook".into())
            .spawn(move || {
                let _lease = lease;
                let mut msg: MSG = unsafe { std::mem::zeroed() };
                unsafe { PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_NOREMOVE) };
                let current =
                    unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
                id.store(current, Ordering::Release);
                if worker_stop.load(Ordering::Acquire) {
                    id.store(0, Ordering::Release);
                    return Ok(());
                }
                let (tx, rx) = mpsc::sync_channel(1024);
                let overflow = Arc::new(AtomicBool::new(false));
                let sink = if record.is_some() {
                    Sink::Record {
                        sender: tx,
                        overflow: overflow.clone(),
                        thread_id: current,
                    }
                } else {
                    Sink::Interrupt(
                        control
                            .clone()
                            .context("Playback interrupt control absent")?,
                    )
                };
                SINK.with(|slot| *slot.borrow_mut() = Some(sink));
                let hooks = match install_hooks(record.is_some()) {
                    Ok(hooks) => {
                        let _ = ready_tx.send(Ok(()));
                        hooks
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("{error:#}")));
                        id.store(0, Ordering::Release);
                        return Err(error);
                    }
                };
                let result = (|| -> Result<()> {
                    loop {
                        if worker_stop.load(Ordering::Acquire) {
                            break;
                        }
                        let result = unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) };
                        ensure!(result >= 0, "Macro hook message pump failed");
                        if result == 0 {
                            break;
                        }
                        if let Some(state) = &record {
                            let mut state = state
                                .lock()
                                .map_err(|_| anyhow::anyhow!("Macro recorder state poisoned"))?;
                            if overflow.load(Ordering::Acquire) {
                                state.stop(
                                    true,
                                    "Session changed or recording event queue overflowed",
                                );
                            }
                            while let Ok(event) = rx.try_recv() {
                                match state.feed(event) {
                                    Ok(_) => {}
                                    Err(error) => {
                                        state.stop(
                                            true,
                                            &format!("Invalid captured event: {error:#}"),
                                        );
                                        break;
                                    }
                                }
                            }
                            if state.is_stopped() {
                                break;
                            }
                        }
                        if msg.message == STOP {
                            break;
                        }
                        unsafe {
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg)
                        };
                    }
                    Ok(())
                })();
                // Explicit stop drains already queued owned events before finalizing;
                // it must not silently lose the matching up behind the stop flag.
                if let Some(state) = &record {
                    if let Ok(mut state) = state.lock() {
                        while let Ok(event) = rx.try_recv() {
                            if state.feed(event).is_err() {
                                state.stop(true, "Invalid final captured event");
                                break;
                            }
                        }
                    }
                }
                drop(hooks);
                id.store(0, Ordering::Release);
                if let Some(state) = record {
                    if let Ok(mut state) = state.lock() {
                        if result.is_err() {
                            state.stop(true, "Native recorder stopped with error");
                        } else if !state.is_stopped() {
                            state.stop(false, "Stopped explicitly");
                        }
                    }
                }
                if let Some(control) = control {
                    if result.is_err() {
                        control.stop();
                    }
                }
                result
            })?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                thread_id,
                stop_requested,
                join: Some(join),
            }),
            other => {
                stop_requested.store(true, Ordering::Release);
                let id = thread_id.load(Ordering::Acquire);
                if id != 0 {
                    unsafe { PostThreadMessageW(id, STOP, 0, 0) };
                }
                bail!("Macro hook startup failed: {other:?}")
            }
        }
    }
    fn try_finish(&mut self) -> Option<Result<()>> {
        if !self.join.as_ref()?.is_finished() {
            return None;
        }
        let result = self
            .join
            .take()
            .unwrap()
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("Macro hook thread panicked")));
        Some(result)
    }
}
impl Drop for HookSession {
    fn drop(&mut self) {
        self.stop(); /* Thread owns hooks/lease until physically unhooked. */
    }
}

/// Only start installs a native hook. stop is nonblocking; try_finish reports
/// physical unhook completion. Drop posts stop, it never blocks a UI thread.
#[derive(Default)]
pub struct NativeRecorder {
    state: Option<Arc<Mutex<RecorderState>>>,
    session: Option<HookSession>,
    cursor: usize,
    #[cfg(test)]
    fixture: Option<Arc<Mutex<RecorderFixtureState>>>,
}
#[cfg(test)]
#[derive(Default)]
struct RecorderFixtureState {
    state: Option<Arc<Mutex<RecorderState>>>,
    active: bool,
    stop_requested: bool,
    cleanup_acknowledged: bool,
    starts: usize,
}
/// Test-only source: owned event data and explicit cleanup acknowledgements.
/// There is no HookSession, native lease, message window, input or thread.
#[cfg(test)]
pub(crate) struct RecorderFixture(Arc<Mutex<RecorderFixtureState>>);
#[cfg(test)]
impl RecorderFixture {
    pub(crate) fn feed(&self, event: CapturedEvent) -> Result<RecordDisposition> {
        let state = {
            let fixture = self.0.lock().unwrap();
            ensure!(
                fixture.active && !fixture.cleanup_acknowledged,
                "Fake recorder source is not active"
            );
            fixture
                .state
                .clone()
                .context("Fake recorder has no state")?
        };
        let result = state.lock().unwrap().feed(event);
        result
    }
    pub(crate) fn stop_requested(&self) -> bool {
        self.0.lock().unwrap().stop_requested
    }
    pub(crate) fn starts(&self) -> usize {
        self.0.lock().unwrap().starts
    }
    pub(crate) fn acknowledge_cleanup(&self) -> Result<()> {
        let mut fixture = self.0.lock().unwrap();
        ensure!(
            fixture.active && !fixture.cleanup_acknowledged,
            "No pending fake cleanup"
        );
        let state = fixture
            .state
            .clone()
            .context("Fake recorder has no state")?;
        let mut state = state.lock().unwrap();
        ensure!(
            fixture.stop_requested || state.is_stopped(),
            "Stop has not been requested"
        );
        state.stop(false, "Fake source cleanup actually acknowledged");
        fixture.cleanup_acknowledged = true;
        Ok(())
    }
}
impl NativeRecorder {
    pub fn new() -> Self {
        Self::default()
    }
    #[cfg(test)]
    pub(crate) fn fixture() -> (Self, RecorderFixture) {
        let state = Arc::new(Mutex::new(RecorderFixtureState::default()));
        (
            Self {
                fixture: Some(state.clone()),
                ..Self::default()
            },
            RecorderFixture(state),
        )
    }
    pub fn start(&mut self, settings: RecorderSettings) -> Result<()> {
        #[cfg(test)]
        if let Some(fixture) = &self.fixture {
            let mut fixture = fixture.lock().unwrap();
            ensure!(!fixture.active, "Fake recorder is still active/unhooking");
            let state = Arc::new(Mutex::new(RecorderState::new(settings)?));
            self.state = Some(state.clone());
            self.cursor = 0;
            fixture.state = Some(state);
            fixture.active = true;
            fixture.stop_requested = false;
            fixture.cleanup_acknowledged = false;
            fixture.starts += 1;
            return Ok(());
        }
        ensure!(
            self.session.is_none(),
            "Recorder session is still active/unhooking"
        );
        let state = Arc::new(Mutex::new(RecorderState::new(settings)?));
        let lease = NativeLease::acquire()?;
        let session = HookSession::start(Some(state.clone()), None, Some(lease))?;
        self.state = Some(state);
        self.session = Some(session);
        self.cursor = 0;
        Ok(())
    }
    pub fn stop(&self) {
        #[cfg(test)]
        if let Some(fixture) = &self.fixture {
            let mut fixture = fixture.lock().unwrap();
            if fixture.active {
                fixture.stop_requested = true;
            }
            return;
        }
        if let Some(session) = &self.session {
            session.stop();
        }
    }
    pub fn drain(&mut self) -> Result<Vec<MacroEvent>> {
        let Some(state) = &self.state else {
            return Ok(vec![]);
        };
        let state = state
            .lock()
            .map_err(|_| anyhow::anyhow!("Macro recorder state poisoned"))?;
        let result = state.events()[self.cursor.min(state.events().len())..].to_vec();
        self.cursor = state.events().len();
        Ok(result)
    }
    pub fn try_finish(&mut self) -> Option<Result<RecordingReport>> {
        #[cfg(test)]
        if let Some(fixture) = &self.fixture {
            {
                let mut fixture = fixture.lock().unwrap();
                if !fixture.active || !fixture.cleanup_acknowledged {
                    return None;
                }
                fixture.active = false;
                fixture.cleanup_acknowledged = false;
            }
            return Some(
                self.state
                    .as_ref()
                    .context("Fake recorder state unavailable")
                    .and_then(|state| {
                        state
                            .lock()
                            .map_err(|_| anyhow::anyhow!("Fake recorder state poisoned"))?
                            .report()
                    }),
            );
        }
        let result = self.session.as_mut()?.try_finish()?;
        self.session = None;
        Some(result.and_then(|()| {
            self.state
                .as_ref()
                .context("Recorder state unavailable")?
                .lock()
                .map_err(|_| anyhow::anyhow!("Macro recorder state poisoned"))?
                .report()
        }))
    }
    pub fn is_active(&self) -> bool {
        #[cfg(test)]
        if let Some(fixture) = &self.fixture {
            return fixture.lock().unwrap().active;
        }
        self.session.is_some()
    }
}

/// Owns a real playback worker until its report (including cleanup) is available.
#[derive(Default)]
pub struct NativePlayer {
    control: Option<PlaybackControl>,
    join: Option<JoinHandle<Result<PlaybackReport>>>,
}
impl NativePlayer {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn start(&mut self, sequence: MacroSequence) -> Result<()> {
        ensure!(
            self.join.is_none(),
            "Previous macro has not physically completed"
        );
        sequence.validate_playback()?;
        let lease = NativeLease::acquire()?;
        let control = PlaybackControl::new();
        let worker_control = control.clone();
        self.join = Some(
            thread::Builder::new()
                .name("jiks-explicit-macro-player".into())
                .spawn(move || {
                    let _lease = lease;
                    let mut monitor = if sequence.interrupt_on_other_key {
                        Some(HookSession::start(
                            None,
                            Some(worker_control.clone()),
                            None,
                        )?)
                    } else {
                        None
                    };
                    let report = play(
                        &sequence,
                        &mut SystemClock,
                        &mut NativeInputTransport,
                        &worker_control,
                    );
                    if let Ok(report) = &report {
                        if !report.unreleased.is_empty() {
                            unreleased()
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .extend(report.unreleased.iter().copied());
                        }
                    }
                    if let Some(monitor) = &mut monitor {
                        monitor.stop();
                        if let Some(join) = monitor.join.take() {
                            join.join().map_err(|_| {
                                anyhow::anyhow!("Playback interrupt monitor panicked")
                            })??;
                        }
                    }
                    report
                })?,
        );
        self.control = Some(control);
        Ok(())
    }
    pub fn stop(&self) {
        if let Some(control) = &self.control {
            control.stop();
        }
    }
    pub fn try_finish(&mut self) -> Option<Result<PlaybackReport>> {
        if !self.join.as_ref()?.is_finished() {
            return None;
        }
        self.control = None;
        let result = self
            .join
            .take()
            .unwrap()
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("Native player thread panicked")));
        Some(result)
    }
    pub fn is_active(&self) -> bool {
        self.join.is_some()
    }
}
impl Drop for NativePlayer {
    fn drop(&mut self) {
        self.stop(); /* Worker owns cleanup and native lease after client drop. */
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn constructors_do_not_start_hooks_threads_or_native_input() {
        let player = NativePlayer::new();
        let recorder = NativeRecorder::new();
        assert!(!player.is_active());
        assert!(!recorder.is_active());
    }
    #[test]
    fn desktop_mapping_handles_virtual_origin_and_pixel_endpoints() {
        let rect = DesktopRect {
            left: -1920,
            top: -100,
            width: 3840,
            height: 1080,
        };
        assert_eq!(
            normalize_position(Point { x: -1920, y: -100 }, rect).unwrap(),
            (0, 0)
        );
        assert_eq!(
            normalize_position(Point { x: 1919, y: 979 }, rect).unwrap(),
            (65535, 65535)
        );
        assert_eq!(
            normalize_position(
                Point {
                    x: i32::MIN,
                    y: i32::MAX
                },
                rect
            )
            .unwrap(),
            (0, 65535)
        );
        assert!(normalize_position(Point::default(), DesktopRect { width: 0, ..rect }).is_err());
    }
    #[test]
    fn mouse_and_extended_key_codec_does_not_call_sendinput() {
        assert!(extended(0xa3));
        assert!(!extended(0x41));
        let encoded = mouse_input(0, 0, (-120i32) as u32, MOUSEEVENTF_HWHEEL);
        let mouse = unsafe { encoded.Anonymous.mi };
        assert_eq!(mouse.mouseData as i32, -120);
        assert_eq!(mouse.dwFlags, MOUSEEVENTF_HWHEEL);
        assert_eq!(mouse.dwExtraInfo, INPUT_MARKER);
    }
}
