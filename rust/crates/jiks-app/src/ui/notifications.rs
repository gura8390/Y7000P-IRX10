//! Native, explicitly requested notifications. No HWND, timer or worker in `new`.
//! The UI message-loop owner must retain this manager through `close_all`.
//! WndProc only borrows stable boxed state; it never frees that state, including
//! during WM_NCDESTROY. No hardware, settings writes, tray balloon or renderer.
use super::native::{accent, font, ink, line, paper, wide};
use anyhow::{Result, anyhow, bail, ensure};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    marker::PhantomData,
    mem::{size_of, zeroed},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{null, null_mut},
    rc::Rc,
    thread::{self, ThreadId},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::*},
    UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
};

const CLASS: &str = "Jiks.NativeNotification.1";
pub(super) const TYPES: [&str; 43] = jiks_runtime::config_actions::NOTIFICATION_TYPES;
const POSITIONS: [&str; 9] = [
    "BottomRight",
    "BottomCenter",
    "BottomLeft",
    "CenterLeft",
    "TopLeft",
    "TopCenter",
    "TopRight",
    "CenterRight",
    "Center",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Bounds {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}
impl From<RECT> for Bounds {
    fn from(r: RECT) -> Self {
        Self {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
}
impl Bounds {
    fn width(self) -> i32 {
        self.right.saturating_sub(self.left).max(1)
    }
    fn height(self) -> i32 {
        self.bottom.saturating_sub(self.top).max(1)
    }
}
#[derive(Clone, Copy)]
struct Monitor {
    work: Bounds,
    primary: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct Plan {
    kind: usize,
    title: String,
    text: String,
    duration_ms: u64,
    position: usize,
    topmost: bool,
    all_screens: bool,
    foreground: Option<u32>,
    text_color: Option<u32>,
    truncated: bool,
    legacy_icon: bool,
}
enum Decision {
    Suppressed(&'static str),
    Show(Plan),
}

fn index(value: &Value, names: &[&str], what: &str) -> Result<usize> {
    if let Some(n) = value.as_u64().filter(|n| *n < names.len() as u64) {
        return Ok(n as usize);
    }
    if let Some(s) = value.as_str() {
        if let Some(n) = names.iter().position(|name| name.eq_ignore_ascii_case(s)) {
            return Ok(n);
        }
        if let Ok(n) = s.parse::<usize>() {
            if n < names.len() {
                return Ok(n);
            }
        }
    }
    bail!("Invalid notification {what}")
}
fn boolean(root: &Value, key: &str, default: bool) -> Result<bool> {
    match root.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| anyhow!("Invalid notification {key}")),
    }
}
fn bounded_text(s: &str, cap: usize) -> (String, bool) {
    let mut output = String::new();
    let mut chars = s
        .chars()
        .filter(|c| *c != '\0' && (!c.is_control() || *c == '\n' || *c == '\t'));
    for _ in 0..cap {
        let Some(c) = chars.next() else {
            return (output, false);
        };
        output.push(c);
    }
    let truncated = chars.next().is_some();
    if truncated {
        output.push('…');
    }
    (output, truncated)
}
fn category(kind: usize) -> &'static str {
    match kind {
        0..=2 => "ACAdapter",
        3..=4 => "AirplaneMode",
        5 => "AutomationNotification",
        6..=7 => "CameraLock",
        8..=9 => "CapsLock",
        10..=11 => "FnLock",
        12..=13 => "Microphone",
        14..=15 => "NumLock",
        16..=19 | 26..=27 | 30..=32 | 36..=38 => "KeyboardBacklight",
        20..=24 => "PowerMode",
        25 => "RefreshRate",
        28..=29 => "SmartKey",
        33..=34 => "TouchpadLock",
        35 => "UpdateAvailable",
        39..=42 => "ITSMode",
        _ => unreachable!(),
    }
}
fn marker(kind: usize) -> &'static str {
    match category(kind) {
        "ACAdapter" => "AC",
        "AirplaneMode" => "飞行",
        "CameraLock" => "相机",
        "CapsLock" => "Aa",
        "FnLock" => "Fn",
        "Microphone" => "麦克风",
        "NumLock" => "123",
        "KeyboardBacklight" => "灯光",
        "PowerMode" => "性能",
        "RefreshRate" => "Hz",
        "SmartKey" => "★",
        "TouchpadLock" => "触控",
        "UpdateAvailable" => "更新",
        "ITSMode" => "AI",
        _ => "拯救者工具箱",
    }
}
pub(super) fn default_text(kind: usize) -> &'static str {
    match kind {
        0 => "电源适配器已连接",
        1 => "已连接低功率电源适配器",
        2 => "电源适配器已断开",
        3 => "飞行模式已开启",
        4 => "飞行模式已关闭",
        5 => "拯救者工具箱自动化通知",
        6 => "相机已开启",
        7 => "相机已关闭",
        8 => "大写锁定已开启",
        9 => "大写锁定已关闭",
        10 => "Fn 锁定已开启",
        11 => "Fn 锁定已关闭",
        12 => "麦克风已静音",
        13 => "麦克风已取消静音",
        14 => "数字锁定已开启",
        15 => "数字锁定已关闭",
        16 => "标志灯已开启",
        17 => "标志灯已关闭",
        18 => "接口灯已开启",
        19 => "接口灯已关闭",
        20 => "安静模式",
        21 => "均衡模式",
        22 => "野兽模式",
        23 => "超能模式",
        24 => "自定义模式",
        25 => "刷新率已更改",
        26 => "键盘背光已更改",
        27 => "键盘背光已关闭",
        28 => "智能按键：双击",
        29 => "智能按键：单击",
        30 => "键盘背光亮度已更改",
        31 => "键盘背光已关闭",
        32 => "键盘背光配置已更改",
        33 => "触控板已开启",
        34 => "触控板已关闭",
        35 => "有可用更新",
        36 => "键盘背光已更改",
        37 => "键盘背光已更改",
        38 => "键盘背光已关闭",
        39 => "智能模式：自动",
        40 => "智能模式：冷静",
        41 => "智能模式：性能",
        42 => "智能模式：极客",
        _ => unreachable!(),
    }
}
fn per_type<'a>(settings: &'a Value, name: &str, kind: usize) -> Option<&'a Value> {
    let table = settings.get("Notifications")?.get(name)?;
    table
        .get(TYPES[kind])
        .or_else(|| table.get(kind.to_string()))
        .filter(|v| !v.is_null())
}
fn override_value<'a>(
    settings: &'a Value,
    payload: &'a Value,
    key: &str,
    table: &str,
    kind: usize,
) -> Option<&'a Value> {
    payload
        .get("overrides")
        .filter(|v| !v.is_null())
        .or_else(|| payload.get("Overrides"))
        .and_then(|v| v.get(key))
        .filter(|v| !v.is_null())
        .or_else(|| per_type(settings, table, kind))
}
fn color(value: &Value) -> Result<u32> {
    let component = |name: &str| -> Result<u32> {
        value
            .get(name)
            .or_else(|| value.get(name.to_lowercase()))
            .and_then(Value::as_u64)
            .filter(|v| *v <= 255)
            .map(|v| v as u32)
            .ok_or_else(|| anyhow!("Invalid notification RGB color"))
    };
    Ok(component("R")? | component("G")? << 8 | component("B")? << 16)
}
fn plan(settings: &Value, payload: &Value) -> Result<Decision> {
    ensure!(
        settings.is_object() || settings.is_null(),
        "Invalid notification settings"
    );
    ensure!(payload.is_object(), "Invalid notification payload");
    ensure!(
        settings
            .get("Notifications")
            .is_none_or(|v| v.is_object() || v.is_null()),
        "Invalid notification categories"
    );
    for key in ["overrides", "Overrides"] {
        ensure!(
            payload
                .get(key)
                .is_none_or(|v| v.is_object() || v.is_null()),
            "Invalid notification overrides"
        );
    }
    for key in [
        "IconOverrides",
        "ColorOverrides",
        "TextColorOverrides",
        "PositionOverrides",
        "DurationOverrides",
    ] {
        ensure!(
            settings
                .get("Notifications")
                .and_then(|n| n.get(key))
                .is_none_or(|v| v.is_object() || v.is_null()),
            "Invalid notification {key}"
        );
    }
    for key in ["title", "text"] {
        ensure!(
            payload
                .get(key)
                .is_none_or(|v| v.is_string() || v.is_null()),
            "Invalid notification {key}"
        );
    }
    if boolean(settings, "DontShowNotifications", false)? {
        return Ok(Decision::Suppressed("通知已在设置中关闭"));
    }
    let kind = payload
        .get("type")
        .or_else(|| payload.get("Type"))
        .map(|v| index(v, &TYPES, "type"))
        .transpose()?
        .unwrap_or(5);
    let key = category(kind);
    let enabled = settings
        .get("Notifications")
        .map(|n| boolean(n, key, !matches!(key, "ACAdapter" | "SmartKey")))
        .transpose()?
        .unwrap_or(!matches!(key, "ACAdapter" | "SmartKey"));
    if !enabled {
        return Ok(Decision::Suppressed("该类别的通知已关闭"));
    }
    let duration = override_value(
        settings,
        payload,
        "DurationOverride",
        "DurationOverrides",
        kind,
    )
    .or_else(|| settings.get("NotificationDuration"))
    .filter(|v| !v.is_null())
    .map(|v| index(v, &["Short", "Normal", "Long"], "duration"))
    .transpose()?
    .unwrap_or(1);
    let duration_ms = if kind == 25 {
        [2000, 3500, 5000][duration]
    } else {
        [500, 1000, 2500][duration]
    };
    let position = override_value(
        settings,
        payload,
        "PositionOverride",
        "PositionOverrides",
        kind,
    )
    .or_else(|| settings.get("NotificationPosition"))
    .filter(|v| !v.is_null())
    .map(|v| index(v, &POSITIONS, "position"))
    .transpose()?
    .unwrap_or(1);
    let (title, tt) = bounded_text(
        payload.get("title").and_then(Value::as_str).unwrap_or(""),
        128,
    );
    let (text, bt) = bounded_text(
        payload
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or(default_text(kind)),
        2048,
    );
    ensure!(
        !text.trim().is_empty() || !title.trim().is_empty(),
        "Notification is empty"
    );
    let legacy_icon = override_value(settings, payload, "IconOverride", "IconOverrides", kind);
    ensure!(
        legacy_icon.is_none_or(|v| v.as_u64().is_some_and(|n| n <= 0x10ffff)),
        "Invalid legacy notification icon"
    );
    Ok(Decision::Show(Plan {
        kind,
        title,
        text,
        duration_ms,
        position,
        topmost: boolean(settings, "NotificationAlwaysOnTop", false)?,
        all_screens: boolean(settings, "NotificationOnAllScreens", false)?,
        foreground: override_value(settings, payload, "ColorOverride", "ColorOverrides", kind)
            .map(color)
            .transpose()?
            .or(match kind {
                20 | 39 => Some(0x00f27b35),
                22 | 41 => Some(0x003333d4),
                23 | 24 | 42 => Some(0x00e33463),
                _ => None,
            }),
        text_color: override_value(
            settings,
            payload,
            "TextColorOverride",
            "TextColorOverrides",
            kind,
        )
        .map(color)
        .transpose()?,
        truncated: tt || bt,
        // WPF SymbolRegular indexes are not Segoe Fluent Icons codepoints. Never
        // silently reinterpret them as a different icon on native Windows.
        legacy_icon: legacy_icon.is_some(),
    }))
}
fn location(work: Bounds, width: i32, height: i32, dpi: u32, position: usize) -> Bounds {
    let width = width.max(1).min(work.width());
    let height = height.max(1).min(work.height());
    let margin = (16i64 * i64::from(dpi) / 96).clamp(0, i64::from(i32::MAX)) as i32;
    let mx = margin.min((work.width() - width).max(0));
    let my = margin.min((work.height() - height).max(0));
    let (col, row) = match position {
        0 => (2, 2),
        1 => (1, 2),
        2 => (0, 2),
        3 => (0, 1),
        4 => (0, 0),
        5 => (1, 0),
        6 => (2, 0),
        7 => (2, 1),
        _ => (1, 1),
    };
    let left = match col {
        0 => work.left + mx,
        2 => work.right - width - mx,
        _ => work.left + (work.width() - width) / 2,
    };
    let top = match row {
        0 => work.top + my,
        2 => work.bottom - height - my,
        _ => work.top + (work.height() - height) / 2,
    };
    Bounds {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

/// Confined to the creating UI thread, including Drop; never Send or Sync.
pub(super) struct Notifications {
    popups: Vec<Popup>,
    clicked: Option<String>,
    owner: ThreadId,
    _thread: PhantomData<Rc<()>>,
}
impl Notifications {
    pub(super) fn new() -> Self {
        Self {
            popups: vec![],
            clicked: None,
            owner: thread::current().id(),
            _thread: PhantomData,
        }
    }
    fn check_thread(&self) -> Result<()> {
        ensure!(
            thread::current().id() == self.owner,
            "Notifications must run on their owning UI thread"
        );
        Ok(())
    }
    pub(super) fn show(
        &mut self,
        _parent: HWND,
        settings: &Value,
        payload: &Value,
    ) -> Result<String> {
        self.check_thread()?;
        let plan = match plan(settings, payload)? {
            Decision::Suppressed(reason) => return Ok(format!("已抑制：{reason}")),
            Decision::Show(plan) => plan,
        };
        if !plan.topmost && unsafe { foreground_fullscreen() } {
            return Ok("已抑制：前台程序正在全屏显示".into());
        }
        let monitors = unsafe { monitors() }?;
        let selected = monitors
            .iter()
            .filter(|m| plan.all_screens || m.primary)
            .collect::<Vec<_>>();
        ensure!(!selected.is_empty(), "没有可显示通知的屏幕工作区");
        self.close_all()?;
        let mut clipped = plan.truncated;
        // Preserve every box until successful destruction, including partial
        // multi-screen failure. Creation and destruction run without a borrow.
        for monitor in selected {
            // Retain the owner before native creation: if initialization and
            // cleanup both fail, close_all can retry rather than orphan it.
            self.popups.push(Popup::new(&plan));
            let popup = self.popups.last_mut().expect("just pushed");
            if let Err(error) = unsafe { popup.create(*monitor, &plan, true) } {
                let cleanup = self.close_all();
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => {
                        Err(error.context(format!("Notification cleanup still pending: {cleanup}")))
                    }
                };
            }
            clipped |= popup.state.borrow().clipped;
        }
        let mut message = format!(
            "已显示 {} 个屏幕，{} 毫秒；不抢占焦点",
            self.popups.len(),
            plan.duration_ms
        );
        if clipped {
            message.push_str("；超长内容已截断")
        }
        if plan.legacy_icon {
            message.push_str("；原版自定义字形不兼容，已使用类别标记")
        }
        Ok(message)
    }
    /// Called by the existing message-loop tick; no private timer or thread.
    pub(super) fn tick(&mut self) -> Result<()> {
        self.check_thread()?;
        let now = Instant::now();
        let mut first_error = None;
        for popup in &mut self.popups {
            let close = {
                let s = popup.state.borrow();
                popup.state.destroyed.get() || s.dismiss || now >= s.expires
            };
            if close {
                if let Some(action) = popup.state.borrow_mut().clicked.take() {
                    self.clicked = Some(action)
                }
                if let Err(error) = popup.close() {
                    first_error.get_or_insert(error);
                }
            }
        }
        self.popups.retain(|popup| !popup.hwnd.is_null());
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }
    pub(super) fn close_all(&mut self) -> Result<()> {
        self.check_thread()?;
        let mut first_error = None;
        for popup in &mut self.popups {
            if let Err(error) = popup.close() {
                first_error.get_or_insert(error);
            }
        }
        self.popups.retain(|popup| !popup.hwnd.is_null());
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }
    pub(super) fn is_active(&self) -> bool {
        !self.popups.is_empty()
    }
    pub(super) fn take_clicked(&mut self) -> Option<String> {
        self.clicked.take()
    }
}
impl Drop for Notifications {
    fn drop(&mut self) {
        let _ = self.close_all();
    }
}

struct Popup {
    hwnd: HWND,
    state: Box<PopupState>,
}
struct PopupState {
    state: RefCell<State>,
    destroyed: Cell<bool>,
}
impl PopupState {
    fn borrow(&self) -> std::cell::Ref<'_, State> {
        self.state.borrow()
    }
    fn borrow_mut(&self) -> std::cell::RefMut<'_, State> {
        self.state.borrow_mut()
    }
}
struct State {
    title: Vec<u16>,
    text: Vec<u16>,
    marker: Vec<u16>,
    body_font: HFONT,
    title_font: HFONT,
    marker_font: HFONT,
    dpi: u32,
    title_height: i32,
    paper: u32,
    ink: u32,
    accent: u32,
    line: u32,
    expires: Instant,
    dismiss: bool,
    clicked: Option<String>,
    click_action: Option<String>,
    clipped: bool,
}
impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            for font in [self.body_font, self.title_font, self.marker_font] {
                if !font.is_null() {
                    DeleteObject(font);
                }
            }
        }
    }
}
impl Popup {
    fn new(plan: &Plan) -> Self {
        Self {
            hwnd: null_mut(),
            state: Box::new(PopupState {
                destroyed: Cell::new(false),
                state: RefCell::new(State {
                    title: wide(&plan.title),
                    text: wide(&plan.text),
                    marker: wide(marker(plan.kind)),
                    body_font: null_mut(),
                    title_font: null_mut(),
                    marker_font: null_mut(),
                    dpi: 96,
                    title_height: 0,
                    paper: paper(),
                    ink: plan.text_color.unwrap_or_else(ink),
                    accent: plan.foreground.unwrap_or_else(accent),
                    line: line(),
                    expires: Instant::now() + Duration::from_millis(plan.duration_ms),
                    dismiss: false,
                    clicked: None,
                    click_action: (plan.kind == 35).then(|| "update.available".into()),
                    clipped: plan.truncated,
                }),
            }),
        }
    }
    unsafe fn create(&mut self, monitor: Monitor, plan: &Plan, visible: bool) -> Result<()> {
        unsafe {
            register()?;
            let pointer = self.state.as_ref() as *const PopupState;
            let caption = if plan.title.is_empty() {
                plan.text.clone()
            } else {
                format!("{}：{}", plan.title, plan.text)
            };
            self.hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | if plan.topmost { WS_EX_TOPMOST } else { 0 },
                wide(CLASS).as_ptr(),
                wide(&caption).as_ptr(),
                WS_POPUP,
                monitor.work.left,
                monitor.work.top,
                300,
                80,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                pointer as *const _,
            );
            ensure!(
                !self.hwnd.is_null(),
                "Cannot create notification: {}",
                std::io::Error::last_os_error()
            );
            let dpi = GetDpiForWindow(self.hwnd).max(96);
            let scale = |v: i32| v.saturating_mul(dpi as i32) / 96;
            let width = scale(600)
                .min(monitor.work.width().saturating_sub(scale(32)))
                .max(1);
            let text_width = (width - scale(96)).max(1);
            let dc = GetDC(self.hwnd);
            ensure!(!dc.is_null(), "Cannot measure notification");
            let measured = (|| -> Result<(i32, i32)> {
                let mut state = self.state.borrow_mut();
                state.dpi = dpi;
                state.body_font = font(dpi, 16, false);
                state.title_font = font(dpi, 16, true);
                state.marker_font = font(dpi, 18, true);
                ensure!(
                    !state.body_font.is_null()
                        && !state.title_font.is_null()
                        && !state.marker_font.is_null(),
                    "Cannot create notification font"
                );
                let old = SelectObject(dc, state.body_font);
                let mut body = RECT {
                    left: 0,
                    top: 0,
                    right: text_width,
                    bottom: 0,
                };
                DrawTextW(
                    dc,
                    state.text.as_mut_ptr(),
                    -1,
                    &mut body,
                    DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
                );
                SelectObject(dc, state.title_font);
                let mut title = RECT {
                    left: 0,
                    top: 0,
                    right: text_width,
                    bottom: 0,
                };
                if !plan.title.is_empty() {
                    DrawTextW(
                        dc,
                        state.title.as_mut_ptr(),
                        -1,
                        &mut title,
                        DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
                    );
                }
                SelectObject(dc, old);
                state.title_height = title.bottom;
                let content_height =
                    body.bottom + title.bottom + if title.bottom > 0 { scale(4) } else { 0 };
                let max_height = scale(280)
                    .min(monitor.work.height().saturating_sub(scale(32)))
                    .max(1);
                let height = (content_height + scale(32)).max(scale(72)).min(max_height);
                state.clipped |= content_height + scale(32) > height;
                let measured_width = (body.right.max(title.right) + scale(96))
                    .max(scale(300))
                    .min(width);
                Ok((measured_width, height))
            })();
            ReleaseDC(self.hwnd, dc);
            let (width, height) = measured?;
            let rect = location(monitor.work, width, height, dpi, plan.position);
            ensure!(
                SetWindowPos(
                    self.hwnd,
                    if plan.topmost { HWND_TOPMOST } else { HWND_TOP },
                    rect.left,
                    rect.top,
                    rect.width(),
                    rect.height(),
                    SWP_NOACTIVATE | if visible { SWP_SHOWWINDOW } else { 0 }
                ) != 0,
                "Cannot position notification: {}",
                std::io::Error::last_os_error()
            );
            // Start duration when the native window has been shown, not at parsing.
            let mut actual: RECT = zeroed();
            ensure!(
                GetWindowRect(self.hwnd, &mut actual) != 0
                    && Bounds::from(actual) == rect
                    && (IsWindowVisible(self.hwnd) != 0) == visible,
                "Notification geometry or visibility readback failed"
            );
            self.state.borrow_mut().expires =
                Instant::now() + Duration::from_millis(plan.duration_ms);
            Ok(())
        }
    }
    fn close(&mut self) -> Result<()> {
        self.close_with(
            |hwnd| unsafe { IsWindow(hwnd) != 0 },
            |hwnd| unsafe { DestroyWindow(hwnd) != 0 },
        )
    }
    fn close_with(
        &mut self,
        exists: impl FnOnce(HWND) -> bool,
        destroy: impl FnOnce(HWND) -> bool,
    ) -> Result<()> {
        if self.hwnd.is_null() {
            return Ok(());
        }
        if !self.state.destroyed.get() && exists(self.hwnd) {
            ensure!(
                destroy(self.hwnd),
                "Cannot close notification: {}",
                std::io::Error::last_os_error()
            );
        }
        self.hwnd = null_mut();
        Ok(())
    }
}
impl Drop for Popup {
    fn drop(&mut self) {
        if self.close().is_err() {
            // A live HWND must never retain a dangling callback pointer. This is an
            // exceptional fail-closed leak, not a false successful cleanup.
            let state = std::mem::replace(
                &mut self.state,
                Box::new(PopupState {
                    destroyed: Cell::new(true),
                    state: RefCell::new(State {
                        title: vec![],
                        text: vec![],
                        marker: vec![],
                        body_font: null_mut(),
                        title_font: null_mut(),
                        marker_font: null_mut(),
                        dpi: 96,
                        title_height: 0,
                        paper: 0,
                        ink: 0,
                        accent: 0,
                        line: 0,
                        expires: Instant::now(),
                        dismiss: true,
                        clicked: None,
                        click_action: None,
                        clipped: false,
                    }),
                }),
            );
            std::mem::forget(state);
        }
    }
}
unsafe fn register() -> Result<()> {
    unsafe {
        let class = wide(CLASS);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: GetModuleHandleW(null()),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        ensure!(
            RegisterClassW(&wc) != 0 || GetLastError() == ERROR_CLASS_ALREADY_EXISTS,
            "Cannot register notification window"
        );
        Ok(())
    }
}
unsafe extern "system" fn procedure(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    catch_unwind(AssertUnwindSafe(|| unsafe {
        if msg == WM_NCCREATE {
            let pointer = (*(lp as *const CREATESTRUCTW)).lpCreateParams as *const PopupState;
            if pointer.is_null() {
                return 0;
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer as isize);
            return 1;
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const PopupState;
        // Always clear native ownership, even if another message is borrowing.
        if msg == WM_NCDESTROY {
            if !pointer.is_null() {
                (*pointer).destroyed.set(true);
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        if pointer.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        match msg {
            WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
            WM_ERASEBKGND => 1,
            // A display/DPI topology transition invalidates the measured
            // work area. Retire this short-lived toast; the next request will
            // enumerate and measure afresh instead of retaining stale bounds.
            WM_DISPLAYCHANGE | WM_DPICHANGED => {
                if let Ok(mut state) = (*pointer).state.try_borrow_mut() {
                    state.dismiss = true;
                    0
                } else {
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
            }
            WM_CLOSE | WM_LBUTTONUP | WM_RBUTTONUP => {
                if let Ok(mut state) = (*pointer).state.try_borrow_mut() {
                    state.dismiss = true;
                    if msg == WM_LBUTTONUP {
                        state.clicked = state.click_action.clone();
                    }
                    0
                } else {
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
            }
            WM_PAINT => {
                let Ok(mut state) = (*pointer).state.try_borrow_mut() else {
                    return DefWindowProcW(hwnd, msg, wp, lp);
                };
                let mut paint: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(hwnd, &mut paint);
                if !dc.is_null() {
                    state.paint(hwnd, dc);
                }
                EndPaint(hwnd, &paint);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }))
    .unwrap_or_else(|_| unsafe { DefWindowProcW(hwnd, msg, wp, lp) })
}
impl State {
    unsafe fn paint(&mut self, hwnd: HWND, dc: HDC) {
        unsafe {
            let mut bounds: RECT = zeroed();
            GetClientRect(hwnd, &mut bounds);
            let brush = CreateSolidBrush(self.paper);
            FillRect(dc, &bounds, brush);
            DeleteObject(brush);
            let border = CreateSolidBrush(self.line);
            FrameRect(dc, &bounds, border);
            DeleteObject(border);
            let scale = |v: i32| v.saturating_mul(self.dpi as i32) / 96;
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, self.accent);
            let old = SelectObject(dc, self.marker_font);
            let mut mark = RECT {
                left: scale(12),
                top: scale(16),
                right: scale(68),
                bottom: bounds.bottom - scale(16),
            };
            DrawTextW(
                dc,
                self.marker.as_mut_ptr(),
                -1,
                &mut mark,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            SetTextColor(dc, self.ink);
            SelectObject(dc, self.title_font);
            let left = scale(80);
            let right = (bounds.right - scale(16)).max(left + 1);
            let mut title = RECT {
                left,
                top: scale(16),
                right,
                bottom: (scale(16) + self.title_height).min(bounds.bottom - scale(16)),
            };
            if self.title_height > 0 {
                DrawTextW(
                    dc,
                    self.title.as_mut_ptr(),
                    -1,
                    &mut title,
                    DT_WORDBREAK | DT_NOPREFIX | DT_END_ELLIPSIS,
                );
            }
            SelectObject(dc, self.body_font);
            let mut body = RECT {
                left,
                top: scale(16)
                    + self.title_height
                    + if self.title_height > 0 { scale(4) } else { 0 },
                right,
                bottom: bounds.bottom - scale(16),
            };
            if self.title_height == 0 {
                let mut measure = body;
                DrawTextW(
                    dc,
                    self.text.as_mut_ptr(),
                    -1,
                    &mut measure,
                    DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
                );
                body.top += (body.bottom - body.top - (measure.bottom - measure.top)).max(0) / 2;
            }
            if body.bottom > body.top {
                DrawTextW(
                    dc,
                    self.text.as_mut_ptr(),
                    -1,
                    &mut body,
                    DT_WORDBREAK | DT_NOPREFIX | DT_END_ELLIPSIS,
                );
            }
            SelectObject(dc, old);
        }
    }
}
unsafe fn monitors() -> Result<Vec<Monitor>> {
    unsafe {
        struct Inventory {
            items: Vec<Monitor>,
            failed: bool,
        }
        unsafe extern "system" fn visit(
            monitor: HMONITOR,
            _: HDC,
            _: *mut RECT,
            param: LPARAM,
        ) -> i32 {
            let inventory = unsafe { &mut *(param as *mut Inventory) };
            let result = catch_unwind(AssertUnwindSafe(|| unsafe {
                let mut info: MONITORINFO = zeroed();
                info.cbSize = size_of::<MONITORINFO>() as u32;
                if GetMonitorInfoW(monitor, &mut info) == 0 || inventory.items.len() >= 64 {
                    inventory.failed = true;
                    return 0;
                }
                if info.rcWork.right <= info.rcWork.left || info.rcWork.bottom <= info.rcWork.top {
                    inventory.failed = true;
                    return 0;
                }
                inventory.items.push(Monitor {
                    work: info.rcWork.into(),
                    primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
                });
                1
            }));
            result.unwrap_or_else(|_| {
                inventory.failed = true;
                0
            })
        }
        let mut inventory = Inventory {
            items: vec![],
            failed: false,
        };
        let ok = EnumDisplayMonitors(
            null_mut(),
            null(),
            Some(visit),
            &mut inventory as *mut Inventory as isize,
        );
        ensure!(
            ok != 0 && !inventory.failed,
            "Cannot enumerate notification screens"
        );
        Ok(inventory.items)
    }
}
unsafe fn foreground_fullscreen() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() || hwnd == GetShellWindow() || hwnd == GetDesktopWindow() {
            return false;
        }
        let mut rect: RECT = zeroed();
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return false;
        }
        let mut info: MONITORINFO = zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info) == 0 {
            return false;
        }
        if rect.left != info.rcMonitor.left
            || rect.top != info.rcMonitor.top
            || rect.right != info.rcMonitor.right
            || rect.bottom != info.rcMonitor.bottom
        {
            return false;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !process.is_null() {
            let mut path = [0u16; 1024];
            let mut size = path.len() as u32;
            let ok = QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut size);
            CloseHandle(process);
            if ok != 0
                && String::from_utf16_lossy(&path[..size as usize])
                    .rsplit('\\')
                    .next()
                    .is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe"))
            {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn shown(settings: Value, payload: Value) -> Plan {
        match plan(&settings, &payload).unwrap() {
            Decision::Show(p) => p,
            Decision::Suppressed(_) => panic!("suppressed"),
        }
    }
    #[test]
    fn construction_is_inert_and_default_contract_matches_original() {
        let manager = Notifications::new();
        assert!(!manager.is_active());
        let p = shown(json!({}), json!({"text":"测试"}));
        assert_eq!(
            (p.kind, p.position, p.duration_ms, p.topmost, p.all_screens),
            (5, 1, 1000, false, false)
        );
    }
    #[test]
    fn all_43_types_map_to_their_original_gate() {
        for (kind, name) in TYPES.iter().enumerate() {
            let mut settings = json!({"Notifications":{}});
            settings["Notifications"][category(kind)] = json!(false);
            assert!(matches!(
                plan(&settings, &json!({"type":name})).unwrap(),
                Decision::Suppressed(_)
            ));
            assert_eq!(index(&json!(kind), &TYPES, "type").unwrap(), kind);
        }
    }
    #[test]
    fn global_and_default_category_suppression() {
        assert!(matches!(
            plan(&json!({"DontShowNotifications":true}), &json!({})).unwrap(),
            Decision::Suppressed(_)
        ));
        for kind in [0, 1, 2, 28, 29] {
            assert!(matches!(
                plan(&json!({}), &json!({"type":kind})).unwrap(),
                Decision::Suppressed(_)
            ));
        }
    }
    #[test]
    fn position_and_duration_precedence_accept_legacy_names_and_numbers() {
        let settings = json!({"NotificationPosition":0,"NotificationDuration":2,"Notifications":{"PositionOverrides":{"AutomationNotification":"TopLeft"},"DurationOverrides":{"5":0}}});
        let p = shown(settings.clone(), json!({}));
        assert_eq!((p.position, p.duration_ms), (4, 500));
        let p = shown(
            settings,
            json!({"overrides":{"PositionOverride":8,"DurationOverride":"Normal"}}),
        );
        assert_eq!((p.position, p.duration_ms), (8, 1000));
    }
    #[test]
    fn refresh_rate_retains_longer_original_duration_table() {
        for (duration, expected) in [2000, 3500, 5000].into_iter().enumerate() {
            assert_eq!(
                shown(
                    json!({"NotificationDuration":duration}),
                    json!({"type":"RefreshRate"})
                )
                .duration_ms,
                expected
            );
        }
    }
    #[test]
    fn rgb_and_text_overrides_are_preserved_without_wpf_glyph_misinterpretation() {
        let p = shown(
            json!({"Notifications":{"ColorOverrides":{"5":{"R":1,"G":2,"B":3}},"TextColorOverrides":{"5":{"R":4,"G":5,"B":6}},"IconOverrides":{"5":1234}}}),
            json!({}),
        );
        assert_eq!(p.foreground, Some(0x030201));
        assert_eq!(p.text_color, Some(0x060504));
        assert!(p.legacy_icon);
    }
    #[test]
    fn invalid_settings_do_not_silently_turn_into_success() {
        for settings in [
            json!({"NotificationDuration":4}),
            json!({"NotificationPosition":9}),
            json!({"NotificationAlwaysOnTop":"true"}),
            json!({"Notifications":{"AutomationNotification":1}}),
            json!({"Notifications":[]}),
            json!({"Notifications":{"DurationOverrides":[]}}),
        ] {
            assert!(plan(&settings, &json!({})).is_err());
        }
        assert!(plan(&json!({}), &json!({"type":"missing"})).is_err());
        assert!(plan(&json!({}), &json!({"text":false})).is_err());
        assert!(plan(&json!({}), &json!({"overrides":{"IconOverride":-1}})).is_err());
        assert!(color(&json!({"R":256,"G":0,"B":0})).is_err());
    }
    #[test]
    fn text_is_bounded_on_unicode_boundaries_and_control_characters_removed() {
        let p = shown(
            json!({}),
            json!({"title":"🙂".repeat(130),"text":format!("a\u{0}b\u{7}{}","测".repeat(3000))}),
        );
        assert!(p.truncated);
        assert_eq!(p.title.chars().count(), 129);
        assert_eq!(p.text.chars().count(), 2049);
        assert!(p.text.starts_with("ab"));
    }
    #[test]
    fn all_nine_positions_include_negative_monitor_origins() {
        let work = Bounds {
            left: -1920,
            top: -200,
            right: 0,
            bottom: 880,
        };
        let expected = [
            (-316, 784),
            (-1110, 784),
            (-1904, 784),
            (-1904, 300),
            (-1904, -184),
            (-1110, -184),
            (-316, -184),
            (-316, 300),
            (-1110, 300),
        ];
        for (position, (left, top)) in expected.into_iter().enumerate() {
            let r = location(work, 300, 80, 96, position);
            assert_eq!((r.left, r.top), (left, top));
        }
    }
    #[test]
    fn layout_clamps_to_tiny_work_area_and_uses_dpi_scaled_margins() {
        let work = Bounds {
            left: 100,
            top: 200,
            right: 200,
            bottom: 260,
        };
        assert_eq!(location(work, 300, 80, 144, 0), work);
        let r = location(
            Bounds {
                left: 0,
                top: 0,
                right: 1000,
                bottom: 800,
            },
            300,
            80,
            144,
            6,
        );
        assert_eq!((r.left, r.top), (676, 24));
    }
    #[test]
    fn nc_destroy_flag_remains_available_during_a_paint_state_borrow() {
        let popup = Popup::new(&shown(json!({}), json!({"text":"retained"})));
        let borrow = popup.state.borrow_mut();
        assert!(popup.state.state.try_borrow_mut().is_err());
        // WM_NCDESTROY writes this independent Cell before default handling,
        // even if reentry prevents borrowing render state. It does not free it.
        popup.state.destroyed.set(true);
        assert!(popup.state.destroyed.get());
        drop(borrow);
        assert_eq!(
            String::from_utf16_lossy(&popup.state.borrow().text[..8]),
            "retained"
        );
    }
    #[test]
    fn failed_destroy_retains_exact_owner_for_retry_and_does_not_ack_cleanup() {
        let mut popup = Popup::new(&shown(json!({}), json!({})));
        popup.hwnd = 123usize as HWND; // passed only to injected pure closures
        let identity = popup.state.as_ref() as *const PopupState;
        assert!(popup.close_with(|_| true, |_| false).is_err());
        assert!(!popup.hwnd.is_null());
        assert!(std::ptr::eq(identity, popup.state.as_ref()));
        assert!(popup.close_with(|_| true, |_| true).is_ok());
        assert!(popup.hwnd.is_null());
    }
    #[test]
    fn destroyed_generation_never_closes_a_reused_native_handle() {
        let mut popup = Popup::new(&shown(json!({}), json!({})));
        popup.hwnd = 123usize as HWND;
        popup.state.destroyed.set(true);
        popup
            .close_with(
                |_| panic!("must not inspect reused HWND"),
                |_| panic!("must not destroy another generation"),
            )
            .unwrap();
        assert!(popup.hwnd.is_null());
    }
    #[test]
    fn hidden_native_window_reentry_destroys_hwnd_without_freeing_borrowed_state() {
        let plan = shown(json!({}), json!({"text":"隔离生命周期：从不显示"}));
        let mut popup = Popup::new(&plan);
        unsafe {
            popup
                .create(
                    Monitor {
                        work: Bounds {
                            left: 0,
                            top: 0,
                            right: 800,
                            bottom: 600,
                        },
                        primary: true,
                    },
                    &plan,
                    false,
                )
                .unwrap();
        }
        let hwnd = popup.hwnd;
        assert!(!hwnd.is_null());
        assert_eq!(unsafe { IsWindowVisible(hwnd) }, 0);
        let borrow = popup.state.borrow_mut();
        // This message is sent only to the known, invisible fixture HWND.
        // Its failed nested borrow must use default WM_CLOSE processing;
        // WM_NCDESTROY still marks the independent generation cell.
        unsafe {
            SendMessageW(hwnd, WM_CLOSE, 0, 0);
        }
        assert!(popup.state.destroyed.get());
        assert_eq!(unsafe { IsWindow(hwnd) }, 0);
        assert!(!borrow.body_font.is_null());
        drop(borrow);
        popup.close().unwrap();
        assert!(popup.hwnd.is_null());
    }
}
