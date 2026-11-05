use super::{
    a11y,
    catalog::{self, Action, ActionKind, FieldKind, Page},
    editors,
    layout::{CONTENT_RIGHT, CONTENT_X, CONTROL_HEIGHT, FOOTER_HEIGHT, GROUP_GAP, RowColumns, VIEW_TOP},
    scrolling::{SCROLL_TIMER, ScrollChild, ScrollMessages, Scrolling, Viewport, install_wheel_routing},
};
use crate::ui_model::*;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetFocus, GetKeyState, IsWindowEnabled, TME_LEAVE, TRACKMOUSEEVENT,
    TrackMouseEvent,
};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_BUTTON_BOUNDS, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DwmDefWindowProc, DwmExtendFrameIntoClientArea,
    DwmGetWindowAttribute, DwmSetWindowAttribute,
};

// Stable Windows SDK constants omitted/gated in windows-sys metadata.
pub(super) const SS_LEFT: u32 = 0;
const SS_CENTER: u32 = 1;
const SS_RIGHT: u32 = 2;
const SS_NOPREFIX: u32 = 0x0080;
const SS_CENTERIMAGE: u32 = 0x0200;
const SS_ENDELLIPSIS: u32 = 0x4000;
pub(super) const TRACKBAR_GET_POSITION: u32 = WM_USER;
const TRAY_KEY_SELECT: u32 = NIN_SELECT | 1;
const APP_ICON: u16 = 101;
#[derive(Clone, Copy, PartialEq, Eq)]
struct Palette {
    paper: u32,
    surface: u32,
    ink: u32,
    accent: u32,
    muted: u32,
    line: u32,
    pressed: u32,
}
impl Default for Palette {
    fn default() -> Self {
        Self {
            paper: 0x00f8f3f1,
            surface: 0x00ffffff,
            ink: 0x00202020,
            accent: 0x00ff9c6b,
            muted: 0x00615b53,
            line: 0x00dad6cf,
            pressed: 0x00f5e8da,
        }
    }
}
thread_local! {
    static PALETTE:Cell<Palette>=Cell::new(Palette::default());
    static FONT_FAMILY:RefCell<Option<String>>=const {RefCell::new(None)};
    static PAINT_BRUSHES:RefCell<PaintBrushes>=const {RefCell::new(PaintBrushes(Vec::new()))};
}
struct PaintBrushes(Vec<(u32, HBRUSH)>);
impl Drop for PaintBrushes {
    fn drop(&mut self) {
        unsafe {
            for (_, brush) in self.0.drain(..) {
                DeleteObject(brush);
            }
        }
    }
}
unsafe fn presentation_brush(color: u32) -> HBRUSH {
    PAINT_BRUSHES.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((_, brush)) = cache.0.iter().find(|(known, _)| *known == color) {
            *brush
        } else {
            let brush = unsafe { CreateSolidBrush(color) };
            cache.0.push((color, brush));
            brush
        }
    })
}
pub(super) fn paper() -> u32 {
    PALETTE.with(|p| p.get().paper)
}
pub(super) fn surface() -> u32 {
    PALETTE.with(|p| p.get().surface)
}
fn pane() -> u32 {
    if paper() & 0xff < 128 { 0x00262626 } else { 0x00fbf9f8 }
}
pub(super) fn ink() -> u32 {
    PALETTE.with(|p| p.get().ink)
}
pub(super) fn accent() -> u32 {
    PALETTE.with(|p| p.get().accent)
}
pub(super) fn muted() -> u32 {
    PALETTE.with(|p| p.get().muted)
}
pub(super) fn line() -> u32 {
    PALETTE.with(|p| p.get().line)
}
fn apply_palette(model: &UiModel, preview_theme: Option<&str>) -> bool {
    let theme = preview_theme.unwrap_or_else(|| model
        .values
        .get("appearance.effective_theme")
        .or_else(|| model.values.get("appearance.theme"))
        .and_then(Value::as_str)
        .unwrap_or("light"));
    let mut next = if theme.eq_ignore_ascii_case("dark") {
        Palette {
            paper: 0x00202020,
            surface: 0x002c2c2c,
            ink: 0x00f0f0f0,
            accent: 0x00e0b060,
            muted: 0x00b4b4b4,
            line: 0x00404040,
            pressed: 0x00383838,
        }
    } else {
        Palette::default()
    };
    if let Some(color) = model
        .values
        .get("appearance.accent")
        .and_then(Value::as_str)
        .and_then(|s| s.strip_prefix('#'))
        .filter(|s| s.len() == 6)
        .and_then(|s| u32::from_str_radix(s, 16).ok())
    {
        next.accent = ((color & 0xff) << 16) | (color & 0x00ff00) | ((color >> 16) & 0xff);
    }
    PALETTE.with(|p| {
        let changed = p.get() != next;
        if changed {
            p.set(next);
        }
        changed
    })
}

pub(super) unsafe fn app_icon() -> HICON {
    unsafe { LoadIconW(GetModuleHandleW(null()), APP_ICON as usize as *const u16) }
}

pub(super) unsafe fn apply_window_chrome(hwnd: HWND) {
    unsafe {
        let dark = u32::from(paper() & 0xff < 128);
        for (attribute, value) in [
            (DWMWA_USE_IMMERSIVE_DARK_MODE, dark),
            (DWMWA_CAPTION_COLOR, paper()),
            (DWMWA_TEXT_COLOR, ink()),
            (DWMWA_BORDER_COLOR, line()),
        ] {
            let _ = DwmSetWindowAttribute(hwnd, attribute as u32, (&value as *const u32).cast(), 4);
        }
    }
}
const WM_TRAY: u32 = WM_APP + 31;
const WM_RELAYOUT: u32 = WM_APP + 32;
const TIMER: usize = 1;
const TRAY_HOVER_TIMER: usize = 2;
const TRAY_HOVER_DELAY: Duration = Duration::from_millis(500);
const ID_NAV: u16 = 100;
const ID_DEVICE: u16 = 180;
const ID_THEME: u16 = 182;
const ID_NAV_LIST: u16 = 190;
const ID_TOOL_NAV: u16 = 140;
const ID_TRAY_OPEN: usize = 300;
const ID_TRAY_EXIT: usize = 301;
const ID_TRAY_STATUS: usize = 302;
const ID_QUICK: usize = 6000;
const CAPTION_HEIGHT: i32 = 32;
const CAPTION_PROPERTY: windows_sys::core::PCWSTR = windows_sys::w!("Jiks.MainCaption");
const MAIN_TOOLBAR: [(u16, &str); 2] = [(ID_DEVICE, "device.details"), (ID_THEME, "app.preferences.save")];

fn main_nav() -> impl Iterator<Item = (usize, Page)> {
    Page::ALL.into_iter().enumerate().filter(|(_, page)| *page != Page::Keyboard)
}

fn active_navigation(page: Page) -> Page {
    match page {
        Page::Keyboard | Page::Performance | Page::Display | Page::Graphics => Page::Dashboard,
        _ => page,
    }
}

fn device_badge(model: &UiModel) -> String {
    let name = model.machine.split('·').next().unwrap_or("").trim();
    if name.starts_with("Y7000P") { format!("Legion {name}") } else { name.to_owned() }
}

unsafe fn extend_main_caption(hwnd: HWND, dpi: u32) -> bool {
    unsafe {
        let margins = MARGINS {
            cxLeftWidth: 0,
            cxRightWidth: 0,
            cyTopHeight: CAPTION_HEIGHT * dpi as i32 / 96,
            cyBottomHeight: 0,
        };
        DwmExtendFrameIntoClientArea(hwnd, &margins) >= 0
    }
}

unsafe fn caption_reserved_width(hwnd: HWND, dpi: u32) -> i32 {
    unsafe {
        let mut buttons: RECT = zeroed();
        if DwmGetWindowAttribute(
            hwnd, DWMWA_CAPTION_BUTTON_BOUNDS as u32, (&mut buttons as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        ) >= 0 && buttons.right > buttons.left {
            ((buttons.right - buttons.left) * 96 + dpi as i32 - 1) / dpi as i32 + 6
        } else {
            150
        }
    }
}

/// Extend only the caption into the client. DefWindowProc keeps the original
/// sizing borders and scroll bar, and DWM owns the native caption buttons.
unsafe fn main_caption_message(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> Option<isize> {
    unsafe {
        if GetPropW(hwnd, CAPTION_PROPERTY).is_null() {
            return None;
        }
        if msg == WM_NCCALCSIZE && lp != 0 {
            DefWindowProcW(hwnd, msg, wp, lp);
            // RECT and NCCALCSIZE_PARAMS both start with the proposed client RECT.
            let client = &mut *(lp as *mut RECT);
            let height = GetSystemMetricsForDpi(SM_CYCAPTION, GetDpiForWindow(hwnd).max(96));
            client.top -= height;
            return Some(0);
        }
        if matches!(msg, WM_NCHITTEST | WM_NCMOUSEMOVE | WM_NCMOUSELEAVE
            | WM_NCLBUTTONDOWN | WM_NCLBUTTONUP | WM_NCLBUTTONDBLCLK
            | WM_NCPAINT | WM_NCACTIVATE) {
            let mut result = 0;
            if DwmDefWindowProc(hwnd, msg, wp, lp, &mut result) != 0 {
                return Some(result);
            }
        }
        if msg == WM_NCHITTEST {
            let result = DefWindowProcW(hwnd, msg, wp, lp);
            if result != HTCLIENT as isize {
                return Some(result);
            }
            let mut point = POINT { x: lp as i16 as i32, y: (lp >> 16) as i16 as i32 };
            if ScreenToClient(hwnd, &mut point) != 0
                && (0..CAPTION_HEIGHT * GetDpiForWindow(hwnd).max(96) as i32 / 96).contains(&point.y)
            {
                let child = ChildWindowFromPointEx(hwnd, point, CWP_SKIPINVISIBLE);
                if !matches!(GetDlgCtrlID(child) as u16, ID_DEVICE | ID_THEME) {
                    return Some(HTCAPTION as isize);
                }
            }
            return Some(result);
        }
        if msg == WM_NCDESTROY {
            RemovePropW(hwnd, CAPTION_PROPERTY);
        }
        None
    }
}
const METRIC_METER: usize = 0x4A494D54;
const TOOL_KEYS: [&str; 8] = [
    "behavior",
    "appearance",
    "power",
    "display",
    "smart-keys",
    "software",
    "updates",
    "integration",
];
fn action_key(action: &Action) -> String {
    let command = match &action.kind {
        ActionKind::Execute {
            command, fields, ..
        } => format!("execute.{command}.{}", fields.join(",")),
        ActionKind::Structured {
            collection, fresh, ..
        } => format!(
            "edit.{collection}.{}",
            if *fresh { "new" } else { "selected" }
        ),
        ActionKind::Document(document) => format!("document.{document}"),
        ActionKind::Navigate(page) => format!("navigate.{}", page.key()),
        ActionKind::SensorSettings => "sensor-settings".into(),
        ActionKind::Cancel(fields) => format!("cancel.{}", fields.join(",")),
    };
    format!("{}.{}", action.accessibility_role, command)
}

fn metric_caption(metric: &UiMetric, compact: bool) -> &str {
    if !compact { return &metric.label; }
    let standard_caption = match metric.key.as_str() {
        "CpuUtilization" | "GpuUtilization" | "MemoryUtilization" => "利用率",
        "CpuFrequency" | "GpuFrequency" => "核心频率",
        "CpuFanSpeed" | "GpuFanSpeed" | "PchFanSpeed" => "风扇",
        "CpuTemperature" => "温度",
        "GpuCoreTemperature" => "核心温度",
        "GpuVramTemperature" => "显存温度",
        "GpuVramUtilization" => "显存占用",
        "CpuPower" | "GpuPower" => "功耗",
        "PchTemperature" => "南桥",
        _ => "",
    };
    if !standard_caption.is_empty() {
        return super::i18n::tr_static(standard_caption);
    }
    let caption = metric.label.trim_start_matches("CPU ").trim_start_matches("GPU ");
    caption.split_once('（').map_or(caption, |(head, _)| head.trim())
}

fn metric_meter_scale(key: &str) -> usize {
    match key {
        "CpuUtilization" | "GpuUtilization" | "GpuVramUtilization" | "MemoryUtilization"
        | "CpuTemperature" | "PchTemperature" => 100,
        "CpuFrequency" => 6000,
        "GpuFrequency" => 3000,
        _ => 0,
    }
}

fn dashboard_direct_command(key: &str) -> Option<&'static str> {
    match key {
        "power.mode" => Some("power.apply"),
        "battery.mode" | "battery.night_charge" => Some("battery.apply"),
        "power.always_on_usb" | "power.instant_boot" | "power.flip_to_start" => Some("power.accessories.apply"),
        "gpu.hybrid_mode" => Some("gpu.hybrid.apply"),
        "gpu.oc.enabled" => Some("gpu.oc.apply"),
        _ => None,
    }
}

fn metric_meter_fraction(value: &str, scale: usize) -> f32 {
    let numeric = value.split_whitespace().next().map(|v| v.trim_end_matches('%'))
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| v.is_finite()).unwrap_or(0.0);
    let numeric = if value.ends_with("°F") { (numeric - 32.0) / 1.8 }
        else if value.ends_with("GHz") { numeric * 1000.0 }
        else { numeric };
    (numeric / scale as f32).clamp(0.0, 1.0)
}

#[cfg(test)]
#[test]
fn dashboard_meter_uses_value_units_and_keeps_unavailable_samples_empty() {
    assert_eq!(metric_meter_fraction("51.5%", 100), 0.515);
    assert_eq!(metric_meter_fraction("3.0 GHz", 6000), 0.5);
    assert_eq!(metric_meter_fraction("122 °F", 100), 0.5);
    assert_eq!(metric_meter_fraction("—", 100), 0.0);
    assert_eq!(metric_meter_fraction("120%", 100), 1.0);
}

// The reading and its meter share one native control. A text change paints
// only that control and the system accessibility provider keeps its text.
unsafe extern "system" fn metric_meter_paint(
    hwnd: HWND, msg: u32, wp: usize, lp: isize, id: usize, scale: usize,
) -> isize {
    unsafe {
        if matches!(msg, WM_PAINT | WM_PRINTCLIENT) {
            let mut ps: PAINTSTRUCT = zeroed();
            let dc = if msg == WM_PAINT { BeginPaint(hwnd, &mut ps) } else { wp as HDC };
            let saved = SaveDC(dc);
            let mut rect: RECT = zeroed();
            GetClientRect(hwnd, &mut rect);
            fill(dc, &rect, surface());
            let dpi = GetDpiForWindow(hwnd).max(96) as i32;
            let meter_width = 32 * dpi / 96;
            let meter_height = 5 * dpi / 96;
            let top = (rect.bottom - meter_height) / 2;
            let track = RECT {
                left: rect.right - meter_width,
                top,
                right: rect.right,
                bottom: top + meter_height,
            };
            let reading = get_text(hwnd);
            fill(dc, &track, line());
            let filled = (meter_width as f32 * metric_meter_fraction(&reading, scale)).round() as i32;
            if filled > 0 {
                fill(dc, &RECT { right: track.left + filled, ..track }, accent());
            }
            rect.right = track.left - 5 * dpi / 96;
            SelectObject(dc, SendMessageW(hwnd, WM_GETFONT, 0, 0) as HFONT);
            SetTextColor(dc, ink());
            SetBkMode(dc, TRANSPARENT as i32);
            DrawTextW(dc, wide(&reading).as_ptr(), -1, &mut rect,
                DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
            RestoreDC(dc, saved);
            if msg == WM_PAINT { EndPaint(hwnd, &ps); }
            return 0;
        }
        if msg == WM_ERASEBKGND { return 1; }
        if msg == WM_NCDESTROY { RemoveWindowSubclass(hwnd, Some(metric_meter_paint), id); }
        let result = DefSubclassProc(hwnd, msg, wp, lp);
        if msg == WM_SETTEXT && result != 0 {
            InvalidateRect(hwnd, null(), 0);
        }
        result
    }
}

fn field_available(model: &UiModel, key: &str, capability: &str) -> bool {
    model.capabilities.get(key).map(|cap| cap.available)
        .unwrap_or_else(|| capability.is_empty() || model.can(capability))
}

const GPU_PROCESS_SELECTOR: &str = "selection.gpu.processes";

fn collection_choices(model: &UiModel, key: &str) -> Vec<UiChoice> {
    model.collections.get(key).into_iter().flatten().map(|item| {
        let name = if item.detail.is_empty() {
            item.label.clone()
        } else {
            format!("{} · {}", item.label, item.detail)
        };
        UiChoice::new(name, item.id.clone())
    }).collect()
}

fn gpu_process_choices(model: &UiModel) -> Vec<UiChoice> {
    let mut choices = collection_choices(model, "gpu.processes");
    let prompt = if !choices.is_empty() {
        "请选择"
    } else if model.collections.contains_key("gpu.processes") {
        "无占用应用"
    } else {
        "尚未读取"
    };
    choices.insert(0, UiChoice::new(super::i18n::tr_static(prompt), Value::Null));
    choices
}

fn field_options(model: &UiModel, field: &catalog::Field) -> Option<Vec<UiChoice>> {
    match &field.kind {
        FieldKind::Choice(fallback) => {
            let mut options = model.options.get(field.key).unwrap_or(fallback).clone();
            if field.key == "packages.sort" {
                for option in &mut options {
                    option.label = super::i18n::tr(&option.label).into_owned();
                }
            }
            Some(options)
        }
        FieldKind::Collection(key) => Some(collection_choices(model, key)),
        _ => None,
    }
}

fn choice_matches(option: &UiChoice, value: &Value) -> bool {
    option.value == *value || (!option.value.is_null() && !value.is_null() && scalar(&option.value) == scalar(value))
}

fn selected_choice(options: &[UiChoice], value: &Value) -> usize {
    options.iter().position(|option| choice_matches(option, value))
        .or_else(|| options.iter().position(|option| option.value.is_null()))
        .unwrap_or(usize::MAX)
}

fn field_display(options: &[UiChoice], value: &Value, empty: &str) -> String {
    if value.is_null() || value.as_str().is_some_and(|value| value.trim().is_empty()) {
        return empty.to_owned();
    }
    if let Some(option) = options.iter().find(|option| choice_matches(option, value)) {
        return option.label.clone();
    }
    match value.as_bool() {
        Some(true) => super::i18n::tr("开启").into_owned(),
        Some(false) => super::i18n::tr("关闭").into_owned(),
        None => scalar(value),
    }
}

fn field_value(model: &UiModel, key: &str) -> Value {
    if key == "gpu.dashboard_state" {
        let now = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() };
        return json!(super::status_popup::dashboard_gpu_state(model, now));
    }
    model.value(key)
}

fn backend_state(model: &UiModel) -> u8 {
    if !model.value("backend.connection_failed").is_null() || !model.value("backend.terminal_error").is_null() { 2 }
    else if model.value("backend.ready").as_bool() == Some(true) { 1 }
    else { 0 }
}

fn field_empty_text(model: &UiModel, field: &catalog::Field) -> String {
    let state = backend_state(model);
    let caption = match &field.kind {
        FieldKind::Collection(key) if *key == "gpu.processes" => {
            if model.collections.contains_key(*key) { "无占用应用" } else { "尚未读取" }
        }
        FieldKind::Collection(_) => "暂无项目",
        _ if state == 2 => "连接中断",
        _ if state == 0 => "正在读取…",
        _ if !field_available(model, field.key, field.capability) => "不可用",
        _ => "暂无数据",
    };
    super::i18n::tr(caption).into_owned()
}

fn status_caption(model: &UiModel) -> &str {
    if model.value("backend.read_only").as_bool() == Some(true) && backend_state(model) == 1 {
        super::i18n::tr_static("只读预览 · 可查看设备状态")
    } else {
        &model.notice
    }
}

fn toolbar_available(model: &UiModel, id: u16, command: &str) -> bool {
    (id == ID_THEME && model.value("backend.read_only").as_bool() == Some(true)) || model.can(command)
}

fn field_help(model: &UiModel, field: &catalog::Field) -> Option<String> {
    model.capabilities.get(field.key)
        .filter(|cap| !cap.available)
        .map(|cap| cap.reason.clone())
        .or_else(|| (!field.capability.is_empty() && !model.can(field.capability))
            .then(|| model.unavailable_reason(field.capability)))
        .filter(|reason| !reason.is_empty())
}

fn action_disabled_reason(model: &UiModel, action: &Action) -> Option<String> {
    (!action.capability.is_empty() && !model.can(action.capability))
        .then(|| model.unavailable_reason(action.capability))
}

#[derive(PartialEq)]
struct DashboardFieldStructure {
    options: Option<Vec<UiChoice>>,
    available: bool,
    bounds: Option<(i32, i32)>,
    empty: String,
    help: Option<String>,
}

/// Compare the data used to create dashboard controls. Documents and choices
/// for other pages can change while this page retains its existing windows.
#[derive(PartialEq)]
struct DashboardStructure {
    sections: Vec<catalog::Section>,
    fields: Vec<DashboardFieldStructure>,
    action_disabled_reasons: Vec<Option<String>>,
    compact: bool,
    sensors_enabled: bool,
    new_sensor_dashboard: bool,
    dashboard_settings: Option<Value>,
    sensor_settings: Option<Value>,
}
impl DashboardStructure {
    fn new(model: &UiModel, sections: &[catalog::Section]) -> Self {
        Self {
            sections: sections.to_vec(),
            fields: sections.iter().flat_map(|section| &section.fields).map(|field| {
                let process_selector = field.key == GPU_PROCESS_SELECTOR;
                DashboardFieldStructure {
                    options: if process_selector { None } else { field_options(model, field) },
                    available: field_available(model, field.key, field.capability),
                    bounds: model.ranges.get(field.key)
                        .filter(|_| matches!(field.kind, FieldKind::Number))
                        .map(|range| (range.minimum, range.maximum)),
                    empty: if process_selector { String::new() } else { field_empty_text(model, field) },
                    help: field_help(model, field),
                }
            }).collect(),
            action_disabled_reasons: sections.iter().flat_map(|section| &section.actions)
                .map(|action| action_disabled_reason(model, action)).collect(),
            compact: model.preferences.compact,
            sensors_enabled: model.value("sensors.enabled").as_bool() == Some(true),
            new_sensor_dashboard: model.value("sensors.new_dashboard").as_bool() == Some(true),
            dashboard_settings: model.documents.get("dashboard").map(|doc| doc.value.clone()),
            sensor_settings: model.documents.get("sensor_dashboard").map(|doc| doc.value.clone()),
        }
    }
}
static REQUEST: AtomicU64 = AtomicU64::new(1);
#[derive(Debug, PartialEq, Eq)]
struct RestoredWindow {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}
fn window_restore_preferences(model: &std::sync::Mutex<UiModel>) -> Option<(bool, Value)> {
    let model = model.lock().ok()?;
    let geometry = model.values.get("app.window")?;
    if !geometry.is_object() {
        return None;
    }
    Some((
        model.values.get("app.remember_position")?.as_bool()?,
        geometry.clone(),
    ))
}
fn saved_window_size(geometry: &Value) -> Option<(f64, f64)> {
    let width = geometry.pointer("/size/Width")?.as_f64()?;
    let height = geometry.pointer("/size/Height")?.as_f64()?;
    (width.is_finite()
        && height.is_finite()
        && width > 0.
        && height > 0.
        && width <= 8192.
        && height <= 8192.)
        .then_some((width, height))
}
fn saved_window_position(geometry: &Value, remember: bool) -> Option<(f64, f64)> {
    if !remember {
        return None;
    }
    let left = geometry.pointer("/position/Left")?.as_f64()?;
    let top = geometry.pointer("/position/Top")?.as_f64()?;
    (left.is_finite()
        && top.is_finite()
        && (-100000. ..=100000.).contains(&left)
        && (-100000. ..=100000.).contains(&top))
    .then_some((left, top))
}
/// Saved size is independent of the position preference, as in the original
/// WPF RestoreSize. Without a usable remembered position, center on the primary
/// work area; all actual placement remains within the selected monitor's work.
fn restored_window(
    geometry: &Value,
    remember: bool,
    dpi: u32,
    work: RECT,
) -> Option<RestoredWindow> {
    let (width, height) = saved_window_size(geometry)?;
    let work_width = work.right.checked_sub(work.left).filter(|n| *n > 0)?;
    let work_height = work.bottom.checked_sub(work.top).filter(|n| *n > 0)?;
    let scale = dpi.max(96) as f64 / 96.;
    let width = (width * scale).round().clamp(
        (650. * scale).round().min(work_width as f64),
        work_width as f64,
    ) as i32;
    let height = (height * scale).round().clamp(
        (600. * scale).round().min(work_height as f64),
        work_height as f64,
    ) as i32;
    let (left, top) = saved_window_position(geometry, remember)
        .map(|(x, y)| ((x * scale).round(), (y * scale).round()))
        .unwrap_or_else(|| {
            (
                work.left as f64 + (work_width - width) as f64 / 2.,
                work.top as f64 + (work_height - height) as f64 / 2.,
            )
        });
    Some(RestoredWindow {
        left: left.clamp(work.left as f64, (work.right - width) as f64) as i32,
        top: top.clamp(work.top as f64, (work.bottom - height) as f64) as i32,
        width,
        height,
    })
}
fn window_save_payload(rect: RECT, dpi: u32, remember: bool) -> Option<Value> {
    if dpi == 0 {
        return None;
    }
    let scale = 96. / dpi as f64;
    let width = rect.right.checked_sub(rect.left)? as f64 * scale;
    let height = rect.bottom.checked_sub(rect.top)? as f64 * scale;
    if width <= 0. || height <= 0. || width > 8192. || height > 8192. {
        return None;
    }
    let mut geometry = json!({"width":width,"height":height});
    if remember {
        geometry["left"] = json!(rect.left as f64 * scale);
        geometry["top"] = json!(rect.top as f64 * scale);
    }
    Some(json!({"app.window":geometry}))
}
#[cfg(test)]
mod window_layout_tests {
    use super::*;
    fn work() -> RECT {
        RECT {
            left: 0,
            top: 40,
            right: 1920,
            bottom: 1080,
        }
    }
    #[test]
    fn saved_size_restores_without_remembered_or_available_position() {
        let mut full = UiModel::default();
        full.values
            .insert("app.remember_position".into(), json!(false));
        full.values.insert(
            "app.window".into(),
            json!({"size":{"Width":1000,"Height":650}}),
        );
        assert!(
            window_restore_preferences(&std::sync::Mutex::new(full.realtime_snapshot())).is_none(),
            "lightweight refresh intentionally omits object metadata; restore must use the full model fallback"
        );
        assert!(window_restore_preferences(&std::sync::Mutex::new(UiModel::default())).is_none());
        let (remember, geometry) =
            window_restore_preferences(&std::sync::Mutex::new(full)).unwrap();
        assert!(!remember);
        assert_eq!(
            restored_window(&geometry, remember, 96, work())
                .unwrap()
                .width,
            1000
        );
        let expected = RestoredWindow {
            left: 460,
            top: 235,
            width: 1000,
            height: 650,
        };
        let with_position =
            json!({"size":{"Width":1000,"Height":650},"position":{"Left":1700,"Top":800}});
        assert_eq!(
            restored_window(&with_position, false, 96, work()),
            Some(expected)
        );
        for geometry in [
            json!({"size":{"Width":1000,"Height":650},"position":null}),
            json!({"size":{"Width":1000,"Height":650}}),
            json!({"size":{"Width":1000,"Height":650},"position":{"Left":40}}),
        ] {
            let bounds = restored_window(&geometry, true, 96, work()).unwrap();
            assert_eq!(
                (bounds.left, bounds.top, bounds.width, bounds.height),
                (460, 235, 1000, 650)
            );
        }
    }
    #[test]
    fn remembered_position_is_clamped_to_the_selected_workarea() {
        let work = RECT {
            left: -1920,
            top: -100,
            right: 0,
            bottom: 980,
        };
        let geometry =
            json!({"size":{"Width":1000,"Height":650},"position":{"Left":-100,"Top":1000}});
        assert_eq!(
            restored_window(&geometry, true, 96, work),
            Some(RestoredWindow {
                left: -1000,
                top: 330,
                width: 1000,
                height: 650
            })
        );
        let geometry =
            json!({"size":{"Width":1000,"Height":650},"position":{"Left":-3000,"Top":-900}});
        assert_eq!(
            restored_window(&geometry, true, 96, work),
            Some(RestoredWindow {
                left: -1920,
                top: -100,
                width: 1000,
                height: 650
            })
        );
    }
    #[test]
    fn invalid_size_does_not_restore_or_move_the_existing_window() {
        for size in [
            Value::Null,
            json!([]),
            json!({"Width":1000}),
            json!({"Width":0,"Height":650}),
            json!({"Width":-1,"Height":650}),
            json!({"Width":"NaN","Height":650}),
            json!({"Width":8193,"Height":650}),
        ] {
            let geometry = json!({"size":size,"position":{"Left":20,"Top":40}});
            assert!(restored_window(&geometry, true, 96, work()).is_none());
            assert!(restored_window(&geometry, false, 96, work()).is_none());
        }
        let enormous: Value =
            serde_json::from_str(r#"{"size":{"Width":1e9999,"Height":650}}"#).unwrap();
        assert!(restored_window(&enormous, true, 96, work()).is_none());
    }
    #[test]
    fn saved_size_scales_at_fractional_dpi_and_fits_tiny_workareas() {
        let geometry = json!({"size":{"Width":799.5,"Height":601.5}});
        let work = RECT {
            left: -1280,
            top: 60,
            right: 0,
            bottom: 780,
        };
        for (dpi, width, height) in [
            (96, 800, 602),
            (120, 999, 720),
            (144, 1199, 720),
            (192, 1280, 720),
        ] {
            let bounds = restored_window(&geometry, false, dpi, work).unwrap();
            assert_eq!((bounds.width, bounds.height), (width, height));
            assert!(bounds.left >= work.left && bounds.top >= work.top);
            assert!(
                bounds.left + bounds.width <= work.right
                    && bounds.top + bounds.height <= work.bottom
            );
        }
        let tiny = RECT {
            left: 10,
            top: 20,
            right: 330,
            bottom: 260,
        };
        assert_eq!(
            restored_window(&geometry, false, 192, tiny),
            Some(RestoredWindow {
                left: 10,
                top: 20,
                width: 320,
                height: 240
            })
        );
        assert!(
            restored_window(
                &geometry,
                false,
                96,
                RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 100
                }
            )
            .is_none()
        );
    }
    #[test]
    fn saving_size_is_independent_of_whether_position_is_remembered() {
        let rect = RECT {
            left: 900,
            top: 60,
            right: 2400,
            bottom: 1035,
        };
        assert_eq!(
            window_save_payload(rect, 144, false),
            Some(json!({"app.window":{"width":1000.,"height":650.}}))
        );
        assert_eq!(
            window_save_payload(rect, 144, true),
            Some(json!({"app.window":{"left":600.,"top":40.,"width":1000.,"height":650.}}))
        );
        assert!(window_save_payload(rect, 0, false).is_none());
        assert!(
            window_save_payload(
                RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 600
                },
                96,
                false
            )
            .is_none()
        );
        assert!(
            window_save_payload(
                RECT {
                    left: 0,
                    top: 0,
                    right: 9000,
                    bottom: 600
                },
                96,
                false
            )
            .is_none()
        );
    }
}
fn navigation_target(page: Page, key: u32, ctrl: bool, shift: bool) -> Option<Page> {
    if !ctrl {
        return None;
    }
    if (0x31..=0x39).contains(&key) {
        return Page::ALL.get((key - 0x31) as usize).copied();
    }
    if key == 0x09 {
        let active = active_navigation(page);
        let count = main_nav().count();
        let index = main_nav().position(|(_, candidate)| candidate == active).unwrap_or(0);
        let next = (index + if shift { count - 1 } else { 1 }) % count;
        return main_nav().nth(next).map(|(_, page)| page);
    }
    None
}
#[cfg(test)]
mod navigation_tests {
    use super::*;
    #[test]
    fn ctrl_navigation_wraps_and_plain_editing_keys_are_untouched() {
        assert_eq!(
            navigation_target(Page::Dashboard, 9, true, true),
            Some(Page::About)
        );
        assert_eq!(
            navigation_target(Page::About, 9, true, false),
            Some(Page::Dashboard)
        );
        assert_eq!(navigation_target(Page::Dashboard, 9, true, false), Some(Page::Battery));
        assert_eq!(navigation_target(Page::Dashboard, 0x32, true, false), Some(Page::Keyboard));
        assert_eq!(
            navigation_target(Page::Battery, 0x31, true, false),
            Some(Page::Dashboard)
        );
        assert_eq!(
            navigation_target(Page::Dashboard, 0x39, true, false),
            Some(Page::About)
        );
        assert_eq!(navigation_target(Page::Dashboard, 0x31, false, false), None);
    }
    #[test]
    fn semantic_control_ids_are_unique_for_all_main_pages_and_settings_categories() {
        for page in Page::ALL {
            for tab in 0..if page == Page::Settings { 8 } else { 1 } {
                let scope = if page == Page::Settings {
                    format!("main.settings.{}", TOOL_KEYS[tab])
                } else {
                    format!("main.{}", page.key())
                };
                let mut ids = BTreeSet::new();
                for section in catalog::sections(page, tab, &UiModel::default()) {
                    let section_key = section
                        .fields
                        .first()
                        .map(|field| field.key.to_owned())
                        .or_else(|| section.actions.first().map(action_key))
                        .unwrap_or_else(|| "content".into());
                    let section_scope = a11y::key(&scope, "section", &section_key);
                    for field in section.fields {
                        assert!(
                            ids.insert(a11y::key(&scope, "field", field.key)),
                            "duplicate field ID in {scope}: {}",
                            field.key
                        );
                    }
                    for action in section.actions {
                        assert!(
                            ids.insert(a11y::key(&section_scope, "action", &action_key(&action))),
                            "duplicate action ID in {scope}"
                        );
                    }
                }
            }
        }
    }
}

pub(super) fn request_id() -> u64 {
    REQUEST.fetch_add(1, Ordering::Relaxed)
}
pub(super) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
pub(super) fn snapshot(bridge: &UiBridge) -> UiModel {
    bridge.model.lock().map(|m| m.clone()).unwrap_or_else(|_| {
        let mut model = UiModel::default();
        model.notice = "后台状态暂不可读，请重新打开拯救者工具箱。".into();
        model
    })
}
pub(super) fn live_snapshot(bridge: &UiBridge) -> UiModel {
    bridge
        .model
        .lock()
        .map(|m| m.realtime_snapshot())
        .unwrap_or_else(|_| UiModel::default())
}
pub(super) fn send(bridge: &UiBridge, action: UiAction) -> Result<()> {
    bridge
        .actions
        .try_send(action)
        .map_err(|error| anyhow!("命令尚未提交：{error}。正在编辑的内容已保留。"))
}
pub(super) unsafe fn text(hwnd: HWND, value: &str) {
    unsafe {
        SetWindowTextW(hwnd, wide(value).as_ptr());
    }
}
pub(super) unsafe fn get_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd).max(0) as usize;
        let mut buffer = vec![0u16; len + 1];
        let count = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        String::from_utf16_lossy(&buffer[..count.max(0) as usize])
    }
}
pub(super) fn scalar(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}
unsafe fn text_if_changed(hwnd: HWND, value: &str) -> bool {
    unsafe {
        if hwnd.is_null() || get_text(hwnd) == value {
            return false;
        }
        text(hwnd, value);
        true
    }
}
unsafe fn enable_if_changed(hwnd: HWND, enabled: bool) {
    unsafe {
        if !hwnd.is_null() && (IsWindowEnabled(hwnd) != 0) != enabled {
            EnableWindow(hwnd, enabled as i32);
        }
    }
}
pub(super) unsafe fn message(hwnd: HWND, title: &str, body: &str, error: bool) {
    unsafe {
        MessageBoxW(
            hwnd,
            wide(&super::i18n::tr(body)).as_ptr(),
            wide(&super::i18n::tr(title)).as_ptr(),
            MB_OK
                | if error {
                    MB_ICONERROR
                } else {
                    MB_ICONINFORMATION
                },
        );
    }
}
pub(super) unsafe fn confirm(hwnd: HWND, body: &str) -> bool {
    unsafe {
        MessageBoxW(
            hwnd,
            wide(&super::i18n::tr(body)).as_ptr(),
            wide(&super::i18n::tr("拯救者工具箱 · 确认操作")).as_ptr(),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
        ) == IDYES
    }
}
pub(super) unsafe fn create(
    parent: HWND,
    class: &str,
    label: &str,
    style: u32,
    id: u16,
    rect: [i32; 4],
    font: HFONT,
) -> HWND {
    unsafe { create_control(parent, class, label, style | WS_VISIBLE, id, rect, font) }
}

unsafe fn create_control(
    parent: HWND,
    class: &str,
    label: &str,
    style: u32,
    id: u16,
    rect: [i32; 4],
    font: HFONT,
) -> HWND {
    unsafe {
        let hwnd = CreateWindowExW(
            0,
            wide(class).as_ptr(),
            wide(label).as_ptr(),
            WS_CHILD | style,
            rect[0],
            rect[1],
            rect[2],
            rect[3],
            parent,
            id as usize as HMENU,
            GetModuleHandleW(null()),
            null(),
        );
        if !hwnd.is_null() {
            SendMessageW(hwnd, WM_SETFONT, font as usize, 1);
            if class == "STATIC" {
                install_label_paint(hwnd);
            }
            if class == "BUTTON" && style & BS_TYPEMASK as u32 == BS_OWNERDRAW as u32 {
                SetWindowSubclass(hwnd, Some(button_hover), 0x4A494B53, 0);
            } else if class == "BUTTON" && style & BS_TYPEMASK as u32 == BS_AUTOCHECKBOX as u32 {
                SetWindowSubclass(hwnd, Some(toggle_paint), 0x4A49544F, 0);
            }
        }
        hwnd
    }
}
pub(super) unsafe fn install_label_paint(hwnd: HWND) {
    unsafe {
        presentation_flag(hwnd, "Jiks.Text", true);
    }
}
/// Keep the native checkbox's state, keyboard handling and UIA Toggle provider.
/// Only its paint is replaced, so BM_CLICK/BM_GETCHECK retain normal semantics.
unsafe extern "system" fn toggle_paint(
    hwnd: HWND,
    msg: u32,
    wp: usize,
    lp: isize,
    _: usize,
    _: usize,
) -> isize {
    unsafe {
        if msg == WM_PAINT || msg == WM_PRINTCLIENT {
            let mut ps: PAINTSTRUCT = zeroed();
            let dc = if msg == WM_PAINT {
                BeginPaint(hwnd, &mut ps)
            } else {
                wp as HDC
            };
            let saved_dc = SaveDC(dc);
            let mut rect: RECT = zeroed();
            GetClientRect(hwnd, &mut rect);
            let on_surface = !GetPropW(hwnd, wide("Jiks.Surface").as_ptr()).is_null();
            fill(dc, &rect, if on_surface { surface() } else { paper() });
            let dpi = GetDpiForWindow(hwnd).max(96) as i32;
            let s = |n: i32| n * dpi / 96;
            let checked = SendMessageW(hwnd, BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
            let enabled = IsWindowEnabled(hwnd) != 0;
            let top = (rect.bottom - s(20)) / 2;
            let color = if enabled && checked {
                accent()
            } else {
                muted()
            };
            let pen = CreatePen(PS_SOLID, 1, color);
            let old_pen = SelectObject(dc, pen);
            let brush = CreateSolidBrush(if checked && enabled {
                accent()
            } else {
                surface()
            });
            let old_brush = SelectObject(dc, brush);
            RoundRect(dc, s(2), top, s(42), top + s(20), s(20), s(20));
            SelectObject(dc, old_brush);
            DeleteObject(brush);
            let knob = CreateSolidBrush(if checked && enabled {
                0x00202020
            } else {
                muted()
            });
            let old_brush = SelectObject(dc, knob);
            let x = if checked { s(25) } else { s(6) };
            Ellipse(dc, x, top + s(4), x + s(12), top + s(16));
            SelectObject(dc, old_brush);
            SelectObject(dc, old_pen);
            DeleteObject(knob);
            DeleteObject(pen);
            let caption = get_text(hwnd);
            if !caption.is_empty() {
                let old_font = SelectObject(dc, SendMessageW(hwnd, WM_GETFONT, 0, 0) as HGDIOBJ);
                SetBkMode(dc, TRANSPARENT as i32);
                SetTextColor(dc, if enabled { ink() } else { muted() });
                let mut label = RECT {
                    left: s(52),
                    ..rect
                };
                DrawTextW(
                    dc,
                    wide(&caption).as_ptr(),
                    -1,
                    &mut label,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                );
                SelectObject(dc, old_font);
            }
            if GetFocus() == hwnd {
                let mut focus = rect;
                InflateRect(&mut focus, -1, -1);
                DrawFocusRect(dc, &focus);
            }
            RestoreDC(dc, saved_dc);
            if msg == WM_PAINT {
                EndPaint(hwnd, &ps);
            }
            return 0;
        }
        if msg == WM_ERASEBKGND {
            return 1;
        }
        if msg == WM_NCDESTROY {
            RemoveWindowSubclass(hwnd, Some(toggle_paint), 0x4A49544F);
        }
        let result = DefSubclassProc(hwnd, msg, wp, lp);
        if matches!(
            msg,
            BM_SETCHECK | BM_CLICK | WM_ENABLE | WM_SETFOCUS | WM_KILLFOCUS
        ) {
            InvalidateRect(hwnd, null(), 0);
        }
        result
    }
}
pub(super) unsafe fn presentation_flag(hwnd: HWND, name: &str, enabled: bool) {
    unsafe {
        if enabled {
            // This subclass owns only these fixed presentation properties.
            // It also covers STATIC/EDIT controls that have no paint subclass.
            if SetWindowSubclass(hwnd, Some(presentation_cleanup), 0x4A495052, 0) != 0 {
                SetPropW(hwnd, wide(name).as_ptr(), 1usize as HANDLE);
            }
        } else {
            RemovePropW(hwnd, wide(name).as_ptr());
        }
    }
}
unsafe extern "system" fn presentation_cleanup(
    hwnd: HWND,
    msg: u32,
    wp: usize,
    lp: isize,
    _: usize,
    _: usize,
) -> isize {
    unsafe {
        if msg == WM_PRINT || msg == WM_PRINTCLIENT {
            let dc = wp as HDC;
            let saved = SaveDC(dc);
            let region = CreateRectRgn(0, 0, 0, 0);
            if GetWindowRgn(hwnd, region) != ERROR {
                let mut bounds: RECT = zeroed();
                GetRgnBox(region, &mut bounds);
                // IntersectClipRect transforms logical coordinates through the
                // print DC's current viewport. ExtSelectClipRgn instead expects
                // device coordinates and can clip a different child here.
                IntersectClipRect(dc, bounds.left, bounds.top, bounds.right, bounds.bottom);
            }
            if !GetPropW(hwnd, wide("Jiks.Text").as_ptr()).is_null() {
                let surface_label = !GetPropW(hwnd, wide("Jiks.Surface").as_ptr()).is_null();
                let muted_label = !GetPropW(hwnd, wide("Jiks.Muted").as_ptr()).is_null();
                let color = label_background(hwnd, surface_label);
                let transparent = !surface_label
                    && !GetPropW(GetParent(hwnd), wide("Jiks.Background").as_ptr()).is_null();
                let mut rect: RECT = zeroed();
                GetClientRect(hwnd, &mut rect);
                if !transparent {
                    FillRect(dc, &rect, presentation_brush(color));
                }
                SetBkColor(dc, color);
                SetBkMode(
                    dc,
                    if transparent {
                        TRANSPARENT as i32
                    } else {
                        OPAQUE as i32
                    },
                );
                SetTextColor(
                    dc,
                    if muted_label || IsWindowEnabled(hwnd) == 0 {
                        muted()
                    } else {
                        ink()
                    },
                );
                SelectObject(dc, SendMessageW(hwnd, WM_GETFONT, 0, 0) as HGDIOBJ);
            }
            let result = DefSubclassProc(hwnd, msg, wp, lp);
            RestoreDC(dc, saved);
            DeleteObject(region);
            return result;
        }
        if msg == WM_NCDESTROY {
            for property in [
                "Jiks.Surface",
                "Jiks.Muted",
                "Jiks.Selected",
                "Jiks.Primary",
                "Jiks.ActionCard",
                "Jiks.Background",
                "Jiks.Text",
                "Jiks.Pane",
            ] {
                RemovePropW(hwnd, wide(property).as_ptr());
            }
            RemoveWindowSubclass(hwnd, Some(presentation_cleanup), 0x4A495052);
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}
unsafe fn label_background(hwnd: HWND, on_surface: bool) -> u32 {
    unsafe {
        if on_surface {
            surface()
        } else if !GetPropW(hwnd, wide("Jiks.Pane").as_ptr()).is_null() {
            pane()
        } else {
            paper()
        }
    }
}

/// Paint-only messages can arrive synchronously while a layout method owns
/// RefMut<App/Editor>. HWND presentation flags avoid reborrowing application
/// state or silently falling back to system-grey controls in that path.
pub(super) unsafe fn presentation_message(msg: u32, wp: usize, lp: isize) -> Option<isize> {
    unsafe {
        match msg {
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
                let hwnd = lp as HWND;
                let dc = wp as HDC;
                let on_surface = !GetPropW(hwnd, wide("Jiks.Surface").as_ptr()).is_null();
                let muted_text = !GetPropW(hwnd, wide("Jiks.Muted").as_ptr()).is_null();
                SetTextColor(dc, if muted_text { muted() } else { ink() });
                SetBkMode(dc, TRANSPARENT as i32);
                let color = label_background(hwnd, on_surface);
                SetBkColor(dc, color);
                SetDCBrushColor(dc, color);
                Some(
                    if msg == WM_CTLCOLORSTATIC
                        && !on_surface
                        && !GetPropW(GetParent(hwnd), wide("Jiks.Background").as_ptr()).is_null()
                    {
                        GetStockObject(NULL_BRUSH) as isize
                    } else {
                        presentation_brush(color) as isize
                    },
                )
            }
            WM_DRAWITEM => {
                let item = &*(lp as *const DRAWITEMSTRUCT);
                if !GetPropW(item.hwndItem, wide("Jiks.Primary").as_ptr()).is_null() {
                    paint_primary_button(item);
                } else {
                    paint_button(
                        item,
                        !GetPropW(item.hwndItem, wide("Jiks.Selected").as_ptr()).is_null(),
                    );
                }
                Some(1)
            }
            _ => None,
        }
    }
}
unsafe extern "system" fn button_hover(
    hwnd: HWND,
    msg: u32,
    wp: usize,
    lp: isize,
    _: usize,
    _: usize,
) -> isize {
    unsafe {
        let property = wide("Jiks.Button.Hover");
        match msg {
            WM_MOUSEMOVE => {
                if GetPropW(hwnd, property.as_ptr()).is_null() {
                    SetPropW(hwnd, property.as_ptr(), 1usize as HANDLE);
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    TrackMouseEvent(&mut track);
                    InvalidateRect(hwnd, null(), 0);
                }
            }
            WM_MOUSELEAVE => {
                RemovePropW(hwnd, property.as_ptr());
                InvalidateRect(hwnd, null(), 0);
            }
            WM_NCDESTROY => {
                RemovePropW(hwnd, property.as_ptr());
                RemoveWindowSubclass(hwnd, Some(button_hover), 0x4A494B53);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}
pub(super) unsafe fn font(dpi: u32, points: i32, bold: bool) -> HFONT {
    unsafe {
        let family = font_family();
        CreateFontW(
            -(points * dpi as i32 / 96),
            0,
            0,
            0,
            if bold { 600 } else { 400 },
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32,
            wide(&family).as_ptr(),
        )
    }
}
unsafe fn exact_font_present(dc: HDC, family: &str) -> bool {
    unsafe {
        struct Probe<'a> {
            family: &'a str,
            present: bool,
        }
        unsafe extern "system" fn found(
            lf: *const LOGFONTW,
            _: *const TEXTMETRICW,
            _: u32,
            param: isize,
        ) -> i32 {
            unsafe {
                let probe = &mut *(param as *mut Probe<'_>);
                let name = (*lf).lfFaceName;
                let end = name.iter().position(|u| *u == 0).unwrap_or(name.len());
                if String::from_utf16_lossy(&name[..end]).eq_ignore_ascii_case(probe.family) {
                    probe.present = true;
                    return 0;
                }
                1
            }
        }
        let mut spec: LOGFONTW = zeroed();
        for (dest, source) in spec.lfFaceName.iter_mut().zip(wide(family)) {
            *dest = source;
        }
        spec.lfCharSet = DEFAULT_CHARSET;
        let mut probe = Probe {
            family,
            present: false,
        };
        EnumFontFamiliesExW(dc, &spec, Some(found), &mut probe as *mut _ as isize, 0);
        probe.present
    }
}
pub(super) unsafe fn preferred_font_available() -> bool {
    unsafe {
        let dc = GetDC(null_mut());
        let found = ["方正公文楷体", "FZGWKaiS"]
            .iter()
            .any(|name| exact_font_present(dc, name));
        ReleaseDC(null_mut(), dc);
        found
    }
}
unsafe fn font_family() -> String {
    FONT_FAMILY.with(|cached| {
        cached
            .borrow_mut()
            .get_or_insert_with(|| unsafe {
                let dc = GetDC(null_mut());
                let family = ["Microsoft YaHei UI", "Segoe UI"]
                    .into_iter()
                    .find(|name| exact_font_present(dc, name))
                    .unwrap_or("Microsoft YaHei UI")
                    .to_owned();
                ReleaseDC(null_mut(), dc);
                family
            })
            .clone()
    })
}
pub(super) unsafe fn fill(dc: HDC, rect: &RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(color);
        FillRect(dc, rect, brush);
        DeleteObject(brush);
    }
}
/// Flat Fluent-style surface shared by the main pages and native dialogs.
/// Painting stays event-driven: no composition/GPU loop or image cache.
pub(super) unsafe fn paint_card(dc: HDC, rect: &RECT, dpi: u32) {
    unsafe {
        let pen = CreatePen(PS_SOLID, 1, line());
        let brush = CreateSolidBrush(surface());
        let old_pen = SelectObject(dc, pen);
        let old_brush = SelectObject(dc, brush);
        let radius = (8 * dpi as i32 / 96).max(2);
        RoundRect(
            dc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            radius,
            radius,
        );
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        DeleteObject(pen);
        DeleteObject(brush);
    }
}
pub(super) unsafe fn paint_button(item: &DRAWITEMSTRUCT, selected: bool) {
    unsafe {
        if !GetPropW(item.hwndItem, windows_sys::w!("Jiks.CaptionTool")).is_null() {
            paint_caption_tool(item);
            return;
        }
        let saved_dc = SaveDC(item.hDC);
        let disabled = item.itemState & ODS_DISABLED != 0;
        let pressed = item.itemState & ODS_SELECTED != 0;
        let main_navigation =
            item.CtlID >= ID_NAV as u32 && item.CtlID < ID_NAV as u32 + Page::ALL.len() as u32;
        let navigation = main_navigation
            || (item.CtlID >= ID_TOOL_NAV as u32 && item.CtlID < ID_TOOL_NAV as u32 + 8);
        let hovered = !GetPropW(item.hwndItem, wide("Jiks.Button.Hover").as_ptr()).is_null();
        let background = if main_navigation && selected {
            surface()
        } else if selected || pressed || (!disabled && hovered) {
            PALETTE.with(|p| p.get().pressed)
        } else if navigation {
            paper()
        } else {
            surface()
        };
        fill(item.hDC, &item.rcItem, if main_navigation { paper() } else { background });
        let pen = CreatePen(PS_SOLID, 1, if !navigation || (main_navigation && selected) { line() } else { background });
        let old_pen = SelectObject(item.hDC, pen);
        let brush = if main_navigation { presentation_brush(background) } else { GetStockObject(NULL_BRUSH) };
        let old_brush = SelectObject(item.hDC, brush);
        let dpi = GetDpiForWindow(item.hwndItem).max(96);
        let corner = (8 * dpi as i32 / 96).max(2);
        RoundRect(
            item.hDC,
            item.rcItem.left,
            item.rcItem.top,
            item.rcItem.right,
            item.rcItem.bottom,
            corner,
            corner,
        );
        SelectObject(item.hDC, old_pen);
        SelectObject(item.hDC, old_brush);
        DeleteObject(pen);
        SetBkMode(item.hDC, TRANSPARENT as i32);
        SetTextColor(item.hDC, if disabled || (navigation && !selected) { muted() } else { ink() });
        let button_font = SendMessageW(item.hwndItem, WM_GETFONT, 0, 0) as HFONT;
        if !button_font.is_null() {
            SelectObject(item.hDC, button_font);
        }
        let label = wide(&get_text(item.hwndItem));
        let mut rect = item.rcItem;
        let action_card = !GetPropW(item.hwndItem, wide("Jiks.ActionCard").as_ptr()).is_null();
        if navigation {
            if selected {
                let width = (3 * dpi as i32 / 96).max(2);
                let margin = ((rect.bottom - rect.top) - 24 * dpi as i32 / 96).max(4) / 2;
                fill(
                    item.hDC,
                    &RECT {
                        left: rect.left + 1,
                        top: rect.top + margin,
                        right: rect.left + width + 1,
                        bottom: rect.bottom - margin,
                    },
                    accent(),
                );
            }
            if main_navigation {
                if selected || item.CtlID == ID_NAV as u32 + 7 {
                    SetTextColor(item.hDC, accent());
                }
                paint_nav_icon(item, item.CtlID - ID_NAV as u32);
                if selected {
                    if item.itemState & ODS_FOCUS != 0 {
                        InflateRect(&mut rect, -4, -4);
                        DrawFocusRect(item.hDC, &rect);
                    }
                    RestoreDC(item.hDC, saved_dc);
                    return;
                }
                SetTextColor(item.hDC, muted());
                rect.top += 29 * dpi as i32 / 96;
            } else {
                // Categories retain the old settings sidebar's icon + title.
                paint_nav_icon(
                    item,
                    [4, 8, 3, 5, 1, 6, 5, 6][(item.CtlID - ID_TOOL_NAV as u32) as usize],
                );
                rect.left += rect.bottom - rect.top + 5;
            }
        } else if action_card {
            paint_nav_icon(item, 6);
            rect.left += 44 * dpi as i32 / 96;
            rect.right -= 30 * dpi as i32 / 96;
            let mut arrow = item.rcItem;
            arrow.left = arrow.right - 30 * dpi as i32 / 96;
            DrawTextW(
                item.hDC,
                wide("›").as_ptr(),
                -1,
                &mut arrow,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }
        DrawTextW(
            item.hDC,
            label.as_ptr(),
            -1,
            &mut rect,
            (if (navigation && !main_navigation) || action_card {
                DT_LEFT
            } else {
                DT_CENTER
            }) | DT_VCENTER
                | DT_SINGLELINE
                | DT_END_ELLIPSIS,
        );
        if item.itemState & ODS_FOCUS != 0 {
            InflateRect(&mut rect, -3, -3);
            DrawFocusRect(item.hDC, &rect);
        }
        RestoreDC(item.hDC, saved_dc);
    }
}

unsafe fn paint_caption_tool(item: &DRAWITEMSTRUCT) {
    unsafe {
        let dc = item.hDC;
        let saved = SaveDC(dc);
        let mut rect = item.rcItem;
        let enabled = item.itemState & ODS_DISABLED == 0;
        let active = enabled && (item.itemState & ODS_SELECTED != 0
            || !GetPropW(item.hwndItem, windows_sys::w!("Jiks.Button.Hover")).is_null());
        let background = if active { PALETTE.with(|palette| palette.get().pressed) } else { paper() };
        let color = if enabled { ink() } else { muted() };
        fill(dc, &rect, paper());
        SelectObject(dc, GetStockObject(DC_PEN));
        SelectObject(dc, GetStockObject(DC_BRUSH));
        SetDCPenColor(dc, line());
        SetDCBrushColor(dc, background);
        let dpi = GetDpiForWindow(item.hwndItem).max(96) as i32;
        let rounding = (6 * dpi / 96).max(2);
        RoundRect(dc, rect.left, rect.top, rect.right, rect.bottom, rounding, rounding);
        if item.CtlID == ID_THEME as u32 {
            let diameter = 13 * dpi / 96;
            let left = (rect.left + rect.right - diameter) / 2;
            let top = (rect.top + rect.bottom - diameter) / 2;
            let middle = top + diameter / 2;
            let right = left + diameter;
            let bottom = top + diameter;
            SetDCPenColor(dc, color);
            Ellipse(dc, left, top, right, bottom);
            SetDCBrushColor(dc, color);
            Chord(dc, left, top, right, bottom, left, middle, right, middle);
        } else {
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, color);
            let font = SendMessageW(item.hwndItem, WM_GETFONT, 0, 0) as HFONT;
            if !font.is_null() { SelectObject(dc, font); }
            DrawTextW(dc, wide(&get_text(item.hwndItem)).as_ptr(), -1, &mut rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
        }
        if item.itemState & ODS_FOCUS != 0 {
            InflateRect(&mut rect, -3, -3);
            DrawFocusRect(dc, &rect);
        }
        RestoreDC(dc, saved);
    }
}
pub(super) unsafe fn paint_primary_button(item: &DRAWITEMSTRUCT) {
    unsafe {
        let saved_dc = SaveDC(item.hDC);
        let disabled = item.itemState & ODS_DISABLED != 0;
        let background = if disabled { line() } else { accent() };
        fill(item.hDC, &item.rcItem, label_background(item.hwndItem, false));
        let brush = CreateSolidBrush(background);
        let pen = CreatePen(PS_SOLID, 1, background);
        let old_brush = SelectObject(item.hDC, brush);
        let old_pen = SelectObject(item.hDC, pen);
        let radius = (8 * GetDpiForWindow(item.hwndItem).max(96) as i32 / 96).max(2);
        RoundRect(
            item.hDC,
            item.rcItem.left,
            item.rcItem.top,
            item.rcItem.right,
            item.rcItem.bottom,
            radius,
            radius,
        );
        SelectObject(item.hDC, old_brush);
        SelectObject(item.hDC, old_pen);
        DeleteObject(brush);
        DeleteObject(pen);
        SetBkMode(item.hDC, TRANSPARENT as i32);
        let channel = |shift| {
            let srgb = ((background >> shift) & 255u32) as f32 / 255.0;
            if srgb <= 0.04045 { srgb / 12.92 } else { ((srgb + 0.055) / 1.055).powf(2.4) }
        };
        let luminance = 0.2126 * channel(0) + 0.7152 * channel(8) + 0.0722 * channel(16);
        SetTextColor(item.hDC, if luminance >= 0.179_129 { 0 } else { 0x00ffffff });
        let font = SendMessageW(item.hwndItem, WM_GETFONT, 0, 0) as HFONT;
        if !font.is_null() { SelectObject(item.hDC, font); }
        let mut rect = item.rcItem;
        DrawTextW(
            item.hDC,
            wide(&get_text(item.hwndItem)).as_ptr(),
            -1,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        if item.itemState & ODS_FOCUS != 0 {
            InflateRect(&mut rect, -4, -4);
            DrawFocusRect(item.hDC, &rect);
        }
        RestoreDC(item.hDC, saved_dc);
    }
}

unsafe fn paint_nav_icon(item: &DRAWITEMSTRUCT, index: u32) {
    unsafe {
        let h = (item.rcItem.bottom - item.rcItem.top).max(1);
        let dpi = GetDpiForWindow(item.hwndItem).max(96) as i32;
        let unit = (2 * dpi / 96).max(1);
        let stacked =
            item.CtlID >= ID_NAV as u32 && item.CtlID < ID_NAV as u32 + Page::ALL.len() as u32;
        let selected = stacked && !GetPropW(item.hwndItem, windows_sys::w!("Jiks.Selected")).is_null();
        let x = item.rcItem.left
            + if stacked {
                (item.rcItem.right - item.rcItem.left - 9 * unit) / 2
            } else {
                10 * dpi / 96
            };
        let y = item.rcItem.top
            + if stacked && !selected {
                12 * dpi / 96
            } else {
                (h - 9 * unit) / 2
            };
        let p = |a: i32, b: i32| POINT {
            x: x + a * unit,
            y: y + b * unit,
        };
        let pen = CreatePen(PS_SOLID, unit, GetTextColor(item.hDC));
        let old = SelectObject(item.hDC, pen);
        let brush = SelectObject(item.hDC, GetStockObject(NULL_BRUSH));
        match index {
            0 => {
                let pts = [p(0, 3), p(4, 0), p(8, 3), p(8, 8), p(0, 8), p(0, 3)];
                if selected {
                    SelectObject(item.hDC, GetStockObject(DC_BRUSH));
                    SetDCBrushColor(item.hDC, GetTextColor(item.hDC));
                    Polygon(item.hDC, pts.as_ptr(), pts.len() as i32);
                    let door = RECT { left: x + 3 * unit, top: y + 5 * unit, right: x + 5 * unit, bottom: y + 9 * unit };
                    fill(item.hDC, &door, surface());
                } else {
                    Polyline(item.hDC, pts.as_ptr(), pts.len() as i32);
                    MoveToEx(item.hDC, x + 3 * unit, y + 8 * unit, null_mut());
                    LineTo(item.hDC, x + 3 * unit, y + 5 * unit);
                    LineTo(item.hDC, x + 5 * unit, y + 5 * unit);
                    LineTo(item.hDC, x + 5 * unit, y + 8 * unit);
                }
            }
            1 => {
                Rectangle(item.hDC, x, y + unit, x + 9 * unit, y + 7 * unit);
                for row in [3, 5] {
                    for col in [2, 4, 6] {
                        let q = p(col, row);
                        MoveToEx(item.hDC, q.x, q.y, null_mut());
                        LineTo(item.hDC, q.x + unit, q.y);
                    }
                }
            }
            2 => {
                Rectangle(item.hDC, x, y + 2 * unit, x + 8 * unit, y + 7 * unit);
                Rectangle(
                    item.hDC,
                    x + 8 * unit,
                    y + 3 * unit,
                    x + 10 * unit,
                    y + 6 * unit,
                );
                MoveToEx(item.hDC, x + 2 * unit, y + 4 * unit, null_mut());
                LineTo(item.hDC, x + 6 * unit, y + 4 * unit);
            }
            3 => {
                let pts = [
                    p(5, 0),
                    p(1, 5),
                    p(4, 5),
                    p(3, 9),
                    p(8, 3),
                    p(5, 3),
                    p(5, 0),
                ];
                Polyline(item.hDC, pts.as_ptr(), pts.len() as i32);
            }
            4 => {
                Rectangle(item.hDC, x, y, x + 8 * unit, y + 9 * unit);
                let pts = [p(3, 2), p(6, 4), p(3, 6), p(3, 2)];
                Polyline(item.hDC, pts.as_ptr(), pts.len() as i32);
            }
            5 => {
                let pts = [
                    p(0, 2),
                    p(4, 0),
                    p(9, 2),
                    p(9, 7),
                    p(4, 9),
                    p(0, 7),
                    p(0, 2),
                    p(4, 4),
                    p(9, 2),
                ];
                Polyline(item.hDC, pts.as_ptr(), pts.len() as i32);
                MoveToEx(item.hDC, x + 4 * unit, y + 4 * unit, null_mut());
                LineTo(item.hDC, x + 4 * unit, y + 9 * unit);
            }
            6 => {
                Ellipse(
                    item.hDC,
                    x + 2 * unit,
                    y + 2 * unit,
                    x + 7 * unit,
                    y + 7 * unit,
                );
                for (a, b, c, d) in [
                    (4, 0, 4, 2),
                    (4, 7, 4, 9),
                    (0, 4, 2, 4),
                    (7, 4, 9, 4),
                    (1, 1, 2, 2),
                    (7, 7, 8, 8),
                    (1, 8, 2, 7),
                    (7, 2, 8, 1),
                ] {
                    let q = p(a, b);
                    let r = p(c, d);
                    MoveToEx(item.hDC, q.x, q.y, null_mut());
                    LineTo(item.hDC, r.x, r.y);
                }
            }
            7 => {
                let pts = [
                    p(4, 2),
                    p(2, 0),
                    p(0, 1),
                    p(0, 4),
                    p(4, 8),
                    p(8, 4),
                    p(8, 1),
                    p(6, 0),
                    p(4, 2),
                ];
                Polyline(item.hDC, pts.as_ptr(), pts.len() as i32);
            }
            9 => {
                Ellipse(item.hDC, x, y, x + 9 * unit, y + 9 * unit);
                let from = p(4, 5);
                let to = p(7, 2);
                MoveToEx(item.hDC, from.x, from.y, null_mut());
                LineTo(item.hDC, to.x, to.y);
            }
            10 => {
                let outer = [p(3, 0), p(6, 1), p(8, 4), p(8, 6), p(6, 8), p(3, 9), p(0, 8)];
                let inner = [p(0, 8), p(3, 6), p(4, 3), p(3, 0)];
                Polyline(item.hDC, outer.as_ptr(), outer.len() as i32);
                Polyline(item.hDC, inner.as_ptr(), inner.len() as i32);
            }
            11 => {
                let leaf = [p(4, 0), p(7, 2), p(8, 5), p(7, 7), p(4, 8), p(1, 6), p(1, 4), p(4, 0)];
                Polyline(item.hDC, leaf.as_ptr(), leaf.len() as i32);
                let start = p(4, 5);
                let end = p(4, 10);
                MoveToEx(item.hDC, start.x, start.y, null_mut());
                LineTo(item.hDC, end.x, end.y);
            }
            12 => {
                Rectangle(item.hDC, x + unit, y + unit, x + 8 * unit, y + 8 * unit);
                Rectangle(item.hDC, x + 3 * unit, y + 3 * unit, x + 6 * unit, y + 6 * unit);
                for a in [2, 4, 6] {
                    for (from, to) in [(p(a, 0), p(a, 1)), (p(a, 8), p(a, 9)),
                        (p(0, a), p(1, a)), (p(8, a), p(9, a))] {
                        MoveToEx(item.hDC, from.x, from.y, null_mut());
                        LineTo(item.hDC, to.x, to.y);
                    }
                }
            }
            13 => {
                Rectangle(item.hDC, x + 2 * unit, y + 3 * unit, x + 7 * unit, y + 9 * unit);
                Rectangle(item.hDC, x + 3 * unit, y, x + 6 * unit, y + 3 * unit);
            }
            _ => {
                Ellipse(item.hDC, x, y, x + 9 * unit, y + 9 * unit);
                MoveToEx(item.hDC, x + 4 * unit, y + 4 * unit, null_mut());
                LineTo(item.hDC, x + 4 * unit, y + 7 * unit);
                SetPixel(item.hDC, x + 4 * unit, y + 2 * unit, GetTextColor(item.hDC));
            }
        }
        SelectObject(item.hDC, brush);
        SelectObject(item.hDC, old);
        DeleteObject(pen);
    }
}
#[derive(Clone)]
enum BindingKind {
    Toggle,
    Text,
    Number,
    Choice(Vec<UiChoice>),
    List(Vec<UiChoice>),
    Display {
        options: Vec<UiChoice>,
        empty: String,
    },
}
struct Binding {
    key: String,
    hwnd: HWND,
    kind: BindingKind,
    capability: String,
}
#[derive(Clone, Copy)]
struct CardSurface {
    rect: [i32; 4],
    icon: Option<u32>,
}
struct MetricPlacement {
    name: HWND,
    value: HWND,
    detail: HWND,
    key: String,
    compact: bool,
}
enum DeferredEditor {
    DeviceInformation,
    Structured(catalog::EditorKind, Option<UiItem>),
    SensorSettings,
    Document(String),
    Picker {
        key: String,
        kind: super::selectors::PickerKind,
        initial: String,
    },
}
fn main_picker(key: &str) -> Option<super::selectors::PickerKind> {
    use super::selectors::{FileFilter as F, PickerKind as P};
    match key {
        "appearance.background" | "display.boot_logo.path" => Some(P::Open(F::Image)),
        "packages.directory" => Some(P::Folder),
        "keyboard.color" | "appearance.accent" => Some(P::Color),
        _ => None,
    }
}
fn package_copy_text(model: &UiModel, selected: &str, command: &str) -> Result<String> {
    let package = &model
        .collections
        .get("packages")
        .and_then(|items| items.iter().find(|item| item.id == selected))
        .context("请先选择当前目录中的项目")?
        .value;
    let string = |key: &str| package.get(key).and_then(Value::as_str).unwrap_or("");
    let value = match command {
        "packages.copy.url" => string("file_url").to_owned(),
        "packages.copy.title" => string("title").to_owned(),
        "packages.copy.info" => format!(
            "{}\r\n{}: {}\r\n{}: {}\r\n{}: {}\r\n{}: {}\r\n{}",
            string("title"),
            super::i18n::tr_static("版本"),
            string("version"),
            super::i18n::tr_static("分类"),
            string("category"),
            super::i18n::tr_static("发布日期"),
            string("release_date"),
            super::i18n::tr_static("大小"),
            string("file_size"),
            string("file_url")
        ),
        _ => bail!("未知复制字段"),
    };
    ensure!(
        !value.is_empty() && !value.contains('\0') && value.len() <= 1024 * 1024,
        "项目复制文本为空或无效"
    );
    Ok(value)
}
pub(super) unsafe fn copy_text(owner: HWND, value: &str) -> Result<()> {
    unsafe {
        ensure!(
            !value.is_empty() && !value.contains('\0') && value.len() <= 1024 * 1024,
            "复制文本为空或超过支持范围"
        );
        use windows_sys::Win32::System::{DataExchange::*, Memory::*};
        let value = wide(value);
        let memory = GlobalAlloc(GMEM_MOVEABLE, value.len() * 2);
        ensure!(!memory.is_null(), "无法分配剪贴板文本");
        let result = (|| -> Result<()> {
            let destination = GlobalLock(memory);
            ensure!(!destination.is_null(), "无法锁定剪贴板文本");
            std::ptr::copy_nonoverlapping(value.as_ptr(), destination.cast(), value.len());
            GlobalUnlock(memory);
            ensure!(OpenClipboard(owner) != 0, "剪贴板正由其它程序占用，请重试");
            let copied = (|| -> Result<()> {
                ensure!(EmptyClipboard() != 0, "无法准备剪贴板");
                ensure!(
                    !SetClipboardData(13, memory).is_null(),
                    "无法提交剪贴板文本"
                );
                Ok(())
            })();
            CloseClipboard();
            copied
        })();
        if result.is_err() {
            GlobalFree(memory);
        }
        result
    }
}
struct App {
    last_ui_effect: u64,
    last_native_effect: u64,
    pending_ui_ack: Option<(u64, bool, String, bool, String)>,
    last_visibility: Option<(bool, String, bool, bool)>,
    bridge: UiBridge,
    hwnd: HWND,
    page: Page,
    tools_tab: usize,
    dpi: u32,
    body_font: HFONT,
    heading_font: HFONT,
    title_font: HFONT,
    caption_font: HFONT,
    brush: HBRUSH,
    surface_brush: HBRUSH,
    fixed: Vec<HWND>,
    content: Vec<ScrollChild>,
    bindings: Vec<Binding>,
    buttons: BTreeMap<u16, Action>,
    pickers: BTreeMap<u16, (String, super::selectors::PickerKind)>,
    drafts: BTreeMap<String, Value>,
    dirty: BTreeSet<String>,
    metrics: Vec<MetricPlacement>,
    observed_metric_keys: BTreeSet<String>,
    dashboard_structure: Option<DashboardStructure>,
    pending_process_options: Option<Vec<UiChoice>>,
    cards: Vec<CardSurface>,
    surface_children: BTreeSet<usize>,
    muted_labels: BTreeSet<usize>,
    caption: HWND,
    notice: HWND,
    rendered_revision: u64,
    metadata_revision: u64,
    backend_state: u8,
    scroll: i32,
    scrolling: Scrolling,
    content_height: i32,
    pending: BTreeMap<u64, bool>,
    direct_pending: BTreeMap<u64, String>,
    rendering: bool,
    layout_posted: bool,
    preview_theme: Option<&'static str>,
    feedback_until: Option<Instant>,
    tray: bool,
    tray_data: NOTIFYICONDATAW,
    tray_hover_at: Option<Instant>,
    taskbar_message: u32,
    show_message: u32,
    popup: super::status_popup::StatusPopup,
    column_x: i32,
    started: Instant,
    smoke_submitted: bool,
    smoke_stage: usize,
    language_prompted: bool,
    modal_active: bool,
    deferred_destroy: bool,
    deferred_editor: Option<DeferredEditor>,
    topmost: bool,
    window_restored: bool,
    background: super::background::Background,
    overlay: super::overlay::Overlay,
    notifications: super::notifications::Notifications,
    timer_ms: u32,
}

pub(super) fn run(bridge: UiBridge) -> Result<()> {
    let _accessibility = a11y::UiThread::enter()?;
    #[cfg(debug_assertions)]
    let _trace = super::scroll_trace::Session::start();
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let common = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES | ICC_BAR_CLASSES,
        };
        InitCommonControlsEx(&common);
        let mut font_count = 0u32;
        let font_resource = bridge
            .font_data
            .as_ref()
            .filter(|_| !preferred_font_available())
            .map(|data| {
                AddFontMemResourceEx(
                    data.as_ptr() as *const _,
                    data.len() as u32,
                    null(),
                    &mut font_count,
                )
            });
        apply_palette(&live_snapshot(&bridge), None);
        super::i18n::set_language(
            live_snapshot(&bridge)
                .value("appearance.language")
                .as_str()
                .unwrap_or("zh-Hans"),
        );
        let brush = CreateSolidBrush(paper());
        let class = wide("Jiks.Native.Main");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: GetModuleHandleW(null()),
            hIcon: app_icon(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: GetStockObject(WHITE_BRUSH) as HBRUSH,
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            DeleteObject(brush);
            bail!("无法注册拯救者工具箱窗口：{}", GetLastError());
        }
        let initial_dpi = GetDpiForSystem().max(96);
        let initial_width = (1250 * initial_dpi as i32 / 96)
            .min(GetSystemMetrics(SM_CXSCREEN) - 40)
            .max(650);
        let initial_height = (780 * initial_dpi as i32 / 96)
            .min(GetSystemMetrics(SM_CYSCREEN) - 80)
            .max(460);
        let overlay = super::overlay::Overlay::new(bridge.clone());
        let state = Box::new(RefCell::new(App {
            last_ui_effect: 0,
            last_native_effect: 0,
            pending_ui_ack: None,
            last_visibility: None,
            bridge,
            hwnd: null_mut(),
            page: Page::Dashboard,
            tools_tab: 0,
            dpi: initial_dpi,
            body_font: null_mut(),
            heading_font: null_mut(),
            title_font: null_mut(),
            caption_font: null_mut(),
            brush,
            surface_brush: CreateSolidBrush(surface()),
            fixed: vec![],
            content: vec![],
            bindings: vec![],
            buttons: BTreeMap::new(),
            pickers: BTreeMap::new(),
            drafts: BTreeMap::new(),
            dirty: BTreeSet::new(),
            metrics: vec![],
            observed_metric_keys: BTreeSet::new(),
            dashboard_structure: None,
            pending_process_options: None,
            cards: vec![],
            surface_children: BTreeSet::new(),
            muted_labels: BTreeSet::new(),
            caption: null_mut(),
            notice: null_mut(),
            rendered_revision: u64::MAX,
            metadata_revision: u64::MAX,
            backend_state: u8::MAX,
            scroll: 0,
            scrolling: Scrolling::default(),
            content_height: 0,
            pending: BTreeMap::new(),
            direct_pending: BTreeMap::new(),
            rendering: false,
            layout_posted: false,
            preview_theme: None,
            feedback_until: None,
            tray: false,
            tray_data: zeroed(),
            tray_hover_at: None,
            taskbar_message: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            show_message: RegisterWindowMessageW(wide("Jiks.ShowExisting.1").as_ptr()),
            popup: super::status_popup::StatusPopup::default(),
            column_x: CONTENT_X,
            started: Instant::now(),
            smoke_submitted: false,
            smoke_stage: 0,
            language_prompted: false,
            modal_active: false,
            deferred_destroy: false,
            deferred_editor: None,
            topmost: false,
            window_restored: false,
            background: super::background::Background::default(),
            overlay,
            notifications: super::notifications::Notifications::new(),
            timer_ms: 250,
        }));
        let raw = Box::into_raw(state);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("拯救者工具箱").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_VSCROLL,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            initial_width,
            initial_height,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            raw as *const _,
        );
        if hwnd.is_null() {
            drop(Box::from_raw(raw));
            bail!("无法创建拯救者工具箱窗口：{}", GetLastError());
        }
        if extend_main_caption(hwnd, GetDpiForWindow(hwnd).max(96))
            && SetPropW(hwnd, CAPTION_PROPERTY, 1usize as HANDLE) != 0
        {
            SetWindowPos(hwnd, null_mut(), 0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
        }
        a11y::annotate(hwnd, "main", None, None);
        let instance_key = (*raw).borrow().bridge.instance_key.clone();
        if let Some(key) = instance_key {
            if SetPropW(hwnd, wide(&key).as_ptr(), 1usize as HANDLE) == 0 {
                DestroyWindow(hwnd);
                drop(Box::from_raw(raw));
                bail!("无法标记拯救者工具箱实例窗口");
            }
        }
        let minimized = (*raw).borrow().bridge.start_minimized;
        ShowWindow(
            hwnd,
            if minimized {
                SW_SHOWMINNOACTIVE
            } else {
                SW_SHOWNORMAL
            },
        );
        UpdateWindow(hwnd);
        let mut msg: MSG = zeroed();
        let mut messages = ScrollMessages::default();
        let mut failure = None;
        loop {
            let timer = (*raw).try_borrow().ok().and_then(|app| app.scrolling.wait_timer());
            let Some(result) = messages.next(hwnd, &mut msg, timer) else {
                continue;
            };
            if result == 0 {
                break;
            }
            if result == -1 {
                failure = Some(anyhow!("窗口消息失败：{}", GetLastError()));
                break;
            }
            #[cfg(debug_assertions)]
            if msg.message == WM_MOUSEWHEEL {
                let delta = ((msg.wParam >> 16) as u16) as i16;
                super::scroll_trace::record("input", i64::from(delta), GetDlgCtrlID(msg.hwnd) as i64);
            }
            if msg.message == WM_KEYDOWN && GetAncestor(msg.hwnd, GA_ROOT) == hwnd {
                if let Ok(mut app) = (*raw).try_borrow_mut() {
                    if !app.modal_active {
                        if let Some(page) = navigation_target(
                            app.page,
                            msg.wParam as u32,
                            GetKeyState(0x11) < 0,
                            GetKeyState(0x10) < 0,
                        ) {
                            app.capture();
                            app.page = page;
                            let hwnd = app.hwnd;
                            app.scrolling.cancel(hwnd);
                            app.scroll = 0;
                            app.render();
                            continue;
                        }
                    }
                }
            }
            #[cfg(debug_assertions)]
            if super::capture_fixture::consume(&msg) {
                continue;
            }
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if IsWindow(hwnd) != 0 {
            DestroyWindow(hwnd);
        }
        UnregisterClassW(class.as_ptr(), GetModuleHandleW(null()));
        drop(Box::from_raw(raw));
        if let Some(handle) = font_resource {
            if !handle.is_null() {
                RemoveFontMemResourceEx(handle);
            }
        }
        if let Some(error) = failure {
            Err(error)
        } else {
            Ok(())
        }
    }
}

// P1: native calls and modal loops synchronously reenter WndProc. The owner
// keeps Box<RefCell<App>> until GetMessage exits. No callback frees the box,
// and modal entry/DestroyWindow occur only after the RefMut has been released.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lp as *const CREATESTRUCTW);
            let raw = cs.lpCreateParams as *mut RefCell<App>;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
            (*raw).borrow_mut().hwnd = hwnd;
            let diagnostic = std::sync::Arc::as_ptr(&(*raw).borrow().bridge.diagnostics);
            SetPropW(
                hwnd,
                wide("Jiks.Diagnostics").as_ptr(),
                diagnostic as HANDLE,
            );
        }
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RefCell<App>;
        if raw.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        if let Some(result) = main_caption_message(hwnd, msg, wp, lp) {
            return result;
        }
        if let Some(result) = presentation_message(msg, wp, lp) {
            return result;
        }
        // Default WM_PRINT calls our WM_PRINTCLIENT synchronously. Delegate
        // before acquiring App so both on-screen and print paint have a page.
        if msg == WM_PRINT {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let (result, destroy, editor, bridge) = {
            let Ok(mut app) = (*raw).try_borrow_mut() else {
                let diag =
                    GetPropW(hwnd, wide("Jiks.Diagnostics").as_ptr()) as *const UiDiagnostics;
                if !diag.is_null() {
                    (*diag).reentrant_defaults.fetch_add(1, Ordering::Relaxed);
                }
                // DefWindowProc preserves native creation/painting defaults. Destruction
                // and a requested close must still be honored without aliasing App.
                if msg == WM_CLOSE {
                    SetPropW(hwnd, wide("Jiks.DeferredClose").as_ptr(), 1usize as HANDLE);
                    return 0;
                }
                if msg == WM_DESTROY {
                    KillTimer(hwnd, TIMER);
                    PostQuitMessage(0);
                    return 0;
                }
                if msg == WM_NCDESTROY {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    return DefWindowProcW(hwnd, msg, wp, lp);
                }
                return DefWindowProcW(hwnd, msg, wp, lp);
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                dispatch_window(&mut app, msg, wp, lp)
            }));
            let mut destroy = std::mem::take(&mut app.deferred_destroy);
            if destroy {
                if let Err(error) = app.notifications.close_all() {
                    text(
                        app.notice,
                        &format!("通知窗口尚未清理，保留界面以便重试：{error:#}"),
                    );
                    destroy = false;
                }
                if let Err(error) = app.overlay.close() {
                    text(app.notice, &format!("OSD清理尚未确认：{error:#}"));
                    destroy = false;
                }
                if let Err(error) = app.popup.hide() {
                    text(app.notice, &format!("状态窗清理尚未确认：{error:#}"));
                    destroy = false;
                }
            }
            let editor = app.deferred_editor.take();
            if editor.is_some() {
                app.scrolling.cancel(hwnd);
                app.modal_active = true;
            }
            (result, destroy, editor, app.bridge.clone())
        };
        if destroy && IsWindow(hwnd) != 0 {
            DestroyWindow(hwnd);
        }
        if let Some(editor) = editor {
            bridge
                .diagnostics
                .modals_opened
                .fetch_add(1, Ordering::Relaxed);
            let opened = match editor {
                DeferredEditor::DeviceInformation => {
                    super::device_information::open(hwnd, bridge.clone())
                }
                DeferredEditor::Structured(kind, item) => {
                    editors::structured(hwnd, bridge.clone(), kind, item)
                }
                DeferredEditor::SensorSettings => editors::sensor_settings(hwnd, bridge.clone()),
                DeferredEditor::Document(id) => editors::document(hwnd, bridge.clone(), id),
                DeferredEditor::Picker { key, kind, initial } => {
                    let selected = super::selectors::pick(hwnd, kind, &initial);
                    match selected {
                        Ok(Some(selected)) => {
                            if IsWindow(hwnd) != 0
                                && GetWindowLongPtrW(hwnd, GWLP_USERDATA) == raw as isize
                            {
                                if let Ok(mut app) = (*raw).try_borrow_mut() {
                                    app.drafts.insert(key.clone(), json!(selected));
                                    app.dirty.insert(key.clone());
                                    if let Some(binding) =
                                        app.bindings.iter().find(|binding| binding.key == key)
                                    {
                                        set_binding(binding, &json!(selected));
                                    }
                                }
                            }
                            Ok(())
                        }
                        Ok(None) => Ok(()),
                        Err(error) => Err(error),
                    }
                }
            };
            bridge
                .diagnostics
                .modals_closed
                .fetch_add(1, Ordering::Relaxed);
            if let Ok(mut app) = (*raw).try_borrow_mut() {
                app.modal_active = false;
            }
            if let Err(error) = opened {
                if IsWindow(hwnd) != 0 {
                    message(hwnd, "操作未完成", &format!("{error:#}"), true);
                }
            }
        }
        if !GetPropW(hwnd, wide("Jiks.DeferredClose").as_ptr()).is_null() {
            RemovePropW(hwnd, wide("Jiks.DeferredClose").as_ptr());
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
        match result {
            Ok(Some(result)) => result,
            Ok(None) => DefWindowProcW(hwnd, msg, wp, lp),
            Err(_) => {
                if IsWindow(hwnd) != 0 {
                    message(
                        hwnd,
                        "界面异常",
                        "操作未完成，原设置不会被当作保存成功。",
                        true,
                    );
                }
                0
            }
        }
    }
}

unsafe fn dispatch_window(app: &mut App, msg: u32, wp: usize, lp: isize) -> Option<isize> {
    if app.show_message != 0 && msg == app.show_message {
        unsafe {
            app.foreground();
            let active = GetLastActivePopup(app.hwnd);
            if !active.is_null() && active != app.hwnd && IsWindowVisible(active) != 0 {
                SetForegroundWindow(active);
            }
        }
        return Some(1);
    }
    unsafe {
        if msg == app.taskbar_message && msg != 0 {
            app.cancel_tray_hover();
            app.add_tray();
            return Some(0);
        }
        match msg {
            WM_CREATE => {
                app.dpi = GetDpiForWindow(app.hwnd).max(96);
                app.make_fonts();
                apply_window_chrome(app.hwnd);
                app.build_fixed();
                app.render();
                app.add_tray();
                SetTimer(app.hwnd, TIMER, 250, None);
                if let Some(ready) = &app.bridge.window_ready {
                    let _ = ready.send(());
                }
                Some(0)
            }
            WM_SIZE => {
                if wp == SIZE_MINIMIZED as usize {
                    app.scrolling.cancel(app.hwnd);
                }
                if wp == SIZE_MINIMIZED as usize
                    && snapshot(&app.bridge).preferences.minimize_to_tray
                    && app.tray
                {
                    ShowWindow(app.hwnd, SW_HIDE);
                } else if wp != SIZE_MINIMIZED as usize && !app.layout_posted {
                    app.layout_posted = PostMessageW(app.hwnd, WM_RELAYOUT, 0, 0) != 0;
                }
                Some(0)
            }
            WM_RELAYOUT => {
                app.layout_posted = false;
                if IsIconic(app.hwnd) == 0 {
                    app.capture();
                    app.render();
                }
                Some(0)
            }
            WM_GETMINMAXINFO => {
                let info = &mut *(lp as *mut MINMAXINFO);
                info.ptMinTrackSize.x = app.s(650);
                info.ptMinTrackSize.y = app.s(600);
                if live_snapshot(&app.bridge).value("app.lock_size").as_bool() == Some(true) {
                    let mut r: RECT = zeroed();
                    GetWindowRect(app.hwnd, &mut r);
                    if r.right > r.left && r.bottom > r.top {
                        info.ptMinTrackSize.x = r.right - r.left;
                        info.ptMaxTrackSize.x = r.right - r.left;
                        info.ptMinTrackSize.y = r.bottom - r.top;
                        info.ptMaxTrackSize.y = r.bottom - r.top;
                    }
                }
                Some(0)
            }
            WM_EXITSIZEMOVE => {
                app.save_window();
                Some(0)
            }
            WM_ENABLE | WM_CANCELMODE | WM_ENTERSIZEMOVE | WM_ENTERMENULOOP
                if msg != WM_ENABLE || wp == 0 =>
            {
                app.scrolling.cancel(app.hwnd);
                None
            }
            WM_DPICHANGED => {
                app.capture();
                app.dpi = (wp & 0xffff) as u32;
                if !GetPropW(app.hwnd, CAPTION_PROPERTY).is_null() {
                    extend_main_caption(app.hwnd, app.dpi);
                }
                let rect = &*(lp as *const RECT);
                SetWindowPos(
                    app.hwnd,
                    null_mut(),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                app.make_fonts();
                app.build_fixed();
                app.render();
                Some(0)
            }
            WM_TIMER if wp == TIMER => {
                app.refresh();
                Some(0)
            }
            WM_TIMER if wp == SCROLL_TIMER => {
                if let Some(next) = app.scrolling.frame(app.hwnd, app.scroll) {
                    app.scroll = next;
                    app.position_content();
                }
                Some(0)
            }
            WM_COMMAND => {
                if lp != 0 && matches!(((wp >> 16) & 0xffff) as u32, BN_CLICKED | CBN_DROPDOWN) {
                    app.scrolling.cancel(app.hwnd);
                }
                if !app.rendering {
                    app.command((wp & 0xffff) as u16, ((wp >> 16) & 0xffff) as u16);
                }
                Some(0)
            }
            WM_VSCROLL if lp == 0 => {
                let next = app.scrolling.command(app.hwnd, app.scroll, (wp & 0xffff) as i32);
                if next != app.scroll {
                    app.scroll = next;
                    app.position_content();
                }
                Some(0)
            }
            WM_MOUSEWHEEL => {
                if let Some(next) = app.scrolling.wheel(app.hwnd, app.scroll, wp) {
                    app.scroll = next;
                    app.position_content();
                }
                Some(0)
            }
            WM_DRAWITEM => {
                let item = &*(lp as *const DRAWITEMSTRUCT);
                let selected = item.CtlID >= ID_NAV as u32
                    && item.CtlID < ID_NAV as u32 + Page::ALL.len() as u32
                    && item.CtlID - ID_NAV as u32
                        == Page::ALL.iter().position(|p| *p == app.page).unwrap_or(0) as u32;
                let selected = selected
                    || (item.CtlID >= ID_TOOL_NAV as u32
                        && item.CtlID < ID_TOOL_NAV as u32 + 8
                        && item.CtlID - ID_TOOL_NAV as u32 == app.tools_tab as u32);
                paint_button(item, selected);
                Some(1)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
                let dc = wp as HDC;
                SetTextColor(
                    dc,
                    if lp as HWND == app.notice || app.muted_labels.contains(&(lp as usize)) {
                        muted()
                    } else {
                        ink()
                    },
                );
                SetBkMode(dc, TRANSPARENT as i32);
                let on_surface = app.surface_children.contains(&(lp as usize));
                SetBkColor(dc, if on_surface { surface() } else { paper() });
                Some(if app.background.visible() && msg == WM_CTLCOLORSTATIC {
                    GetStockObject(NULL_BRUSH) as isize
                } else {
                    (if on_surface {
                        app.surface_brush
                    } else {
                        app.brush
                    }) as isize
                })
            }
            // WM_PAINT already fills the invalid region. An erase pass here
            // would draw the cards twice and expose a separate frame of paint.
            WM_ERASEBKGND => Some(1),
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(app.hwnd, &mut ps);
                if ps.rcPaint.left == 0 && ps.rcPaint.top == 0 {
                    let mut client: RECT = zeroed();
                    if GetClientRect(app.hwnd, &mut client) != 0
                        && ps.rcPaint.right == client.right && ps.rcPaint.bottom == client.bottom
                        && client.right > 0 && client.bottom > 0
                    {
                        app.bridge.diagnostics.main_full_paints.fetch_add(1, Ordering::Relaxed);
                    }
                }
                #[cfg(debug_assertions)]
                super::scroll_trace::record("paint-start", app.scroll as i64, (ps.rcPaint.bottom - ps.rcPaint.top) as i64);
                app.paint_client(dc);
                EndPaint(app.hwnd, &ps);
                #[cfg(debug_assertions)]
                super::scroll_trace::record("paint-end", app.scroll as i64, 0);
                Some(0)
            }
            WM_PRINTCLIENT => {
                app.paint_client(wp as HDC);
                Some(0)
            }
            WM_TRAY => {
                match (lp as u32) & 0xffff {
                    WM_LBUTTONUP | NIN_SELECT | TRAY_KEY_SELECT => app.foreground(),
                    WM_RBUTTONUP | WM_CONTEXTMENU => app.tray_menu(),
                    NIN_POPUPOPEN if app.tray && !app.popup.is_visible() => {
                        app.tray_hover_at = Some(Instant::now() + TRAY_HOVER_DELAY);
                        SetTimer(app.hwnd, TRAY_HOVER_TIMER, TRAY_HOVER_DELAY.as_millis() as u32, None);
                    }
                    NIN_POPUPCLOSE => app.cancel_tray_hover(),
                    _ => {}
                }
                Some(0)
            }
            WM_TIMER if wp == TRAY_HOVER_TIMER => {
                if app.tray_hover_at.is_some_and(|due| Instant::now() >= due) {
                    app.tray_hover_at = None;
                    KillTimer(app.hwnd, TRAY_HOVER_TIMER);
                    app.show_status_popup(false);
                }
                Some(0)
            }
            WM_CLOSE => {
                app.scrolling.cancel(app.hwnd);
                app.bridge
                    .diagnostics
                    .window_close_messages
                    .fetch_add(1, Ordering::Relaxed);
                if snapshot(&app.bridge).preferences.close_to_tray && app.tray {
                    ShowWindow(app.hwnd, SW_HIDE);
                } else {
                    app.bridge
                        .diagnostics
                        .last_exit_origin
                        .store(1, Ordering::Relaxed);
                    app.shutdown();
                }
                Some(0)
            }
            WM_QUERYENDSESSION | WM_ENDSESSION => {
                app.bridge
                    .diagnostics
                    .session_end_messages
                    .fetch_add(1, Ordering::Relaxed);
                if msg == WM_ENDSESSION && wp != 0 {
                    app.bridge
                        .diagnostics
                        .last_exit_origin
                        .store(6, Ordering::Relaxed);
                }
                None
            }
            WM_DESTROY => {
                app.scrolling.cancel(app.hwnd);
                app.bridge
                    .diagnostics
                    .window_destroy_messages
                    .fetch_add(1, Ordering::Relaxed);
                KillTimer(app.hwnd, TIMER);
                app.hide_status_popup();
                if app.tray {
                    Shell_NotifyIconW(NIM_DELETE, &app.tray_data);
                }
                PostQuitMessage(0);
                Some(0)
            }
            WM_NCDESTROY => {
                if let Some(key) = app.bridge.instance_key.as_ref() {
                    RemovePropW(app.hwnd, wide(key).as_ptr());
                }
                SetWindowLongPtrW(app.hwnd, GWLP_USERDATA, 0);
                let hwnd = app.hwnd;
                Some(DefWindowProcW(hwnd, msg, wp, lp))
            }
            _ => None,
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        unsafe {
            for font in [self.body_font, self.heading_font, self.title_font, self.caption_font] {
                if !font.is_null() {
                    DeleteObject(font);
                }
            }
            if !self.brush.is_null() {
                DeleteObject(self.brush);
            }
            if !self.surface_brush.is_null() {
                DeleteObject(self.surface_brush);
            }
        }
    }
}
impl App {
    fn accessibility_scope(&self) -> String {
        if self.page == Page::Settings {
            format!("main.settings.{}", TOOL_KEYS[self.tools_tab.min(7)])
        } else {
            format!("main.{}", self.page.key())
        }
    }
    fn s(&self, value: i32) -> i32 {
        value * self.dpi as i32 / 96
    }
    unsafe fn size(&self) -> (i32, i32) {
        unsafe {
            let mut r: RECT = zeroed();
            GetClientRect(self.hwnd, &mut r);
            (
                r.right * 96 / self.dpi as i32,
                r.bottom * 96 / self.dpi as i32,
            )
        }
    }
    unsafe fn make_fonts(&mut self) {
        unsafe {
            for f in [self.body_font, self.heading_font, self.title_font, self.caption_font] {
                if !f.is_null() {
                    DeleteObject(f);
                }
            }
            self.body_font = font(self.dpi, 14, false);
            self.heading_font = font(self.dpi, 17, true);
            self.title_font = font(self.dpi, 22, true);
            self.caption_font = font(self.dpi, 12, false);
        }
    }
    unsafe fn build_fixed(&mut self) {
        unsafe {
            self.rendering = true;
            for h in self.fixed.drain(..) {
                DestroyWindow(h);
            }
            for (index, page) in main_nav() {
                let h = create(
                    self.hwnd,
                    "BUTTON",
                    page.title(),
                    BS_OWNERDRAW as u32 | WS_TABSTOP,
                    ID_NAV + index as u16,
                    [0, 0, 0, 0],
                    self.body_font,
                );
                self.fixed.push(h);
                a11y::annotate(
                    h,
                    &a11y::key("main", "navigate", page.key()),
                    Some(page.title()),
                    None,
                );
            }
            let nav = create(
                self.hwnd,
                "LISTBOX",
                "",
                WS_TABSTOP | WS_VSCROLL | LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32,
                ID_NAV_LIST,
                [0, 0, 0, 0],
                self.body_font,
            );
            for (_, page) in main_nav() {
                SendMessageW(nav, LB_ADDSTRING, 0, wide(page.title()).as_ptr() as isize);
            }
            SendMessageW(nav, LB_SETITEMHEIGHT, 0, self.s(34) as isize);
            self.fixed.push(nav);
            a11y::annotate(
                nav,
                "main.navigation",
                Some(&super::i18n::tr("主导航")),
                None,
            );
            let model = live_snapshot(&self.bridge);
            for (id, name, label) in [
                (ID_DEVICE, "main.device-information", "设备信息"),
                (ID_THEME, "main.theme", "切换主题"),
            ] {
                let caption = if id == ID_DEVICE {
                    device_badge(&model)
                } else {
                    super::i18n::tr(label).into_owned()
                };
                let h = create(
                    self.hwnd,
                    "BUTTON",
                    &caption,
                    BS_OWNERDRAW as u32 | WS_TABSTOP,
                    id,
                    [0, 0, 0, 0],
                    self.body_font,
                );
                self.fixed.push(h);
                presentation_flag(h, "Jiks.CaptionTool", true);
                a11y::annotate(h, name, Some(&super::i18n::tr(label)), None);
            }
            self.caption = create(
                self.hwnd,
                "STATIC",
                "拯救者工具箱",
                SS_CENTERIMAGE | SS_ENDELLIPSIS | SS_NOPREFIX,
                0,
                [0, 0, 0, 0],
                self.caption_font,
            );
            self.fixed.push(self.caption);
            a11y::annotate(self.caption, "main.caption", None, None);
            self.notice = create(
                self.hwnd,
                "STATIC",
                "",
                SS_CENTERIMAGE | SS_ENDELLIPSIS | SS_NOPREFIX,
                0,
                [0, 0, 0, 0],
                self.body_font,
            );
            self.fixed.push(self.notice);
            a11y::annotate(self.notice, "main.status", None, None);
            presentation_flag(self.notice, "Jiks.Muted", true);
            self.layout_fixed();
            self.rendering = false;
        }
    }
    unsafe fn layout_fixed(&mut self) {
        unsafe {
            let (w, h) = self.size();
            // A short client area uses one native scrolling navigation list.
            // Main and footer items never occupy the same row.
            let nav_bottom = h - 12;
            let nav_space = nav_bottom - VIEW_TOP;
            let nav_count = main_nav().count() as i32;
            let nav_step = ((nav_space - 12) / nav_count).clamp(39, 64);
            let compact_nav = nav_space < nav_count * 39 + 12;
            let active_page = active_navigation(self.page);
            let nav = GetDlgItem(self.hwnd, ID_NAV_LIST as i32);
            SetWindowPos(
                nav,
                null_mut(),
                self.s(12),
                self.s(VIEW_TOP),
                self.s(60),
                self.s(nav_space.max(60)),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            SendMessageW(
                nav,
                LB_SETCURSEL,
                main_nav().position(|(_, page)| page == active_page).unwrap_or(0),
                0,
            );
            ShowWindow(nav, if compact_nav { SW_SHOWNA } else { SW_HIDE });
            for (position, (index, page)) in main_nav().enumerate() {
                let control = GetDlgItem(self.hwnd, ID_NAV as i32 + index as i32);
                ShowWindow(control, if compact_nav { SW_HIDE } else { SW_SHOWNA });
                let position = position as i32;
                let y = if index < 6 { VIEW_TOP + position * nav_step }
                    else { nav_bottom - (nav_count - position) * nav_step };
                SetWindowPos(control, null_mut(), self.s(12), self.s(y), self.s(60), self.s(nav_step - 4), SWP_NOZORDER | SWP_NOACTIVATE);
                let selected = page == active_page;
                presentation_flag(control, "Jiks.Selected", selected);
                InvalidateRect(control, null(), 0);
                a11y::annotate(
                    GetDlgItem(self.hwnd, ID_NAV as i32 + index as i32),
                    &a11y::key("main", "navigate", page.key()),
                    Some(page.title()),
                    selected.then(|| super::i18n::tr_static("当前页面")),
                );
            }
            let right = w - CONTENT_RIGHT;
            let integrated = !GetPropW(self.hwnd, CAPTION_PROPERTY).is_null();
            let caption_right = w - if integrated { caption_reserved_width(self.hwnd, self.dpi) } else { 12 };
            let theme_x = caption_right - 32;
            let device_x = theme_x - 163;
            ShowWindow(self.caption, if integrated { SW_SHOWNA } else { SW_HIDE });
            for (hwnd, x, y, width, height) in [
                (self.caption, 12, 0, device_x - 24, CAPTION_HEIGHT),
                (GetDlgItem(self.hwnd, ID_DEVICE as i32), device_x, 2, 160, 26),
                (GetDlgItem(self.hwnd, ID_THEME as i32), theme_x, 2, 32, 26),
                (self.notice, CONTENT_X, h - FOOTER_HEIGHT + 5, right - CONTENT_X, 26),
            ] {
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    self.s(x),
                    self.s(y),
                    self.s(width.max(20)),
                    self.s(height),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }
    unsafe fn placed(
        &mut self,
        class: &str,
        label: &str,
        style: u32,
        id: u16,
        rect: [i32; 4],
        font: HFONT,
    ) -> HWND {
        unsafe {
            let h = create_control(
                self.hwnd,
                class,
                label,
                style,
                id,
                [
                    self.s(rect[0]),
                    self.s(rect[1]),
                    self.s(rect[2]),
                    self.s(rect[3]),
                ],
                font,
            );
            install_wheel_routing(h, class);
            self.content.push(ScrollChild::new(
                h,
                rect,
                if class == "COMBOBOX" { CONTROL_HEIGHT } else { rect[3] },
                id >= ID_TOOL_NAV && id < ID_TOOL_NAV + 8,
            ));
            presentation_flag(h, "Jiks.Pane", true);
            if !(id >= ID_TOOL_NAV && id < ID_TOOL_NAV + 8) {
                self.surface_children.insert(h as usize);
                presentation_flag(h, "Jiks.Surface", true);
            }
            h
        }
    }
    unsafe fn label(&mut self, label: &str, y: i32, width: i32, large: bool, height: i32) -> HWND {
        unsafe {
            self.placed(
                "STATIC",
                label,
                SS_LEFT | SS_NOPREFIX,
                0,
                [self.column_x, y, width, height],
                if large {
                    self.heading_font
                } else {
                    self.body_font
                },
            )
        }
    }
    unsafe fn paint_client(&self, dc: HDC) {
        unsafe {
            let saved = SaveDC(dc);
            let mut rect: RECT = zeroed();
            GetClientRect(self.hwnd, &mut rect);
            if !self.background.paint(dc, &rect) {
                FillRect(dc, &rect, self.brush);
            }
            self.paint_surfaces(dc);
            RestoreDC(dc, saved);
        }
    }
    unsafe fn paint_surfaces(&self, dc: HDC) {
        unsafe {
            let (width, height) = self.size();
            // The page viewport has its own surface, like the original frame.
            if !self.background.visible() {
                fill(
                    dc,
                    &RECT {
                        left: self.s(CONTENT_X - 12),
                        top: self.s(VIEW_TOP),
                        right: self.s(width - 12),
                        bottom: self.s(height - FOOTER_HEIGHT),
                    },
                    pane(),
                );
            }
            let saved = SaveDC(dc);
            IntersectClipRect(
                dc,
                self.s(CONTENT_X - 12),
                self.s(VIEW_TOP),
                self.s(width - 12),
                self.s(height - FOOTER_HEIGHT),
            );
            for card in &self.cards {
                let [x, y, w, h] = card.rect;
                let y = y - self.scroll;
                if y + h > VIEW_TOP && y < height - FOOTER_HEIGHT {
                    paint_card(
                        dc,
                        &RECT {
                            left: self.s(x),
                            top: self.s(y),
                            right: self.s(x + w),
                            bottom: self.s(y + h),
                        },
                        self.dpi,
                    );
                    if let Some(icon) = card.icon {
                        SetTextColor(dc, ink());
                        let item = DRAWITEMSTRUCT {
                            CtlID: ID_TOOL_NAV as u32,
                            hDC: dc,
                            hwndItem: self.hwnd,
                            rcItem: RECT {
                                left: self.s(x + 4),
                                top: self.s(y + (h - 36) / 2),
                                right: self.s(x + 40),
                                bottom: self.s(y + (h + 36) / 2),
                            },
                            ..zeroed()
                        };
                        paint_nav_icon(&item, icon);
                    }
                }
            }
            RestoreDC(dc, saved);
            let pen = CreatePen(PS_SOLID, 1, line());
            let old = SelectObject(dc, pen);
            MoveToEx(dc, self.s(CONTENT_X), self.s(height - FOOTER_HEIGHT + 3), null_mut());
            LineTo(dc, self.s(width - CONTENT_RIGHT), self.s(height - FOOTER_HEIGHT + 3));
            SelectObject(dc, old);
            DeleteObject(pen);
        }
    }
    unsafe fn measured_height(&self, dc: HDC, value: &str, width: i32, font: HFONT) -> i32 {
        if value.is_empty() {
            return 0;
        }
        if dc.is_null() {
            return 24;
        }
        unsafe {
            let old = SelectObject(dc, font);
            let mut rect = RECT { left: 0, top: 0, right: self.s(width), bottom: 0 };
            DrawTextW(dc, wide(value).as_ptr(), -1, &mut rect, DT_CALCRECT | DT_NOPREFIX | DT_WORDBREAK);
            SelectObject(dc, old);
            ((rect.bottom * 96 + self.dpi as i32 - 1) / self.dpi as i32).max(16) + 3
        }
    }
    unsafe fn measured_button_width(&self, dc: HDC, value: &str) -> i32 {
        if dc.is_null() { return (value.chars().count() as i32 * 14 + 28).max(72); }
        unsafe {
            let old = SelectObject(dc, self.body_font);
            let mut rect: RECT = zeroed();
            DrawTextW(dc, wide(value).as_ptr(), -1, &mut rect, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
            SelectObject(dc, old);
            ((rect.right * 96 + self.dpi as i32 - 1) / self.dpi as i32 + 28).max(72)
        }
    }
    unsafe fn metric(&mut self, metric: &UiMetric, y: i32, width: i32, compact: bool) {
        unsafe {
            let origin = self.column_x;
            let dashboard = self.page == Page::Dashboard;
            let meter = if dashboard && width >= 170 { metric_meter_scale(&metric.key) } else { 0 };
            let row_height = if dashboard { 22 } else { 26 };
            let value_width = if meter != 0 { (width * 56 / 100).clamp(98, 140) }
                else { (width * 46 / 100).clamp(70, 140) };
            let detail_width = if compact { 0 } else { (width / 4).clamp(64, 180) + 8 };
            let name_width = (width - value_width - detail_width - 8).max(24);
            let name = self.placed(
                "STATIC", metric_caption(metric, compact), SS_CENTERIMAGE | SS_ENDELLIPSIS | SS_NOPREFIX,
                0, [origin, y, name_width, row_height], self.body_font,
            );
            if dashboard {
                presentation_flag(name, "Jiks.Muted", true);
            }
            self.column_x = origin + name_width + 8;
            let value = self.placed(
                "STATIC",
                &metric.value,
                SS_RIGHT | SS_CENTERIMAGE | SS_ENDELLIPSIS | SS_NOPREFIX,
                0,
                [self.column_x, y, value_width, row_height],
                self.body_font,
            );
            if meter != 0 {
                SetWindowSubclass(value, Some(metric_meter_paint), METRIC_METER, meter);
            }
            let scope = self.accessibility_scope();
            let detail = if compact {
                null_mut()
            } else {
                let detail = self.placed(
                    "STATIC", &metric.detail, SS_RIGHT | SS_CENTERIMAGE | SS_ENDELLIPSIS | SS_NOPREFIX,
                    0, [self.column_x + value_width + 8, y, detail_width - 8, 26], self.body_font,
                );
                presentation_flag(detail, "Jiks.Muted", true);
                a11y::annotate(detail, &a11y::key(&scope, "metric-detail", &metric.key), None, None);
                detail
            };
            a11y::annotate(
                name,
                &a11y::key(&scope, "metric-label", &metric.key),
                Some(&metric.label),
                None,
            );
            a11y::annotate(
                value,
                &a11y::key(&scope, "metric", &metric.key),
                Some(&format!(
                    "{} {} {}",
                    metric.label, metric.value, metric.detail
                )),
                Some(&metric.detail),
            );
            self.metrics.push(MetricPlacement {
                name,
                value,
                detail,
                key: metric.key.clone(),
                compact,
            });
            self.column_x = origin;
        }
    }
    unsafe fn render_actions(
        &mut self, actions: Vec<Action>, scope: &str, next_button: &mut u16, dc: HDC, bounds: [i32; 3], model: &UiModel,
    ) -> i32 {
        unsafe {
            let [left, mut y, width] = bounds;
            let mut x = left;
            let mut primary_used = false;
            for action in actions {
                let link = matches!(action.kind, ActionKind::Document(_) | ActionKind::SensorSettings | ActionKind::Navigate(_));
                let button_width = if link { width } else { self.measured_button_width(dc, action.label).min(width) };
                if x != left && (link || x + button_width > left + width) {
                    x = left;
                    y += CONTROL_HEIGHT + 8;
                }
                let id = *next_button;
                *next_button += 1;
                let h = self.placed(
                    "BUTTON", action.label, BS_OWNERDRAW as u32 | WS_TABSTOP, id,
                    [x, y, button_width, if link { 44 } else { CONTROL_HEIGHT }], self.body_font,
                );
                if link {
                    presentation_flag(h, "Jiks.ActionCard", true);
                } else if !primary_used && matches!(&action.kind, ActionKind::Execute { command, confirm: None, .. }
                    if command.ends_with(".save") || command.ends_with(".apply")) {
                    presentation_flag(h, "Jiks.Primary", true);
                    primary_used = true;
                }
                let unavailable = action_disabled_reason(model, &action);
                a11y::annotate(h, &a11y::key(scope, "action", &action_key(&action)), None, unavailable.as_deref());
                if unavailable.is_some() {
                    EnableWindow(h, 0);
                }
                self.buttons.insert(id, action);
                if link {
                    y += 52;
                } else {
                    x += button_width + 8;
                }
            }
            if x != left { y + CONTROL_HEIGHT + 8 } else { y }
        }
    }
    unsafe fn render(&mut self) {
        unsafe {
            if self.hwnd.is_null() {
                return;
            }
            #[cfg(debug_assertions)]
            super::scroll_trace::record("rebuild", self.scroll as i64, 0);
            self.bridge
                .diagnostics
                .main_rebuilds
                .fetch_add(1, Ordering::Relaxed);
            self.rendering = true;
            for p in self.content.drain(..) {
                DestroyWindow(p.hwnd);
            }
            self.bindings.clear();
            self.buttons.clear();
            self.pickers.clear();
            self.metrics.clear();
            self.pending_process_options = None;
            self.cards.clear();
            self.surface_children.clear();
            self.muted_labels.clear();
            let model = snapshot(&self.bridge);
            self.rendered_revision = model.revision;
            self.metadata_revision = model.metadata_revision;
            self.backend_state = backend_state(&model);
            self.observed_metric_keys = model.metrics.iter().map(|metric| metric.key.clone()).collect();
            let (w, _) = self.size();
            let width = (w - CONTENT_X - CONTENT_RIGHT).max(260);
            let mut y = VIEW_TOP + 14;
            self.column_x = CONTENT_X;
            let measure_dc = GetDC(self.hwnd);
            let mut content_origin = CONTENT_X;
            let mut content_width = width;
            if self.page == Page::Settings {
                let nav_width = if width >= 680 { 184 } else { 140 };
                for (index, title) in catalog::TOOL_TABS.iter().enumerate() {
                    let translated = super::i18n::tr(title);
                    let title = translated.as_ref();
                    let button = self.placed(
                        "BUTTON",
                        title,
                        BS_OWNERDRAW as u32 | WS_TABSTOP,
                        ID_TOOL_NAV + index as u16,
                        [CONTENT_X, y + index as i32 * 42, nav_width, 37],
                        self.body_font,
                    );
                    if index == self.tools_tab {
                        presentation_flag(button, "Jiks.Selected", true);
                    }
                    a11y::annotate(
                        button,
                        &a11y::key("main.settings", "category", TOOL_KEYS[index]),
                        Some(title),
                        (index == self.tools_tab).then(|| super::i18n::tr_static("当前页面")),
                    );
                }
                content_origin = CONTENT_X + nav_width + GROUP_GAP;
                content_width = (width - nav_width - GROUP_GAP).max(1);
            }
            if self.page == Page::Dashboard {
                let groups = super::sensors::groups(&model);
                let columns = ((width + 8) / 208).clamp(1, 4);
                for row in groups.chunks(columns as usize) {
                    let row_columns = row.len() as i32;
                    let card_width = (width - (row_columns - 1) * 8) / row_columns;
                    let card_height = 44 + 22 * row.iter().map(|g| g.metrics.len()).max().unwrap_or(0) as i32;
                    for (column, group) in row.iter().enumerate() {
                        let card_x = CONTENT_X + column as i32 * (card_width + 8);
                        self.column_x = card_x + 12;
                        let hardware_name = match group.title.as_str() {
                            "CPU" => model.machine.splitn(3, " · ").nth(2),
                            "GPU" => model.values.get("sensors.gpu.name").or_else(|| model.values.get("gpu.name"))
                                .and_then(Value::as_str),
                            _ => None,
                        }.filter(|name| !name.trim().is_empty());
                        let heading = self.placed(
                            "STATIC", &group.title, SS_CENTERIMAGE | SS_ENDELLIPSIS | SS_NOPREFIX,
                            0, [self.column_x, y + 8, if hardware_name.is_some() { 44 } else { card_width - 24 }, 24],
                            self.heading_font,
                        );
                        if let Some(name) = hardware_name {
                            let name = name.split(" @ ").next().unwrap_or(name)
                                .replace("(R)", "").replace("(TM)", "");
                            let label = self.placed(
                                "STATIC", &name, SS_RIGHT | SS_CENTERIMAGE | SS_ENDELLIPSIS | SS_NOPREFIX,
                                0, [card_x + 60, y + 8, card_width - 72, 24], self.body_font,
                            );
                            presentation_flag(label, "Jiks.Muted", true);
                        }
                        if let Some(first) = group.metrics.first() {
                            a11y::annotate(heading, &a11y::key("main.dashboard", "group", &first.key), None, None);
                        }
                        for (index, metric) in group.metrics.iter().enumerate() {
                            self.metric(metric, y + 34 + index as i32 * 22, card_width - 24, true);
                        }
                        self.cards.push(CardSurface { icon: None, rect: [card_x, y, card_width, card_height] });
                    }
                    y += card_height + 14;
                }
                self.column_x = CONTENT_X;
            } else if self.page == Page::Battery {
                let top = y;
                self.column_x = CONTENT_X + 16;
                for metric in model
                    .metrics
                    .iter()
                    .filter(|m| m.key.to_ascii_lowercase().starts_with("battery"))
                {
                    self.metric(metric, y + 16, width - 32, width < 600);
                    y += 28;
                }
                if y > top {
                    self.cards.push(CardSurface {
                        icon: None,
                        rect: [CONTENT_X, top, width, y - top + 26],
                    });
                    y += 42;
                }
            } else if self.page == Page::About {
                let top = y;
                self.column_x = CONTENT_X + 20;
                let name = self.label(
                    &super::i18n::tr("拯救者工具箱"),
                    y + 18,
                    width - 40,
                    true,
                    34,
                );
                SendMessageW(name, WM_SETFONT, self.title_font as usize, 0);
                let subtitle = self.label(
                    &format!("{}  ·  {}", model.machine, model.version),
                    y + 62,
                    width - 40,
                    false,
                    26,
                );
                self.muted_labels.insert(subtitle as usize);
                presentation_flag(subtitle, "Jiks.Muted", true);
                self.label(
                    &super::i18n::tr("Lenovo Legion Toolkit · Rust"),
                    y + 98,
                    width - 40,
                    false,
                    26,
                );
                self.cards.push(CardSurface {
                    icon: None,
                    rect: [CONTENT_X, top, width, 144],
                });
                y += 160;
            }
            let sections = catalog::sections(self.page, self.tools_tab, &model);
            self.dashboard_structure =
                (self.page == Page::Dashboard).then(|| DashboardStructure::new(&model, &sections));
            let mut next_field = 1000u16;
            let mut next_button = 2000u16;
            let two_columns = self.page == Page::Dashboard && width >= 830;
            let full_width = content_width;
            let mut bottoms = [y, y];
            let mut pair_y = y;
            for (section_index, section) in sections.into_iter().enumerate() {
                let column = if two_columns { section_index % 2 } else { 0 };
                let width = if two_columns {
                    (full_width - GROUP_GAP) / 2
                } else {
                    full_width
                };
                let card_x = content_origin + column as i32 * (width + GROUP_GAP);
                self.column_x = card_x + 16;
                let width = width - 32;
                if two_columns {
                    if column == 0 {
                        pair_y = bottoms[0].max(bottoms[1]);
                    }
                    y = pair_y;
                }
                y += 8;
                let section_key = section
                    .fields
                    .first()
                    .map(|field| field.key.to_owned())
                    .or_else(|| section.actions.first().map(action_key))
                    .unwrap_or_else(|| "content".into());
                let section_scope = a11y::key(&self.accessibility_scope(), "section", &section_key);
                let heading_font = if self.page == Page::Dashboard { self.title_font } else { self.heading_font };
                let heading_height = self.measured_height(measure_dc, &section.title, width, heading_font).max(27);
                let heading = self.placed(
                    "STATIC", &section.title, SS_CENTERIMAGE | SS_NOPREFIX, 0,
                    [self.column_x, y, width, heading_height], heading_font,
                );
                self.surface_children.remove(&(heading as usize));
                presentation_flag(heading, "Jiks.Surface", false);
                a11y::annotate(
                    heading,
                    &a11y::key(&section_scope, "text", "title"),
                    None,
                    None,
                );
                y += heading_height + 6;
                let description = if section_key == "battery.threshold.read" {
                    match model.capabilities.get("battery.threshold.apply") {
                        Some(cap) if !cap.reason.is_empty() => std::borrow::Cow::Borrowed(cap.reason.as_str()),
                        _ => super::i18n::tr("正在检测本机充电阈值…"),
                    }
                } else {
                    super::i18n::tr(section.hint)
                };
                let hint_height = self.measured_height(measure_dc, &description, width, self.body_font);
                if hint_height > 0 {
                    let hint = self.label(&description, y, width, false, hint_height);
                    presentation_flag(hint, "Jiks.Muted", true);
                    self.surface_children.remove(&(hint as usize));
                    presentation_flag(hint, "Jiks.Surface", false);
                    a11y::annotate(hint, &a11y::key(&section_scope, "text", "description"), None, None);
                    y += hint_height + 6;
                }
                y += 8;
                let mut attached_actions: BTreeMap<&str, Vec<Action>> = BTreeMap::new();
                let mut remaining_actions = Vec::new();
                for action in section.actions {
                    // The overview keeps each Apply button beside its inputs.
                    // The detailed pages keep grouped actions in the footer.
                    let anchor = if self.page == Page::Dashboard {
                        match &action.kind {
                            ActionKind::Execute { command, .. } if matches!(*command, "gpu.refresh" | "gpu.processes.refresh") =>
                                section.fields.iter().find(|f| f.key == "selection.gpu.processes").map(|f| f.key),
                            ActionKind::Execute { fields, .. } | ActionKind::Cancel(fields) =>
                                section.fields.iter().rev().find(|f| fields.contains(&f.key)).map(|f| f.key),
                            ActionKind::Navigate(Page::Graphics) =>
                                section.fields.iter().find(|f| f.key == "gpu.dashboard_state").map(|f| f.key),
                            ActionKind::Document("gpu_oc") =>
                                section.fields.iter().find(|f| f.key == "gpu.oc.enabled").map(|f| f.key),
                            ActionKind::Structured { collection: "presets", .. } =>
                                section.fields.iter().find(|f| f.key == "selection.presets").map(|f| f.key),
                            _ => None,
                        }
                    } else { None };
                    if let Some(key) = anchor {
                        attached_actions.entry(key).or_default().push(action);
                    } else {
                        remaining_actions.push(action);
                    }
                }
                for spec in section.fields {
                    let row_top = y;
                    let mut row = RowColumns::new(card_x, width + 32, matches!(spec.kind, FieldKind::Toggle));
                    let value = self.drafts.get(spec.key).cloned().unwrap_or_else(|| field_value(&model, spec.key));
                    let process_selector = self.page == Page::Dashboard && spec.key == GPU_PROCESS_SELECTOR;
                    let options = if process_selector {
                        Some(gpu_process_choices(&model))
                    } else {
                        field_options(&model, &spec)
                    };
                    let editable = process_selector || (field_available(&model, spec.key, spec.capability)
                        && !matches!(spec.kind, FieldKind::ReadOnly)
                        && options.as_ref().is_none_or(|items| !items.is_empty()));
                    let show_list = editable && matches!(spec.kind, FieldKind::Collection("automation" | "macros" | "packages"));
                    let control_height = if show_list {
                        row.label_width = card_x + width + 18 - row.label_x;
                        row.control_x = row.label_x;
                        row.control_width = row.label_width;
                        row.stacked = true;
                        8 + options.as_ref().map_or(0, |items| items.len().clamp(2, 5) as i32) * 28
                    } else {
                        CONTROL_HEIGHT
                    };
                    let picker = main_picker(spec.key)
                        .filter(|_| editable && matches!(spec.kind, FieldKind::Text));
                    let field_label = model.ranges.get(spec.key)
                        .filter(|_| matches!(spec.kind, FieldKind::Number))
                        .map(|range| format!("{} ({}–{})", spec.label, range.minimum, range.maximum))
                        .unwrap_or_else(|| spec.label.to_owned());
                    let description = super::i18n::tr(super::presentation::field_hint(spec.key));
                    let title_height = self.measured_height(measure_dc, &field_label, row.label_width, self.body_font);
                    let desc_height = self.measured_height(measure_dc, &description, row.label_width, self.body_font);
                    let copy_height = title_height + if desc_height == 0 { 0 } else { 3 + desc_height };
                    let pad = if model.preferences.compact { 8 } else { 12 };
                    let row_height = if row.stacked {
                        2 * pad + copy_height + 8 + control_height
                    } else {
                        2 * pad + copy_height.max(CONTROL_HEIGHT)
                    };
                    let label_y = if row.stacked { row_top + pad } else { row_top + (row_height - copy_height) / 2 };
                    let editor_y = if row.stacked { label_y + copy_height + 8 } else { row_top + (row_height - CONTROL_HEIGHT) / 2 };
                    self.column_x = row.label_x;
                    let label = self.label(&field_label, label_y, row.label_width, false, title_height);
                    let scope = self.accessibility_scope();
                    a11y::annotate(label, &a11y::key(&scope, "label", spec.key), None, None);
                    if desc_height > 0 {
                        let hint = self.label(&description, label_y + title_height + 3, row.label_width, false, desc_height);
                        presentation_flag(hint, "Jiks.Muted", true);
                    }
                    let id = next_field;
                    next_field += 1;
                    let editor_width = row.control_width - if picker.is_some() { 70 } else { 0 };
                    let rect = [row.control_x, editor_y, editor_width, control_height];
                    let (hwnd, kind) = if !editable {
                        let options = options.unwrap_or_default();
                        let empty = field_empty_text(&model, &spec);
                        let display = field_display(&options, &value, &empty);
                        let h = self.placed(
                            "STATIC", &display,
                            SS_CENTER | SS_CENTERIMAGE | SS_ENDELLIPSIS | SS_NOPREFIX,
                            id, rect, self.body_font,
                        );
                        presentation_flag(h, "Jiks.Muted", value.is_null());
                        (h, BindingKind::Display { options, empty })
                    } else if let Some(mut options) = options {
                        if show_list {
                            let h = self.placed(
                                "LISTBOX", "", WS_BORDER | WS_TABSTOP | WS_VSCROLL | WS_HSCROLL
                                    | LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32,
                                id, rect, self.body_font,
                            );
                            for choice in &options {
                                SendMessageW(h, LB_ADDSTRING, 0, wide(&choice.label).as_ptr() as isize);
                            }
                            let extent = options.iter().map(|choice| self.measured_button_width(measure_dc, &choice.label)).max().unwrap_or(0);
                            SendMessageW(h, LB_SETITEMHEIGHT, 0, self.s(28) as isize);
                            SendMessageW(h, LB_SETHORIZONTALEXTENT, self.s(extent) as usize, 0);
                            SendMessageW(h, LB_SETCURSEL, selected_choice(&options, &value), 0);
                            (h, BindingKind::List(options))
                        } else {
                        // A selectable list always has a visible selection. The
                        // prompt has a null value and is never sent as a setting.
                        if !options.iter().any(|option| choice_matches(option, &value)) {
                            options.insert(0, UiChoice::new(super::i18n::tr_static("请选择"), Value::Null));
                        }
                        let h = self.placed(
                            "COMBOBOX", "", CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
                            id, [rect[0], rect[1], rect[2], 320], self.body_font,
                        );
                        SendMessageW(h, CB_SETITEMHEIGHT, usize::MAX, self.s(CONTROL_HEIGHT - 6) as isize);
                        SendMessageW(h, CB_SETITEMHEIGHT, 0, self.s(28) as isize);
                        let drop_width = options.iter().map(|choice| choice.label.chars().count() as i32 * 12 + 32)
                            .max().unwrap_or(editor_width).clamp(editor_width, editor_width.max(360));
                        SendMessageW(h, CB_SETDROPPEDWIDTH, self.s(drop_width) as usize, 0);
                        set_choices(h, &options, &value);
                        if process_selector && options.len() == 1 {
                            EnableWindow(h, 0);
                        }
                        (h, BindingKind::Choice(options))
                        }
                    } else if matches!(spec.kind, FieldKind::Toggle) {
                        let h = self.placed("BUTTON", "", BS_AUTOCHECKBOX as u32 | WS_TABSTOP, id, rect, self.body_font);
                        SendMessageW(h, BM_SETCHECK, if value.as_bool() == Some(true) { BST_CHECKED } else { BST_UNCHECKED } as usize, 0);
                        (h, BindingKind::Toggle)
                    } else {
                        let h = self.placed("EDIT", &scalar(&value), WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32, id, rect, self.body_font);
                        SendMessageW(h, EM_SETLIMITTEXT, 65536, 0);
                        SendMessageW(h, EM_SETCUEBANNER, 0, wide(super::i18n::tr_static("请输入")).as_ptr() as isize);
                        let kind = if matches!(spec.kind, FieldKind::Number) { BindingKind::Number } else { BindingKind::Text };
                        (h, kind)
                    };
                    let help = field_help(&model, &spec);
                    a11y::annotate(hwnd, &a11y::key(&scope, "field", spec.key), Some(spec.label), help.as_deref());
                    self.bindings.push(Binding {
                        key: spec.key.into(),
                        hwnd,
                        kind,
                        capability: spec.capability.into(),
                    });
                    if let Some(kind) = picker {
                        let picker_id = id + 8000;
                        let browse = self.placed(
                            "BUTTON", &super::i18n::tr("浏览…"), BS_OWNERDRAW as u32 | WS_TABSTOP,
                            picker_id, [row.control_x + editor_width + 6, editor_y, 64, CONTROL_HEIGHT], self.body_font,
                        );
                        a11y::annotate(
                            browse,
                            &a11y::key(&scope, "browse", spec.key),
                            Some(&format!("{} · {}", spec.label, super::i18n::tr("浏览…"))),
                            None,
                        );
                        self.pickers.insert(picker_id, (spec.key.into(), kind));
                    }
                    let mut card_bottom = row_top + row_height;
                    self.column_x = card_x + 16;
                    if let Some(actions) = attached_actions.remove(spec.key) {
                        let inner_right = card_x + width + 18;
                        card_bottom = self.render_actions(
                            actions, &section_scope, &mut next_button, measure_dc,
                            [row.label_x, card_bottom - pad + 4, inner_right - row.label_x], &model,
                        );
                    }
                    self.cards.push(CardSurface {
                        rect: [card_x, row_top, width + 32, card_bottom - row_top],
                        icon: Some(super::presentation::field_icon(spec.key)),
                    });
                    y = card_bottom + 8;
                }
                y = self.render_actions(remaining_actions, &section_scope, &mut next_button, measure_dc, [card_x, y, width + 32], &model) + GROUP_GAP;
                if two_columns {
                    bottoms[column] = y;
                }
            }
            if two_columns {
                y = bottoms[0].max(bottoms[1]);
            }
            self.column_x = CONTENT_X;
            self.content_height = y;
            if !measure_dc.is_null() { ReleaseDC(self.hwnd, measure_dc); }
            self.position_content();
            self.layout_fixed();
            self.rendering = false;
            self.apply_preferences(&model);
            self.overlay.sync(&model);
            for (id, command) in MAIN_TOOLBAR {
                EnableWindow(GetDlgItem(self.hwnd, id as i32), toolbar_available(&model, id, command) as i32);
            }
            text_if_changed(GetDlgItem(self.hwnd, ID_DEVICE as i32), &device_badge(&model));
            if self.feedback_until.is_none() || get_text(self.notice).is_empty() {
                text(self.notice, a11y::diagnostic().as_deref().unwrap_or_else(|| status_caption(&model)));
            }
            InvalidateRect(self.hwnd, null(), 1);
            #[cfg(debug_assertions)]
            super::capture_fixture::queue(self.hwnd, &self.bridge, &self.accessibility_scope());
        }
    }
    unsafe fn position_content(&mut self) {
        unsafe {
            let (width, height) = self.size();
            let bottom = height - FOOTER_HEIGHT;
            self.scrolling.place(
                self.hwnd,
                &mut self.scroll,
                Viewport {
                    bounds: [CONTENT_X - 12, VIEW_TOP, width - 12, bottom],
                    extent: (self.content_height - VIEW_TOP).max(0),
                    page: bottom - VIEW_TOP,
                    dpi: self.dpi,
                },
                &mut self.content,
            );
        }
    }
    unsafe fn capture(&mut self) {
        unsafe {
            for binding in &self.bindings {
                if !matches!(binding.kind, BindingKind::Display { .. }) && let Ok(value) = read_binding(binding) {
                    if self.dirty.contains(&binding.key) || binding.key.starts_with("selection.") {
                        self.drafts.insert(binding.key.clone(), value);
                    }
                }
            }
        }
    }
    unsafe fn values(&self, fields: &[&str]) -> Result<Value> {
        unsafe {
            let mut object = serde_json::Map::new();
            for key in fields {
                if let Some(binding) = self.bindings.iter().find(|b| b.key == *key) {
                    if IsWindowEnabled(binding.hwnd) != 0 {
                        let value = read_binding(binding)?;
                        if !value.is_null() {
                            object.insert((*key).into(), value);
                        }
                    }
                } else if let Some(value) = self.drafts.get(*key) {
                    object.insert((*key).into(), value.clone());
                }
            }
            Ok(Value::Object(object))
        }
    }
    unsafe fn command(&mut self, id: u16, notification: u16) {
        unsafe {
            if id == ID_NAV_LIST && notification == LBN_SELCHANGE as u16 {
                let index = SendMessageW(
                    GetDlgItem(self.hwnd, ID_NAV_LIST as i32),
                    LB_GETCURSEL,
                    0,
                    0,
                );
                if let Some((_, page)) = main_nav().nth(index as usize) {
                    self.capture();
                    self.page = page;
                    self.scrolling.cancel(self.hwnd);
                    self.scroll = 0;
                    self.render();
                }
                return;
            }
            if id >= ID_NAV && id < ID_NAV + Page::ALL.len() as u16 {
                self.capture();
                self.page = Page::ALL[(id - ID_NAV) as usize];
                self.scrolling.cancel(self.hwnd);
                self.scroll = 0;
                self.render();
                return;
            }
            if matches!(id, ID_DEVICE | ID_THEME) {
                if id == ID_DEVICE {
                    self.modal_active = true;
                    self.deferred_editor = Some(DeferredEditor::DeviceInformation);
                    return;
                }
                let model = live_snapshot(&self.bridge);
                if model.value("backend.read_only").as_bool() == Some(true) {
                    self.preview_theme = Some(if paper() & 0xff < 128 { "light" } else { "dark" });
                    self.sync_palette(&model);
                    return;
                }
                let command = "app.preferences.save";
                let theme = model
                    .values
                    .get("appearance.effective_theme")
                    .or_else(|| model.values.get("appearance.theme"))
                    .and_then(Value::as_str)
                    .unwrap_or("light");
                let payload = json!({"appearance.theme":if theme.eq_ignore_ascii_case("dark"){"light"}else{"dark"}});
                if !model.can(command) {
                    message(
                        self.hwnd,
                        "当前不可用",
                        &model.unavailable_reason(command),
                        false,
                    );
                    return;
                }
                let request = request_id();
                match send(
                    &self.bridge,
                    UiAction::Execute {
                        request_id: request,
                        command: command.into(),
                        payload,
                    },
                ) {
                    Ok(()) => {
                        self.pending.insert(request, false);
                        text(self.notice, "正在处理…");
                    }
                    Err(e) => message(self.hwnd, "操作未完成", &e.to_string(), true),
                }
                return;
            }

            if id >= ID_TOOL_NAV && id < ID_TOOL_NAV + 8 {
                self.capture();
                self.tools_tab = (id - ID_TOOL_NAV) as usize;
                self.scrolling.cancel(self.hwnd);
                self.scroll = 0;
                self.render();
                return;
            }
            if let Some((key, kind)) = self.pickers.get(&id).cloned() {
                let Some(binding) = self.bindings.iter().find(|binding| binding.key == key) else {
                    return;
                };
                if IsWindowEnabled(binding.hwnd) == 0 {
                    return;
                }
                self.deferred_editor = Some(DeferredEditor::Picker {
                    initial: get_text(binding.hwnd),
                    key,
                    kind,
                });
                return;
            }
            if let Some(binding) = self
                .bindings
                .iter()
                .find(|b| GetDlgCtrlID(b.hwnd) == id as i32)
            {
                if self.page == Page::Dashboard && let Some(command) = dashboard_direct_command(&binding.key) {
                    let committed = match binding.kind {
                        BindingKind::Choice(_) => {
                            notification == CBN_CLOSEUP as u16
                                || (notification == CBN_SELCHANGE as u16
                                    && SendMessageW(binding.hwnd, CB_GETDROPPEDSTATE, 0, 0) == 0)
                        }
                        BindingKind::Toggle => notification == BN_CLICKED as u16,
                        _ => false,
                    };
                    if committed && IsWindowEnabled(binding.hwnd) != 0 {
                        self.apply_dashboard_control(id, command);
                    }
                    return;
                }
                if notification == EN_CHANGE as u16
                    || notification == CBN_SELCHANGE as u16
                    || notification == BN_CLICKED as u16
                {
                    self.dirty.insert(binding.key.clone());
                }
                return;
            }
            let Some(action) = self.buttons.get(&id).cloned() else {
                return;
            };
            self.capture();
            let model = snapshot(&self.bridge);
            if !action.capability.is_empty() && !model.can(action.capability) {
                message(
                    self.hwnd,
                    "功能不可用",
                    &model.unavailable_reason(action.capability),
                    false,
                );
                return;
            }
            let result: Result<()> = match action.kind {
                ActionKind::Execute {
                    command,
                    fields,
                    extra,
                    confirm: question,
                } => {
                    if command.starts_with("packages.copy.") {
                        let selected = self
                            .drafts
                            .get("selection.packages")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        match package_copy_text(&model, selected, command)
                            .and_then(|value| copy_text(self.hwnd, &value))
                        {
                            Ok(()) => text(self.notice, &super::i18n::tr("已复制所选项目")),
                            Err(error) => {
                                message(self.hwnd, "复制未完成", &format!("{error:#}"), true)
                            }
                        }
                        return;
                    }
                    if command == "tools.about" {
                        message(
                            self.hwnd,
                            "关于拯救者工具箱",
                            &format!(
                                "拯救者工具箱 {}\nRust原生Win32实现 · GPL-3.0\n基于Lenovo Legion Toolkit迁移，原版设备信息、OSD及功能设计归原作者。\n源码：github.com/gura8390/Y7000P-IRX10",
                                env!("CARGO_PKG_VERSION")
                            ),
                            false,
                        );
                        return;
                    }
                    if command == "script.console" {
                        self.deferred_editor =
                            Some(DeferredEditor::Document("script_console".into()));
                        return;
                    }
                    if question.is_some_and(|body| !confirm(self.hwnd, body)) {
                        return;
                    }
                    let changed_fields_only = matches!(command, "app.preferences.save" | "gpu.settings.save");
                    let fields = if changed_fields_only {
                        fields
                            .into_iter()
                            .filter(|field| self.dirty.contains(*field))
                            .collect::<Vec<_>>()
                    } else {
                        fields
                    };
                    if changed_fields_only && fields.is_empty() {
                        text(self.notice, "没有需要保存的更改");
                        return;
                    }
                    match self.values(&fields) {
                        Ok(mut payload) => {
                            if command == "refresh" {
                                payload["page"] = json!(self.page.key());
                            }
                            if let (Some(dest), Some(source)) =
                                (payload.as_object_mut(), extra.as_object())
                            {
                                dest.extend(source.clone());
                            }
                            let request = request_id();
                            match send(
                                &self.bridge,
                                UiAction::Execute {
                                    request_id: request,
                                    command: command.into(),
                                    payload,
                                },
                            ) {
                                Ok(()) => {
                                    self.pending.insert(request, false);
                                    text(self.notice, "命令已提交，正在等待后台确认…");
                                    Ok(())
                                }
                                Err(e) => Err(e),
                            }
                        }
                        Err(e) => Err(e),
                    }
                }
                ActionKind::Structured {
                    kind,
                    collection,
                    fresh,
                } => {
                    let selected = self
                        .drafts
                        .get(&format!("selection.{collection}"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let item = if fresh {
                        None
                    } else {
                        model
                            .collections
                            .get(collection)
                            .and_then(|items| items.iter().find(|i| i.id == selected))
                            .cloned()
                    };
                    if !fresh && item.is_none() {
                        Err(anyhow!("请先选择要编辑的项目。"))
                    } else {
                        self.deferred_editor = Some(DeferredEditor::Structured(kind, item));
                        Ok(())
                    }
                }
                ActionKind::SensorSettings => {
                    self.deferred_editor = Some(DeferredEditor::SensorSettings);
                    Ok(())
                }
                ActionKind::Document(document) => {
                    self.deferred_editor = Some(DeferredEditor::Document(document.into()));
                    Ok(())
                }
                ActionKind::Navigate(page) => {
                    self.page = page;
                    self.scrolling.cancel(self.hwnd);
                    self.scroll = 0;
                    self.render();
                    Ok(())
                }
                ActionKind::Cancel(fields) => {
                    for field in fields {
                        self.drafts.remove(field);
                        self.dirty.remove(field);
                    }
                    self.render();
                    Ok(())
                }
            };
            if let Err(error) = result {
                message(self.hwnd, "操作未完成", &format!("{error:#}"), true);
            }
        }
    }
    unsafe fn sync_palette(&mut self, model: &UiModel) {
        unsafe {
            if apply_palette(model, self.preview_theme) {
                apply_window_chrome(self.hwnd);
                DeleteObject(self.brush);
                DeleteObject(self.surface_brush);
                self.brush = CreateSolidBrush(paper());
                self.surface_brush = CreateSolidBrush(surface());
                RedrawWindow(self.hwnd, null(), null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN);
            }
        }
    }

    unsafe fn apply_dashboard_control(&mut self, id: u16, command: &str) {
        unsafe {
            let Some(binding) = self.bindings.iter().find(|binding| GetDlgCtrlID(binding.hwnd) == id as i32) else {
                return;
            };
            let model = live_snapshot(&self.bridge);
            let current = model.value(&binding.key);
            if self.direct_pending.values().any(|key| key == &binding.key) {
                set_binding(binding, self.drafts.get(&binding.key).unwrap_or(&current));
                return;
            }
            let Ok(value) = read_binding(binding) else { return; };
            if value.is_null() || choice_matches(&UiChoice::new("", value.clone()), &current) {
                return;
            }
            if !model.can(command) {
                set_binding(binding, &current);
                text_if_changed(self.notice, &model.unavailable_reason(command));
                self.feedback_until = Some(Instant::now() + Duration::from_secs(6));
                return;
            }
            if command == "gpu.hybrid.apply"
                && !confirm(self.hwnd, "应用所选显卡工作模式？若需要重启，后台将先返回说明。")
            {
                set_binding(binding, &current);
                return;
            }
            let key = binding.key.clone();
            let request = request_id();
            let payload = json!({(key.clone()): value.clone()});
            match send(&self.bridge, UiAction::Execute { request_id: request, command: command.into(), payload }) {
                Ok(()) => {
                    self.pending.insert(request, false);
                    self.direct_pending.insert(request, key.clone());
                    self.dirty.insert(key.clone());
                    self.drafts.insert(key, value);
                    text_if_changed(self.notice, super::i18n::tr_static("正在处理…"));
                }
                Err(error) => {
                    set_binding(binding, &current);
                    message(self.hwnd, "操作未完成", &error.to_string(), true);
                }
            }
        }
    }

    unsafe fn refresh(&mut self) {
        unsafe {
            if !self.smoke_submitted
                && self
                    .bridge
                    .smoke_close_after
                    .is_some_and(|after| self.started.elapsed() >= after)
            {
                self.smoke_submitted = true;
                self.bridge
                    .diagnostics
                    .smoke_deadline_exits
                    .fetch_add(1, Ordering::Relaxed);
                self.bridge
                    .diagnostics
                    .last_exit_origin
                    .store(4, Ordering::Relaxed);
                self.shutdown();
            }
            if self.bridge.smoke_exercise_reentrancy && !self.smoke_submitted && !self.modal_active
            {
                match self.smoke_stage {
                    0 => {
                        self.smoke_stage = 1;
                        PostMessageW(self.hwnd, WM_SIZE, SIZE_RESTORED as usize, 0);
                    }
                    1 => {
                        self.smoke_stage = 2;
                        self.capture();
                        self.page = Page::Settings;
                        self.tools_tab = 0;
                        self.render();
                        self.deferred_editor = Some(DeferredEditor::SensorSettings);
                    }
                    2 => {
                        self.smoke_stage = 3;
                        self.capture();
                        self.page = Page::Dashboard;
                        self.render();
                    }
                    3 => {
                        self.smoke_stage = 4;
                        if let Ok(mut model) = self.bridge.model.lock() {
                            model.set_value("osd.visible", true);
                        }
                    }
                    4 => {
                        self.smoke_stage = 5;
                        if let Ok(mut model) = self.bridge.model.lock() {
                            model.set_value("osd.visible", false);
                            model.values.insert("keyboard.profile".into(), json!(1));
                            model.values.insert("keyboard.effects".into(), json!([]));
                            model.touch_metadata();
                        }
                        self.deferred_editor =
                            Some(DeferredEditor::Document("spectrum_keyboard".into()));
                    }
                    5 => {
                        self.smoke_stage = 6;
                        if let Ok(mut model) = self.bridge.model.lock() {
                            model.collections.insert("device.information".into(),vec![UiItem{id:"fixture".into(),label:"设备信息".into(),detail:"纯界面fixture".into(),value:json!({"groups":[{"title":"CPU","rows":[{"label":"处理器","value":"确定性界面样本"}]},{"title":"磁盘","rows":[{"label":"序列号","value":"FIXTURE_PRIVATE","sensitive":true}]}]})}]);
                            model.touch_metadata();
                        }
                        self.deferred_editor = Some(DeferredEditor::DeviceInformation);
                    }
                    6 => {
                        self.smoke_stage = 7;
                        if let Ok(mut model) = self.bridge.model.lock() {
                            model.values.insert(
                                "automation.ui_effect".into(),
                                json!({"effect_id":u64::MAX-1,"kind":"show","payload":{}}),
                            );
                            model.revision += 1;
                        }
                    }
                    7 if self
                        .bridge
                        .diagnostics
                        .ui_effects_completed
                        .load(Ordering::Relaxed)
                        >= 1 =>
                    {
                        self.smoke_stage = 8;
                        if let Ok(mut model) = self.bridge.model.lock() {
                            model.values.insert(
                                "automation.ui_effect".into(),
                                json!({"effect_id":u64::MAX-2,"kind":"show","payload":{}}),
                            );
                            model.revision += 1;
                        }
                    }
                    8 if self
                        .bridge
                        .diagnostics
                        .ui_effects_completed
                        .load(Ordering::Relaxed)
                        >= 2 =>
                    {
                        self.smoke_stage = 9;
                        if let Ok(mut model) = self.bridge.model.lock() {
                            model
                                .values
                                .insert("appearance.language".into(), json!("en"));
                            model.touch_metadata();
                        }
                    }
                    9 if super::i18n::language() == "en"
                        && self.navigation_text(6) == super::i18n::tr_static("设置") =>
                    {
                        self.bridge
                            .diagnostics
                            .locale_switches
                            .fetch_add(1, Ordering::Relaxed);
                        self.smoke_stage = 10;
                        if let Ok(mut model) = self.bridge.model.lock() {
                            model
                                .values
                                .insert("appearance.language".into(), json!("zh-Hans"));
                            model.touch_metadata();
                        }
                    }
                    10 if super::i18n::language() != "en" && self.navigation_text(6) == "设置" => {
                        self.bridge
                            .diagnostics
                            .locale_switches
                            .fetch_add(1, Ordering::Relaxed);
                        self.smoke_stage = 11;
                    }
                    11 => {
                        self.smoke_stage = 12;
                        self.show_status_popup(true);
                    }
                    12 => {
                        self.smoke_stage = 13;
                        if let Ok(mut model) = self.bridge.model.lock() {
                            model.set_value("battery.capacity", json!(43210));
                        }
                    }
                    13 => {
                        self.smoke_stage = 14;
                        self.hide_status_popup();
                    }
                    _ => {}
                }
            }
            self.poll_ui_effect();
            if let Err(error) = self.notifications.tick() {
                text(self.notice, &format!("通知清理待重试：{error:#}"));
            }
            if self.notifications.take_clicked().as_deref() == Some("update.available") {
                self.capture();
                self.page = Page::Settings;
                self.tools_tab = catalog::UPDATE_TAB;
                self.scrolling.cancel(self.hwnd);
                self.scroll = 0;
                self.render();
            }
            self.report_visibility();
            let background_model = live_snapshot(&self.bridge);
            if self.feedback_until.is_some_and(|until| Instant::now() >= until) {
                self.feedback_until = None;
                if self.pending.is_empty() { text_if_changed(self.notice, status_caption(&background_model)); }
            }
            if !self.language_prompted
                && self.bridge.startup_language_prompt
                && self.bridge.smoke_close_after.is_none()
                && background_model.value("backend.ready").as_bool() == Some(true)
                && background_model.value("backend.read_only").as_bool() != Some(true)
                && background_model
                    .value("appearance.language_saved")
                    .as_bool()
                    == Some(false)
                && !self.modal_active
                && self.deferred_editor.is_none()
                && IsWindowVisible(self.hwnd) != 0
                && IsIconic(self.hwnd) == 0
                && !self.pending.values().any(|shutdown| *shutdown)
            {
                self.language_prompted = true;
                self.deferred_editor = Some(DeferredEditor::Document("first_language".into()));
            }
            if super::i18n::set_language(
                background_model
                    .value("appearance.language")
                    .as_str()
                    .unwrap_or("zh-Hans"),
            ) {
                self.capture();
                self.build_fixed();
                self.render();
                self.metadata_revision = u64::MAX;
            }
            if let Err(error) = self.popup.sync(&background_model) {
                text(self.notice, &format!("状态窗口清理或刷新待重试：{error:#}"));
            }
            self.report_visibility();
            let mut background_rect: RECT = zeroed();
            GetClientRect(self.hwnd, &mut background_rect);
            if let Some(error) = self.background.sync(
                &background_model,
                background_rect.right,
                background_rect.bottom,
                paper(),
            ) {
                text(self.notice, &error);
            }
            let has_background = self.background.visible();
            if (!GetPropW(self.hwnd, wide("Jiks.Background").as_ptr()).is_null()) != has_background
            {
                presentation_flag(self.hwnd, "Jiks.Background", has_background);
            }
            if self.background.take_dirty() {
                InvalidateRect(self.hwnd, null(), 1);
            }
            let unchanged = self
                .bridge
                .model
                .lock()
                .map(|m| {
                    m.revision == self.rendered_revision
                        && m.metadata_revision == self.metadata_revision
                        && !m.shutdown_ready
                })
                .unwrap_or(false);
            if unchanged && self.pending.is_empty() {
                self.sync_gpu_process_selector();
                self.overlay.flush();
                self.report_visibility();
                return;
            }
            let mut model = live_snapshot(&self.bridge);
            if self.pending.values().any(|shutdown| *shutdown) {
                model.values.insert("osd.visible".into(), json!(false));
            }
            self.overlay.sync(&model);
            self.report_visibility();
            let desired = self.overlay.desired_tick();
            if desired != self.timer_ms {
                self.timer_ms = desired;
                SetTimer(self.hwnd, TIMER, desired, None);
            }
            self.sync_palette(&model);
            for (id, command) in MAIN_TOOLBAR {
                enable_if_changed(GetDlgItem(self.hwnd, id as i32), toolbar_available(&model, id, command));
            }
            if model.shutdown_ready {
                self.deferred_destroy = true;
                return;
            }
            if IsWindowVisible(self.hwnd) != 0
                && (backend_state(&model) != self.backend_state
                    || self.has_changed_page_structure(&model)
                    || self.has_new_metric_rows(&model))
            {
                self.capture();
                self.render();
            }
            self.sync_gpu_process_selector();
            let mut completed = vec![];
            for (request, shutdown) in &self.pending {
                if let Some(result) = model.result(*request) {
                    text(self.notice, &result.message);
                    self.feedback_until = Some(Instant::now() + Duration::from_secs(6));
                    if *shutdown && result.ok {
                        self.deferred_destroy = true;
                        return;
                    }
                    completed.push(*request);
                }
            }
            for request in completed {
                self.pending.remove(&request);
                if let Some(key) = self.direct_pending.remove(&request) {
                    self.drafts.remove(&key);
                    self.dirty.remove(&key);
                    if let Some(binding) = self.bindings.iter().find(|binding| binding.key == key) {
                        set_binding(binding, &model.value(&key));
                    }
                }
            }
            if model.revision != self.rendered_revision {
                self.rendering = true;
                self.rendered_revision = model.revision;
                text_if_changed(GetDlgItem(self.hwnd, ID_DEVICE as i32), &device_badge(&model));
                if self.pending.is_empty() && self.feedback_until.is_none() {
                    text_if_changed(self.notice, status_caption(&model));
                }
                let mut changed_readings = 0;
                for placed in &self.metrics {
                    if let Some(metric) = model.metrics.iter().find(|m| m.key == placed.key) {
                        let value_changed = text_if_changed(placed.value, &metric.value);
                        changed_readings += u64::from(value_changed);
                        let mut changed = value_changed;
                        for (hwnd, value) in [
                            (placed.name, metric_caption(metric, placed.compact)),
                            (placed.detail, metric.detail.as_str()),
                        ] {
                            changed |= text_if_changed(hwnd, value);
                        }
                        if changed {
                            a11y::annotate(
                                placed.value,
                                &a11y::key(&self.accessibility_scope(), "metric", &metric.key),
                                Some(&format!(
                                    "{} {} {}",
                                    metric.label, metric.value, metric.detail
                                )),
                                Some(&metric.detail),
                            );
                        }
                    } else {
                        // Keep the row and its neighbors stable after a
                        // missed sample, and clear the stale reading.
                        let missing = super::i18n::tr_static("暂无数据");
                        let value_changed = text_if_changed(placed.value, "—");
                        changed_readings += u64::from(value_changed);
                        let changed = text_if_changed(placed.detail, missing) || value_changed;
                        if changed {
                            a11y::annotate(
                                placed.value,
                                &a11y::key(&self.accessibility_scope(), "metric", &placed.key),
                                Some(&format!("{} · {}", get_text(placed.name), missing)),
                                Some(missing),
                            );
                        }
                    }
                }
                if changed_readings != 0 {
                    self.bridge.diagnostics.main_metric_text_updates
                        .fetch_add(changed_readings, Ordering::Relaxed);
                }
                for binding in &self.bindings {
                    if self.page == Page::Dashboard && binding.key == GPU_PROCESS_SELECTOR {
                        continue;
                    }
                    if !matches!(binding.kind, BindingKind::Display { .. }) {
                        let enabled = field_available(&model, &binding.key, &binding.capability);
                        enable_if_changed(binding.hwnd, enabled);
                    }
                    let selecting = matches!(binding.kind, BindingKind::Choice(_))
                        && SendMessageW(binding.hwnd, CB_GETDROPPEDSTATE, 0, 0) != 0;
                    if !self.dirty.contains(&binding.key) && !selecting {
                        set_binding(binding, &field_value(&model, &binding.key));
                    }
                }
                for (id, (key, _)) in &self.pickers {
                    if let Some(binding) = self.bindings.iter().find(|binding| binding.key == *key)
                    {
                        enable_if_changed(
                            GetDlgItem(self.hwnd, *id as i32),
                            IsWindowEnabled(binding.hwnd) != 0,
                        );
                    }
                }
                for (id, action) in &self.buttons {
                    enable_if_changed(
                        GetDlgItem(self.hwnd, *id as i32),
                        action.capability.is_empty() || model.can(action.capability),
                    );
                }
                self.rendering = false;
            }
            self.apply_preferences(&model);
        }
    }
    fn has_changed_page_structure(&mut self, model: &UiModel) -> bool {
        if self.metadata_revision == model.metadata_revision {
            return false;
        }
        if self.page != Page::Dashboard {
            return true;
        }
        let full = snapshot(&self.bridge);
        let sections = catalog::sections(self.page, self.tools_tab, &full);
        let structure = DashboardStructure::new(&full, &sections);
        if self.dashboard_structure.as_ref() != Some(&structure) {
            return true;
        }
        self.metadata_revision = full.metadata_revision;
        self.pending_process_options = Some(gpu_process_choices(&full));
        false
    }
    unsafe fn sync_gpu_process_selector(&mut self) {
        unsafe {
            let Some(options) = self.pending_process_options.as_ref() else { return };
            let Some(binding) = self.bindings.iter_mut().find(|binding| binding.key == GPU_PROCESS_SELECTOR)
                else {
                    self.pending_process_options = None;
                    return;
                };
            let BindingKind::Choice(previous) = &mut binding.kind else {
                self.pending_process_options = None;
                return;
            };
            if previous == options {
                self.pending_process_options = None;
                return;
            }
            // Replacing a combo's items closes its native drop-down. Keep it
            // interactive and apply the latest list once the user closes it.
            if SendMessageW(binding.hwnd, CB_GETDROPPEDSTATE, 0, 0) != 0 {
                return;
            }
            let Some(options) = self.pending_process_options.take() else { return };
            let index = SendMessageW(binding.hwnd, CB_GETCURSEL, 0, 0) as usize;
            let selected = previous.get(index).map(|choice| &choice.value).unwrap_or(&Value::Null);
            set_choices(binding.hwnd, &options, selected);
            let dpi = self.dpi as i32;
            let mut client: RECT = zeroed();
            GetClientRect(binding.hwnd, &mut client);
            let dropped_width = options.iter().map(|choice| choice.label.chars().count() as i32 * 12 + 32)
                .max().unwrap_or(0) * dpi / 96;
            let dropped_width = dropped_width.clamp(client.right, client.right.max(360 * dpi / 96));
            SendMessageW(binding.hwnd, CB_SETDROPPEDWIDTH, dropped_width as usize, 0);
            enable_if_changed(binding.hwnd, options.len() > 1);
            *previous = options;
        }
    }
    fn has_new_metric_rows(&mut self, model: &UiModel) -> bool {
        if !matches!(self.page, Page::Dashboard | Page::Battery) {
            return false;
        }
        let mut observed_new_key = false;
        for metric in &model.metrics {
            if !self.observed_metric_keys.contains(&metric.key) {
                self.observed_metric_keys.insert(metric.key.clone());
                observed_new_key = true;
            }
        }
        if !observed_new_key {
            return false;
        }
        let has_row = |key: &str| self.metrics.iter().any(|placed| placed.key == key);
        match self.page {
            Page::Dashboard => super::sensors::groups(&snapshot(&self.bridge))
                .iter()
                .flat_map(|group| &group.metrics)
                .any(|metric| !has_row(&metric.key)),
            Page::Battery => model.metrics.iter().any(|metric| {
                metric.key.to_ascii_lowercase().starts_with("battery") && !has_row(&metric.key)
            }),
            _ => false,
        }
    }
    unsafe fn poll_ui_effect(&mut self) {
        unsafe {
            if self.pending_ui_ack.is_none() {
                let effect = self.bridge.model.lock().ok().and_then(|m| {
                    [
                        (
                            "automation.ui_effect",
                            "automation.effect.complete",
                            self.last_ui_effect,
                        ),
                        (
                            "native.ui_effect",
                            "native.effect.complete",
                            self.last_native_effect,
                        ),
                    ]
                    .into_iter()
                    .find_map(|(key, command, last)| {
                        m.values
                            .get(key)
                            .filter(|effect| {
                                effect
                                    .get("effect_id")
                                    .and_then(Value::as_u64)
                                    .is_some_and(|id| id != last)
                            })
                            .map(|effect| (effect.clone(), command))
                    })
                });
                if let Some((effect, ack_command)) = effect {
                    if let Some(id) = effect.get("effect_id").and_then(Value::as_u64) {
                        let kind = effect.get("kind").and_then(Value::as_str).unwrap_or("");
                        let payload = &effect["payload"];
                        let (ok, message, close) = match kind {
                            "show" => {
                                self.foreground();
                                (
                                    IsWindowVisible(self.hwnd) != 0,
                                    "主窗口已显示；前台焦点由Windows决定".into(),
                                    false,
                                )
                            }
                            "snip" => {
                                let result = ShellExecuteW(
                                    self.hwnd,
                                    wide("open").as_ptr(),
                                    wide("ms-screenclip:").as_ptr(),
                                    null(),
                                    null(),
                                    SW_SHOWNORMAL,
                                ) as isize;
                                (
                                    result > 32,
                                    if result > 32 {
                                        "Windows已接受截图工具协议请求；截图由系统工具处理".into()
                                    } else {
                                        format!("截图工具启动请求失败：{result}")
                                    },
                                    false,
                                )
                            }
                            "notification" => {
                                let settings = self
                                    .bridge
                                    .model
                                    .lock()
                                    .ok()
                                    .and_then(|model| {
                                        model
                                            .documents
                                            .get("notifications")
                                            .map(|doc| doc.value.clone())
                                    })
                                    .unwrap_or_else(|| json!({}));
                                match self.notifications.show(self.hwnd, &settings, payload) {
                                    Ok(message) => (true, message, false),
                                    Err(error) => {
                                        (false, format!("通知显示未完成：{error:#}"), false)
                                    }
                                }
                            }
                            "close" => (
                                true,
                                "已收到安全退出请求；等待后台实际清理与所有权释放".into(),
                                true,
                            ),
                            _ => (false, "未知的界面自动化动作".into(), false),
                        };
                        if ack_command == "native.effect.complete" {
                            self.last_native_effect = id;
                        } else {
                            self.last_ui_effect = id;
                        }
                        if ok {
                            self.bridge
                                .diagnostics
                                .ui_effects_completed
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        self.pending_ui_ack = Some((id, ok, message, close, ack_command.into()));
                    }
                }
            }
            if let Some((id, ok, message, close, command)) = self.pending_ui_ack.as_ref() {
                if send(
                    &self.bridge,
                    UiAction::Execute {
                        request_id: request_id(),
                        command: command.clone(),
                        payload: json!({"effect_id":id,"ok":ok,"message":message}),
                    },
                )
                .is_ok()
                {
                    let close = *close;
                    self.pending_ui_ack = None;
                    if close {
                        self.bridge
                            .diagnostics
                            .ui_effect_exits
                            .fetch_add(1, Ordering::Relaxed);
                        self.bridge
                            .diagnostics
                            .last_exit_origin
                            .store(5, Ordering::Relaxed);
                        self.shutdown();
                    }
                }
            }
        }
    }
    unsafe fn report_visibility(&mut self) {
        let next = (
            unsafe { IsWindowVisible(self.hwnd) != 0 && IsIconic(self.hwnd) == 0 },
            self.page.key().to_owned(),
            self.overlay.actual_visible(),
            self.popup.is_visible(),
        );
        if self.last_visibility.as_ref() == Some(&next) {
            return;
        }
        if send(&self.bridge, UiAction::Execute { request_id: request_id(), command: "ui.visibility".into(), payload: json!({"main_visible":next.0,"page":next.1,"osd_visible":next.2,"status_visible":next.3}) }).is_ok() {
            if self.bridge.smoke_exercise_reentrancy&&self.last_visibility.as_ref().map(|previous|previous.3).unwrap_or(false)!=next.3{if next.3{self.bridge.diagnostics.status_opened.fetch_add(1,Ordering::Relaxed);}else{self.bridge.diagnostics.status_closed.fetch_add(1,Ordering::Relaxed);}}
            self.last_visibility = Some(next);
        }
    }
    unsafe fn navigation_text(&self, index: usize) -> String {
        unsafe {
            let list = GetDlgItem(self.hwnd, ID_NAV_LIST as i32);
            if IsWindowVisible(list) != 0 {
                let Some(index) = main_nav().position(|(id, _)| id == index) else {
                    return String::new();
                };
                let length = SendMessageW(list, LB_GETTEXTLEN, index, 0);
                if (0..512).contains(&length) {
                    let mut buffer = vec![0u16; length as usize + 1];
                    if SendMessageW(list, LB_GETTEXT, index, buffer.as_mut_ptr() as isize) >= 0 {
                        return String::from_utf16_lossy(&buffer[..length as usize]);
                    }
                }
                String::new()
            } else {
                get_text(GetDlgItem(self.hwnd, ID_NAV as i32 + index as i32))
            }
        }
    }
    unsafe fn apply_preferences(&mut self, model: &UiModel) {
        unsafe {
            if !self.window_restored {
                // Realtime snapshots intentionally omit object-valued metadata.
                // Read both preferences coherently from the full model once,
                // including when startup keeps the main window minimized.
                let preferences = window_restore_preferences(&self.bridge.model);
                if let Some((remember, geometry)) = preferences {
                    self.window_restored = true;
                    if let Some((w, h)) = saved_window_size(&geometry) {
                        let monitor =
                            if let Some((x, y)) = saved_window_position(&geometry, remember) {
                                let scale = self.dpi as f64 / 96.;
                                let rect = RECT {
                                    left: (x * scale).round() as i32,
                                    top: (y * scale).round() as i32,
                                    right: ((x + w) * scale).round() as i32,
                                    bottom: ((y + h) * scale).round() as i32,
                                };
                                MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST)
                            } else {
                                // The original centers an independently saved size on
                                // the primary display when position is not remembered.
                                MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
                            };
                        let mut info: MONITORINFO = zeroed();
                        info.cbSize = size_of::<MONITORINFO>() as u32;
                        if GetMonitorInfoW(monitor, &mut info) != 0 {
                            if let Some(bounds) =
                                restored_window(&geometry, remember, self.dpi, info.rcWork)
                            {
                                SetWindowPos(
                                    self.hwnd,
                                    null_mut(),
                                    bounds.left,
                                    bounds.top,
                                    bounds.width,
                                    bounds.height,
                                    SWP_NOZORDER | SWP_NOACTIVATE,
                                );
                            }
                        }
                    }
                }
            }
            if self.topmost != model.preferences.always_on_top {
                self.topmost = model.preferences.always_on_top;
                SetWindowPos(
                    self.hwnd,
                    if self.topmost {
                        HWND_TOPMOST
                    } else {
                        HWND_NOTOPMOST
                    },
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }
    unsafe fn foreground(&mut self) {
        unsafe {
            self.hide_status_popup();
            ShowWindow(self.hwnd, SW_RESTORE);
            SetForegroundWindow(self.hwnd);
        }
    }
    unsafe fn shutdown(&mut self) {
        unsafe {
            if self.pending.values().any(|shutdown| *shutdown) {
                return;
            }
            self.cancel_tray_hover();
            if let Err(error) = self.popup.hide() {
                message(
                    self.hwnd,
                    "尚未退出",
                    &format!("状态窗口清理未确认，保留界面：{error:#}"),
                    true,
                );
                return;
            }
            if self.notifications.is_active() {
                if let Err(error) = self.notifications.close_all() {
                    message(
                        self.hwnd,
                        "尚未退出",
                        &format!("通知窗口清理失败，界面保留，可再次退出重试：{error:#}"),
                        true,
                    );
                    return;
                }
            }
            if let Err(error) = self.overlay.close() {
                message(
                    self.hwnd,
                    "尚未退出",
                    &format!("OSD窗口清理未确认，保留界面：{error:#}"),
                    true,
                );
                return;
            }
            self.save_window();
            let id = request_id();
            self.bridge
                .diagnostics
                .shutdown_requests
                .fetch_add(1, Ordering::Relaxed);
            match send(&self.bridge, UiAction::Shutdown { request_id: id }) {
                Ok(()) => {
                    self.pending.insert(id, true);
                    text(self.notice, "正在停止后台任务并确认恢复；完成后退出…");
                }
                Err(e) => message(self.hwnd, "尚未退出", &e.to_string(), true),
            }
        }
    }
    unsafe fn save_window(&mut self) {
        unsafe {
            let model = live_snapshot(&self.bridge);
            if !self.window_restored
                || self.bridge.smoke_close_after.is_some()
                || model.value("backend.read_only").as_bool() == Some(true)
                || IsIconic(self.hwnd) != 0
                || IsZoomed(self.hwnd) != 0
            {
                return;
            }
            let mut r: RECT = zeroed();
            if GetWindowRect(self.hwnd, &mut r) == 0 {
                return;
            }
            let Some(payload) = window_save_payload(
                r,
                self.dpi,
                model.value("app.remember_position").as_bool() == Some(true),
            ) else {
                return;
            };
            if let Err(error) = send(
                &self.bridge,
                UiAction::Execute {
                    request_id: request_id(),
                    command: "app.preferences.save".into(),
                    payload,
                },
            ) {
                text(self.notice, &format!("窗口大小或位置尚未保存：{error:#}"));
            }
        }
    }
    unsafe fn add_tray(&mut self) {
        unsafe {
            self.tray_data = zeroed();
            self.tray_data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            self.tray_data.hWnd = self.hwnd;
            self.tray_data.uID = 1;
            self.tray_data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            self.tray_data.uCallbackMessage = WM_TRAY;
            self.tray_data.hIcon = app_icon();
            copy_wide(&mut self.tray_data.szTip, "拯救者工具箱");
            self.tray = Shell_NotifyIconW(NIM_ADD, &self.tray_data) != 0;
            if self.tray {
                self.tray_data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                Shell_NotifyIconW(NIM_SETVERSION, &self.tray_data);
            }
        }
    }
    unsafe fn cancel_tray_hover(&mut self) {
        unsafe {
            self.tray_hover_at = None;
            KillTimer(self.hwnd, TRAY_HOVER_TIMER);
            if let Err(error) = self.popup.hide_hover() {
                text(self.notice, &format!("托盘状态卡清理未确认：{error:#}"));
            }
            self.report_visibility();
        }
    }
    unsafe fn tray_menu(&mut self) {
        unsafe {
            self.hide_status_popup();
            let model = snapshot(&self.bridge);
            let menu = CreatePopupMenu();
            for (index, quick) in model.quick_actions.iter().enumerate() {
                AppendMenuW(
                    menu,
                    MF_STRING,
                    ID_QUICK + index,
                    wide(&format!(
                        "{}{}{}",
                        crate::ui_model::quick_action_glyph(&quick.icon),
                        if quick.icon.is_empty() { "" } else { "  " },
                        quick.label
                    ))
                    .as_ptr(),
                );
            }
            if !model.quick_actions.is_empty() {
                AppendMenuW(menu, MF_SEPARATOR, 0, null());
            }
            for (index, page) in Page::ALL.iter().enumerate() {
                AppendMenuW(
                    menu,
                    MF_STRING,
                    ID_NAV as usize + index,
                    wide(page.title()).as_ptr(),
                );
            }
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(
                menu,
                MF_STRING,
                ID_TRAY_STATUS,
                wide(&super::i18n::tr("查看本机状态")).as_ptr(),
            );
            AppendMenuW(
                menu,
                MF_STRING,
                ID_TRAY_OPEN,
                wide(&super::i18n::tr("显示拯救者工具箱")).as_ptr(),
            );
            AppendMenuW(
                menu,
                MF_STRING,
                ID_TRAY_EXIT,
                wide(&super::i18n::tr("退出拯救者工具箱")).as_ptr(),
            );
            let mut point: POINT = zeroed();
            GetCursorPos(&mut point);
            SetForegroundWindow(self.hwnd);
            self.scrolling.cancel(self.hwnd);
            let command = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                self.hwnd,
                null(),
            ) as usize;
            DestroyMenu(menu);
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            if command >= ID_NAV as usize && command < ID_NAV as usize + Page::ALL.len() {
                self.page = Page::ALL[command - ID_NAV as usize];
                self.scrolling.cancel(self.hwnd);
                self.scroll = 0;
                self.foreground();
                self.capture();
                self.render();
            } else if command == ID_TRAY_OPEN {
                self.foreground();
            } else if command == ID_TRAY_EXIT {
                self.bridge
                    .diagnostics
                    .tray_exit_invocations
                    .fetch_add(1, Ordering::Relaxed);
                self.bridge
                    .diagnostics
                    .last_exit_origin
                    .store(3, Ordering::Relaxed);
                self.shutdown();
            } else if command == ID_TRAY_STATUS {
                self.show_status_popup(true);
            } else if command >= ID_QUICK {
                if let Some(quick) = model.quick_actions.get(command - ID_QUICK) {
                    let id = request_id();
                    if let Err(error) = send(
                        &self.bridge,
                        UiAction::Execute {
                            request_id: id,
                            command: "automation.run".into(),
                            payload: json!({"selection.quick_action":quick.id}),
                        },
                    ) {
                        message(self.hwnd, "快捷操作未提交", &error.to_string(), true);
                    } else {
                        self.pending.insert(id, false);
                    }
                }
            }
        }
    }
    unsafe fn show_status_popup(&mut self, pinned: bool) {
        let model = live_snapshot(&self.bridge);
        if let Err(error) = self.popup.show(self.hwnd, &model, self.dpi, pinned) {
            unsafe {
                text(self.notice, &format!("状态窗口未显示：{error:#}"));
            }
        }
        unsafe {
            self.report_visibility();
        }
    }
    unsafe fn hide_status_popup(&mut self) {
        unsafe {
            self.tray_hover_at = None;
            KillTimer(self.hwnd, TRAY_HOVER_TIMER);
        }
        if let Err(error) = self.popup.hide() {
            unsafe {
                text(self.notice, &format!("状态窗口清理未确认：{error:#}"));
            }
        }
    }
}

pub(super) unsafe fn set_choices(hwnd: HWND, options: &[UiChoice], value: &Value) {
    unsafe {
        SendMessageW(hwnd, CB_RESETCONTENT, 0, 0);
        for option in options {
            SendMessageW(hwnd, CB_ADDSTRING, 0, wide(&option.label).as_ptr() as isize);
        }
        if !options.is_empty() {
            let index = selected_choice(options, value);
            SendMessageW(hwnd, CB_SETCURSEL, index, 0);
        }
    }
}
unsafe fn read_binding(binding: &Binding) -> Result<Value> {
    unsafe {
        Ok(match &binding.kind {
            BindingKind::Toggle => {
                json!(SendMessageW(binding.hwnd, BM_GETCHECK, 0, 0) == BST_CHECKED as isize)
            }
            BindingKind::Text => json!(get_text(binding.hwnd)),
            BindingKind::Number => {
                let value = get_text(binding.hwnd);
                if value.trim().is_empty() {
                    Value::Null
                } else {
                    serde_json::from_str::<Value>(value.trim())
                        .ok()
                        .filter(Value::is_number)
                        .ok_or_else(|| anyhow!("{} 需要有效数字。", binding.key))?
                }
            }
            BindingKind::Choice(options) => {
                let index = SendMessageW(binding.hwnd, CB_GETCURSEL, 0, 0);
                if index < 0 {
                    Value::Null
                } else {
                    options
                        .get(index as usize)
                        .map(|o| o.value.clone())
                        .unwrap_or(Value::Null)
                }
            }
            BindingKind::List(options) => {
                let index = SendMessageW(binding.hwnd, LB_GETCURSEL, 0, 0);
                options.get(index as usize).map_or(Value::Null, |option| option.value.clone())
            }
            BindingKind::Display { .. } => Value::Null,
        })
    }
}
unsafe fn set_binding(binding: &Binding, value: &Value) {
    unsafe {
        match &binding.kind {
            BindingKind::Toggle => {
                let state = if value.as_bool().unwrap_or(false) {
                    BST_CHECKED
                } else {
                    BST_UNCHECKED
                };
                if SendMessageW(binding.hwnd, BM_GETCHECK, 0, 0) != state as isize {
                    SendMessageW(binding.hwnd, BM_SETCHECK, state as usize, 0);
                }
            }
            BindingKind::Text | BindingKind::Number => {
                let v = scalar(value);
                if get_text(binding.hwnd) != v {
                    text(binding.hwnd, &v);
                }
            }
            BindingKind::Choice(options) => {
                let index = selected_choice(options, value);
                if SendMessageW(binding.hwnd, CB_GETCURSEL, 0, 0) as usize != index {
                    SendMessageW(binding.hwnd, CB_SETCURSEL, index, 0);
                }
            }
            BindingKind::List(options) => {
                let index = selected_choice(options, value);
                if SendMessageW(binding.hwnd, LB_GETCURSEL, 0, 0) as usize != index {
                    SendMessageW(binding.hwnd, LB_SETCURSEL, index, 0);
                }
            }
            BindingKind::Display { options, empty } => {
                let caption = field_display(options, value, empty);
                if get_text(binding.hwnd) != caption {
                    presentation_flag(binding.hwnd, "Jiks.Muted", value.is_null());
                    text(binding.hwnd, &caption);
                }
            }
        }
    }
}
unsafe fn copy_wide<const N: usize>(dest: &mut [u16; N], source: &str) {
    dest.fill(0);
    for (d, s) in dest.iter_mut().take(N - 1).zip(source.encode_utf16()) {
        *d = s;
    }
}

#[cfg(test)]
mod presentation_regression_tests {
    use super::*;
    #[test]
    fn dashboard_structure_ignores_live_values_and_unrelated_metadata() {
        let mut model = UiModel::default();
        let structure = |m: &UiModel| DashboardStructure::new(m, &catalog::sections(Page::Dashboard, 0, m));
        let initial = structure(&model);
        model.set_value("display.brightness", 40);
        model.metrics.push(UiMetric { key: "CpuTemperature".into(), value: "62 °C".into(), ..Default::default() });
        model.collections.insert("packages".into(), vec![UiItem { id: "driver".into(), ..Default::default() }]);
        model.touch_metadata();
        assert!(structure(&model) == initial);

        model.collections.insert("gpu.processes".into(), vec![UiItem { id: "1:2".into(), label: "example.exe".into(), ..Default::default() }]);
        model.touch_metadata();
        assert!(structure(&model) == initial);
        assert_eq!(gpu_process_choices(&model)[1].value, json!("1:2"));

        model.options.insert("power.mode".into(), vec![UiChoice::new("均衡", 2)]);
        model.touch_metadata();
        assert!(structure(&model) != initial);
    }
    #[test]
    fn native_static_print_uses_solid_surface_and_muted_text_from_the_parent() {
        unsafe extern "system" fn parent_proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
            unsafe {
                presentation_message(msg, wp, lp)
                    .unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp))
            }
        }
        unsafe {
            let class = wide("Jiks.Test.Presentation.StaticPrint");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(parent_proc),
                hInstance: GetModuleHandleW(null()),
                lpszClassName: class.as_ptr(),
                ..zeroed()
            };
            assert_ne!(RegisterClassW(&wc), 0);
            let parent = CreateWindowExW(
                0,
                class.as_ptr(),
                null(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                400,
                200,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            let body_font = font(96, 14, false);
            let label = create(
                parent,
                "STATIC",
                "Synthetic caption",
                0,
                1000,
                [10, 10, 240, 40],
                body_font,
            );
            presentation_flag(label, "Jiks.Surface", true);
            presentation_flag(label, "Jiks.Muted", true);
            let screen = GetDC(null_mut());
            let dc = CreateCompatibleDC(screen);
            let bitmap = CreateCompatibleBitmap(screen, 240, 40);
            ReleaseDC(null_mut(), screen);
            let old_bitmap = SelectObject(dc, bitmap);
            fill(
                dc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: 240,
                    bottom: 40,
                },
                0x000000ff,
            );
            SendMessageW(
                label,
                WM_PRINTCLIENT,
                dc as usize,
                (PRF_CLIENT | PRF_ERASEBKGND) as isize,
            );
            assert_eq!(GetPixel(dc, 239, 39), surface());
            let mut darkest = 255;
            for y in 0..25 {
                for x in 0..200 {
                    let color = GetPixel(dc, x, y);
                    darkest = darkest.min(color & 255);
                }
            }
            assert!(
                darkest >= (muted() & 0xff).saturating_sub(3)
                    && darkest <= (muted() & 0xff) + 18,
                "muted text should be gray rather than primary ink: {darkest}"
            );
            fill(
                dc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: 240,
                    bottom: 40,
                },
                0x000000ff,
            );
            let region = CreateRectRgn(0, 5, 100, 25);
            assert_ne!(SetWindowRgn(label, region, 0), 0);
            SetViewportOrgEx(dc, 20, 10, null_mut());
            SendMessageW(
                label,
                WM_PRINTCLIENT,
                dc as usize,
                (PRF_CLIENT | PRF_ERASEBKGND) as isize,
            );
            SetViewportOrgEx(dc, 0, 0, null_mut());
            assert_eq!(
                GetPixel(dc, 115, 33),
                surface(),
                "window-local print clip follows the DC viewport"
            );
            assert_eq!(
                GetPixel(dc, 115, 12),
                0x000000ff,
                "pixels before the clipped top remain untouched"
            );
            assert_eq!(
                GetPixel(dc, 125, 33),
                0x000000ff,
                "pixels after the clipped right remain untouched"
            );
            SelectObject(dc, old_bitmap);
            DeleteObject(bitmap);
            DeleteDC(dc);
            assert_eq!(IsWindowVisible(parent), 0);
            DestroyWindow(parent);
            DeleteObject(body_font);
            UnregisterClassW(class.as_ptr(), GetModuleHandleW(null()));
        }
    }
    #[test]
    fn switch_paint_preserves_native_toggle_state_keyboard_contract_and_caption() {
        let _accessibility = a11y::UiThread::enter().unwrap();
        unsafe {
            let parent = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                wide("Synthetic never-visible presentation").as_ptr(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                400,
                200,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!parent.is_null());
            let body_font = font(96, 14, false);
            let toggle = create(
                parent,
                "BUTTON",
                "Synthetic caption",
                BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
                1000,
                [10, 10, 240, 32],
                body_font,
            );
            presentation_flag(toggle, "Jiks.Surface", true);
            a11y::annotate(toggle, "synthetic.toggle", None, None);
            SendMessageW(toggle, BM_SETCHECK, BST_UNCHECKED as usize, 0);
            let before = a11y::read_for_test(&[toggle]).unwrap();
            assert_eq!(before[0].toggle, Some(0));
            SendMessageW(toggle, BM_CLICK, 0, 0);
            assert_eq!(
                SendMessageW(toggle, BM_GETCHECK, 0, 0),
                BST_CHECKED as isize
            );
            let after = a11y::read_for_test(&[toggle]).unwrap();
            assert_eq!(after[0].toggle, Some(1));
            assert_eq!(after[0].name, "Synthetic caption");
            assert_eq!(
                after[0].control_type,
                windows::Win32::UI::Accessibility::UIA_CheckBoxControlTypeId.0
            );
            let dc = CreateCompatibleDC(null_mut());
            SetTextColor(dc, 0x00123456);
            SetBkMode(dc, OPAQUE as i32);
            SendMessageW(toggle, WM_PRINTCLIENT, dc as usize, PRF_CLIENT as isize);
            assert_eq!(GetTextColor(dc), 0x00123456);
            assert_eq!(GetBkMode(dc), OPAQUE as i32);
            assert_eq!(IsWindowVisible(parent), 0);
            DeleteDC(dc);
            DestroyWindow(parent);
            DeleteObject(body_font);
        }
    }
    #[test]
    fn owner_draw_preserves_the_callers_dc_and_color_queries_need_no_app_borrow() {
        unsafe {
            let parent = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                300,
                200,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            let body_font = font(96, 14, false);
            let button = create(
                parent,
                "BUTTON",
                "Synthetic action",
                BS_OWNERDRAW as u32,
                2000,
                [10, 10, 200, 32],
                body_font,
            );
            presentation_flag(button, "Jiks.Surface", true);
            presentation_flag(button, "Jiks.Muted", true);
            let dc = CreateCompatibleDC(null_mut());
            assert!(presentation_message(WM_CTLCOLORBTN, dc as usize, button as isize).is_some());
            assert_eq!(GetBkColor(dc), surface());
            assert_eq!(GetTextColor(dc), muted());
            let app_state = RefCell::new(());
            let _layout_borrow = app_state.borrow_mut();
            for primary in [false, true] {
                presentation_flag(button, "Jiks.Primary", primary);
                SetTextColor(dc, 0x00654321);
                SetBkMode(dc, OPAQUE as i32);
                let item = DRAWITEMSTRUCT {
                    CtlID: 2000,
                    hwndItem: button,
                    hDC: dc,
                    rcItem: RECT {
                        left: 0,
                        top: 0,
                        right: 200,
                        bottom: 32,
                    },
                    ..zeroed()
                };
                assert_eq!(
                    presentation_message(WM_DRAWITEM, 2000, &item as *const _ as isize),
                    Some(1)
                );
                assert_eq!(GetTextColor(dc), 0x00654321);
                assert_eq!(GetBkMode(dc), OPAQUE as i32);
            }
            assert_eq!(IsWindowVisible(parent), 0);
            DeleteDC(dc);
            DestroyWindow(parent);
            DeleteObject(body_font);
        }
    }
}
