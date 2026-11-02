//! Event-driven GDI OSD. Runtime `osd.visible` is the sole visibility gate.
//! No device reads, timers, file access, graphics runtime or background renderer.
use super::native::{font, request_id, send, wide};
use crate::ui_model::{UiAction, UiBridge, UiChoice, UiMetric, UiModel};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    mem::zeroed,
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
};
const CLASS: &str = "Jiks.Native.Osd";
const ITEMS: &[(&str, &str)] = &[
    ("Fps", "提交 FPS"),
    ("LowFps", "1% 低提交 FPS"),
    ("FrameTime", "提交间隔"),
    ("CpuFrequency", "CPU 频率"),
    ("CpuPCoreFrequency", "P 核频率"),
    ("CpuECoreFrequency", "E 核频率"),
    ("CpuUtilization", "CPU 占用"),
    ("CpuTemperature", "CPU 温度"),
    ("CpuPower", "CPU 功耗"),
    ("CpuVoltage", "CPU 电压"),
    ("CpuFan", "CPU 风扇"),
    ("GpuFrequency", "GPU 频率"),
    ("GpuUtilization", "GPU 占用"),
    ("GpuTemperature", "GPU 温度"),
    ("GpuVramUtilization", "显存占用"),
    ("GpuVramTemperature", "显存温度"),
    ("GpuPower", "GPU 功耗"),
    ("GpuFan", "GPU 风扇"),
    ("MemoryUtilization", "内存占用"),
    ("MemoryTemperature", "内存温度"),
    ("Disk1Temperature", "硬盘 1 温度"),
    ("Disk2Temperature", "硬盘 2 温度"),
    ("PchTemperature", "PCH 温度"),
    ("PchFan", "PCH 风扇"),
];
pub(super) fn item_choices() -> Vec<UiChoice> {
    ITEMS
        .iter()
        .map(|(id, label)| UiChoice::new(*label, *id))
        .collect()
}
#[derive(Clone, Debug, PartialEq)]
struct Settings {
    bar: bool,
    opacity: u8,
    refresh_ms: u64,
    radius_top: i32,
    radius_bottom: i32,
    category: u32,
    low_fps_delta: f64,
    background: u32,
    label: u32,
    value: u32,
    warning: u32,
    critical: u32,
    font_size: i32,
    locked: bool,
    snap: i32,
    temp_warning: f64,
    temp_critical: f64,
    usage_warning: f64,
    usage_critical: f64,
    fps_critical: f64,
    items: Vec<String>,
    x: f64,
    y: f64,
}
fn color(value: Option<&Value>, fallback: u32) -> u32 {
    value
        .and_then(Value::as_str)
        .and_then(|s| s.strip_prefix('#'))
        .filter(|s| s.len() == 6)
        .and_then(|s| u32::from_str_radix(s, 16).ok())
        .map(|n| ((n & 255) << 16) | (n & 0xff00) | ((n >> 16) & 255))
        .unwrap_or(fallback)
}
impl Settings {
    fn parse(value: &Value) -> Self {
        let n = |key: &str, default: f64| {
            value
                .get(key)
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite())
                .unwrap_or(default)
        };
        let bar = value.get("SelectedStyleIndex").and_then(Value::as_i64) == Some(1);
        let position = value.get(if bar { "BarPosition" } else { "PanelPosition" });
        Self {
            bar,
            opacity: (n("BackgroundOpacity", 0.85).clamp(0.1, 1.) * 255.) as u8,
            refresh_ms: (n("OsdRefreshInterval", 1.).clamp(0.1, 60.) * 1000.) as u64,
            radius_top: n("CornerRadiusTop", 6.).clamp(0., 50.) as i32,
            radius_bottom: n("CornerRadiusBottom", 6.).clamp(0., 50.) as i32,
            category: color(value.get("CategoryColor"), 0x00f39621),
            low_fps_delta: n("LowFpsDeltaThreshold", 30.).clamp(0., 1000.),
            background: color(value.get("BackgroundColor"), 0x001e1e1e),
            label: color(value.get("LabelColor"), 0x002fffad),
            value: color(value.get("ValueColor"), 0x00ffffff),
            warning: color(value.get("WarningColor"), 0x0000ffff),
            critical: color(value.get("CriticalColor"), 0x000000ff),
            font_size: n("FontSize", 12.).clamp(8., 72.) as i32,
            locked: value
                .get("IsLocked")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            snap: n("SnapThreshold", 20.).clamp(0., 200.) as i32,
            temp_warning: n("TempThresholdWarning", 75.),
            temp_critical: n("TempThresholdCritical", 90.),
            usage_warning: n("UsageThresholdWarning", 70.),
            usage_critical: n("UsageThresholdCritical", 90.),
            fps_critical: n("FpsThresholdCritical", 30.),
            items: value
                .get("Items")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_else(|| ITEMS.iter().map(|(id, _)| (*id).into()).collect()),
            x: position
                .and_then(|p| p.get("XRatio"))
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite())
                .unwrap_or(0.02)
                .clamp(0., 1.),
            y: position
                .and_then(|p| p.get("YRatio"))
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite())
                .unwrap_or(0.08)
                .clamp(0., 1.),
        }
    }
}
#[derive(Clone, PartialEq)]
struct Row {
    key:String,
    label: String,
    value: String,
    color: u32,
    heading: bool,
}
fn number(text: &str) -> Option<f64> {
    let text = text.trim();
    let end = text
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit() && !matches!(c, '.' | '-' | '+'))
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    text[..end].parse::<f64>().ok().filter(|n| n.is_finite())
}
fn rows(settings: &Settings, metrics: &[UiMetric]) -> Vec<Row> {
    let rows: Vec<Row> = settings
        .items
        .iter()
        .map(|key| {
            let canonical = super::sensors::canonical(key);
            let metric = metrics
                .iter()
                .find(|m| super::sensors::canonical(&m.key) == canonical);
            let label = ITEMS
                .iter()
                .find(|(id, _)| *id == key)
                .map(|(_, label)| super::i18n::tr(label).into_owned())
                .unwrap_or_else(|| key.clone());
            let value = metric
                .map(|m| m.value.clone())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "—".into());
            let color = if let Some(number) = number(&value).map(|n| {
                if key.contains("Temperature") && value.contains("°F") {
                    (n - 32.) / 1.8
                } else {
                    n
                }
            }) {
                let (warning, critical, low) = if key.ends_with("Temperature") {
                    (settings.temp_warning, settings.temp_critical, false)
                } else if key.ends_with("Utilization") {
                    (settings.usage_warning, settings.usage_critical, false)
                } else if key == "Fps" || key == "LowFps" {
                    (settings.fps_critical, settings.fps_critical, true)
                } else {
                    (f64::INFINITY, f64::INFINITY, false)
                };
                if (low && number < critical) || (!low && number >= critical) {
                    settings.critical
                } else if !low && number >= warning {
                    settings.warning
                } else {
                    settings.value
                }
            } else {
                0x00a0a0a0
            };
            let color = if key == "LowFps" {
                if let (Some(low), Some(fps)) = (
                    number(&value),
                    metrics
                        .iter()
                        .find(|m| m.key.eq_ignore_ascii_case("Fps"))
                        .and_then(|m| number(&m.value)),
                ) {
                    if fps - low >= settings.low_fps_delta {
                        settings.critical
                    } else {
                        color
                    }
                } else {
                    color
                }
            } else {
                color
            };
            Row {
                key:key.clone(),
                label,
                value,
                color,
                heading: false,
            }
        })
        .collect();
    if settings.bar {
        return rows;
    }
    let mut output = Vec::new();
    let mut previous = String::new();
    for (key, row) in settings.items.iter().zip(rows) {
        let category = if key.starts_with("Cpu") {
            "CPU"
        } else if key.starts_with("Gpu") {
            "GPU"
        } else if key.starts_with("Memory") {
            "内存"
        } else if key.starts_with("Disk") {
            "磁盘"
        } else if key.starts_with("Pch") {
            "主板"
        } else {
            "游戏"
        };
        if previous != category {
            output.push(Row {
                key:String::new(),
                label: super::i18n::tr(category).into_owned(),
                value: String::new(),
                color: settings.category,
                heading: true,
            });
            previous = category.into();
        }
        output.push(row);
    }
    output
}
pub(super) struct Overlay {
    state: Box<RefCell<State>>,
    metadata: u64,
    revision: u64,
    geometry: Option<(i32, i32, i32, i32, bool, u8)>,
    reposition: bool,
    visible: bool,
    dirty: bool,
    last_draw: Option<Instant>,
}
struct State {
    bridge: UiBridge,
    hwnd: HWND,
    settings: Settings,
    rows: Vec<Row>,
    font: HFONT,
    dpi: u32,
    missing: bool,
}
impl Overlay {
    pub(super) fn close(&mut self)->anyhow::Result<()> {
        let hwnd=self.state.borrow().hwnd;
        if !hwnd.is_null()&&unsafe{IsWindow(hwnd)}!=0 {
            anyhow::ensure!(unsafe{DestroyWindow(hwnd)}!=0,"OSD窗口尚未实际销毁：{}",std::io::Error::last_os_error());
        }
        self.state.borrow_mut().hwnd=null_mut();self.visible=false;self.dirty=false;Ok(())
    }
    pub(super) fn new(bridge: UiBridge) -> Self {
        Self {
            state: Box::new(RefCell::new(State {
                bridge,
                hwnd: null_mut(),
                settings: Settings::parse(&json!({})),
                rows: vec![],
                font: null_mut(),
                dpi: 96,
                missing: true,
            })),
            metadata: u64::MAX,
            revision: u64::MAX,
            geometry: None,
            reposition: true,
            visible: false,
            dirty: false,
            last_draw: None,
        }
    }
    pub(super) unsafe fn sync(&mut self, model: &UiModel) {
        unsafe {
            if self.metadata != model.metadata_revision {
                self.metadata = model.metadata_revision;
                // Clone only the OSD document when metadata changes, including while main is hidden.
                let document = self
                    .state
                    .borrow()
                    .bridge
                    .model
                    .lock()
                    .ok()
                    .and_then(|m| m.documents.get("osd").map(|d| d.value.clone()))
                    .unwrap_or_else(|| json!({}));
                let next = Settings::parse(&document);
                let mut state = self.state.borrow_mut();
                if next != state.settings {
                    state.settings = next;
                    if !state.font.is_null() {
                        DeleteObject(state.font);
                        state.font = null_mut();
                    }
                    self.revision = u64::MAX;
                    self.reposition = true;
                    self.dirty = true;
                    self.last_draw = None;
                }
            }
            let visible = model.values.get("osd.visible").and_then(Value::as_bool) == Some(true);
            let mut hwnd = self.state.borrow().hwnd;
            if !hwnd.is_null() && IsWindow(hwnd) == 0 {
                self.state.borrow_mut().hwnd = null_mut();
                hwnd = null_mut();
            }
            if !visible {
                if !hwnd.is_null() {
                    DestroyWindow(hwnd);
                }
                self.geometry = None;
                self.visible = false;
                self.dirty = false;
                self.last_draw = None;
                return;
            }
            self.visible = true;
            if self.revision != model.revision {
                self.revision = model.revision;
                let mut state = self.state.borrow_mut();
                let next = rows(&state.settings, &model.metrics);
                let missing = model.metrics.is_empty();
                if next != state.rows || missing != state.missing {
                    state.rows = next;
                    state.missing = missing;
                    self.dirty = true;
                }
            }
            if hwnd.is_null() {
                self.dirty = true;
                self.last_draw = None;
            }
            self.flush();
        }
    }
    pub(super) fn desired_tick(&self) -> u32 {
        if self.visible {
            self.state.borrow().settings.refresh_ms.clamp(50, 250) as u32
        } else {
            250
        }
    }
    pub(super) fn actual_visible(&self) -> bool {
        let hwnd = self.state.borrow().hwnd;
        !hwnd.is_null() && unsafe { IsWindow(hwnd) != 0 && IsWindowVisible(hwnd) != 0 }
    }
    pub(super) unsafe fn flush(&mut self) {
        unsafe {
            if !self.visible || !self.dirty {
                return;
            }
            let interval = Duration::from_millis(self.state.borrow().settings.refresh_ms);
            if self.last_draw.is_some_and(|last| last.elapsed() < interval) {
                return;
            }
            let hwnd = self.state.borrow().hwnd;
            let mut created = false;
            let hwnd = if hwnd.is_null() {
                let class = wide(CLASS);
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(procedure),
                    hInstance: GetModuleHandleW(null()),
                    hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                    hbrBackground: GetStockObject(NULL_BRUSH) as HBRUSH,
                    lpszClassName: class.as_ptr(),
                    ..zeroed()
                };
                if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                    return;
                }
                let pointer = self.state.as_ref() as *const RefCell<State>;
                let hwnd = CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                    class.as_ptr(),
                    wide("机控 OSD").as_ptr(),
                    WS_POPUP,
                    0,
                    0,
                    240,
                    200,
                    null_mut(),
                    null_mut(),
                    GetModuleHandleW(null()),
                    pointer as *const _,
                );
                if hwnd.is_null() {
                    return;
                }
                created = true;
                hwnd
            } else {
                hwnd
            };
            let (width, height, x, y, locked, opacity) = {
                let mut state = self.state.borrow_mut();
                state.dpi = GetDpiForWindow(hwnd).max(96);
                if state.font.is_null() {
                    state.font = font(state.dpi, state.settings.font_size, false);
                }
                let scale = |n: i32| n * state.dpi as i32 / 96;
                let screen_w = GetSystemMetrics(SM_CXSCREEN);
                let screen_h = GetSystemMetrics(SM_CYSCREEN);
                let cell = (state.settings.font_size * 17).max(170);
                let row = state.settings.font_size + 8;
                let columns = if state.settings.bar {
                    ((screen_w - 40) / scale(cell)).max(1) as usize
                } else {
                    1
                };
                let count = state.rows.len().max(1);
                let width = scale(if state.settings.bar {
                    cell * (count.min(columns) as i32)
                } else {
                    cell + 40
                })
                .min(screen_w - 20)
                .max(120);
                let height = scale(if state.settings.bar {
                    ((count + columns - 1) / columns) as i32 * row + 20
                } else {
                    count as i32 * row + if state.missing { 44 } else { 20 }
                })
                .min(screen_h - 40)
                .max(40);
                (
                    width,
                    height,
                    ((screen_w - width).max(0) as f64 * state.settings.x) as i32,
                    ((screen_h - height).max(0) as f64 * state.settings.y) as i32,
                    state.settings.locked,
                    state.settings.opacity,
                )
            };
            let geometry = (width, height, x, y, locked, opacity);
            if created || self.reposition || self.geometry != Some(geometry) {
                let extended = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
                let desired = if locked {
                    extended | WS_EX_TRANSPARENT
                } else {
                    extended & !WS_EX_TRANSPARENT
                };
                if extended != desired {
                    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, desired as isize);
                }
                SetLayeredWindowAttributes(hwnd, 0, opacity, LWA_ALPHA);
                let state = self.state.borrow();
                let top = state.settings.radius_top * state.dpi as i32 / 96;
                let bottom = state.settings.radius_bottom * state.dpi as i32 / 96;
                drop(state);
                set_round_corners(hwnd, width, height, top, bottom);
                if created || self.reposition {
                    SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        x,
                        y,
                        width,
                        height,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                } else {
                    SetWindowPos(
                        hwnd,
                        null_mut(),
                        0,
                        0,
                        width,
                        height,
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                self.geometry = Some(geometry);
                self.reposition = false;
            }
            InvalidateRect(hwnd, null(), 0);
            self.dirty = false;
            self.last_draw = Some(Instant::now());
        }
    }
}
unsafe fn set_round_corners(hwnd: HWND, width: i32, height: i32, top: i32, bottom: i32) {
    unsafe {
        let upper = CreateRoundRectRgn(0, 0, width + 1, height + 1, top * 2, top * 2);
        let lower = CreateRoundRectRgn(0, 0, width + 1, height + 1, bottom * 2, bottom * 2);
        let half_top = CreateRectRgn(0, 0, width + 1, height / 2 + 1);
        let half_bottom = CreateRectRgn(0, height / 2, width + 1, height + 1);
        CombineRgn(upper, upper, half_top, RGN_AND);
        CombineRgn(lower, lower, half_bottom, RGN_AND);
        CombineRgn(upper, upper, lower, RGN_OR);
        DeleteObject(lower);
        DeleteObject(half_top);
        DeleteObject(half_bottom);
        if SetWindowRgn(hwnd, upper, 1) == 0 {
            DeleteObject(upper);
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            if self.close().is_err() {
                let bridge=self.state.borrow().bridge.clone();
                let retained=std::mem::replace(&mut self.state,Box::new(RefCell::new(State{bridge,hwnd:null_mut(),settings:Settings::parse(&json!({})),rows:vec![],font:null_mut(),dpi:96,missing:true})));
                std::mem::forget(retained); // A live HWND must not retain a freed callback pointer.
            }
            UnregisterClassW(wide(CLASS).as_ptr(), GetModuleHandleW(null()));
        }
    }
}
impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            if !self.font.is_null() {
                DeleteObject(self.font);
            }
        }
    }
}
impl State {
    unsafe fn paint(&self, dc: HDC, rect: &RECT) {
        unsafe {
            let brush = CreateSolidBrush(self.settings.background);
            FillRect(dc, rect, brush);
            DeleteObject(brush);
            let old = SelectObject(dc, self.font);
            SetBkMode(dc, TRANSPARENT as i32);
            let scale = |n: i32| n * self.dpi as i32 / 96;
            let height = scale(self.settings.font_size + 8);
            let cell = scale((self.settings.font_size * 17).max(170));
            let columns = if self.settings.bar {
                ((rect.right - 20) / cell).max(1) as usize
            } else {
                1
            };
            let mut top = scale(10);
            if self.missing && !self.settings.bar {
                SetTextColor(dc, self.settings.warning);
                let mut r = RECT {
                    left: scale(10),
                    top,
                    right: rect.right - scale(10),
                    bottom: top + height,
                };
                DrawTextW(
                    dc,
                    wide("尚无传感器数据").as_ptr(),
                    -1,
                    &mut r,
                    DT_SINGLELINE | DT_END_ELLIPSIS,
                );
                top += height;
            }
            for (index, row) in self.rows.iter().enumerate() {
                let x = scale(10) + (index % columns) as i32 * cell;
                let y = top + (index / columns) as i32 * height;
                if y + height > rect.bottom {
                    break;
                }
                if self.settings.bar {
                    SetTextColor(dc, row.color);
                    let mut r = RECT {
                        left: x,
                        top: y,
                        right: (x + cell - scale(8)).min(rect.right),
                        bottom: y + height,
                    };
                    DrawTextW(
                        dc,
                        wide(&format!("{} {}", row.label, row.value)).as_ptr(),
                        -1,
                        &mut r,
                        DT_SINGLELINE | DT_END_ELLIPSIS,
                    );
                } else {
                    if row.heading {
                        SetTextColor(dc, row.color);
                        let mut r = RECT {
                            left: x,
                            top: y,
                            right: rect.right - scale(10),
                            bottom: y + height,
                        };
                        DrawTextW(
                            dc,
                            wide(&row.label).as_ptr(),
                            -1,
                            &mut r,
                            DT_SINGLELINE | DT_END_ELLIPSIS,
                        );
                        continue;
                    }
                    SetTextColor(dc, self.settings.label);
                    let mut r = RECT {
                        left: x,
                        top: y,
                        right: rect.right - scale(90),
                        bottom: y + height,
                    };
                    DrawTextW(
                        dc,
                        wide(&row.label).as_ptr(),
                        -1,
                        &mut r,
                        DT_SINGLELINE | DT_END_ELLIPSIS,
                    );
                    SetTextColor(dc, row.color);
                    let mut r = RECT {
                        left: rect.right - scale(100),
                        top: y,
                        right: rect.right - scale(10),
                        bottom: y + height,
                    };
                    DrawTextW(
                        dc,
                        wide(&row.value).as_ptr(),
                        -1,
                        &mut r,
                        DT_RIGHT | DT_SINGLELINE | DT_END_ELLIPSIS,
                    );
                }
            }
            SelectObject(dc, old);
        }
    }
    fn hide_request(&self) {
        let id = request_id();
        let _ = send(
            &self.bridge,
            UiAction::Execute {
                request_id: id,
                command: "osd.apply".into(),
                payload: json!({"osd.enabled":false}),
            },
        );
    }
    unsafe fn save_position(&mut self) {
        unsafe {
            let mut rect: RECT = zeroed();
            GetWindowRect(self.hwnd, &mut rect);
            let screen_w = GetSystemMetrics(SM_CXSCREEN);
            let screen_h = GetSystemMetrics(SM_CYSCREEN);
            let snap = self.settings.snap * self.dpi as i32 / 96;
            let mut x = rect.left;
            let mut y = rect.top;
            let max_x = (screen_w - (rect.right - rect.left)).max(0);
            let max_y = (screen_h - (rect.bottom - rect.top)).max(0);
            if x.abs() <= snap {
                x = 0;
            } else if (max_x - x).abs() <= snap {
                x = max_x;
            }
            if y.abs() <= snap {
                y = 0;
            } else if (max_y - y).abs() <= snap {
                y = max_y;
            }
            self.settings.x = x.clamp(0, max_x) as f64 / max_x.max(1) as f64;
            self.settings.y = y.clamp(0, max_y) as f64 / max_y.max(1) as f64;
            SetWindowPos(
                self.hwnd,
                null_mut(),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            let _ = send(
                &self.bridge,
                UiAction::Execute {
                    request_id: request_id(),
                    command: "osd.position.save".into(),
                    payload: json!({"style":if self.settings.bar{"bar"}else{"panel"},"x_ratio":self.settings.x,"y_ratio":self.settings.y}),
                },
            );
        }
    }
}
unsafe extern "system" fn procedure(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    unsafe {
        if msg == WM_NCCREATE {
            let pointer = (*(lp as *const CREATESTRUCTW)).lpCreateParams as *mut RefCell<State>;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer as isize);
            (*pointer).borrow_mut().hwnd = hwnd;
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RefCell<State>;
        if pointer.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let Ok(mut state) = (*pointer).try_borrow_mut() else {
            if msg == WM_NCDESTROY {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        match msg {
            WM_CREATE => {
                state
                    .bridge
                    .diagnostics
                    .overlays_opened
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                0
            }
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(hwnd, &mut ps);
                let mut rect: RECT = zeroed();
                GetClientRect(hwnd, &mut rect);
                state.paint(dc, &rect);
                EndPaint(hwnd, &ps);
                0
            }
            WM_NCHITTEST => {
                if state.settings.locked {
                    HTTRANSPARENT as isize
                } else {
                    HTCAPTION as isize
                }
            }
            WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
            WM_EXITSIZEMOVE => {
                state.save_position();
                0
            }
            WM_RBUTTONUP | WM_NCRBUTTONUP | WM_CONTEXTMENU => {
                drop(state);
                let menu = CreatePopupMenu();
                AppendMenuW(menu, MF_STRING, 1, wide("隐藏 OSD").as_ptr());
                let mut point: POINT = zeroed();
                GetCursorPos(&mut point);
                let chosen = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_NONOTIFY,
                    point.x,
                    point.y,
                    0,
                    hwnd,
                    null(),
                );
                DestroyMenu(menu);
                if chosen == 1 {
                    if let Ok(state) = (*pointer).try_borrow() {
                        state.hide_request();
                    }
                }
                0
            }
            WM_CLOSE => {
                state.hide_request();
                0
            }
            WM_NCDESTROY => {
                state
                    .bridge
                    .diagnostics
                    .overlays_closed
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                state.hwnd = null_mut();
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_data_is_explicit_and_thresholds_are_numeric() {
        let s = Settings::parse(&json!({"Items":["CpuTemperature","Fps"]}));
        let r = rows(&s, &[]);
        assert_eq!(r.iter().find(|r| r.label == "CPU 温度").unwrap().value, "—");
        let r = rows(
            &s,
            &[UiMetric {
                key: "cpu.temp".into(),
                value: "95°C".into(),
                ..Default::default()
            }],
        );
        assert_eq!(
            r.iter().find(|r| r.key == "CpuTemperature").unwrap().color,
            s.critical
        );
        assert_eq!(r.iter().find(|r| r.key == "Fps").unwrap().value, "—");
    }
    #[test]
    fn fahrenheit_display_still_compares_warning_thresholds_in_celsius() {
        let settings = Settings::parse(
            &json!({"Items":["CpuTemperature"],"TempThresholdWarning":75,"TempThresholdCritical":90}),
        );
        let metric = UiMetric {
            key: "CpuTemperature".into(),
            value: "167 °F".into(),
            ..Default::default()
        };
        let rendered = rows(&settings, &[metric]);
        assert_eq!(
            rendered
                .iter()
                .find(|r| r.label == "CPU 温度")
                .unwrap()
                .color,
            settings.warning
        );
    }
    #[test]
    fn malformed_style_values_are_bounded() {
        let s = Settings::parse(
            &json!({"FontSize":999,"BackgroundOpacity":-1,"BackgroundColor":"invalid"}),
        );
        assert_eq!(s.font_size, 72);
        assert_eq!(s.opacity, 25);
        assert_eq!(s.background, 0x1e1e1e);
    }
}
