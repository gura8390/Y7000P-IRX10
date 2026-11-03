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
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, GetKeyState, IsWindowEnabled};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// Stable Windows SDK constants omitted/gated in windows-sys metadata.
pub(super) const SS_LEFT: u32 = 0;
pub(super) const SS_ETCHEDHORZ: u32 = 0x10;
pub(super) const TRACKBAR_GET_POSITION: u32 = WM_USER;
const TRAY_KEY_SELECT: u32 = NIN_SELECT | 1;
#[derive(Clone, Copy, PartialEq, Eq)]
struct Palette {
    paper: u32,
    ink: u32,
    accent: u32,
    muted: u32,
    line: u32,
    pressed: u32,
}
impl Default for Palette {
    fn default() -> Self {
        Self {
            paper: 0x00f0f5f5,
            ink: 0x002e1a1a,
            accent: 0x0060340f,
            muted: 0x00666666,
            line: 0x00c8d0d0,
            pressed: 0x00e4eaea,
        }
    }
}
thread_local! {static PALETTE:Cell<Palette>=Cell::new(Palette::default());}
pub(super) fn paper() -> u32 {
    PALETTE.with(|p| p.get().paper)
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
            paper: 0x002c2421,
            ink: 0x00e8ecee,
            accent: 0x0086552c,
            muted: 0x00b8b3ae,
            line: 0x005e524b,
            pressed: 0x003d332e,
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
        }
        hwnd
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
    unsafe {
        let dc = GetDC(null_mut());
        let family = ["方正公文楷体", "FZGWKaiS", "Microsoft YaHei UI"]
            .into_iter()
            .find(|name| exact_font_present(dc, name))
            .unwrap_or("Microsoft YaHei UI")
            .to_owned();
        ReleaseDC(null_mut(), dc);
        family
    }
}
pub(super) unsafe fn fill(dc: HDC, rect: &RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(color);
        FillRect(dc, rect, brush);
        DeleteObject(brush);
    }
}
pub(super) unsafe fn paint_button(item: &DRAWITEMSTRUCT, selected: bool) {
    unsafe {
        let disabled = item.itemState & ODS_DISABLED != 0;
        let pressed = item.itemState & ODS_SELECTED != 0;
        let background = if selected {
            accent()
        } else if pressed {
            PALETTE.with(|p| p.get().pressed)
        } else {
            paper()
        };
        fill(item.hDC, &item.rcItem, background);
        let pen = CreatePen(PS_SOLID, 1, if selected { accent() } else { line() });
        let old_pen = SelectObject(item.hDC, pen);
        let old_brush = SelectObject(item.hDC, GetStockObject(NULL_BRUSH));
        Rectangle(
            item.hDC,
            item.rcItem.left,
            item.rcItem.top,
            item.rcItem.right,
            item.rcItem.bottom,
        );
        SelectObject(item.hDC, old_pen);
        SelectObject(item.hDC, old_brush);
        DeleteObject(pen);
        SetBkMode(item.hDC, TRANSPARENT as i32);
        SetTextColor(
            item.hDC,
            if disabled {
                0x00909090
            } else if selected {
                0x00ffffff
            } else {
                ink()
            },
        );
        let label = wide(&get_text(item.hwndItem));
        let mut rect = item.rcItem;
        let navigation =
            item.CtlID >= ID_NAV as u32 && item.CtlID < ID_NAV as u32 + Page::ALL.len() as u32;
        if navigation {
            paint_nav_icon(item, item.CtlID - ID_NAV as u32);
            rect.left += rect.bottom - rect.top + 7;
        }
        DrawTextW(
            item.hDC,
            label.as_ptr(),
            -1,
            &mut rect,
            (if navigation { DT_LEFT } else { DT_CENTER })
                | DT_VCENTER
                | DT_SINGLELINE
                | DT_END_ELLIPSIS,
        );
        if item.itemState & ODS_FOCUS != 0 {
            InflateRect(&mut rect, -3, -3);
            DrawFocusRect(item.hDC, &rect);
        }
    }
}

unsafe fn paint_nav_icon(item: &DRAWITEMSTRUCT, index: u32) {
    unsafe {
        let h = (item.rcItem.bottom - item.rcItem.top).max(1);
        let unit = (h / 16).max(1);
        let x = item.rcItem.left + 6 * unit;
        let y = item.rcItem.top + 4 * unit;
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
    fixed: Vec<HWND>,
    content: Vec<Placed>,
    bindings: Vec<Binding>,
    buttons: BTreeMap<u16, Action>,
    pickers: BTreeMap<u16, (String, super::selectors::PickerKind)>,
    drafts: BTreeMap<String, Value>,
    dirty: BTreeSet<String>,
    metrics: Vec<(HWND, String)>,
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
            fixed: vec![],
            content: vec![],
            bindings: vec![],
            buttons: BTreeMap::new(),
            pickers: BTreeMap::new(),
            drafts: BTreeMap::new(),
            dirty: BTreeSet::new(),
            metrics: vec![],
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
            column_x: 212,
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
                    if lp as HWND == app.notice {
                        muted()
                    } else {
                        ink()
                    },
                );
                SetBkMode(dc, TRANSPARENT as i32);
                SetBkColor(dc, paper());
                Some(if app.background.visible() && msg == WM_CTLCOLORSTATIC {
                    GetStockObject(NULL_BRUSH) as isize
                } else {
                    app.brush as isize
                })
            }
            WM_ERASEBKGND => {
                let mut r: RECT = zeroed();
                GetClientRect(app.hwnd, &mut r);
                if !app.background.paint(wp as HDC, &r) {
                    FillRect(wp as HDC, &r, app.brush);
                }
                Some(1)
            }
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(app.hwnd, &mut ps);
                let mut r: RECT = zeroed();
                GetClientRect(app.hwnd, &mut r);
                let pen = CreatePen(PS_SOLID, 1, line());
                let old = SelectObject(dc, pen);
                MoveToEx(dc, app.s(190), app.s(104), null_mut());
                LineTo(dc, app.s(190), r.bottom - app.s(58));
                MoveToEx(dc, app.s(210), app.s(104), null_mut());
                LineTo(dc, r.right - app.s(20), app.s(104));
                SelectObject(dc, old);
                DeleteObject(pen);
                EndPaint(app.hwnd, &ps);
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
                if snapshot(&app.bridge).preferences.close_to_tray && app.tray {
                    ShowWindow(app.hwnd, SW_HIDE);
                } else {
                    app.shutdown();
                }
                Some(0)
            }
            WM_DESTROY => {
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
            self.body_font = font(self.dpi, 15, false);
            self.heading_font = font(self.dpi, 20, true);
            self.title_font = font(self.dpi, 29, true);
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
                SS_LEFT as u32,
                0,
                [self.s(20), self.s(24), self.s(150), self.s(42)],
                self.title_font,
            );
            self.fixed.push(brand);
            a11y::annotate(brand, "main.brand", None, None);
            let sub = create(
                self.hwnd,
                "STATIC",
                &super::i18n::tr("Jiks · 本机管理"),
                SS_LEFT as u32,
                0,
                [self.s(22), self.s(70), self.s(150), self.s(24)],
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
                        self.s(20),
                        self.s(if index < 6 {
                            124 + index as i32 * 40
                        } else {
                            420 + (index as i32 - 6) * 40
                        }),
                        self.s(150),
                        self.s(32),
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
            self.layout_fixed();
            self.rendering = false;
        }
    }
    unsafe fn layout_fixed(&mut self) {
        unsafe {
            let (w, h) = self.size();
            // A short client area uses one native scrolling navigation list.
            // Main and footer items never occupy the same row.
            let compact_nav = h < 568;
            let nav = GetDlgItem(self.hwnd, ID_NAV_LIST as i32);
            SetWindowPos(
                nav,
                null_mut(),
                self.s(20),
                self.s(124),
                self.s(150),
                self.s((h - 198).max(76)),
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
                    self.s(20),
                    self.s(h - 174 + (index as i32 - 6) * 40),
                    self.s(150),
                    self.s(32),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            for (hwnd, x, y, width, height) in [
                (GetDlgItem(self.hwnd, ID_EXIT as i32), 20, h - 52, 150, 32),
                (self.title, 210, 25, w - 470, 40),
                (GetDlgItem(self.hwnd, ID_DEVICE as i32), w - 240, 30, 66, 30),
                (GetDlgItem(self.hwnd, ID_LOG as i32), w - 166, 30, 66, 30),
                (GetDlgItem(self.hwnd, ID_THEME as i32), w - 92, 30, 66, 30),
                (self.machine, 212, 72, w - 234, 24),
                (self.notice, 210, h - 48, w - 232, 40),
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
            let model = snapshot(&self.bridge);
            self.rendered_revision = model.revision;
            self.metadata_revision = model.metadata_revision;
            let (w, _) = self.size();
            let width = (w - 236).max(360);
            let mut y = 124;
            self.column_x = 212;
            let mut content_origin = 212;
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
                        [212, 124 + index as i32 * 42, nav_width, 32],
                        self.body_font,
                    );
                    a11y::annotate(
                        button,
                        &a11y::key("main.settings", "category", TOOL_KEYS[index]),
                        Some(title),
                        (index == self.tools_tab).then(|| super::i18n::tr_static("当前页面")),
                    );
                }
                content_origin = 212 + nav_width + 20;
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
                    let mut group_y = pair_y;
                    self.column_x = 212 + column as i32 * (width / columns as i32);
                    let heading =
                        self.label(&group.title, group_y, width / columns as i32 - 12, true, 28);
                    if let Some(first) = group.metrics.first() {
                        a11y::annotate(
                            heading,
                            &a11y::key("main.dashboard", "group", &first.key),
                            None,
                            None,
                        );
                    }
                    group_y += 34;
                    for metric in group.metrics {
                        let h = self.label(
                            &format!("{}    {}    {}", metric.label, metric.value, metric.detail),
                            group_y,
                            width / columns as i32 - 12,
                            false,
                            28,
                        );
                        a11y::annotate(
                            h,
                            &a11y::key("main.dashboard", "metric", &metric.key),
                            None,
                            None,
                        );
                        self.metrics.push((h, metric.key));
                        group_y += 30;
                    }
                    bottoms[column] = group_y + 16;
                }
                y = bottoms[0].max(bottoms[1]);
                self.column_x = 212;
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
                for metric in model
                    .metrics
                    .iter()
                    .filter(|m| m.key.to_ascii_lowercase().starts_with("battery"))
                {
                    let h = self.label(
                        &format!("{}    {}    {}", metric.label, metric.value, metric.detail),
                        y,
                        width,
                        false,
                        28,
                    );
                    a11y::annotate(
                        h,
                        &a11y::key("main.battery", "metric", &metric.key),
                        None,
                        None,
                    );
                    self.metrics.push((h, metric.key.clone()));
                    y += 30;
                }
                y += 16;
            }
            let row_height = if model.preferences.compact { 36 } else { 42 };
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
                self.column_x = content_origin + column as i32 * (width + 24);
                if two_columns {
                    if column == 0 {
                        pair_y = bottoms[0].max(bottoms[1]);
                    }
                    y = pair_y;
                }
                let label_width = if width < 550 { 145 } else { 205 };
                let editor_x = self.column_x + label_width + 12;
                let section_key = section
                    .fields
                    .first()
                    .map(|field| field.key.to_owned())
                    .or_else(|| section.actions.first().map(action_key))
                    .unwrap_or_else(|| "content".into());
                let section_scope = a11y::key(&self.accessibility_scope(), "section", &section_key);
                let heading = self.label(&section.title, y, width, true, 30);
                a11y::annotate(
                    heading,
                    &a11y::key(&section_scope, "text", "title"),
                    None,
                    None,
                );
                y += 38;
                let hint = self.label(section.hint, y, width, false, 44);
                a11y::annotate(
                    hint,
                    &a11y::key(&section_scope, "text", "description"),
                    None,
                    None,
                );
                y += 51;
                for spec in section.fields {
                    let field_label = if let Some(range) = model.ranges.get(spec.key) {
                        format!(
                            "{} · {}–{} {}",
                            spec.label, range.minimum, range.maximum, range.unit
                        )
                    } else {
                        spec.label.into()
                    };
                    let label = self.label(&field_label, y + 6, label_width, false, 28);
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
                    let available_width = (width - label_width - 12).max(120);
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
                    y += row_height;
                }
                let mut x = self.column_x;
                let mut unavailable = vec![];
                for action in section.actions {
                    let button_width =
                        ((action.label.chars().count() as i32 * 15) + 30).clamp(96, 230);
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
                        [x, y + 6, button_width, 32],
                        self.body_font,
                    );
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
                        let line = format!("{}：{}", action.label, reason);
                        if !unavailable.contains(&line) {
                            unavailable.push(line);
                        }
                    }
                    self.buttons.insert(id, action);
                    x += button_width + 10;
                }
                y += 52;
                for reason in unavailable {
                    self.label(&reason, y, width, false, 30);
                    y += 32;
                }
                self.placed(
                    "STATIC",
                    "",
                    SS_ETCHEDHORZ as u32,
                    0,
                    [self.column_x, y + 8, width, 1],
                    self.body_font,
                );
                y += 35;
                if two_columns {
                    bottoms[column] = y;
                }
            }
            if two_columns {
                y = bottoms[0].max(bottoms[1]);
            }
            self.column_x = 212;
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
                let visible = y >= 114 && y + p.clip_height <= height - 58;
                ShowWindow(p.hwnd, if visible { SW_SHOWNA } else { SW_HIDE });
            }
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
                self.brush = CreateSolidBrush(paper());
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
                for (hwnd, key) in &self.metrics {
                    if let Some(metric) = model.metrics.iter().find(|m| m.key == *key) {
                        let value =
                            format!("{}    {}    {}", metric.label, metric.value, metric.detail);
                        if get_text(*hwnd) != value {
                            text(*hwnd, &value);
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
            if !self.window_restored && model.values.contains_key("app.remember_position") {
                self.window_restored = true;
                if model.value("app.remember_position").as_bool() == Some(true) {
                    let geometry = self
                        .bridge
                        .model
                        .lock()
                        .ok()
                        .and_then(|m| m.values.get("app.window").cloned())
                        .unwrap_or(Value::Null);
                    if let (Some(x), Some(y), Some(w), Some(h)) = (
                        geometry.pointer("/position/Left").and_then(Value::as_f64),
                        geometry.pointer("/position/Top").and_then(Value::as_f64),
                        geometry.pointer("/size/Width").and_then(Value::as_f64),
                        geometry.pointer("/size/Height").and_then(Value::as_f64),
                    ) {
                        let scale = self.dpi as f64 / 96.;
                        let mut r = RECT {
                            left: (x * scale) as i32,
                            top: (y * scale) as i32,
                            right: ((x + w) * scale) as i32,
                            bottom: ((y + h) * scale) as i32,
                        };
                        let monitor = MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST);
                        let mut info: MONITORINFO = zeroed();
                        info.cbSize = size_of::<MONITORINFO>() as u32;
                        if GetMonitorInfoW(monitor, &mut info) != 0 {
                            let width = (r.right - r.left).clamp(
                                self.s(650).min(info.rcWork.right - info.rcWork.left),
                                info.rcWork.right - info.rcWork.left,
                            );
                            let height = (r.bottom - r.top).clamp(
                                self.s(600).min(info.rcWork.bottom - info.rcWork.top),
                                info.rcWork.bottom - info.rcWork.top,
                            );
                            r.left = r.left.clamp(info.rcWork.left, info.rcWork.right - width);
                            r.top = r.top.clamp(info.rcWork.top, info.rcWork.bottom - height);
                            SetWindowPos(
                                self.hwnd,
                                null_mut(),
                                r.left,
                                r.top,
                                width,
                                height,
                                SWP_NOZORDER | SWP_NOACTIVATE,
                            );
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
            if self.bridge.smoke_close_after.is_some()
                || model.value("backend.read_only").as_bool() == Some(true)
                || model.value("app.remember_position").as_bool() != Some(true)
                || IsIconic(self.hwnd) != 0
                || IsZoomed(self.hwnd) != 0
            {
                return;
            }
            let mut r: RECT = zeroed();
            if GetWindowRect(self.hwnd, &mut r) == 0 {
                return;
            }
            let scale = 96. / self.dpi as f64;
            if let Err(error) = send(
                &self.bridge,
                UiAction::Execute {
                    request_id: request_id(),
                    command: "app.preferences.save".into(),
                    payload: json!({"app.window":{"left":r.left as f64*scale,"top":r.top as f64*scale,"width":(r.right-r.left) as f64*scale,"height":(r.bottom-r.top) as f64*scale}}),
                },
            ) {
                text(self.notice, &format!("窗口位置尚未保存：{error:#}"));
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
