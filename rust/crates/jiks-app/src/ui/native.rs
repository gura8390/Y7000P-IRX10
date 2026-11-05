use super::{
    a11y,
    catalog::{self, Action, ActionKind, FieldKind, Page},
    editors,
};
use crate::ui_model::*;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
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

// Stable Windows SDK constants omitted/gated in windows-sys metadata.
pub(super) const SS_LEFT: u32 = 0;
pub(super) const TRACKBAR_GET_POSITION: u32 = WM_USER;
const TRAY_KEY_SELECT: u32 = NIN_SELECT | 1;
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
            muted: 0x00666666,
            line: 0x00e2e2e2,
            pressed: 0x00e7e7e7,
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
fn apply_palette(model: &UiModel) -> bool {
    let theme = model
        .values
        .get("appearance.effective_theme")
        .or_else(|| model.values.get("appearance.theme"))
        .and_then(Value::as_str)
        .unwrap_or("light");
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
const WM_TRAY: u32 = WM_APP + 31;
const TIMER: usize = 1;
const ID_NAV: u16 = 100;
const ID_EXIT: u16 = 120;
const ID_DEVICE: u16 = 180;
const ID_LOG: u16 = 181;
const ID_THEME: u16 = 182;
const ID_NAV_LIST: u16 = 190;
const ID_TOOL_NAV: u16 = 140;
const ID_TRAY_OPEN: usize = 300;
const ID_TRAY_EXIT: usize = 301;
const ID_TRAY_STATUS: usize = 302;
const ID_QUICK: usize = 6000;
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
        ActionKind::SensorSettings => "sensor-settings".into(),
        ActionKind::Cancel(fields) => format!("cancel.{}", fields.join(",")),
    };
    format!("{}.{}", action.accessibility_role, command)
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
        let index = Page::ALL
            .iter()
            .position(|candidate| *candidate == page)
            .unwrap_or(0);
        return Some(
            Page::ALL[(index + if shift { Page::ALL.len() - 1 } else { 1 }) % Page::ALL.len()],
        );
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
        model.notice = "后台状态暂不可读，请重新打开机控。".into();
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
            wide(&super::i18n::tr("机控 · 确认操作")).as_ptr(),
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
    unsafe {
        let hwnd = CreateWindowExW(
            0,
            wide(class).as_ptr(),
            wide(label).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
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
                let color = if surface_label { surface() } else { paper() };
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
            ] {
                RemovePropW(hwnd, wide(property).as_ptr());
            }
            RemoveWindowSubclass(hwnd, Some(presentation_cleanup), 0x4A495052);
        }
        DefSubclassProc(hwnd, msg, wp, lp)
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
                let color = if on_surface { surface() } else { paper() };
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
/// Clip a scrolled control to its viewport rather than hiding its entire
/// contents when only the first/last line crosses a viewport edge.
pub(super) unsafe fn clip_to_viewport(
    hwnd: HWND,
    y: i32,
    width: i32,
    height: i32,
    top: i32,
    bottom: i32,
    dpi: u32,
) -> bool {
    unsafe {
        let visible_top = (top - y).max(0).min(height);
        let visible_bottom = (bottom - y).max(0).min(height);
        let scale = |n: i32| n * dpi as i32 / 96;
        let region = CreateRectRgn(
            0,
            scale(visible_top),
            scale(width.max(0)),
            scale(visible_bottom),
        );
        if SetWindowRgn(hwnd, region, 1) == 0 {
            DeleteObject(region);
        }
        visible_bottom > visible_top
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
        let saved_dc = SaveDC(item.hDC);
        let disabled = item.itemState & ODS_DISABLED != 0;
        let pressed = item.itemState & ODS_SELECTED != 0;
        let navigation = (item.CtlID >= ID_NAV as u32
            && item.CtlID < ID_NAV as u32 + Page::ALL.len() as u32)
            || (item.CtlID >= ID_TOOL_NAV as u32 && item.CtlID < ID_TOOL_NAV as u32 + 8);
        let hovered = !GetPropW(item.hwndItem, wide("Jiks.Button.Hover").as_ptr()).is_null();
        let background = if selected || pressed || (!disabled && hovered) {
            PALETTE.with(|p| p.get().pressed)
        } else if navigation {
            paper()
        } else {
            surface()
        };
        fill(item.hDC, &item.rcItem, background);
        let pen = CreatePen(PS_SOLID, 1, if navigation { background } else { line() });
        let old_pen = SelectObject(item.hDC, pen);
        let old_brush = SelectObject(item.hDC, GetStockObject(NULL_BRUSH));
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
        SetTextColor(item.hDC, if disabled { 0x00909090 } else { ink() });
        let label = wide(&get_text(item.hwndItem));
        let mut rect = item.rcItem;
        let main_navigation =
            item.CtlID >= ID_NAV as u32 && item.CtlID < ID_NAV as u32 + Page::ALL.len() as u32;
        let action_card = !GetPropW(item.hwndItem, wide("Jiks.ActionCard").as_ptr()).is_null();
        if navigation {
            if selected {
                let width = (3 * dpi as i32 / 96).max(2);
                let margin = (10 * dpi as i32 / 96).max(4);
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
                paint_nav_icon(item, item.CtlID - ID_NAV as u32);
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
pub(super) unsafe fn paint_primary_button(item: &DRAWITEMSTRUCT) {
    unsafe {
        let saved_dc = SaveDC(item.hDC);
        let disabled = item.itemState & ODS_DISABLED != 0;
        let background = if disabled { line() } else { accent() };
        fill(item.hDC, &item.rcItem, paper());
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
        // Theme/accent can be customized. Choose readable ink rather than
        // assuming that every user-selected accent is dark.
        let luminance = (background & 255) * 299
            + ((background >> 8) & 255) * 587
            + ((background >> 16) & 255) * 114;
        SetTextColor(
            item.hDC,
            if disabled {
                muted()
            } else if luminance > 150000 {
                0x00202020
            } else {
                0x00ffffff
            },
        );
        let mut rect = item.rcItem;
        DrawTextW(
            item.hDC,
            wide(&get_text(item.hwndItem)).as_ptr(),
            -1,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
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
        let x = item.rcItem.left
            + if stacked {
                (item.rcItem.right - item.rcItem.left - 9 * unit) / 2
            } else {
                10 * dpi / 96
            };
        let y = item.rcItem.top
            + if stacked {
                7 * dpi / 96
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
                Polyline(item.hDC, pts.as_ptr(), pts.len() as i32);
                MoveToEx(item.hDC, x + 3 * unit, y + 8 * unit, null_mut());
                LineTo(item.hDC, x + 3 * unit, y + 5 * unit);
                LineTo(item.hDC, x + 5 * unit, y + 5 * unit);
                LineTo(item.hDC, x + 5 * unit, y + 8 * unit);
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
}
struct Binding {
    key: String,
    hwnd: HWND,
    kind: BindingKind,
    capability: String,
}
struct Placed {
    hwnd: HWND,
    rect: [i32; 4],
    clip_height: i32,
    pinned: bool,
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
    brush: HBRUSH,
    surface_brush: HBRUSH,
    fixed: Vec<HWND>,
    content: Vec<Placed>,
    bindings: Vec<Binding>,
    buttons: BTreeMap<u16, Action>,
    pickers: BTreeMap<u16, (String, super::selectors::PickerKind)>,
    drafts: BTreeMap<String, Value>,
    dirty: BTreeSet<String>,
    metrics: Vec<MetricPlacement>,
    cards: Vec<CardSurface>,
    surface_children: BTreeSet<usize>,
    muted_labels: BTreeSet<usize>,
    title: HWND,
    machine: HWND,
    notice: HWND,
    rendered_revision: u64,
    metadata_revision: u64,
    scroll: i32,
    content_height: i32,
    pending: BTreeMap<u64, bool>,
    rendering: bool,
    tray: bool,
    tray_data: NOTIFYICONDATAW,
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
        apply_palette(&live_snapshot(&bridge));
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
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: GetStockObject(WHITE_BRUSH) as HBRUSH,
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            DeleteObject(brush);
            bail!("无法注册机控窗口：{}", GetLastError());
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
            cards: vec![],
            surface_children: BTreeSet::new(),
            muted_labels: BTreeSet::new(),
            title: null_mut(),
            machine: null_mut(),
            notice: null_mut(),
            rendered_revision: u64::MAX,
            metadata_revision: u64::MAX,
            scroll: 0,
            content_height: 0,
            pending: BTreeMap::new(),
            rendering: false,
            tray: false,
            tray_data: zeroed(),
            taskbar_message: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            show_message: RegisterWindowMessageW(wide("Jiks.ShowExisting.1").as_ptr()),
            popup: super::status_popup::StatusPopup::default(),
            column_x: 132,
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
            wide("机控 / Jiks").as_ptr(),
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
            bail!("无法创建机控窗口：{}", GetLastError());
        }
        a11y::annotate(hwnd, "main", None, None);
        let instance_key = (*raw).borrow().bridge.instance_key.clone();
        if let Some(key) = instance_key {
            if SetPropW(hwnd, wide(&key).as_ptr(), 1usize as HANDLE) == 0 {
                DestroyWindow(hwnd);
                drop(Box::from_raw(raw));
                bail!("无法标记机控实例窗口");
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
        let mut failure = None;
        loop {
            let result = GetMessageW(&mut msg, null_mut(), 0, 0);
            if result == 0 {
                break;
            }
            if result == -1 {
                failure = Some(anyhow!("窗口消息失败：{}", GetLastError()));
                break;
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
        if let Some(result) = presentation_message(msg, wp, lp) {
            return result;
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
            app.add_tray();
            return Some(0);
        }
        match msg {
            WM_CREATE => {
                app.dpi = GetDpiForWindow(app.hwnd).max(96);
                app.make_fonts();
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
                if wp == SIZE_MINIMIZED as usize
                    && snapshot(&app.bridge).preferences.minimize_to_tray
                    && app.tray
                {
                    ShowWindow(app.hwnd, SW_HIDE);
                } else {
                    app.capture();
                    app.layout_fixed();
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
            WM_DPICHANGED => {
                app.capture();
                app.dpi = (wp & 0xffff) as u32;
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
            WM_COMMAND => {
                if !app.rendering {
                    app.command((wp & 0xffff) as u16, ((wp >> 16) & 0xffff) as u16);
                }
                Some(0)
            }
            WM_VSCROLL => {
                let command = (wp & 0xffff) as i32;
                let old = app.scroll;
                app.scroll = match command {
                    SB_LINEUP => old - 32,
                    SB_LINEDOWN => old + 32,
                    SB_PAGEUP => old - 240,
                    SB_PAGEDOWN => old + 240,
                    SB_THUMBTRACK | SB_THUMBPOSITION => {
                        let mut si: SCROLLINFO = zeroed();
                        si.cbSize = size_of::<SCROLLINFO>() as u32;
                        si.fMask = SIF_TRACKPOS;
                        GetScrollInfo(app.hwnd, SB_VERT, &mut si);
                        si.nTrackPos
                    }
                    SB_TOP => 0,
                    SB_BOTTOM => app.content_height,
                    _ => old,
                };
                app.position_content();
                Some(0)
            }
            WM_MOUSEWHEEL => {
                app.scroll -= ((wp >> 16) as u16 as i16 as i32) / 120 * 64;
                app.position_content();
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
            WM_ERASEBKGND => {
                app.paint_client(wp as HDC);
                Some(1)
            }
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(app.hwnd, &mut ps);
                app.paint_client(dc);
                EndPaint(app.hwnd, &ps);
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
                    NIN_POPUPOPEN => app.show_status_popup(false),
                    NIN_POPUPCLOSE => app.popup.schedule_hide(),
                    _ => {}
                }
                Some(0)
            }
            WM_CLOSE => {
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
            for font in [self.body_font, self.heading_font, self.title_font] {
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
            for f in [self.body_font, self.heading_font, self.title_font] {
                if !f.is_null() {
                    DeleteObject(f);
                }
            }
            self.body_font = font(self.dpi, 14, false);
            self.heading_font = font(self.dpi, 17, true);
            self.title_font = font(self.dpi, 26, true);
        }
    }
    unsafe fn build_fixed(&mut self) {
        unsafe {
            self.rendering = true;
            for h in self.fixed.drain(..) {
                DestroyWindow(h);
            }
            let brand = create(
                self.hwnd,
                "STATIC",
                &super::i18n::tr("机控"),
                1,
                0,
                [self.s(12), self.s(20), self.s(88), self.s(30)],
                self.heading_font,
            );
            self.fixed.push(brand);
            a11y::annotate(brand, "main.brand", None, None);
            let sub = create(
                self.hwnd,
                "STATIC",
                "Jiks",
                1,
                0,
                [self.s(12), self.s(54), self.s(88), self.s(22)],
                self.body_font,
            );
            self.fixed.push(sub);
            a11y::annotate(sub, "main.subtitle", None, None);
            for (index, page) in Page::ALL.iter().enumerate() {
                let h = create(
                    self.hwnd,
                    "BUTTON",
                    page.title(),
                    BS_OWNERDRAW as u32 | WS_TABSTOP,
                    ID_NAV + index as u16,
                    [
                        self.s(12),
                        self.s(92 + index as i32 * 54),
                        self.s(88),
                        self.s(50),
                    ],
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
            for page in Page::ALL {
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
            let exit = create(
                self.hwnd,
                "BUTTON",
                &super::i18n::tr("退出机控"),
                BS_OWNERDRAW as u32 | WS_TABSTOP,
                ID_EXIT,
                [0, 0, 0, 0],
                self.body_font,
            );
            self.fixed.push(exit);
            a11y::annotate(exit, "main.exit", None, None);
            for (id, label) in [(ID_DEVICE, "设备"), (ID_LOG, "日志"), (ID_THEME, "主题")] {
                let h = create(
                    self.hwnd,
                    "BUTTON",
                    &super::i18n::tr(label),
                    BS_OWNERDRAW as u32 | WS_TABSTOP,
                    id,
                    [0, 0, 0, 0],
                    self.body_font,
                );
                self.fixed.push(h);
                a11y::annotate(
                    h,
                    match id {
                        ID_DEVICE => "main.device-information",
                        ID_LOG => "main.logs",
                        _ => "main.theme",
                    },
                    Some(&super::i18n::tr(label)),
                    None,
                );
            }
            self.title = create(
                self.hwnd,
                "STATIC",
                self.page.title(),
                0,
                0,
                [0, 0, 0, 0],
                self.title_font,
            );
            self.fixed.push(self.title);
            a11y::annotate(self.title, "main.page-title", None, None);
            self.machine = create(self.hwnd, "STATIC", "", 0, 0, [0, 0, 0, 0], self.body_font);
            self.fixed.push(self.machine);
            a11y::annotate(self.machine, "main.machine", None, None);
            self.notice = create(
                self.hwnd,
                "STATIC",
                "",
                SS_LEFT as u32,
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
            let compact_nav = h < 642;
            let nav = GetDlgItem(self.hwnd, ID_NAV_LIST as i32);
            SetWindowPos(
                nav,
                null_mut(),
                self.s(12),
                self.s(92),
                self.s(88),
                self.s((h - 162).max(76)),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            SendMessageW(
                nav,
                LB_SETCURSEL,
                Page::ALL.iter().position(|p| *p == self.page).unwrap_or(0),
                0,
            );
            ShowWindow(nav, if compact_nav { SW_SHOWNA } else { SW_HIDE });
            for index in 0..Page::ALL.len() {
                ShowWindow(
                    GetDlgItem(self.hwnd, ID_NAV as i32 + index as i32),
                    if compact_nav { SW_HIDE } else { SW_SHOWNA },
                );
                let page = Page::ALL[index];
                let control = GetDlgItem(self.hwnd, ID_NAV as i32 + index as i32);
                if page == self.page {
                    presentation_flag(control, "Jiks.Selected", true);
                } else {
                    presentation_flag(control, "Jiks.Selected", false);
                }
                InvalidateRect(control, null(), 0);
                a11y::annotate(
                    GetDlgItem(self.hwnd, ID_NAV as i32 + index as i32),
                    &a11y::key("main", "navigate", page.key()),
                    Some(page.title()),
                    (page == self.page).then(|| super::i18n::tr_static("当前页面")),
                );
            }
            for index in 6..Page::ALL.len() {
                SetWindowPos(
                    GetDlgItem(self.hwnd, ID_NAV as i32 + index as i32),
                    null_mut(),
                    self.s(12),
                    self.s(h - 222 + (index as i32 - 6) * 54),
                    self.s(88),
                    self.s(50),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            for (hwnd, x, y, width, height) in [
                (GetDlgItem(self.hwnd, ID_EXIT as i32), 12, h - 52, 88, 32),
                (self.title, 132, 25, w - 392, 40),
                (GetDlgItem(self.hwnd, ID_DEVICE as i32), w - 240, 30, 66, 30),
                (GetDlgItem(self.hwnd, ID_LOG as i32), w - 166, 30, 66, 30),
                (GetDlgItem(self.hwnd, ID_THEME as i32), w - 92, 30, 66, 30),
                (self.machine, 134, 72, w - 156, 24),
                (self.notice, 132, h - 48, w - 154, 40),
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
            text(self.title, self.page.title());
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
            let h = create(
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
            self.content.push(Placed {
                hwnd: h,
                rect,
                clip_height: if class == "COMBOBOX" { 30 } else { rect[3] },
                pinned: id >= ID_TOOL_NAV && id < ID_TOOL_NAV + 8,
            });
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
                SS_LEFT as u32,
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
            // Match the original frame: quiet sidebar, distinct content pane,
            // and a compact status strip rather than heavy dividing rules.
            if !self.background.visible() {
                fill(
                    dc,
                    &RECT {
                        left: 0,
                        top: 0,
                        right: self.s(width),
                        bottom: self.s(height),
                    },
                    paper(),
                );
            }
            let saved = SaveDC(dc);
            IntersectClipRect(
                dc,
                self.s(124),
                self.s(114),
                self.s(width - 16),
                self.s(height - 58),
            );
            for card in &self.cards {
                let [x, y, w, h] = card.rect;
                let y = y - self.scroll;
                if y + h > 114 && y < height - 58 {
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
            MoveToEx(dc, self.s(132), self.s(height - 56), null_mut());
            LineTo(dc, self.s(width - 20), self.s(height - 56));
            SelectObject(dc, old);
            DeleteObject(pen);
        }
    }
    unsafe fn metric(&mut self, metric: &UiMetric, y: i32, width: i32) {
        unsafe {
            let origin = self.column_x;
            let value_width = (width / 3).clamp(92, 160);
            let detail_width = (width / 4).clamp(64, 180);
            let name_width = (width - value_width - detail_width - 16).max(80);
            let name = self.label(&metric.label, y + 3, name_width, false, 24);
            self.column_x = origin + name_width + 8;
            let value = self.placed(
                "STATIC",
                &metric.value,
                2 | 0x4000,
                0,
                [self.column_x, y, value_width, 26],
                self.heading_font,
            );
            self.column_x += value_width + 8;
            let detail = self.placed(
                "STATIC",
                &metric.detail,
                2 | 0x4000,
                0,
                [self.column_x, y + 3, detail_width, 24],
                self.body_font,
            );
            self.muted_labels.insert(detail as usize);
            presentation_flag(detail, "Jiks.Muted", true);
            let scope = self.accessibility_scope();
            a11y::annotate(
                name,
                &a11y::key(&scope, "metric-label", &metric.key),
                None,
                None,
            );
            a11y::annotate(
                value,
                &a11y::key(&scope, "metric", &metric.key),
                Some(&format!(
                    "{} {} {}",
                    metric.label, metric.value, metric.detail
                )),
                None,
            );
            a11y::annotate(
                detail,
                &a11y::key(&scope, "metric-detail", &metric.key),
                None,
                None,
            );
            self.metrics.push(MetricPlacement {
                name,
                value,
                detail,
                key: metric.key.clone(),
            });
            self.column_x = origin;
        }
    }
    unsafe fn render(&mut self) {
        unsafe {
            if self.hwnd.is_null() {
                return;
            }
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
            self.cards.clear();
            self.surface_children.clear();
            self.muted_labels.clear();
            let model = snapshot(&self.bridge);
            self.rendered_revision = model.revision;
            self.metadata_revision = model.metadata_revision;
            let (w, _) = self.size();
            let width = (w - 156).max(360);
            let mut y = 124;
            self.column_x = 132;
            let mut content_origin = 132;
            let mut content_width = width;
            if self.page == Page::Settings {
                let nav_width = if width >= 650 { 190 } else { 64 };
                for (index, title) in catalog::TOOL_TABS.iter().enumerate() {
                    let translated = super::i18n::tr(title);
                    let title = translated.as_ref();
                    let short = title.chars().take(2).collect::<String>();
                    let button = self.placed(
                        "BUTTON",
                        if nav_width < 100 { &short } else { title },
                        BS_OWNERDRAW as u32 | WS_TABSTOP,
                        ID_TOOL_NAV + index as u16,
                        [132, 124 + index as i32 * 44, nav_width, 38],
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
                content_origin = 132 + nav_width + 20;
                content_width = (width - nav_width - 20).max(250);
            }
            if self.page == Page::Dashboard {
                let groups = super::sensors::groups(&model);
                let columns = if width > 780 { 2 } else { 1 };
                let mut bottoms = [y, y];
                let mut pair_y = y;
                for (index, group) in groups.into_iter().enumerate() {
                    let column = index % columns;
                    if column == 0 {
                        pair_y = bottoms[0].max(bottoms[1]);
                    }
                    let card_x = 132 + column as i32 * (width / columns as i32);
                    let card_width = width / columns as i32 - 12;
                    let card_top = pair_y;
                    let mut group_y = card_top + 16;
                    self.column_x = card_x + 16;
                    let heading = self.label(&group.title, group_y, card_width - 32, true, 28);
                    if let Some(first) = group.metrics.first() {
                        a11y::annotate(
                            heading,
                            &a11y::key("main.dashboard", "group", &first.key),
                            None,
                            None,
                        );
                    }
                    group_y += 38;
                    for metric in group.metrics {
                        self.metric(&metric, group_y, card_width - 32);
                        group_y += 36;
                    }
                    self.cards.push(CardSurface {
                        icon: None,
                        rect: [card_x, card_top, card_width, group_y - card_top + 10],
                    });
                    bottoms[column] = group_y + 26;
                }
                y = bottoms[0].max(bottoms[1]);
                self.column_x = 132;
                if self.metrics.is_empty()
                    && model
                        .documents
                        .get("dashboard")
                        .and_then(|d| d.value.get("ShowSensors"))
                        .and_then(Value::as_bool)
                        != Some(false)
                {
                    self.label(&super::i18n::tr("传感器数据尚不可用"), y, width, false, 28);
                    y += 44;
                }
            } else if self.page == Page::Battery {
                let top = y;
                self.column_x = 148;
                for metric in model
                    .metrics
                    .iter()
                    .filter(|m| m.key.to_ascii_lowercase().starts_with("battery"))
                {
                    self.metric(metric, y + 16, width - 32);
                    y += 36;
                }
                if y > top {
                    self.cards.push(CardSurface {
                        icon: None,
                        rect: [132, top, width, y - top + 26],
                    });
                    y += 42;
                }
            } else if self.page == Page::About {
                let top = y;
                self.column_x = 152;
                let name = self.label(
                    &super::i18n::tr("机控 / Jiks"),
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
                    rect: [132, top, width, 144],
                });
                y += 160;
            }
            let sections = catalog::sections(self.page, self.tools_tab, &model);
            let mut next_field = 1000u16;
            let mut next_button = 2000u16;
            let two_columns = self.page == Page::Dashboard && width > 970;
            let full_width = content_width;
            let mut bottoms = [y, y];
            let mut pair_y = y;
            for (section_index, section) in sections.into_iter().enumerate() {
                let column = if two_columns { section_index % 2 } else { 0 };
                let width = if two_columns {
                    (full_width - 24) / 2
                } else {
                    full_width
                };
                let card_x = content_origin + column as i32 * (width + 24);
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
                let heading = self.label(&section.title, y, width, true, 30);
                self.surface_children.remove(&(heading as usize));
                presentation_flag(heading, "Jiks.Surface", false);
                a11y::annotate(
                    heading,
                    &a11y::key(&section_scope, "text", "title"),
                    None,
                    None,
                );
                y += 38;
                let hint_height = if width < 440 { 52 } else { 38 };
                let hint = self.label(section.hint, y, width, false, hint_height);
                self.muted_labels.insert(hint as usize);
                presentation_flag(hint, "Jiks.Muted", true);
                self.surface_children.remove(&(hint as usize));
                presentation_flag(hint, "Jiks.Surface", false);
                a11y::annotate(
                    hint,
                    &a11y::key(&section_scope, "text", "description"),
                    None,
                    None,
                );
                y += hint_height + 12;
                for spec in section.fields {
                    let header_x = self.column_x;
                    let row_top = y;
                    let description = super::i18n::tr(super::presentation::field_hint(spec.key));
                    let row_height = if description.is_empty() {
                        if model.preferences.compact { 56 } else { 64 }
                    } else if model.preferences.compact {
                        72
                    } else {
                        80
                    };
                    let control_width = if matches!(spec.kind, FieldKind::Toggle) {
                        48
                    } else {
                        (width * 38 / 100).clamp(130, 240)
                    };
                    let editor_x = card_x + 16 + width - control_width;
                    self.column_x = card_x + 52;
                    let label_width = (editor_x - self.column_x - 12).max(80);
                    y = row_top + (row_height - 30) / 2;
                    let field_label = if let Some(range) = model.ranges.get(spec.key) {
                        format!(
                            "{} · {}–{} {}",
                            spec.label, range.minimum, range.maximum, range.unit
                        )
                    } else {
                        spec.label.into()
                    };
                    let label = self.label(
                        &field_label,
                        row_top
                            + if description.is_empty() {
                                (row_height - 28) / 2
                            } else {
                                12
                            },
                        label_width,
                        false,
                        28,
                    );
                    if !description.is_empty() {
                        let hint = self.label(&description, row_top + 38, label_width, false, 34);
                        self.muted_labels.insert(hint as usize);
                        presentation_flag(hint, "Jiks.Muted", true);
                    }
                    a11y::annotate(
                        label,
                        &a11y::key(&self.accessibility_scope(), "label", spec.key),
                        None,
                        None,
                    );
                    let value = self
                        .drafts
                        .get(spec.key)
                        .cloned()
                        .unwrap_or_else(|| model.value(spec.key));
                    let id = next_field;
                    next_field += 1;
                    let picker =
                        main_picker(spec.key).filter(|_| matches!(spec.kind, FieldKind::Text));
                    let available_width = control_width;
                    let editor_width =
                        (available_width - if picker.is_some() { 66 } else { 0 }).max(60);
                    let options = model.options.get(spec.key).cloned();
                    let (hwnd, kind) = match spec.kind {
                        FieldKind::Toggle => {
                            let h = self.placed(
                                "BUTTON",
                                "",
                                BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
                                id,
                                [editor_x, y, editor_width, 30],
                                self.body_font,
                            );
                            SendMessageW(
                                h,
                                BM_SETCHECK,
                                if value.as_bool().unwrap_or(false) {
                                    BST_CHECKED as usize
                                } else {
                                    BST_UNCHECKED as usize
                                },
                                0,
                            );
                            (h, BindingKind::Toggle)
                        }
                        FieldKind::Text | FieldKind::Number | FieldKind::ReadOnly => {
                            let numeric = matches!(spec.kind, FieldKind::Number);
                            let h = self.placed(
                                "EDIT",
                                &scalar(&value),
                                WS_BORDER
                                    | WS_TABSTOP
                                    | ES_AUTOHSCROLL as u32
                                    | if matches!(spec.kind, FieldKind::ReadOnly) {
                                        ES_READONLY as u32
                                    } else {
                                        0
                                    },
                                id,
                                [editor_x, y, editor_width, 30],
                                self.body_font,
                            );
                            SendMessageW(h, EM_SETLIMITTEXT, 65536, 0);
                            (
                                h,
                                if numeric {
                                    BindingKind::Number
                                } else {
                                    BindingKind::Text
                                },
                            )
                        }
                        FieldKind::Choice(fallback) => {
                            let mut options = options.unwrap_or(fallback);
                            if spec.key == "packages.sort" {
                                for option in &mut options {
                                    option.label = super::i18n::tr(&option.label).into_owned();
                                }
                            }
                            let h = self.placed(
                                "COMBOBOX",
                                "",
                                CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
                                id,
                                [editor_x, y, editor_width, 320],
                                self.body_font,
                            );
                            set_choices(h, &options, &value);
                            (h, BindingKind::Choice(options))
                        }
                        FieldKind::Collection(collection) => {
                            let options: Vec<UiChoice> = model
                                .collections
                                .get(collection)
                                .map(|items| {
                                    items
                                        .iter()
                                        .map(|item| {
                                            UiChoice::new(
                                                if item.detail.is_empty() {
                                                    item.label.clone()
                                                } else {
                                                    format!("{} · {}", item.label, item.detail)
                                                },
                                                item.id.clone(),
                                            )
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            let h = self.placed(
                                "COMBOBOX",
                                "",
                                CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
                                id,
                                [editor_x, y, editor_width, 360],
                                self.body_font,
                            );
                            set_choices(h, &options, &value);
                            (h, BindingKind::Choice(options))
                        }
                    };
                    let help = model
                        .capabilities
                        .get(spec.key)
                        .filter(|cap| !cap.available)
                        .map(|cap| cap.reason.clone())
                        .or_else(|| {
                            (!spec.capability.is_empty() && !model.can(spec.capability))
                                .then(|| model.unavailable_reason(spec.capability))
                        })
                        .filter(|reason| !reason.is_empty());
                    a11y::annotate(
                        hwnd,
                        &a11y::key(&self.accessibility_scope(), "field", spec.key),
                        Some(spec.label),
                        help.as_deref(),
                    );
                    EnableWindow(
                        hwnd,
                        if model
                            .capabilities
                            .get(spec.key)
                            .map(|cap| cap.available)
                            .unwrap_or_else(|| {
                                spec.capability.is_empty() || model.can(spec.capability)
                            })
                        {
                            1
                        } else {
                            0
                        },
                    );
                    self.bindings.push(Binding {
                        key: spec.key.into(),
                        hwnd,
                        kind,
                        capability: spec.capability.into(),
                    });
                    if let Some(kind) = picker {
                        let picker_id = id + 8000;
                        let browse = self.placed(
                            "BUTTON",
                            &super::i18n::tr("浏览…"),
                            BS_OWNERDRAW as u32 | WS_TABSTOP,
                            picker_id,
                            [editor_x + editor_width + 6, y, 60, 30],
                            self.body_font,
                        );
                        EnableWindow(browse, IsWindowEnabled(hwnd));
                        a11y::annotate(
                            browse,
                            &a11y::key(&self.accessibility_scope(), "browse", spec.key),
                            Some(&format!("{} · {}", spec.label, super::i18n::tr("浏览…"))),
                            None,
                        );
                        self.pickers.insert(picker_id, (spec.key.into(), kind));
                    }
                    self.cards.push(CardSurface {
                        rect: [card_x, row_top, width + 32, row_height - 4],
                        icon: Some(super::presentation::field_icon(spec.key)),
                    });
                    self.column_x = header_x;
                    y = row_top + row_height + 4;
                }
                let mut x = self.column_x;
                let mut unavailable: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for action in section.actions {
                    let card_link = self.page == Page::Settings
                        && matches!(
                            action.kind,
                            ActionKind::Document(_) | ActionKind::SensorSettings
                        );
                    let button_width = if card_link {
                        width
                    } else {
                        ((action.label.chars().count() as i32 * 15) + 30).clamp(96, 230)
                    };
                    if card_link && x != self.column_x {
                        x = self.column_x;
                        y += 42;
                    }
                    if x + button_width > self.column_x + width {
                        x = self.column_x;
                        y += 42;
                    }
                    let id = next_button;
                    next_button += 1;
                    let h = self.placed(
                        "BUTTON",
                        action.label,
                        BS_OWNERDRAW as u32 | WS_TABSTOP,
                        id,
                        [x, y + 6, button_width, if card_link { 48 } else { 32 }],
                        self.body_font,
                    );
                    if card_link {
                        presentation_flag(h, "Jiks.ActionCard", true);
                    }
                    let available = action.capability.is_empty() || model.can(action.capability);
                    a11y::annotate(
                        h,
                        &a11y::key(&section_scope, "action", &action_key(&action)),
                        None,
                        (!available)
                            .then(|| model.unavailable_reason(action.capability))
                            .as_deref(),
                    );
                    EnableWindow(h, available as i32);
                    if !available {
                        let reason = model.unavailable_reason(action.capability);
                        unavailable
                            .entry(reason)
                            .or_default()
                            .push(action.label.to_owned());
                    }
                    self.buttons.insert(id, action);
                    if card_link {
                        x = self.column_x;
                        y += 56;
                    } else {
                        x += button_width + 10;
                    }
                }
                y += 52;
                for (reason, actions) in unavailable {
                    let h = self.label(
                        &format!("{}：{}", actions.join("、"), reason),
                        y,
                        width,
                        false,
                        52,
                    );
                    presentation_flag(h, "Jiks.Surface", false);
                    presentation_flag(h, "Jiks.Muted", true);
                    y += 56;
                }
                y += 20;
                if two_columns {
                    bottoms[column] = y;
                }
            }
            if two_columns {
                y = bottoms[0].max(bottoms[1]);
            }
            self.column_x = 132;
            self.content_height = y;
            self.position_content();
            self.layout_fixed();
            self.rendering = false;
            self.apply_preferences(&model);
            self.overlay.sync(&model);
            for (id, command) in [
                (ID_DEVICE, "device.details"),
                (ID_LOG, "tools.open_logs"),
                (ID_THEME, "app.preferences.save"),
            ] {
                EnableWindow(GetDlgItem(self.hwnd, id as i32), model.can(command) as i32);
            }
            text(
                self.machine,
                &format!("{}  ·  {}", model.machine, model.version),
            );
            text(
                self.notice,
                a11y::diagnostic().as_deref().unwrap_or(&model.notice),
            );
            InvalidateRect(self.hwnd, null(), 1);
            #[cfg(debug_assertions)]
            super::capture_fixture::queue(self.hwnd, &self.bridge, &self.accessibility_scope());
        }
    }
    unsafe fn position_content(&mut self) {
        unsafe {
            let (_, height) = self.size();
            let viewport = (height - 188).max(100);
            let maximum = (self.content_height - 124 - viewport).max(0);
            self.scroll = self.scroll.clamp(0, maximum);
            let info = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: (self.content_height - 124).max(0),
                nPage: viewport as u32,
                nPos: self.scroll,
                nTrackPos: 0,
            };
            SetScrollInfo(self.hwnd, SB_VERT, &info, 1);
            for p in &self.content {
                let r = p.rect;
                let y = r[1] - if p.pinned { 0 } else { self.scroll };
                SetWindowPos(
                    p.hwnd,
                    null_mut(),
                    self.s(r[0]),
                    self.s(y),
                    self.s(r[2]),
                    self.s(r[3]),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                let visible =
                    clip_to_viewport(p.hwnd, y, r[2], p.clip_height, 114, height - 58, self.dpi);
                ShowWindow(p.hwnd, if visible { SW_SHOWNA } else { SW_HIDE });
            }
            InvalidateRect(self.hwnd, null(), 0);
        }
    }
    unsafe fn capture(&mut self) {
        unsafe {
            for binding in &self.bindings {
                if let Ok(value) = read_binding(binding) {
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
                if let Some(page) = Page::ALL.get(index.max(0) as usize) {
                    self.capture();
                    self.page = *page;
                    self.scroll = 0;
                    self.render();
                }
                return;
            }
            if id >= ID_NAV && id < ID_NAV + Page::ALL.len() as u16 {
                self.capture();
                self.page = Page::ALL[(id - ID_NAV) as usize];
                self.scroll = 0;
                self.render();
                return;
            }
            if id == ID_EXIT {
                self.bridge
                    .diagnostics
                    .main_exit_invocations
                    .fetch_add(1, Ordering::Relaxed);
                self.bridge
                    .diagnostics
                    .last_exit_origin
                    .store(2, Ordering::Relaxed);
                self.shutdown();
                return;
            }
            if [ID_DEVICE, ID_LOG, ID_THEME].contains(&id) {
                if id == ID_DEVICE {
                    self.modal_active = true;
                    self.deferred_editor = Some(DeferredEditor::DeviceInformation);
                    return;
                }
                let model = live_snapshot(&self.bridge);
                let command = if id == ID_DEVICE {
                    "device.details"
                } else if id == ID_LOG {
                    "tools.open_logs"
                } else {
                    "app.preferences.save"
                };
                let payload = if id == ID_THEME {
                    let theme = model
                        .values
                        .get("appearance.effective_theme")
                        .or_else(|| model.values.get("appearance.theme"))
                        .and_then(Value::as_str)
                        .unwrap_or("light");
                    json!({"appearance.theme":if theme.eq_ignore_ascii_case("dark"){"light"}else{"dark"}})
                } else {
                    json!({})
                };
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
                            "关于机控",
                            &format!(
                                "机控 / Jiks {}\nRust原生Win32实现 · GPL-3.0\n基于Lenovo Legion Toolkit迁移，原版设备信息、OSD及功能设计归原作者。\n源码：github.com/gura8390/Y7000P-IRX10",
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
                    let fields = if command == "app.preferences.save" {
                        fields
                            .into_iter()
                            .filter(|field| self.dirty.contains(*field))
                            .collect::<Vec<_>>()
                    } else {
                        fields
                    };
                    if command == "app.preferences.save" && fields.is_empty() {
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
                self.render();
            }
            self.report_visibility();
            let background_model = live_snapshot(&self.bridge);
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
            if apply_palette(&model) {
                DeleteObject(self.brush);
                DeleteObject(self.surface_brush);
                self.brush = CreateSolidBrush(paper());
                self.surface_brush = CreateSolidBrush(surface());
                RedrawWindow(
                    self.hwnd,
                    null(),
                    null_mut(),
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
                );
            }
            for (id, command) in [
                (ID_DEVICE, "device.details"),
                (ID_LOG, "tools.open_logs"),
                (ID_THEME, "app.preferences.save"),
            ] {
                EnableWindow(GetDlgItem(self.hwnd, id as i32), model.can(command) as i32);
            }
            if model.shutdown_ready {
                self.deferred_destroy = true;
                return;
            }
            if model.metadata_revision != self.metadata_revision && IsWindowVisible(self.hwnd) != 0
            {
                self.capture();
                self.render();
            }
            let mut completed = vec![];
            for (request, shutdown) in &self.pending {
                if let Some(result) = model.result(*request) {
                    text(self.notice, &result.message);
                    if *shutdown && result.ok {
                        self.deferred_destroy = true;
                        return;
                    }
                    completed.push(*request);
                }
            }
            for request in completed {
                self.pending.remove(&request);
            }
            if model.revision != self.rendered_revision {
                self.rendering = true;
                self.rendered_revision = model.revision;
                text(
                    self.machine,
                    &format!("{}  ·  {}", model.machine, model.version),
                );
                if self.pending.is_empty() {
                    text(self.notice, &model.notice);
                }
                for placed in &self.metrics {
                    if let Some(metric) = model.metrics.iter().find(|m| m.key == placed.key) {
                        let mut changed = false;
                        for (hwnd, value) in [
                            (placed.name, &metric.label),
                            (placed.value, &metric.value),
                            (placed.detail, &metric.detail),
                        ] {
                            if get_text(hwnd) != *value {
                                text(hwnd, value);
                                changed = true;
                            }
                        }
                        if changed {
                            a11y::annotate(
                                placed.value,
                                &a11y::key(&self.accessibility_scope(), "metric", &metric.key),
                                Some(&format!(
                                    "{} {} {}",
                                    metric.label, metric.value, metric.detail
                                )),
                                None,
                            );
                        }
                    }
                }
                for binding in &mut self.bindings {
                    EnableWindow(
                        binding.hwnd,
                        model
                            .capabilities
                            .get(&binding.key)
                            .map(|cap| cap.available)
                            .unwrap_or_else(|| {
                                binding.capability.is_empty() || model.can(&binding.capability)
                            }) as i32,
                    );
                    if !self.dirty.contains(&binding.key) {
                        set_binding(binding, &model.value(&binding.key));
                    }
                }
                for (id, (key, _)) in &self.pickers {
                    if let Some(binding) = self.bindings.iter().find(|binding| binding.key == *key)
                    {
                        EnableWindow(
                            GetDlgItem(self.hwnd, *id as i32),
                            IsWindowEnabled(binding.hwnd),
                        );
                    }
                }
                for (id, action) in &self.buttons {
                    EnableWindow(
                        GetDlgItem(self.hwnd, *id as i32),
                        (action.capability.is_empty() || model.can(action.capability)) as i32,
                    );
                }
                self.rendering = false;
                self.update_tray_tip(&model);
            }
            self.apply_preferences(&model);
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
            self.tray_data.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
            copy_wide(&mut self.tray_data.szTip, "机控 / Jiks");
            self.tray = Shell_NotifyIconW(NIM_ADD, &self.tray_data) != 0;
            if self.tray {
                self.tray_data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                Shell_NotifyIconW(NIM_SETVERSION, &self.tray_data);
            }
        }
    }
    unsafe fn update_tray_tip(&mut self, model: &UiModel) {
        unsafe {
            if !self.tray {
                return;
            }
            let summary = model
                .metrics
                .iter()
                .take(3)
                .map(|m| format!("{} {}", m.label, m.value))
                .collect::<Vec<_>>()
                .join(" · ");
            copy_wide(
                &mut self.tray_data.szTip,
                &format!("机控 / Jiks\n{summary}"),
            );
            Shell_NotifyIconW(NIM_MODIFY, &self.tray_data);
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
                wide(&super::i18n::tr("显示机控")).as_ptr(),
            );
            AppendMenuW(
                menu,
                MF_STRING,
                ID_TRAY_EXIT,
                wide(&super::i18n::tr("退出机控")).as_ptr(),
            );
            let mut point: POINT = zeroed();
            GetCursorPos(&mut point);
            SetForegroundWindow(self.hwnd);
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
            let index = options
                .iter()
                .position(|o| o.value == *value || scalar(&o.value) == scalar(value))
                .unwrap_or(usize::MAX);
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
        })
    }
}
unsafe fn set_binding(binding: &Binding, value: &Value) {
    unsafe {
        match &binding.kind {
            BindingKind::Toggle => {
                SendMessageW(
                    binding.hwnd,
                    BM_SETCHECK,
                    if value.as_bool().unwrap_or(false) {
                        BST_CHECKED as usize
                    } else {
                        BST_UNCHECKED as usize
                    },
                    0,
                );
            }
            BindingKind::Text | BindingKind::Number => {
                let v = scalar(value);
                if get_text(binding.hwnd) != v {
                    text(binding.hwnd, &v);
                }
            }
            BindingKind::Choice(options) => {
                let index = options
                    .iter()
                    .position(|o| o.value == *value || scalar(&o.value) == scalar(value))
                    .unwrap_or(usize::MAX);
                SendMessageW(binding.hwnd, CB_SETCURSEL, index, 0);
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
                darkest >= 0x60 && darkest < 0xc0,
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
    fn tall_controls_remain_partly_visible_at_both_viewport_edges() {
        unsafe {
            let parent = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                400,
                400,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            let child = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                WS_CHILD,
                0,
                0,
                240,
                136,
                parent,
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            let region = CreateRectRgn(0, 0, 0, 0);
            assert!(clip_to_viewport(child, 150, 240, 136, 90, 255, 96));
            assert_ne!(GetWindowRgn(child, region), ERROR);
            let mut bounds: RECT = zeroed();
            GetRgnBox(region, &mut bounds);
            assert_eq!((bounds.top, bounds.bottom), (0, 105));
            assert!(clip_to_viewport(child, 86, 240, 136, 90, 255, 96));
            GetWindowRgn(child, region);
            GetRgnBox(region, &mut bounds);
            assert_eq!((bounds.top, bounds.bottom), (4, 136));
            assert!(!clip_to_viewport(child, 300, 240, 136, 90, 255, 96));
            DeleteObject(region);
            DestroyWindow(parent);
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
