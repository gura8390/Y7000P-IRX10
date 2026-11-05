//! A live, non-activating status view. The parent owns visibility subscriptions
//! and calls sync on its UI tick. This module only presents completed UI data.
use super::{
    a11y, i18n,
    native::{accent, font, ink, line, paper, wide},
};
use crate::ui_model::UiModel;
use anyhow::{Result, ensure};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
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
    System::{LibraryLoader::GetModuleHandleW, SystemInformation::GetTickCount64},
    UI::{Controls::*, WindowsAndMessaging::*},
};

const CLASS: &str = "Jiks.StatusPopup.1";
const CLOSE: u16 = 2;
const FRESH_MS: u64 = 3000;
const HIDE_DELAY: Duration = Duration::from_millis(350);

fn small_revision(model: &UiModel) -> u64 {
    let mut hash = DefaultHasher::new();
    model.machine.hash(&mut hash);
    model.version.hash(&mut hash);
    for key in [
        "power.mode",
        "preset.active_name",
        "gpu.name",
        "gpu.state",
        "gpu.state_at_tick_ms",
        "gpu.current_pstate",
        "gpu.current_pstate_at_tick_ms",
        "gpu.keep_awake.active",
        "battery.mode",
        "battery.capacity",
        "battery.capacity_unit",
        "battery.full_charged_capacity",
        "battery.designed_capacity",
        "battery.health_percent",
        "battery.cycle_count",
        "battery.rate",
        "battery.rate_unit",
        "battery.minimum_rate_w",
        "battery.maximum_rate_w",
        "battery.on_battery_since",
        "battery.manufacture_date",
        "battery.first_use_date",
        "update.available",
    ] {
        key.hash(&mut hash);
        match model.values.get(key) {
            Some(Value::String(value)) => value.hash(&mut hash),
            Some(Value::Number(value)) => value.to_string().hash(&mut hash),
            Some(Value::Bool(value)) => value.hash(&mut hash),
            _ => 0u8.hash(&mut hash),
        }
    }
    hash.finish()
}

fn label(chinese: &'static str, english: &'static str) -> &'static str {
    let translated = i18n::tr_static(chinese);
    if i18n::language() == "en" && translated == chinese {
        english
    } else {
        translated
    }
}
fn clean(text: &str, limit: usize) -> String {
    let mut chars = text.chars().filter(|c| !c.is_control());
    let mut text: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        text.push('…');
    }
    text
}
fn scalar(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) if !text.is_empty() => clean(text, 512),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(on)) => on.to_string(),
        _ => "—".into(),
    }
}
fn fresh(model: &UiModel, key: &str, now: u64) -> bool {
    model
        .values
        .get(key)
        .and_then(Value::as_u64)
        .is_some_and(|tick| tick <= now && now - tick <= FRESH_MS)
}
fn power_mode(value: Option<&Value>) -> &'static str {
    let raw = raw_mode(value);
    match raw {
        Some(1) => label("安静模式", "Quiet"),
        Some(2) => label("均衡模式", "Balanced"),
        Some(3) => label("野兽模式", "Performance"),
        Some(224) => label("超能模式", "Extreme"),
        Some(255) => label("自定义模式", "Custom"),
        _ => "—",
    }
}
fn raw_mode(value: Option<&Value>) -> Option<u64> {
    value.and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
}
fn actual_pstate(model: &UiModel, now: u64) -> String {
    if !fresh(model, "gpu.current_pstate_at_tick_ms", now) {
        return "—".into();
    }
    let value = model.values.get("gpu.current_pstate");
    let id = value.and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str()?.strip_prefix('P')?.parse().ok())
    });
    id.filter(|id| *id < 16)
        .map(|id| format!("P{id}"))
        .unwrap_or_else(|| "—".into())
}
fn row(output: &mut String, title: &str, value: impl AsRef<str>) {
    output.push_str(title);
    output.push_str(": ");
    output.push_str(value.as_ref());
    output.push_str("\r\n");
}
fn heading(output: &mut String, chinese: &'static str, english: &'static str) {
    output.push_str("\r\n");
    output.push_str(label(chinese, english));
    output.push_str("\r\n");
    output.push_str("────────────────────────\r\n");
}
fn metrics(output: &mut String, model: &UiModel, prefix: &str) {
    let mut count = 0;
    for metric in model
        .metrics
        .iter()
        .filter(|m| m.key.to_ascii_lowercase().starts_with(prefix))
    {
        row(
            output,
            &i18n::tr(&metric.label),
            if metric.value.is_empty() {
                "—".into()
            } else {
                clean(&metric.value, 512)
            },
        );
        if !metric.detail.is_empty() {
            output.push_str("  ");
            output.push_str(&clean(&metric.detail, 768));
            output.push_str("\r\n");
        }
        count += 1;
    }
    if count == 0 {
        output.push_str(label("暂无可用传感器数据", "No available sensor readings"));
        output.push_str("\r\n");
    }
}
/// Pure formatting. Device names, preset names, values and source strings are
/// never translated. A requested/locked P-state is deliberately not consulted.
fn format_status(model: &UiModel, now: u64) -> String {
    let get = |key: &str| scalar(model.values.get(key));
    let mut output = format!(
        "{} · {}\r\n",
        clean(&model.machine, 256),
        clean(&model.version, 64)
    );
    row(
        &mut output,
        label("性能模式", "Power mode"),
        power_mode(model.values.get("power.mode")),
    );
    // A saved preset selection is not active while a different firmware mode
    // is current. The original StatusWindow shows this only in GodMode.
    if raw_mode(model.values.get("power.mode")) == Some(255) {
        row(
            &mut output,
            label("活动预设", "Active preset"),
            get("preset.active_name"),
        );
    }
    heading(&mut output, "独显状态", "Discrete GPU status");
    row(&mut output, label("显卡", "GPU"), get("gpu.name"));
    let state: String = if fresh(model, "gpu.state_at_tick_ms", now) {
        match model.values.get("gpu.state").and_then(Value::as_str) {
            Some("PoweredOff") => label("已断电", "Powered off").into(),
            Some("Active") => label("正在使用", "Active").into(),
            Some("Inactive") => label("空闲", "Inactive").into(),
            Some("MonitorConnected") => label(
                "显示器已连接或独显直连",
                "Display connected or discrete mode",
            )
            .into(),
            _ => "—".into(),
        }
    } else {
        "—".into()
    };
    row(&mut output, label("实际状态", "Observed state"), state);
    row(
        &mut output,
        label("实际 P-state", "Observed P-state"),
        actual_pstate(model, now),
    );
    let active = match model
        .values
        .get("gpu.keep_awake.active")
        .and_then(Value::as_bool)
    {
        Some(true) => label("已启用", "Active"),
        Some(false) => label("未启用", "Inactive"),
        None => "—",
    };
    row(
        &mut output,
        label("保持唤醒实际会话", "Keep-awake session"),
        active,
    );
    heading(&mut output, "CPU", "CPU");
    metrics(&mut output, model, "cpu");
    heading(&mut output, "GPU", "GPU");
    metrics(&mut output, model, "gpu");
    heading(&mut output, "主板与系统风扇", "Motherboard and system fan");
    metrics(&mut output, model, "pch");
    for metric in model
        .metrics
        .iter()
        .filter(|m| m.key.to_ascii_lowercase().starts_with("systemfan"))
    {
        row(&mut output, &i18n::tr(&metric.label), &metric.value);
    }
    heading(&mut output, "电池", "Battery");
    row(
        &mut output,
        label("充电模式", "Charge mode"),
        get("battery.mode"),
    );
    let unit = get("battery.capacity_unit");
    for (key, title, en) in [
        ("battery.capacity", "当前容量", "Current capacity"),
        (
            "battery.full_charged_capacity",
            "满充容量",
            "Full-charge capacity",
        ),
        ("battery.designed_capacity", "设计容量", "Design capacity"),
    ] {
        let value = get(key);
        row(
            &mut output,
            label(title, en),
            if value == "—" {
                "—".into()
            } else {
                format!("{value} {unit}")
            },
        );
    }
    row(
        &mut output,
        label("电池健康度 · %", "Battery health · %"),
        get("battery.health_percent"),
    );
    row(
        &mut output,
        label("循环次数", "Cycle count"),
        get("battery.cycle_count"),
    );
    let rate = get("battery.rate");
    let rate_unit = get("battery.rate_unit");
    row(
        &mut output,
        label("当前充放电速率", "Charge/discharge rate"),
        if rate == "—" {
            "—".into()
        } else {
            format!("{rate} {rate_unit}")
        },
    );
    for (key, title, en) in [
        (
            "battery.minimum_rate_w",
            "最低充放电功率",
            "Minimum charge/discharge power",
        ),
        (
            "battery.maximum_rate_w",
            "最高充放电功率",
            "Maximum charge/discharge power",
        ),
    ] {
        let value = if rate_unit == "mW" {
            get(key)
        } else {
            "—".into()
        };
        row(
            &mut output,
            label(title, en),
            if value == "—" {
                value
            } else {
                format!("{value} W")
            },
        );
    }
    row(
        &mut output,
        label("电池使用起点 · UTC", "On battery since · UTC"),
        get("battery.on_battery_since"),
    );
    row(
        &mut output,
        label("生产日期", "Manufacture date"),
        get("battery.manufacture_date"),
    );
    row(
        &mut output,
        label("首次使用日期", "First use date"),
        get("battery.first_use_date"),
    );
    metrics(&mut output, model, "battery");
    heading(&mut output, "更新", "Updates");
    row(
        &mut output,
        label("更新状态", "Update status"),
        match model
            .values
            .get("update.available")
            .and_then(Value::as_bool)
        {
            Some(true) => label("有可用更新", "Update available"),
            Some(false) => label("未发现更新", "No update reported"),
            None => "—",
        },
    );
    output.push_str("\r\n");
    output.push_str(label(
        "— 表示未知或读取已过期；读数来源随项目标注。",
        "— means unknown or stale; sources are shown with the readings.",
    ));
    output
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Bounds {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}
impl From<RECT> for Bounds {
    fn from(v: RECT) -> Self {
        Self {
            left: v.left,
            top: v.top,
            right: v.right,
            bottom: v.bottom,
        }
    }
}
fn placement(work: Bounds, point: POINT, dpi: u32, pinned: bool) -> Bounds {
    let scale = |n: i32| ((n as i64 * dpi.clamp(96, 768) as i64) / 96).min(i32::MAX as i64) as i32;
    let available_w = work.right.saturating_sub(work.left).max(1);
    let available_h = work.bottom.saturating_sub(work.top).max(1);
    let width = scale(480).min(available_w);
    let height = scale(680).min(available_h);
    let left = if pinned {
        work.left.saturating_add((available_w - width) / 2)
    } else {
        point.x.saturating_sub(width).saturating_sub(scale(8))
    }
    .clamp(work.left, work.right.saturating_sub(width).max(work.left));
    let top = if pinned {
        work.top.saturating_add((available_h - height) / 2)
    } else {
        point.y.saturating_sub(height).saturating_sub(scale(8))
    }
    .clamp(work.top, work.bottom.saturating_sub(height).max(work.top));
    Bounds {
        left,
        top,
        right: left.saturating_add(width),
        bottom: top.saturating_add(height),
    }
}
fn hide_due(pinned: bool, due: Option<Instant>, now: Instant, inside: bool) -> bool {
    !pinned && !inside && due.is_some_and(|due| now >= due)
}

struct Callback {
    state: RefCell<State>,
    destroyed: Cell<bool>,
    close_requested: Cell<bool>,
    hide_at: Cell<Option<Instant>>,
    pinned: Cell<bool>,
    pending_dpi: Cell<u32>,
    reflow: Cell<bool>,
}
impl Callback {
    fn new() -> Self {
        Self {
            state: RefCell::new(State::new()),
            destroyed: Cell::new(false),
            close_requested: Cell::new(false),
            hide_at: Cell::new(None),
            pinned: Cell::new(false),
            pending_dpi: Cell::new(0),
            reflow: Cell::new(false),
        }
    }
}
struct State {
    body: HWND,
    close: HWND,
    font: HFONT,
    title_font: HFONT,
    brush: HBRUSH,
    dpi: u32,
    paper: u32,
    ink: u32,
    accent: u32,
    line: u32,
    revision: u64,
    small_revision: u64,
    language: &'static str,
    pstate_fresh: bool,
    state_fresh: bool,
    body_text: String,
}
impl State {
    fn annotate_controls(&self, hwnd: HWND) {
        a11y::annotate(hwnd, "status-popup.root", None, None);
        a11y::annotate(
            self.body,
            "status-popup.body",
            Some(label("本机状态详情", "Local status details")),
            Some(label(
                "只读。可选择和复制文本；破折号表示未知或读取已过期。",
                "Read-only. Text can be selected and copied; a dash means unknown or stale.",
            )),
        );
        a11y::annotate(
            self.close,
            "status-popup.close",
            Some(label("关闭本机状态窗口", "Close local status window")),
            None,
        );
    }
    fn new() -> Self {
        Self {
            body: null_mut(),
            close: null_mut(),
            font: null_mut(),
            title_font: null_mut(),
            brush: null_mut(),
            dpi: 96,
            paper: 0,
            ink: 0,
            accent: 0,
            line: 0,
            revision: u64::MAX,
            small_revision: 0,
            language: "",
            pstate_fresh: false,
            state_fresh: false,
            body_text: String::new(),
        }
    }
    fn s(&self, n: i32) -> i32 {
        ((n as i64 * self.dpi as i64) / 96).min(i32::MAX as i64) as i32
    }
    unsafe fn layout(&self, hwnd: HWND, pinned: bool) {
        unsafe {
            let mut rect: RECT = zeroed();
            if GetClientRect(hwnd, &mut rect) == 0 {
                return;
            }
            let w = rect.right.max(1);
            let h = rect.bottom.max(1);
            let header = self.s(48).min(h);
            if !self.body.is_null() {
                MoveWindow(
                    self.body,
                    self.s(12).min(w),
                    header,
                    (w - self.s(24)).max(1),
                    (h - header - self.s(10)).max(1),
                    1,
                );
            }
            if !self.close.is_null() {
                MoveWindow(
                    self.close,
                    (w - self.s(88)).max(0),
                    self.s(8),
                    self.s(76).min(w),
                    self.s(30).min(h),
                    1,
                );
                ShowWindow(self.close, if pinned { SW_SHOWNOACTIVATE } else { SW_HIDE });
            }
        }
    }
    unsafe fn palette(&mut self, dpi: u32) -> Result<()> {
        unsafe {
            let changed_font = self.font.is_null() || dpi != self.dpi;
            if changed_font {
                let body = font(dpi, 14, false);
                let title = font(dpi, 18, true);
                if body.is_null() || title.is_null() {
                    if !body.is_null() {
                        DeleteObject(body);
                    }
                    if !title.is_null() {
                        DeleteObject(title);
                    }
                    anyhow::bail!("Cannot create status fonts");
                }
                let old = [self.font, self.title_font];
                self.font = body;
                self.title_font = title;
                self.dpi = dpi;
                for hwnd in [self.body, self.close] {
                    if !hwnd.is_null() {
                        SendMessageW(hwnd, WM_SETFONT, self.font as usize, 1);
                    }
                }
                for font in old {
                    if !font.is_null() {
                        DeleteObject(font);
                    }
                }
            }
            if self.brush.is_null() || self.paper != paper() {
                let brush = CreateSolidBrush(paper());
                ensure!(!brush.is_null(), "Cannot create status background");
                let old = self.brush;
                self.brush = brush;
                if !old.is_null() {
                    DeleteObject(old);
                }
            }
            self.paper = paper();
            self.ink = ink();
            self.accent = accent();
            self.line = line();
            Ok(())
        }
    }
    unsafe fn update(&mut self, model: &UiModel, now: u64) -> Result<bool> {
        unsafe {
            let pstate = fresh(model, "gpu.current_pstate_at_tick_ms", now);
            let state = fresh(model, "gpu.state_at_tick_ms", now);
            let small = small_revision(model);
            if self.revision == model.revision
                && self.small_revision == small
                && self.language == i18n::language()
                && self.pstate_fresh == pstate
                && self.state_fresh == state
            {
                return Ok(false);
            }
            let next = format_status(model, now);
            if next == self.body_text {
                self.revision = model.revision;
                self.small_revision = small;
                self.language = i18n::language();
                self.pstate_fresh = pstate;
                self.state_fresh = state;
                return Ok(false);
            }
            let line = SendMessageW(self.body, EM_GETFIRSTVISIBLELINE, 0, 0);
            let (mut start, mut end) = (0u32, 0u32);
            SendMessageW(
                self.body,
                EM_GETSEL,
                (&mut start as *mut u32) as usize,
                (&mut end as *mut u32) as isize,
            );
            ensure!(
                SetWindowTextW(self.body, wide(&next).as_ptr()) != 0,
                "Cannot update status text"
            );
            let length = next.encode_utf16().count() as u32;
            SendMessageW(
                self.body,
                EM_SETSEL,
                start.min(length) as usize,
                end.min(length) as isize,
            );
            SendMessageW(self.body, EM_LINESCROLL, 0, line);
            self.body_text = next;
            self.revision = model.revision;
            self.small_revision = small;
            self.language = i18n::language();
            self.pstate_fresh = pstate;
            self.state_fresh = state;
            Ok(true)
        }
    }
}
impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            for font in [self.font, self.title_font] {
                if !font.is_null() {
                    DeleteObject(font);
                }
            }
            if !self.brush.is_null() {
                DeleteObject(self.brush);
            }
        }
    }
}
struct Owned {
    hwnd: HWND,
    callback: Box<Callback>,
}
impl Owned {
    fn new() -> Self {
        Self {
            hwnd: null_mut(),
            callback: Box::new(Callback::new()),
        }
    }
    unsafe fn create(
        &mut self,
        owner: HWND,
        dpi: u32,
        pinned: bool,
        bounds: Bounds,
        visible: bool,
    ) -> Result<()> {
        unsafe {
            register()?;
            self.callback.pinned.set(pinned);
            self.callback.destroyed.set(false);
            self.callback.state.borrow_mut().palette(dpi)?;
            self.hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                wide(CLASS).as_ptr(),
                wide(label("拯救者工具箱 · 本机状态", "拯救者工具箱 · Status")).as_ptr(),
                WS_POPUP | WS_BORDER | WS_CLIPCHILDREN,
                bounds.left,
                bounds.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                owner,
                null_mut(),
                GetModuleHandleW(null()),
                (self.callback.as_ref() as *const Callback).cast(),
            );
            ensure!(
                !self.hwnd.is_null(),
                "Cannot create status popup: {}",
                std::io::Error::last_os_error()
            );
            {
                let mut state = self.callback.state.borrow_mut();
                state.body = CreateWindowExW(
                    0,
                    wide("EDIT").as_ptr(),
                    null(),
                    WS_CHILD
                        | WS_VISIBLE
                        | WS_VSCROLL
                        | ES_MULTILINE as u32
                        | ES_READONLY as u32
                        | ES_AUTOVSCROLL as u32
                        | ES_NOHIDESEL as u32,
                    0,
                    0,
                    1,
                    1,
                    self.hwnd,
                    1usize as HMENU,
                    GetModuleHandleW(null()),
                    null(),
                );
                ensure!(!state.body.is_null(), "Cannot create status text");
                state.close = CreateWindowExW(
                    0,
                    wide("BUTTON").as_ptr(),
                    wide(label("关闭", "Close")).as_ptr(),
                    WS_CHILD | BS_OWNERDRAW as u32,
                    0,
                    0,
                    1,
                    1,
                    self.hwnd,
                    CLOSE as usize as HMENU,
                    GetModuleHandleW(null()),
                    null(),
                );
                ensure!(!state.close.is_null(), "Cannot create status close button");
                SendMessageW(state.body, EM_SETLIMITTEXT, 256 * 1024, 0);
                SendMessageW(state.body, WM_SETFONT, state.font as usize, 0);
                SendMessageW(state.close, WM_SETFONT, state.font as usize, 0);
                state.annotate_controls(self.hwnd);
                state.layout(self.hwnd, pinned);
            }
            if visible {
                ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
            Ok(())
        }
    }
    fn close_with(
        &mut self,
        exists: impl FnOnce(HWND) -> bool,
        destroy: impl FnOnce(HWND) -> bool,
    ) -> Result<()> {
        if self.hwnd.is_null() {
            return Ok(());
        }
        if !self.callback.destroyed.get() && exists(self.hwnd) {
            ensure!(destroy(self.hwnd), "Status popup cleanup is still pending");
        }
        self.hwnd = null_mut();
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        self.close_with(
            |hwnd| unsafe { IsWindow(hwnd) != 0 },
            |hwnd| unsafe { DestroyWindow(hwnd) != 0 },
        )
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if self.close().is_err() {
            let retained = std::mem::replace(&mut self.callback, Box::new(Callback::new()));
            std::mem::forget(retained);
        }
    }
}

pub(super) struct StatusPopup {
    window: Option<Owned>,
    thread: Option<ThreadId>,
    _thread_bound: PhantomData<Rc<()>>,
}
impl Default for StatusPopup {
    fn default() -> Self {
        Self::new()
    }
}
impl StatusPopup {
    pub(super) fn new() -> Self {
        Self {
            window: None,
            thread: None,
            _thread_bound: PhantomData,
        }
    }
    fn check_thread(&mut self) -> Result<()> {
        let current = thread::current().id();
        ensure!(
            self.thread.is_none_or(|thread| thread == current),
            "Status popup belongs to another UI thread"
        );
        self.thread = Some(current);
        Ok(())
    }
    pub(super) fn show(
        &mut self,
        owner: HWND,
        model: &UiModel,
        dpi: u32,
        pinned: bool,
    ) -> Result<()> {
        self.check_thread()?;
        self.reap();
        let pinned = pinned
            || self
                .window
                .as_ref()
                .is_some_and(|window| window.callback.pinned.get());
        let dpi = dpi.clamp(96, 768);
        let (mut point, mut monitor_info) = (
            POINT { x: 0, y: 0 },
            MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..unsafe { zeroed() }
            },
        );
        unsafe {
            ensure!(GetCursorPos(&mut point) != 0, "Cannot locate status popup");
            ensure!(
                GetMonitorInfoW(
                    MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST),
                    &mut monitor_info
                ) != 0,
                "Cannot read status work area"
            );
        }
        let bounds = placement(monitor_info.rcWork.into(), point, dpi, pinned);
        if self.window.is_none() {
            self.window = Some(Owned::new());
            let result = unsafe {
                self.window
                    .as_mut()
                    .unwrap()
                    .create(owner, dpi, pinned, bounds, false)
            };
            if let Err(error) = result {
                let cleanup = self.hide();
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(error.context(format!("{cleanup:#}"))),
                };
            }
        }
        let window = self.window.as_ref().unwrap();
        window.callback.pinned.set(pinned);
        window.callback.close_requested.set(false);
        window.callback.hide_at.set(None);
        {
            let mut state = window.callback.state.borrow_mut();
            unsafe {
                state.palette(dpi)?;
                state.layout(window.hwnd, pinned);
                state.update(model, GetTickCount64())?;
                SetWindowTextW(
                    window.hwnd,
                    wide(label("拯救者工具箱 · 本机状态", "拯救者工具箱 · Status")).as_ptr(),
                );
                SetWindowTextW(state.close, wide(label("关闭", "Close")).as_ptr());
                state.annotate_controls(window.hwnd);
                ensure!(
                    SetWindowPos(
                        window.hwnd,
                        HWND_TOPMOST,
                        bounds.left,
                        bounds.top,
                        bounds.right - bounds.left,
                        bounds.bottom - bounds.top,
                        SWP_NOACTIVATE,
                    ) != 0,
                    "Cannot position status popup"
                );
            }
        }
        unsafe {
            ShowWindow(window.hwnd, SW_SHOWNOACTIVATE);
        }
        ensure!(self.is_visible(), "Status popup did not become visible");
        Ok(())
    }
    fn reap(&mut self) {
        if self
            .window
            .as_ref()
            .is_some_and(|window| window.callback.destroyed.get())
        {
            self.window.take();
        }
    }
    pub(super) fn sync(&mut self, model: &UiModel) -> Result<()> {
        self.check_thread()?;
        self.reap();
        let Some(window) = self.window.as_ref() else {
            return Ok(());
        };
        if window.callback.close_requested.get() {
            return self.hide();
        }
        let due = window.callback.hide_at.get();
        if due.is_some() {
            let (mut point, mut rect) = (POINT { x: 0, y: 0 }, unsafe { zeroed::<RECT>() });
            let inside = unsafe {
                GetCursorPos(&mut point) != 0
                    && GetWindowRect(window.hwnd, &mut rect) != 0
                    && point.x >= rect.left
                    && point.x < rect.right
                    && point.y >= rect.top
                    && point.y < rect.bottom
            };
            if hide_due(window.callback.pinned.get(), due, Instant::now(), inside) {
                return self.hide();
            }
        }
        let mut state = window.callback.state.borrow_mut();
        let dpi = window.callback.pending_dpi.replace(0);
        let reflow = window.callback.reflow.replace(false);
        let changed_palette = state.paper != paper()
            || state.ink != ink()
            || state.accent != accent()
            || state.line != line();
        let changed_language = state.language != i18n::language();
        unsafe {
            let current_dpi = if dpi == 0 {
                state.dpi
            } else {
                dpi.clamp(96, 768)
            };
            state.palette(current_dpi)?;
            let updated = state.update(model, GetTickCount64())?;
            if changed_language {
                SetWindowTextW(
                    window.hwnd,
                    wide(label("拯救者工具箱 · 本机状态", "拯救者工具箱 · Status")).as_ptr(),
                );
                SetWindowTextW(state.close, wide(label("关闭", "Close")).as_ptr());
                state.annotate_controls(window.hwnd);
            }
            if reflow || dpi != 0 {
                let mut rect: RECT = zeroed();
                let mut info = MONITORINFO {
                    cbSize: size_of::<MONITORINFO>() as u32,
                    ..zeroed()
                };
                ensure!(
                    GetWindowRect(window.hwnd, &mut rect) != 0
                        && GetMonitorInfoW(
                            MonitorFromWindow(window.hwnd, MONITOR_DEFAULTTONEAREST),
                            &mut info
                        ) != 0,
                    "Cannot refresh status work area"
                );
                let bounds = placement(
                    info.rcWork.into(),
                    POINT {
                        x: rect.right.saturating_add(state.s(8)),
                        y: rect.bottom.saturating_add(state.s(8)),
                    },
                    current_dpi,
                    window.callback.pinned.get(),
                );
                ensure!(
                    SetWindowPos(
                        window.hwnd,
                        HWND_TOPMOST,
                        bounds.left,
                        bounds.top,
                        bounds.right - bounds.left,
                        bounds.bottom - bounds.top,
                        SWP_NOACTIVATE
                    ) != 0,
                    "Cannot reposition status popup"
                );
            }
            if changed_palette || updated || changed_language || dpi != 0 || reflow {
                state.layout(window.hwnd, window.callback.pinned.get());
                InvalidateRect(window.hwnd, null(), 1);
                InvalidateRect(state.body, null(), 1);
            }
        }
        Ok(())
    }
    pub(super) fn schedule_hide(&mut self) {
        if let Some(window) = &self.window {
            if !window.callback.pinned.get() {
                window
                    .callback
                    .hide_at
                    .set(Some(Instant::now() + HIDE_DELAY));
            }
        }
    }
    pub(super) fn hide(&mut self) -> Result<()> {
        self.check_thread()?;
        if let Some(window) = self.window.as_mut() {
            window.close()?;
        }
        self.window.take();
        Ok(())
    }
    pub(super) fn is_visible(&self) -> bool {
        self.window.as_ref().is_some_and(|window| {
            !window.callback.destroyed.get()
                && !window.hwnd.is_null()
                && unsafe { IsWindowVisible(window.hwnd) != 0 }
        })
    }
}
impl Drop for StatusPopup {
    fn drop(&mut self) {
        let _ = self.hide();
    }
}

unsafe fn register() -> Result<()> {
    unsafe {
        let class = wide(CLASS);
        let definition = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: GetModuleHandleW(null()),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        ensure!(
            RegisterClassW(&definition) != 0 || GetLastError() == ERROR_CLASS_ALREADY_EXISTS,
            "Cannot register status popup"
        );
        Ok(())
    }
}
unsafe extern "system" fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    catch_unwind(AssertUnwindSafe(|| unsafe {
        if message == WM_NCCREATE {
            if l == 0 {
                return 0;
            }
            let pointer = (*(l as *const CREATESTRUCTW)).lpCreateParams as *const Callback;
            if pointer.is_null() {
                return 0;
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer as isize);
            return 1;
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Callback;
        if message == WM_NCDESTROY {
            if !pointer.is_null() {
                (*pointer).destroyed.set(true);
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            return DefWindowProcW(hwnd, message, w, l);
        }
        if pointer.is_null() {
            return DefWindowProcW(hwnd, message, w, l);
        }
        let callback = &*pointer;
        if message == WM_MOUSEACTIVATE {
            return MA_NOACTIVATE as isize;
        }
        if message == WM_CLOSE {
            // Close admission must not destroy a control while an update is
            // inside synchronous native messages. The manager destroys it
            // after the current borrow ends and verifies that destruction.
            callback.close_requested.set(true);
            return 0;
        }
        if message == WM_COMMAND
            && (w & 0xffff) == CLOSE as usize
            && ((w >> 16) & 0xffff) == BN_CLICKED as usize
        {
            callback.close_requested.set(true);
            return 0;
        }
        if message == WM_DPICHANGED {
            callback.pending_dpi.set((w & 0xffff) as u32);
            callback.reflow.set(true);
            return 0;
        }
        if message == WM_DISPLAYCHANGE {
            callback.reflow.set(true);
            return 0;
        }
        let Ok(state) = callback.state.try_borrow() else {
            return DefWindowProcW(hwnd, message, w, l);
        };
        match message {
            WM_SIZE => {
                state.layout(hwnd, callback.pinned.get());
                0
            }
            WM_ERASEBKGND => 1,
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
                let dc = w as HDC;
                SetBkColor(dc, state.paper);
                SetTextColor(dc, state.ink);
                state.brush as isize
            }
            WM_PAINT => {
                let mut paint: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(hwnd, &mut paint);
                if !dc.is_null() {
                    let mut rect: RECT = zeroed();
                    GetClientRect(hwnd, &mut rect);
                    FillRect(dc, &rect, state.brush);
                    SetBkMode(dc, TRANSPARENT as i32);
                    SetTextColor(dc, state.accent);
                    let previous = SelectObject(dc, state.title_font);
                    let mut title = RECT {
                        left: state.s(12),
                        top: state.s(10),
                        right: (rect.right
                            - if callback.pinned.get() {
                                state.s(96)
                            } else {
                                state.s(12)
                            })
                        .max(state.s(13)),
                        bottom: state.s(42),
                    };
                    DrawTextW(
                        dc,
                        wide(label("本机状态", "Status")).as_ptr(),
                        -1,
                        &mut title,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                    );
                    SelectObject(dc, previous);
                }
                EndPaint(hwnd, &paint);
                0
            }
            WM_DRAWITEM => {
                if l == 0 {
                    return DefWindowProcW(hwnd, message, w, l);
                }
                let item = &*(l as *const DRAWITEMSTRUCT);
                if item.CtlID == CLOSE as u32 {
                    FillRect(item.hDC, &item.rcItem, state.brush);
                    SetBkMode(item.hDC, TRANSPARENT as i32);
                    SetTextColor(item.hDC, state.accent);
                    let old = SelectObject(item.hDC, state.font);
                    let mut rect = item.rcItem;
                    DrawTextW(
                        item.hDC,
                        wide(label("关闭", "Close")).as_ptr(),
                        -1,
                        &mut rect,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                    );
                    SelectObject(item.hDC, old);
                    1
                } else {
                    DefWindowProcW(hwnd, message, w, l)
                }
            }
            _ => DefWindowProcW(hwnd, message, w, l),
        }
    }))
    .unwrap_or_else(|_| unsafe { DefWindowProcW(hwnd, message, w, l) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_model::UiMetric;
    use serde_json::json;
    #[test]
    fn formatting_keeps_user_names_and_all_metrics_and_ignores_desired_pstate() {
        i18n::set_language("en");
        let mut model = UiModel::default();
        model.machine = "设置".into();
        model
            .values
            .insert("preset.active_name".into(), json!("设置"));
        model.values.insert("power.mode".into(), json!(224));
        model.values.insert("gpu.locked_pstate".into(), json!(0));
        model.values.insert("gpu.keep_awake".into(), json!(true));
        for n in 0..20 {
            model.metrics.push(UiMetric {
                key: format!("CpuFixture{n}"),
                label: format!("CPU fixture {n}"),
                value: n.to_string(),
                detail: "synthetic source".into(),
            });
        }
        let body = format_status(&model, 5000);
        assert!(
            body.contains("设置") && body.contains("Extreme") && body.contains("CPU fixture 19")
        );
        assert!(body.contains("Observed P-state: —") && body.contains("Keep-awake session: —"));
        assert!(!body.contains("Observed P-state: P0"));
        assert!(!body.contains("Active preset:"));
        model.values.insert("power.mode".into(), json!(255));
        assert!(format_status(&model, 5000).contains("Active preset: 设置"));
    }
    #[test]
    fn actual_pstate_expires_without_a_new_revision_and_future_ticks_are_unknown() {
        let mut model = UiModel::default();
        model.values.insert("gpu.current_pstate".into(), json!(8));
        model
            .values
            .insert("gpu.current_pstate_at_tick_ms".into(), json!(1000));
        assert_eq!(actual_pstate(&model, 4000), "P8");
        assert_eq!(actual_pstate(&model, 4001), "—");
        assert_eq!(actual_pstate(&model, 999), "—");
        model.values.insert("gpu.current_pstate".into(), json!(16));
        assert_eq!(actual_pstate(&model, 1001), "—");
    }
    #[test]
    fn relative_capacity_and_rate_are_never_mislabeled_watts() {
        i18n::set_language("en");
        let mut model = UiModel::default();
        for (key, value) in [
            ("battery.capacity", json!(50)),
            ("battery.capacity_unit", json!("relative")),
            ("battery.rate", json!(-9)),
            ("battery.rate_unit", json!("relative/hour")),
            ("battery.minimum_rate_w", json!(-7.0)),
        ] {
            model.values.insert(key.into(), value);
        }
        let body = format_status(&model, 1000);
        assert!(body.contains("50 relative") && body.contains("-9 relative/hour"));
        assert!(body.contains("Minimum charge/discharge power: —"));
        assert!(!body.contains("-7.0 W"));
    }
    #[test]
    fn placement_clamps_negative_monitors_tiny_workareas_and_large_dpi() {
        for work in [
            Bounds {
                left: -1920,
                top: -900,
                right: -700,
                bottom: 0,
            },
            Bounds {
                left: 20,
                top: 30,
                right: 100,
                bottom: 90,
            },
        ] {
            for pinned in [true, false] {
                let result = placement(work, POINT { x: 9000, y: -9000 }, 384, pinned);
                assert!(
                    result.left >= work.left
                        && result.top >= work.top
                        && result.right <= work.right
                        && result.bottom <= work.bottom
                );
            }
        }
        let now = Instant::now();
        assert!(!hide_due(true, Some(now), now, false));
        assert!(!hide_due(false, Some(now), now, true));
        assert!(!hide_due(false, Some(now + HIDE_DELAY), now, false));
        assert!(hide_due(false, Some(now), now, false));
    }
    #[test]
    fn construction_is_inert_failed_close_retains_owner_and_destroyed_generation_is_not_reused() {
        let manager = StatusPopup::new();
        assert!(manager.window.is_none() && !manager.is_visible());
        let mut owned = Owned::new();
        owned.hwnd = 123usize as HWND;
        let pointer = owned.callback.as_ref() as *const Callback;
        assert!(owned.close_with(|_| true, |_| false).is_err());
        assert_eq!(pointer, owned.callback.as_ref() as *const Callback);
        assert!(!owned.hwnd.is_null());
        owned.callback.destroyed.set(true);
        owned
            .close_with(
                |_| panic!("must not inspect reused HWND"),
                |_| panic!("must not destroy reused HWND"),
            )
            .unwrap();
        assert!(owned.hwnd.is_null());
    }
    #[test]
    fn hidden_native_lifecycle_updates_same_hwnd_and_keeps_borrowed_state_until_destroyed() {
        let _accessibility = a11y::UiThread::enter().unwrap();
        i18n::set_language("en");
        let mut owned = Owned::new();
        unsafe {
            owned
                .create(
                    null_mut(),
                    96,
                    true,
                    Bounds {
                        left: 0,
                        top: 0,
                        right: 480,
                        bottom: 600,
                    },
                    false,
                )
                .unwrap();
        }
        let hwnd = owned.hwnd;
        assert_eq!(unsafe { IsWindowVisible(hwnd) }, 0);
        let mut model = UiModel::default();
        {
            let mut state = owned.callback.state.borrow_mut();
            unsafe {
                state.update(&model, 1000).unwrap();
            }
            assert!(!state.font.is_null());
            assert_ne!(
                unsafe { GetWindowLongW(state.body, GWL_STYLE) } as u32 & ES_READONLY as u32,
                0
            );
        }
        model
            .values
            .insert("preset.active_name".into(), json!("synthetic preset"));
        model.values.insert("power.mode".into(), json!(255));
        {
            let mut state = owned.callback.state.borrow_mut();
            unsafe {
                state.update(&model, 1001).unwrap();
            }
            assert!(state.body_text.contains("synthetic preset"));
        }
        assert_eq!(owned.hwnd, hwnd);
        let borrow = owned.callback.state.borrow_mut();
        unsafe {
            SendMessageW(hwnd, WM_CLOSE, 0, 0);
        }
        assert!(owned.callback.close_requested.get());
        assert_ne!(unsafe { IsWindow(hwnd) }, 0);
        // A direct native destruction still delivers NCDESTROY while borrowed;
        // only this known never-visible fixture HWND is touched.
        assert_ne!(unsafe { DestroyWindow(hwnd) }, 0);
        assert!(owned.callback.destroyed.get());
        assert_eq!(unsafe { IsWindow(hwnd) }, 0);
        assert!(!borrow.font.is_null());
        drop(borrow);
        owned.close().unwrap();
        assert!(owned.hwnd.is_null());
    }
    #[test]
    fn never_visible_status_uia_keeps_live_native_value_and_localizes_stable_semantics() {
        let _language = i18n::TestLanguage::new("en");
        let _accessibility = a11y::UiThread::enter().unwrap();
        let mut owned = Owned::new();
        unsafe {
            owned
                .create(
                    null_mut(),
                    96,
                    true,
                    Bounds {
                        left: 0,
                        top: 0,
                        right: 480,
                        bottom: 600,
                    },
                    false,
                )
                .unwrap();
        }
        let mut model = UiModel::default();
        model.values.insert("power.mode".into(), json!(255));
        model
            .values
            .insert("preset.active_name".into(), json!("synthetic preset one"));
        let (body, close) = {
            let mut state = owned.callback.state.borrow_mut();
            unsafe {
                state.update(&model, 1000).unwrap();
            }
            (state.body, state.close)
        };
        let first = a11y::read_for_test(&[owned.hwnd, body, close]).unwrap();
        assert_eq!(first[0].id, "status-popup.root");
        assert_eq!(first[1].id, "status-popup.body");
        assert_eq!(first[1].name, "Local status details");
        assert_eq!(
            first[1].control_type,
            windows::Win32::UI::Accessibility::UIA_DocumentControlTypeId.0
        );
        assert_eq!(first[1].read_only, Some(true));
        assert!(
            first[1]
                .value
                .as_ref()
                .unwrap()
                .contains("synthetic preset one")
        );
        assert_eq!(first[2].id, "status-popup.close");
        assert_eq!(first[2].name, "Close local status window");
        assert!(first[2].invoke);
        assert_eq!(
            first[2].control_type,
            windows::Win32::UI::Accessibility::UIA_ButtonControlTypeId.0
        );
        assert_eq!(unsafe { IsWindowVisible(owned.hwnd) }, 0);
        assert_eq!(unsafe { IsWindowVisible(body) }, 0);
        assert_eq!(unsafe { IsWindowVisible(close) }, 0);
        model
            .values
            .insert("preset.active_name".into(), json!("synthetic preset two"));
        i18n::set_language("zh-Hans");
        {
            let mut state = owned.callback.state.borrow_mut();
            unsafe {
                state.update(&model, 1001).unwrap();
            }
            state.annotate_controls(owned.hwnd);
        }
        let second = a11y::read_for_test(&[body, close]).unwrap();
        assert_eq!(second[0].id, first[1].id);
        assert_eq!(second[0].name, "本机状态详情");
        assert!(
            second[0]
                .value
                .as_ref()
                .unwrap()
                .contains("synthetic preset two")
        );
        assert!(!second[0].name.contains("synthetic preset"));
        assert_eq!(second[1].id, first[2].id);
        assert_eq!(second[1].name, "关闭本机状态窗口");
        assert_eq!(unsafe { IsWindowVisible(owned.hwnd) }, 0);
        owned.close().unwrap();
    }
}
