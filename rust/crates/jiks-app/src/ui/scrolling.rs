//! Shared Win32 scroll layout. Keep the native control handles and move only
//! children that intersect the viewport, then queue one clipped repaint.
use std::{
    cell::Cell,
    mem::size_of,
    ptr::{null, null_mut},
    rc::Rc,
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::Threading::{
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateWaitableTimerExW,
        INFINITE, SYNCHRONIZATION_SYNCHRONIZE, SetWaitableTimer, TIMER_MODIFY_STATE,
    },
    UI::{
        Controls::SetScrollInfo,
        Input::KeyboardAndMouse::IsWindowEnabled,
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

pub(super) const SCROLL_TIMER: usize = 0x4A13;
const FRAME_DUE_100NS: i64 = -80_000;
const FALLBACK_INTERVAL_MS: u32 = 16;

pub(super) struct FrameTimer {
    handle: HANDLE,
    armed: Cell<bool>,
    failed: Cell<bool>,
}

impl FrameTimer {
    fn new() -> Option<Self> {
        let handle = unsafe {
            CreateWaitableTimerExW(
                null(),
                null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                SYNCHRONIZATION_SYNCHRONIZE | TIMER_MODIFY_STATE,
            )
        };
        (!handle.is_null()).then(|| Self {
            handle,
            armed: Cell::new(false),
            failed: Cell::new(false),
        })
    }

    fn arm(&self) -> bool {
        // Relative, one-shot expiration. Arm again only while moving.
        let armed = !self.failed.get()
            && unsafe {
                SetWaitableTimer(self.handle, &FRAME_DUE_100NS, 0, None, null(), 0) != 0
            };
        self.armed.set(armed);
        armed
    }
}

impl Drop for FrameTimer {
    fn drop(&mut self) {
        unsafe {
            CancelWaitableTimer(self.handle);
            CloseHandle(self.handle);
        }
    }
}

#[derive(Default)]
pub(super) struct ScrollMessages {
    after_frame: bool,
}

impl ScrollMessages {
    pub(super) unsafe fn next(
        &mut self,
        hwnd: HWND,
        msg: &mut MSG,
        timer: Option<Rc<FrameTimer>>,
    ) -> Option<i32> {
        unsafe {
            let Some(timer) = timer else {
                self.after_frame = false;
                return Some(GetMessageW(msg, null_mut(), 0, 0));
            };
            // A slow frame must still leave room for input, paint and WM_QUIT.
            if std::mem::take(&mut self.after_frame)
                && PeekMessageW(msg, null_mut(), 0, 0, PM_REMOVE) != 0
            {
                return Some(i32::from(msg.message != WM_QUIT));
            }
            if IsWindow(hwnd) == 0 {
                return None;
            }
            // PeekMessage can dispatch a synchronous cancellation. Revisit the
            // host state if it did so rather than waiting on a canceled timer.
            // The Rc keeps the handle valid across that callback.
            if !timer.armed.get() {
                return None;
            }
            let result = MsgWaitForMultipleObjectsEx(
                1,
                &timer.handle,
                INFINITE,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
            if result == WAIT_OBJECT_0 + 1 {
                return (PeekMessageW(msg, null_mut(), 0, 0, PM_REMOVE) != 0)
                    .then_some(i32::from(msg.message != WM_QUIT));
            }
            if result != WAIT_OBJECT_0 {
                timer.failed.set(true);
            }
            timer.armed.set(false);
            self.after_frame = true;
            *msg = MSG {
                hwnd,
                message: WM_TIMER,
                wParam: SCROLL_TIMER,
                ..Default::default()
            };
            Some(1)
        }
    }
}

#[derive(Default)]
enum FrameClock {
    #[default]
    Uninitialized,
    HighResolution(Rc<FrameTimer>),
    WindowTimer,
}

struct WheelMotion {
    position: f64,
    target: f64,
    last_frame: Instant,
}

pub(super) struct ScrollChild {
    pub hwnd: HWND,
    pub rect: [i32; 4],
    pub pinned: bool,
    clip_height: i32,
    position: Option<[i32; 4]>,
    region: Option<[i32; 4]>,
    visible: bool,
}

impl ScrollChild {
    pub(super) fn new(hwnd: HWND, rect: [i32; 4], clip_height: i32, pinned: bool) -> Self {
        Self {
            hwnd,
            rect,
            pinned,
            clip_height,
            position: None,
            region: None,
            // The parent can still be hidden during construction. Cache the
            // child's own style, since IsWindowVisible includes its ancestors.
            visible: unsafe { GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_VISIBLE != 0 },
        }
    }
}

pub(super) struct Viewport {
    /// Client rectangle in logical units, left/top/right/bottom.
    pub bounds: [i32; 4],
    pub extent: i32,
    pub page: i32,
    pub dpi: u32,
}

#[derive(Default)]
pub(super) struct Scrolling {
    motion: Option<WheelMotion>,
    scrollbar: Option<(i32, u32, i32)>,
    clock: FrameClock,
}

impl Scrolling {
    fn maximum(&self) -> i32 {
        self.scrollbar
            .map_or(0, |(max, page, _)| (max + 1 - page as i32).max(0))
    }

    fn add_wheel_delta(&mut self, current: i32, delta: i16, now: Instant) {
        if delta == 0 {
            return;
        }
        let max = f64::from(self.maximum());
        let motion = self.motion.get_or_insert(WheelMotion {
            position: f64::from(current),
            target: f64::from(current),
            last_frame: now,
        });
        // C# SmoothScrollBehavior subtracts the raw wheel delta from the DIP
        // target. A standard detent is 120 DIPs, including partial detents.
        motion.target = (motion.target - f64::from(delta)).clamp(0.0, max);
        if motion.target == motion.position {
            self.motion = None;
        }
    }

    fn advance(&mut self, now: Instant) -> Option<i32> {
        let motion = self.motion.as_mut()?;
        let dt = now
            .saturating_duration_since(motion.last_frame)
            .as_secs_f64()
            .min(0.1);
        if dt == 0.0 {
            return None;
        }
        motion.last_frame = now;
        let diff = motion.target - motion.position;
        if diff.abs() < 0.2 {
            let position = motion.target.round() as i32;
            self.motion = None;
            return Some(position);
        }
        motion.position += diff * (1.0 - (-15.0 * dt).exp());
        Some(motion.position.round() as i32)
    }

    pub(super) unsafe fn wheel(&mut self, hwnd: HWND, current: i32, wparam: usize) -> Option<i32> {
        unsafe {
            if IsWindowEnabled(hwnd) == 0 {
                return None;
            }
            let was_active = self.motion.is_some();
            self.add_wheel_delta(current, ((wparam >> 16) as u16) as i16, Instant::now());
            if was_active {
                if self.motion.is_none() {
                    self.stop_clock(hwnd);
                }
                return None;
            }
            let target = self.motion.as_ref()?.target.round() as i32;
            if self.start_clock(hwnd) {
                return None;
            }
            // Preserve navigation even if neither timer can be started.
            self.motion = None;
            (target != current).then_some(target)
        }
    }

    pub(super) unsafe fn frame(&mut self, hwnd: HWND, current: i32) -> Option<i32> {
        unsafe {
            if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 || IsWindowEnabled(hwnd) == 0 {
                self.cancel(hwnd);
                return None;
            }
            let target = self.motion.as_ref()?.target.round() as i32;
            let mut position = self.advance(Instant::now());
            if self.motion.is_none() {
                self.stop_clock(hwnd);
            } else if let FrameClock::HighResolution(timer) = &self.clock {
                if !timer.arm() && !self.use_window_timer(hwnd) {
                    self.motion = None;
                    position = Some(target);
                }
            }
            position.filter(|position| *position != current)
        }
    }

    pub(super) unsafe fn cancel(&mut self, hwnd: HWND) {
        if self.motion.take().is_some() {
            unsafe {
                self.stop_clock(hwnd);
            }
        }
    }

    pub(super) fn wait_timer(&self) -> Option<Rc<FrameTimer>> {
        if let FrameClock::HighResolution(timer) = &self.clock {
            if self.motion.is_some() && timer.armed.get() {
                return Some(Rc::clone(timer));
            }
        }
        None
    }

    unsafe fn start_clock(&mut self, hwnd: HWND) -> bool {
        unsafe {
            if matches!(self.clock, FrameClock::Uninitialized) {
                if let Some(timer) = FrameTimer::new() {
                    self.clock = FrameClock::HighResolution(Rc::new(timer));
                }
            }
            if let FrameClock::HighResolution(timer) = &self.clock {
                if timer.arm() {
                    // GetMessage may have dispatched a sent wheel message
                    // while blocking. Wake it to start waiting on our handle.
                    PostMessageW(hwnd, WM_NULL, 0, 0);
                    return true;
                }
            }
            self.use_window_timer(hwnd)
        }
    }

    unsafe fn use_window_timer(&mut self, hwnd: HWND) -> bool {
        self.clock = FrameClock::WindowTimer;
        unsafe { SetTimer(hwnd, SCROLL_TIMER, FALLBACK_INTERVAL_MS, None) != 0 }
    }

    unsafe fn stop_clock(&self, hwnd: HWND) {
        unsafe {
            match &self.clock {
                FrameClock::Uninitialized => {}
                FrameClock::HighResolution(timer) => {
                    timer.armed.set(false);
                    CancelWaitableTimer(timer.handle);
                }
                FrameClock::WindowTimer => {
                    KillTimer(hwnd, SCROLL_TIMER);
                }
            }
        }
    }

    pub(super) unsafe fn command(&mut self, hwnd: HWND, current: i32, command: i32) -> i32 {
        unsafe {
            let page = self.scrollbar.map_or(0, |(_, page, _)| page as i32);
            let maximum = self.maximum();
            let next = match command {
                SB_LINEUP => current - 32,
                SB_LINEDOWN => current + 32,
                SB_PAGEUP => current - (page - 32).max(32),
                SB_PAGEDOWN => current + (page - 32).max(32),
                SB_TOP => 0,
                SB_BOTTOM => maximum,
                SB_THUMBTRACK | SB_THUMBPOSITION => {
                    let mut info = SCROLLINFO {
                        cbSize: size_of::<SCROLLINFO>() as u32,
                        fMask: SIF_TRACKPOS,
                        ..Default::default()
                    };
                    if GetScrollInfo(hwnd, SB_VERT, &mut info) == 0 {
                        current
                    } else {
                        info.nTrackPos
                    }
                }
                SB_ENDSCROLL => current,
                _ => return current,
            };
            self.cancel(hwnd);
            next.clamp(0, maximum)
        }
    }

    pub(super) unsafe fn place<'a>(
        &mut self,
        parent: HWND,
        scroll: &mut i32,
        viewport: Viewport,
        children: impl IntoIterator<Item = &'a mut ScrollChild>,
    ) {
        unsafe {
            let max = (viewport.extent - 1).max(0);
            let page = viewport.page.max(1) as u32;
            let maximum = (max + 1 - page as i32).max(0);
            *scroll = (*scroll).clamp(0, maximum);
            if let Some(motion) = self.motion.as_mut() {
                motion.position = motion.position.clamp(0.0, f64::from(maximum));
                motion.target = motion.target.clamp(0.0, f64::from(maximum));
                if motion.position == motion.target || motion.position.round() as i32 != *scroll {
                    self.cancel(parent);
                }
            }
            let new_scrollbar = (max, page, *scroll);
            let mut repaint = self.scrollbar != Some(new_scrollbar);
            if repaint {
                let info = SCROLLINFO {
                    cbSize: size_of::<SCROLLINFO>() as u32,
                    fMask: SIF_RANGE | SIF_PAGE | SIF_POS | SIF_DISABLENOSCROLL,
                    nMin: 0,
                    nMax: max,
                    nPage: page,
                    nPos: *scroll,
                    nTrackPos: 0,
                };
                SetScrollInfo(parent, SB_VERT, &info, 1);
                self.scrollbar = Some(new_scrollbar);
            }
            let scale = |dip: i32| dip * viewport.dpi as i32 / 96;
            let [left, top, right, bottom] = viewport.bounds.map(scale);
            let mut moves = Vec::new();
            for child in children {
                if child.hwnd.is_null() {
                    continue;
                }
                let [x, origin_y, w, h] = child.rect;
                let y = origin_y - if child.pinned { 0 } else { *scroll };
                let rect = [scale(x), scale(y), scale(w), scale(h)];
                let clip_width = rect[2].max(0);
                let clip_height = scale(child.clip_height).max(0);
                let clip_left = (left - rect[0]).clamp(0, clip_width);
                let clip_right = (right - rect[0]).clamp(0, clip_width);
                let clip_top = (top - rect[1]).clamp(0, clip_height);
                let clip_bottom = (bottom - rect[1]).clamp(0, clip_height);
                let mut visible = clip_bottom > clip_top && clip_right > clip_left;
                if visible {
                    let clip = [clip_left, clip_top, clip_right, clip_bottom];
                    let region = (clip != [0, 0, rect[2], rect[3]]).then_some(clip);
                    if region != child.region {
                        repaint = true;
                        if set_region(child.hwnd, region) {
                            child.region = region;
                        } else {
                            // Retry on a later layout. Never expose a window
                            // across the header or footer with a stale clip.
                            visible = false;
                        }
                    }
                }
                let moved = visible && child.position != Some(rect);
                if moved || visible != child.visible {
                    let mut flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW;
                    if !moved {
                        flags |= SWP_NOMOVE | SWP_NOSIZE;
                    } else if child.position.is_some_and(|old| old[2..] == rect[2..]) {
                        flags |= SWP_NOSIZE;
                    }
                    if visible != child.visible {
                        flags |= if visible {
                            SWP_SHOWWINDOW
                        } else {
                            SWP_HIDEWINDOW
                        };
                    }
                    moves.push(WindowMove {
                        child,
                        rect,
                        flags,
                        visible,
                        moved,
                    });
                }
            }
            if !moves.is_empty() {
                repaint = true;
                let mut batch = BeginDeferWindowPos(moves.len() as i32);
                for step in &moves {
                    if batch.is_null() {
                        break;
                    }
                    let [x, y, w, h] = step.rect;
                    batch =
                        DeferWindowPos(batch, step.child.hwnd, null_mut(), x, y, w, h, step.flags);
                }
                let applied = !batch.is_null() && EndDeferWindowPos(batch) != 0;
                for step in moves {
                    let [x, y, w, h] = step.rect;
                    if applied
                        || SetWindowPos(step.child.hwnd, null_mut(), x, y, w, h, step.flags) != 0
                    {
                        if step.moved {
                            step.child.position = Some(step.rect);
                        }
                        step.child.visible = step.visible;
                    } else {
                        step.child.position = None;
                        step.child.visible =
                            GetWindowLongW(step.child.hwnd, GWL_STYLE) as u32 & WS_VISIBLE != 0;
                    }
                }
            }
            if repaint && right > left && bottom > top {
                let rect = RECT {
                    left,
                    top,
                    right,
                    bottom,
                };
                RedrawWindow(
                    parent,
                    &rect,
                    null_mut(),
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
                );
            }
        }
    }
}

const WHEEL_SUBCLASS: usize = 0x4A495753;

pub(super) unsafe fn install_wheel_routing(hwnd: HWND, class: &str) {
    let kind = match class {
        "COMBOBOX" => 1,
        "EDIT" | "LISTBOX" => 2,
        _ => return,
    };
    if !hwnd.is_null() {
        unsafe {
            SetWindowSubclass(hwnd, Some(route_wheel), WHEEL_SUBCLASS, kind);
        }
    }
}

unsafe extern "system" fn route_wheel(
    hwnd: HWND,
    msg: u32,
    wp: usize,
    lp: isize,
    _: usize,
    kind: usize,
) -> isize {
    unsafe {
        if msg == WM_MOUSEWHEEL {
            let keeps_wheel = if kind == 1 {
                SendMessageW(hwnd, CB_GETDROPPEDSTATE, 0, 0) != 0
            } else {
                let mut info = SCROLLINFO {
                    cbSize: size_of::<SCROLLINFO>() as u32,
                    fMask: SIF_RANGE | SIF_PAGE,
                    ..Default::default()
                };
                GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_VSCROLL != 0
                    && GetScrollInfo(hwnd, SB_VERT, &mut info) != 0
                    && info.nMax > info.nMin
                    && i64::from(info.nMax) - i64::from(info.nMin) + 1 > i64::from(info.nPage)
            };
            if !keeps_wheel {
                // DefWindowProc propagates this message to the parent once.
                // The closed ComboBox never sees it as a selection change.
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
        }
        if msg == WM_NCDESTROY {
            RemoveWindowSubclass(hwnd, Some(route_wheel), WHEEL_SUBCLASS);
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, ptr::null, time::Duration};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

    struct HiddenWindow(HWND);

    impl HiddenWindow {
        unsafe fn new() -> Self {
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    windows_sys::w!("STATIC"),
                    windows_sys::w!("Hidden native scroll regression"),
                    WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_VSCROLL,
                    0,
                    0,
                    850,
                    700,
                    null_mut(),
                    null_mut(),
                    GetModuleHandleW(null()),
                    null(),
                )
            };
            assert!(!hwnd.is_null());
            Self(hwnd)
        }

        unsafe fn child(&self, rect: [i32; 4], pinned: bool) -> ScrollChild {
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    windows_sys::w!("STATIC"),
                    windows_sys::w!("Fixture label"),
                    WS_CHILD | WS_VISIBLE,
                    rect[0],
                    rect[1],
                    rect[2],
                    rect[3],
                    self.0,
                    null_mut(),
                    GetModuleHandleW(null()),
                    null(),
                )
            };
            assert!(!hwnd.is_null());
            ScrollChild::new(hwnd, rect, rect[3], pinned)
        }
    }

    impl Drop for HiddenWindow {
        fn drop(&mut self) {
            unsafe {
                DestroyWindow(self.0);
            }
        }
    }

    unsafe fn child_top(parent: HWND, child: HWND) -> i32 {
        unsafe {
            let mut rect: RECT = std::mem::zeroed();
            assert_ne!(GetWindowRect(child, &mut rect), 0);
            let mut p = POINT {
                x: rect.left,
                y: rect.top,
            };
            MapWindowPoints(null_mut(), parent, &mut p, 1);
            p.y
        }
    }

    unsafe fn update_bounds_area(hwnd: HWND) -> i32 {
        unsafe {
            let mut rect: RECT = std::mem::zeroed();
            if GetUpdateRect(hwnd, &mut rect, 0) == 0 {
                0
            } else {
                (rect.right - rect.left) * (rect.bottom - rect.top)
            }
        }
    }

    #[test]
    fn native_scroll_keeps_partial_edges_and_pinned_controls_at_high_dpi() {
        unsafe {
            let parent = HiddenWindow::new();
            let mut controls = [
                parent.child([62, 112, 50, 26], true),
                parent.child([120, 104, 160, 136], false),
                parent.child([120, 270, 160, 100], false),
                parent.child([50, 116, 20, 26], true),
            ];
            let mut engine = Scrolling::default();
            let mut scroll = 0;
            let viewport = || Viewport {
                bounds: [60, 100, 340, 260],
                extent: 360,
                page: 160,
                dpi: 144,
            };
            engine.place(parent.0, &mut scroll, viewport(), &mut controls);
            assert_eq!(IsWindowVisible(parent.0), 0);
            assert_eq!(child_top(parent.0, controls[0].hwnd), 168);
            assert_eq!(
                GetWindowLongW(controls[2].hwnd, GWL_STYLE) as u32 & WS_VISIBLE,
                0
            );
            for step in 1..=8 {
                scroll = step * 8;
                engine.place(parent.0, &mut scroll, viewport(), &mut controls);
                assert_eq!(scroll, step * 8);
                assert_eq!(
                    child_top(parent.0, controls[1].hwnd),
                    (104 - scroll) * 144 / 96
                );
                assert_eq!(child_top(parent.0, controls[0].hwnd), 168);
            }
            let region = CreateRectRgn(0, 0, 0, 0);
            let mut bounds: RECT = std::mem::zeroed();
            for (index, expected) in [
                (1, [0, 90, 240, 204]),
                (2, [0, 0, 240, 81]),
                (3, [15, 0, 30, 39]),
            ] {
                assert_ne!(GetWindowRgn(controls[index].hwnd, region), ERROR);
                GetRgnBox(region, &mut bounds);
                assert_eq!(
                    [bounds.left, bounds.top, bounds.right, bounds.bottom],
                    expected
                );
            }
            DeleteObject(region);
            scroll = engine.command(parent.0, scroll, SB_BOTTOM);
            engine.place(parent.0, &mut scroll, viewport(), &mut controls);
            assert_eq!(scroll, 200);
            assert_eq!(GetScrollPos(parent.0, SB_VERT), 200);
            assert_eq!(engine.command(parent.0, scroll, SB_ENDSCROLL), 200);
            engine.add_wheel_delta(scroll, -120, Instant::now());
            assert!(engine.motion.is_none());
        }
    }

    #[test]
    fn wheel_frames_preserve_original_delta_reversal_and_scrollbar_cancellation() {
        unsafe {
            let parent = HiddenWindow::new();
            let mut engine = Scrolling {
                scrollbar: Some((499, 200, 0)),
                ..Default::default()
            };
            let start = Instant::now();
            let frame = |index: u64| start + Duration::from_millis(index * 16);
            engine.add_wheel_delta(0, -120, start);
            let frames = (1..=40)
                .filter_map(|index| engine.advance(frame(index)))
                .collect::<Vec<_>>();
            assert!(frames[0] > 0 && frames[0] < 120);
            assert!(frames.windows(2).all(|pair| pair[0] <= pair[1]));
            assert!(frames.windows(2).filter(|pair| pair[0] != pair[1]).count() > 5);
            assert_eq!(frames.last(), Some(&120));
            assert!(engine.motion.is_none());

            for _ in 0..8 {
                engine.add_wheel_delta(0, -15, frame(41));
            }
            let moved = engine.advance(frame(42)).unwrap();
            assert_eq!(moved, frames[0]);
            engine.add_wheel_delta(moved, 120, frame(42));
            let reversed = (43..=90)
                .filter_map(|index| engine.advance(frame(index)))
                .collect::<Vec<_>>();
            assert!(reversed.windows(2).all(|pair| pair[0] >= pair[1]));
            assert_eq!(reversed.last(), Some(&0));

            engine.add_wheel_delta(299, -120, frame(91));
            assert_eq!(engine.motion.as_ref().unwrap().target, 300.0);
            assert_eq!(engine.command(parent.0, 299, SB_TOP), 0);
            assert_eq!(engine.advance(frame(92)), None);
            engine.add_wheel_delta(0, -120, frame(93));
            assert_eq!(engine.command(parent.0, 0, SB_PAGEDOWN), 168);
            assert_eq!(engine.advance(frame(94)), None);
            engine.add_wheel_delta(300, -15, frame(95));
            assert!(engine.motion.is_none());
            let wheel_down = ((-120i16) as u16 as usize) << 16;
            assert_eq!(engine.wheel(parent.0, 0, wheel_down), None);
            assert!(engine.motion.is_some());
            assert_eq!(engine.frame(parent.0, 0), None);
            assert!(engine.motion.is_none(), "hiding the host stops its timer");
        }
    }

    #[test]
    fn native_scroll_pump_drives_one_shot_frames_and_delivers_quit() {
        unsafe {
            let host = HiddenWindow::new();
            let mut scrolling = Scrolling {
                scrollbar: Some((499, 200, 0)),
                ..Default::default()
            };
            let mut messages = ScrollMessages::default();
            let mut msg = MSG::default();
            let down = ((-120i16) as u16 as usize) << 16;
            assert!(scrolling.wait_timer().is_none());
            assert_eq!(scrolling.wheel(host.0, 0, down), None);
            let Some(timer) = scrolling.wait_timer() else {
                // Older Windows can use the ordinary window timer.
                assert!(matches!(scrolling.clock, FrameClock::WindowTimer));
                scrolling.cancel(host.0);
                return;
            };
            // The start marker wakes a GetMessage already blocked on input.
            assert_ne!(PeekMessageW(&mut msg, host.0, WM_NULL, WM_NULL, PM_REMOVE), 0);
            while !messages.after_frame {
                assert_ne!(messages.next(host.0, &mut msg, scrolling.wait_timer()), Some(0));
            }
            assert_eq!((msg.hwnd, msg.message, msg.wParam), (host.0, WM_TIMER, SCROLL_TIMER));
            assert!(!timer.armed.get(), "a delivered frame consumes the one-shot");

            assert!(timer.arm());
            assert_ne!(PostMessageW(host.0, WM_APP, 27, 0), 0);
            assert_eq!(messages.next(host.0, &mut msg, scrolling.wait_timer()), Some(1));
            assert_eq!((msg.message, msg.wParam), (WM_APP, 27));

            scrolling.cancel(host.0);
            assert!(scrolling.wait_timer().is_none());
            assert!(!timer.armed.get());
            PostQuitMessage(19);
            assert_eq!(messages.next(host.0, &mut msg, scrolling.wait_timer()), Some(0));
            assert_eq!(msg.wParam, 19);
        }
    }

    unsafe extern "system" fn count_parent_wheel(
        hwnd: HWND,
        msg: u32,
        wp: usize,
        lp: isize,
        _: usize,
        data: usize,
    ) -> isize {
        unsafe {
            if msg == WM_MOUSEWHEEL {
                let count = &*(data as *const Cell<u32>);
                count.set(count.get() + 1);
                return 0;
            }
            DefSubclassProc(hwnd, msg, wp, lp)
        }
    }

    #[test]
    fn native_wheel_routing_preserves_closed_combo_selection_and_scrollable_list() {
        unsafe {
            let count = Cell::new(0u32);
            let parent = HiddenWindow::new();
            assert_ne!(
                SetWindowSubclass(
                    parent.0,
                    Some(count_parent_wheel),
                    1,
                    &count as *const _ as usize
                ),
                0
            );
            let combo = CreateWindowExW(
                0,
                windows_sys::w!("COMBOBOX"),
                null(),
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST as u32 | WS_VSCROLL,
                20,
                20,
                150,
                120,
                parent.0,
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!combo.is_null());
            SendMessageW(combo, CB_ADDSTRING, 0, windows_sys::w!("One") as isize);
            SendMessageW(combo, CB_ADDSTRING, 0, windows_sys::w!("Two") as isize);
            SendMessageW(combo, CB_SETCURSEL, 0, 0);
            assert_eq!(SendMessageW(combo, CB_GETDROPPEDSTATE, 0, 0), 0);
            install_wheel_routing(combo, "COMBOBOX");
            SendMessageW(combo, WM_MOUSEWHEEL, ((-120i16) as u16 as usize) << 16, 0);
            assert_eq!(count.get(), 1);
            assert_eq!(SendMessageW(combo, CB_GETCURSEL, 0, 0), 0);

            let list = CreateWindowExW(
                0,
                windows_sys::w!("LISTBOX"),
                null(),
                WS_CHILD | WS_VISIBLE | WS_VSCROLL | LBS_NOINTEGRALHEIGHT as u32,
                20,
                70,
                150,
                40,
                parent.0,
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!list.is_null());
            for _ in 0..20 {
                SendMessageW(
                    list,
                    LB_ADDSTRING,
                    0,
                    windows_sys::w!("Scrollable row") as isize,
                );
            }
            let mut info = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE,
                ..Default::default()
            };
            assert_ne!(GetScrollInfo(list, SB_VERT, &mut info), 0);
            assert!(info.nMax - info.nMin + 1 > info.nPage as i32);
            install_wheel_routing(list, "LISTBOX");
            SendMessageW(list, WM_MOUSEWHEEL, ((-120i16) as u16 as usize) << 16, 0);
            assert_eq!(count.get(), 1, "native scrollable list retains the wheel");
            assert_eq!(IsWindowVisible(parent.0), 0);
        }
    }

    unsafe extern "system" fn count_moves(
        hwnd: HWND,
        msg: u32,
        wp: usize,
        lp: isize,
        _: usize,
        data: usize,
    ) -> isize {
        unsafe {
            if msg == WM_WINDOWPOSCHANGED {
                let count = &*(data as *const Cell<u32>);
                count.set(count.get() + 1);
            }
            DefSubclassProc(hwnd, msg, wp, lp)
        }
    }

    #[test]
    fn native_scroll_batches_visible_children_instead_of_moving_entire_page() {
        unsafe {
            // Use identical real Win32 child lists for both paths. Timings are
            // diagnostic only because this hidden fixture is not a frame test.
            let callbacks = Cell::new(0u32);
            let parent = HiddenWindow::new();
            let mut children = (0..160)
                .map(|row| parent.child([96, 96 + 30 * row, 240, 26], false))
                .collect::<Vec<_>>();
            for child in &children {
                assert_ne!(
                    SetWindowSubclass(
                        child.hwnd,
                        Some(count_moves),
                        1,
                        &callbacks as *const _ as usize
                    ),
                    0
                );
            }
            let mut baseline_times = Vec::new();
            for tick in 0..=32 {
                let now = Instant::now();
                let info = SCROLLINFO {
                    cbSize: size_of::<SCROLLINFO>() as u32,
                    fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                    nMin: 0,
                    nMax: 4820,
                    nPage: 360,
                    nPos: tick * 16,
                    nTrackPos: 0,
                };
                SetScrollInfo(parent.0, SB_VERT, &info, 1);
                for child in &children {
                    let [x, y, w, h] = child.rect;
                    let y = y - tick * 16;
                    SetWindowPos(
                        child.hwnd,
                        null_mut(),
                        x,
                        y,
                        w,
                        h,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    let top = (80 - y).clamp(0, h);
                    let bottom = (440 - y).clamp(0, h);
                    let region = CreateRectRgn(0, top, w, bottom);
                    if SetWindowRgn(child.hwnd, region, 1) == 0 {
                        DeleteObject(region);
                    }
                    ShowWindow(child.hwnd, if bottom > top { SW_SHOWNA } else { SW_HIDE });
                }
                InvalidateRect(parent.0, null(), 0);
                if tick == 0 {
                    // Both paths lay out the initial viewport before timing.
                    callbacks.set(0);
                    ValidateRect(parent.0, null());
                } else {
                    baseline_times.push(now.elapsed());
                }
            }
            let baseline_callbacks = callbacks.get();
            let baseline_damage = update_bounds_area(parent.0);
            // Start the new path with fresh controls so cached state matches
            // the real initial HWND state, as after building a page.
            for child in children.drain(..) {
                DestroyWindow(child.hwnd);
            }
            children.extend((0..160).map(|row| parent.child([96, 96 + 30 * row, 240, 26], false)));
            for child in &children {
                assert_ne!(
                    SetWindowSubclass(
                        child.hwnd,
                        Some(count_moves),
                        1,
                        &callbacks as *const _ as usize
                    ),
                    0
                );
            }
            let mut engine = Scrolling::default();
            let viewport = || Viewport {
                bounds: [84, 80, 700, 440],
                extent: 4820,
                page: 360,
                dpi: 96,
            };
            let mut scroll = 0;
            engine.place(parent.0, &mut scroll, viewport(), &mut children);
            callbacks.set(0);
            ValidateRect(parent.0, null());
            let mut new_times = Vec::new();
            for tick in 1..=32 {
                scroll = tick * 16;
                let now = Instant::now();
                engine.place(parent.0, &mut scroll, viewport(), &mut children);
                new_times.push(now.elapsed());
                let first_full = ((scroll - 16).max(0) + 29) / 30;
                assert_eq!(
                    child_top(parent.0, children[first_full as usize].hwnd),
                    96 + first_full * 30 - scroll
                );
            }
            let new_callbacks = callbacks.get();
            let new_damage = update_bounds_area(parent.0);
            assert!(
                new_callbacks < baseline_callbacks / 4,
                "{new_callbacks} versus {baseline_callbacks}"
            );
            engine.place(parent.0, &mut scroll, viewport(), &mut children);
            assert_eq!(
                callbacks.get(),
                new_callbacks,
                "unchanged layout touches no child"
            );
            baseline_times.sort_unstable();
            new_times.sort_unstable();
            println!(
                "NATIVE_SCROLL_FIXTURE steps=32 controls=160 callbacks_old={baseline_callbacks} callbacks_new={new_callbacks} damage_bounds_old_px={baseline_damage} damage_bounds_new_px={new_damage} median_old_us={} median_new_us={} p95_old_us={} p95_new_us={}",
                (baseline_times[15].as_micros() + baseline_times[16].as_micros()) / 2,
                (new_times[15].as_micros() + new_times[16].as_micros()) / 2,
                baseline_times[30].as_micros(),
                new_times[30].as_micros(),
            );
        }
    }
}

struct WindowMove<'a> {
    child: &'a mut ScrollChild,
    rect: [i32; 4],
    flags: u32,
    visible: bool,
    moved: bool,
}

unsafe fn set_region(hwnd: HWND, region: Option<[i32; 4]>) -> bool {
    unsafe {
        let handle = if let Some([left, top, right, bottom]) = region {
            let handle = CreateRectRgn(left, top, right, bottom);
            if handle.is_null() {
                return false;
            }
            handle
        } else {
            null_mut()
        };
        if SetWindowRgn(hwnd, handle, 0) == 0 {
            if !handle.is_null() {
                DeleteObject(handle);
            }
            return false;
        }
        // Win32 owns the region after a successful SetWindowRgn.
        true
    }
}
