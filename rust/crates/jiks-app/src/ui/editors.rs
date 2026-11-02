//! Structured native editors. All persistence is acknowledged by the runtime.
//!
//! Execute payload contracts:
//! preset.save: { id, name, values:{"cpu.pl1":70,...}, fan:{full_speed,table}, overrides:{...} }
//! automation.save: { id,name,enabled,exclusive,run_on_startup,
//!   trigger:null|{type,parameters}|{type:"all"|"any",conditions:[...]},
//!   steps:[{type,parameters:{...}}] }
//! macro.save: { id,name,trigger_key,repeat,ignore_delays,interrupt_on_other_key,
//!   events:[{kind,key,delay_ms,x,y,...}] }
//! thermal.apply / sensors.settings.save: flat semantic field keys, as on main forms.
//! Old input values are retained in the payload; the runtime owns schema migration.
use crate::ui::i18n::{format_product, tr, tr_static};
#[path = "forms.rs"]
mod forms;
#[path = "layout.rs"]
mod layout;
#[path = "spectrum_editor.rs"]
mod spectrum_editor;
use super::catalog::{AUTOMATION_ACTIONS, AUTOMATION_TRIGGERS, EditorKind};
use super::native::*;
use super::selectors::{self, FileFilter, PickerKind};
use crate::ui_model::*;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::Ordering;
use std::time::Instant;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const SAVE: u16 = 1;
const CANCEL: u16 = 2;
const EDITOR_TAB: u16 = 10;
const ACTION_TYPE: u16 = 20;
const TRIGGER_TYPE: u16 = 21;
const STEP_LIST: u16 = 22;
const CONDITION_LIST: u16 = 23;
const STEP_ADD: u16 = 30;
const STEP_UPDATE: u16 = 31;
const STEP_DELETE: u16 = 32;
const STEP_UP: u16 = 33;
const STEP_DOWN: u16 = 34;
const CONDITION_ADD: u16 = 35;
const CONDITION_DELETE: u16 = 36;
const EVENT_ADD: u16 = 40;
const EVENT_UPDATE: u16 = 41;
const EVENT_DELETE: u16 = 42;
const EVENT_UP: u16 = 43;
const EVENT_DOWN: u16 = 44;
const RECORD_START: u16 = 50;
const RECORD_STOP: u16 = 51;
const EVENT_KIND: u16 = 52;
const EVENT_CLEAR: u16 = 53;
const SENSOR_DEFAULTS: u16 = 60;

#[derive(Clone)]
enum Mode {
    Document(String),
    Form(String),
    Layout(String),
    Backup,
    Hotkeys,
    Spectrum,
    Console,
    Arguments,
    FirstLanguage,
    Preset,
    Automation,
    Macro,
    SensorSettings,
}
#[derive(Clone)]
enum Kind {
    Text,
    Number,
    Lines,
    MultiChoice(Vec<UiChoice>),
    Check,
    Choice(Vec<UiChoice>),
}
struct Edit {
    key: String,
    hwnd: HWND,
    kind: Kind,
}
struct Placement {
    hwnd: HWND,
    rect: [i32; 4],
    clip_height: i32,
}
struct Slider {
    key: String,
    slider: HWND,
    edit: HWND,
    range: UiRange,
}
/// An edited node can retain opaque legacy fields only while its kind is unchanged.
fn retain_legacy(original: &Value, replacement: &mut Value, discriminator: &str) {
    if original.get(discriminator) == replacement.get(discriminator) {
        if let Some(legacy) = original.get("legacy") {
            replacement["legacy"] = legacy.clone();
        }
    }
}
struct Pending {
    id: u64,
    close: bool,
    recording: Option<bool>,
}
#[derive(Clone)]
struct PickerRequest {
    key: String,
    kind: PickerKind,
    initial: String,
    append: bool,
}
struct ConfirmedSubmit {
    message: &'static str,
    command: &'static str,
    payload: Value,
}
#[derive(Default)]
struct RecordingStop {
    inflight: Option<u64>,
    requested: bool,
    confirmed: bool,
    cleanup_uncertain: bool,
    close_requested: bool,
    had_started: bool,
    final_events_loaded: bool,
}
impl RecordingStop {
    fn busy(&self) -> bool {
        self.inflight.is_some() || self.cleanup_uncertain || self.needs_final_events()
    }
    fn acknowledge(&mut self, ok: bool) {
        self.inflight = None;
        self.confirmed = ok;
        self.cleanup_uncertain = !ok;
    }
    fn acknowledge_start(&mut self, ok: bool) -> bool {
        self.had_started |= ok;
        ok && !self.confirmed
    }
    fn needs_final_events(&self) -> bool {
        self.confirmed && self.had_started && !self.final_events_loaded
    }
    fn can_close(&self, start_pending: bool, recording: bool) -> bool {
        self.close_requested && self.confirmed && !self.busy() && !start_pending && !recording
    }
}
struct Editor {
    bridge: UiBridge,
    hwnd: HWND,
    mode: Mode,
    dpi: u32,
    font: HFONT,
    heading: HFONT,
    brush: HBRUSH,
    data: Value,
    drafts: BTreeMap<String, Value>,
    edits: Vec<Edit>,
    placements: Vec<Placement>,
    sliders: Vec<Slider>,
    body_buttons: Vec<(HWND, String)>,
    status: HWND,
    save: HWND,
    cancel: HWND,
    rendering: bool,
    tab: usize,
    steps: Vec<Value>,
    conditions: Vec<Value>,
    selected_step: Option<usize>,
    selected_condition: Option<usize>,
    action_type: String,
    trigger_type: String,
    event_kind: String,
    action_labels: Vec<String>,
    action_palette_ids: Vec<String>,
    trigger_labels: Vec<String>,
    pending: Option<Pending>,
    recording: bool,
    recording_stop: RecordingStop,
    recording_revision: u64,
    scroll: i32,
    content_height: i32,
    deferred_destroy: bool,
    deferred_picker: Option<PickerRequest>,
    deferred_submit: Option<ConfirmedSubmit>,
    modal_active: bool,
    close_after_modal: bool,
    picker_buttons: BTreeMap<u16, (String, PickerKind, bool)>,
    started: Instant,
    key_code: Option<i64>,
    key_changes: BTreeMap<String, Value>,
    layout_extra: i32,
    flow_y: Option<i32>,
    flow_next_x: i32,
    flow_wrapped: bool,
}

pub(super) fn structured(
    parent: HWND,
    bridge: UiBridge,
    kind: EditorKind,
    item: Option<UiItem>,
) -> Result<()> {
    let mode = match kind {
        EditorKind::Preset => Mode::Preset,
        EditorKind::Automation => Mode::Automation,
        EditorKind::Macro => Mode::Macro,
    };
    let mut data = item
        .as_ref()
        .map(|i| i.value.clone())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    if data.get("id").is_none() {
        data["id"] = json!(item.as_ref().map(|i| i.id.clone()).unwrap_or_default());
    }
    if data.get("name").is_none() {
        data["name"] = json!(item.as_ref().map(|i| i.label.clone()).unwrap_or_default());
    }
    open(parent, bridge, mode, data)
}
pub(super) fn sensor_settings(parent: HWND, bridge: UiBridge) -> Result<()> {
    open(parent, bridge, Mode::SensorSettings, json!({}))
}
pub(super) fn document(parent: HWND, bridge: UiBridge, id: String) -> Result<()> {
    if id == "first_language" {
        return open(parent, bridge, Mode::FirstLanguage, json!({}));
    }
    if id == "arguments" {
        let request = request_id();
        send(
            &bridge,
            UiAction::Execute {
                request_id: request,
                command: "startup.arguments.load".into(),
                payload: json!({}),
            },
        )?;
        return open(
            parent,
            bridge,
            Mode::Arguments,
            json!({"load_request":request,"loaded":false}),
        );
    }
    if id == "legacy_extensions" {
        unsafe {
            message(
                parent,
                "脚本与扩展",
                "旧 C# / CLR 脚本和二进制插件保留原文，Rust不会加载它们。\r\n\r\n本机脚本控制台支持明确的可执行文件、原始参数和工作目录；PowerShell/Python脚本请指定对应解释器。自动化中的运行程序动作使用同一原生异步执行器。输出有界，停止只取消等待，不强杀外部进程。",
                false,
            );
        }
        return Ok(());
    }
    if id == "script_console" {
        return open(parent, bridge, Mode::Console, json!({}));
    }
    let model = snapshot(&bridge);
    if id == "spectrum_keyboard" {
        let effects = spectrum_editor::cached(&model);
        let profile = model.value("keyboard.profile");
        return open(
            parent,
            bridge,
            Mode::Spectrum,
            json!({"Profile":profile,"Loaded":effects.is_some(),"Effects":effects.unwrap_or_default()}),
        );
    }
    if id == "special_keys" {
        let document = model
            .documents
            .get("special_keys")
            .ok_or_else(|| anyhow!("{}", tr_static("按键配置当前不可用。")))?;
        return open(parent, bridge, Mode::Hotkeys, document.value.clone());
    }
    if id == "settings_backup" {
        return open(parent, bridge, Mode::Backup, json!({}));
    }
    if id == "dashboard" || id == "sensor_dashboard" {
        let document = model
            .documents
            .get(&id)
            .ok_or_else(|| anyhow!("{}", tr_static("此设置当前不可用。")))?;
        return open(parent, bridge, Mode::Layout(id), document.value.clone());
    }
    if let Some(form) = forms::schema(&id, &model) {
        let document = model
            .documents
            .get(form.document)
            .ok_or_else(|| anyhow!("{}", tr_static("此设置当前不可用；请查看后台状态说明。")))?;
        return open(parent, bridge, Mode::Form(id), document.value.clone());
    }
    let id = id.strip_prefix("advanced:").unwrap_or(&id).to_owned();
    let Some(document) = model.documents.get(&id) else {
        bail!("{}", tr_static("此设置当前不可用；请查看后台状态说明。"));
    };
    open(parent, bridge, Mode::Document(id), document.value.clone())
}

fn open(parent: HWND, bridge: UiBridge, mode: Mode, data: Value) -> Result<()> {
    unsafe {
        let class = wide("Jiks.Native.Editor");
        let brush = CreateSolidBrush(paper());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(editor_proc),
            hInstance: GetModuleHandleW(null()),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: GetStockObject(WHITE_BRUSH) as HBRUSH,
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            DeleteObject(brush);
            bail!("无法创建设置窗口：{}", GetLastError());
        }
        let dpi = GetDpiForWindow(parent).max(96);
        let title = tr_static(match &mode {
            Mode::Preset => "自定义模式设置",
            Mode::Automation => "编辑自动化规则",
            Mode::Macro => "编辑键鼠宏",
            Mode::SensorSettings => "传感器设置",
            Mode::Document(_) => "高级配置导入与编辑",
            Mode::Form(id) => forms::schema(id, &snapshot(&bridge))
                .map(|f| f.title)
                .unwrap_or("设置"),
            Mode::Backup => "备份与还原设置",
            Mode::Hotkeys => "自定义 Fn 快捷键",
            Mode::Spectrum => "Spectrum 多灯效与分区",
            Mode::Console => "本机脚本控制台",
            Mode::Arguments => "启动参数",
            Mode::FirstLanguage => "选择语言",
            Mode::Layout(id) => {
                if id == "sensor_dashboard" {
                    "传感器分组"
                } else {
                    "自定义控制台"
                }
            }
        });
        let steps = match mode {
            Mode::Automation => data.get("steps"),
            Mode::Macro => data.get("events"),
            Mode::Layout(_) => data.get("Groups"),
            Mode::Spectrum => data.get("Effects"),
            _ => None,
        }
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
        let trigger = data.get("trigger").cloned().unwrap_or(Value::Null);
        let conditions = if trigger.is_null() {
            vec![]
        } else if let Some(c) = trigger.get("conditions").and_then(Value::as_array) {
            c.clone()
        } else {
            vec![trigger.clone()]
        };
        let action_type = steps
            .first()
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("delay")
            .into();
        let trigger_type = conditions
            .first()
            .and_then(|c| c.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("manual")
            .into();
        let state = Box::new(RefCell::new(Editor {
            bridge,
            hwnd: null_mut(),
            mode,
            dpi,
            font: font(dpi, 15, false),
            heading: font(dpi, 21, true),
            brush,
            data,
            drafts: BTreeMap::new(),
            edits: vec![],
            placements: vec![],
            sliders: vec![],
            body_buttons: vec![],
            status: null_mut(),
            save: null_mut(),
            cancel: null_mut(),
            rendering: false,
            tab: 0,
            steps,
            conditions,
            selected_step: None,
            selected_condition: None,
            action_type,
            trigger_type,
            event_kind: "key_down".into(),
            action_labels: vec![],
            action_palette_ids: vec![],
            trigger_labels: vec![],
            pending: None,
            recording: false,
            recording_stop: RecordingStop::default(),
            recording_revision: 0,
            scroll: 0,
            content_height: 0,
            deferred_destroy: false,
            deferred_picker: None,
            deferred_submit: None,
            modal_active: false,
            close_after_modal: false,
            picker_buttons: BTreeMap::new(),
            started: Instant::now(),
            key_code: None,
            key_changes: BTreeMap::new(),
            layout_extra: 0,
            flow_y: None,
            flow_next_x: 20,
            flow_wrapped: false,
        }));
        state.borrow_mut().seed();
        let raw = Box::into_raw(state);
        let mut parent_rect: RECT = zeroed();
        GetWindowRect(parent, &mut parent_rect);
        let width = if matches!((*raw).borrow().mode, Mode::SensorSettings) {
            650
        } else {
            960
        };
        let height = if matches!((*raw).borrow().mode, Mode::SensorSettings) {
            690
        } else {
            780
        };
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let width = (width * dpi as i32 / 96).min(screen_w - 40).max(560);
        let height = (height * dpi as i32 / 96).min(screen_h - 70).max(480);
        let x = (parent_rect.left + (parent_rect.right - parent_rect.left - width) / 2)
            .clamp(0, (screen_w - width).max(0));
        let y = (parent_rect.top + (parent_rect.bottom - parent_rect.top - height) / 2)
            .clamp(0, (screen_h - height).max(0));
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class.as_ptr(),
            wide(title).as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_VSCROLL,
            x,
            y,
            width,
            height,
            parent,
            null_mut(),
            GetModuleHandleW(null()),
            raw as *const _,
        );
        if hwnd.is_null() {
            drop(Box::from_raw(raw));
            bail!("无法打开编辑窗口：{}", GetLastError());
        }
        EnableWindow(parent, 0);
        ShowWindow(hwnd, SW_SHOWNORMAL);
        SetForegroundWindow(hwnd);
        let mut msg: MSG = zeroed();
        while IsWindow(hwnd) != 0 {
            let result = GetMessageW(&mut msg, null_mut(), 0, 0);
            if result <= 0 {
                if result == 0 {
                    PostQuitMessage(msg.wParam as i32);
                }
                break;
            }
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if IsWindow(hwnd) != 0 {
            DestroyWindow(hwnd);
        }
        EnableWindow(parent, 1);
        SetForegroundWindow(parent);
        drop(Box::from_raw(raw));
        Ok(())
    }
}
impl Drop for Editor {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.font);
            DeleteObject(self.heading);
            DeleteObject(self.brush);
        }
    }
}

// The modal function owns this Box<RefCell<Editor>> through its message loop.
// No WM_NCDESTROY path releases it; DestroyWindow runs after RefMut is dropped.
unsafe extern "system" fn editor_proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lp as *const CREATESTRUCTW);
            let raw = cs.lpCreateParams as *mut RefCell<Editor>;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
            (*raw).borrow_mut().hwnd = hwnd;
            let diagnostic = std::sync::Arc::as_ptr(&(*raw).borrow().bridge.diagnostics);
            SetPropW(
                hwnd,
                wide("Jiks.Diagnostics").as_ptr(),
                diagnostic as HANDLE,
            );
        }
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RefCell<Editor>;
        if raw.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let (mut result, destroy, picker, confirmed_submit) = {
            let Ok(mut editor) = (*raw).try_borrow_mut() else {
                let diag =
                    GetPropW(hwnd, wide("Jiks.Diagnostics").as_ptr()) as *const UiDiagnostics;
                if !diag.is_null() {
                    (*diag).reentrant_defaults.fetch_add(1, Ordering::Relaxed);
                }
                if msg == WM_CLOSE {
                    SetPropW(hwnd, wide("Jiks.DeferredClose").as_ptr(), 1usize as HANDLE);
                    return 0;
                }
                if msg == WM_DESTROY {
                    KillTimer(hwnd, 1);
                    return 0;
                }
                if msg == WM_NCDESTROY {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    return DefWindowProcW(hwnd, msg, wp, lp);
                }
                return DefWindowProcW(hwnd, msg, wp, lp);
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                editor_message(&mut editor, msg, wp, lp)
            }));
            let destroy = std::mem::take(&mut editor.deferred_destroy);
            let picker = editor.deferred_picker.take();
            let confirmed_submit = editor.deferred_submit.take();
            (result, destroy, picker, confirmed_submit)
        };
        if destroy && IsWindow(hwnd) != 0 {
            DestroyWindow(hwnd);
        }
        if let Some(picker) = picker {
            if !destroy {
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_picker(hwnd, raw, picker)
                })) {
                    result = Err(panic);
                }
            }
        }
        if let Some(request) = confirmed_submit {
            if !destroy {
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_confirmed_submit(hwnd, raw, request)
                })) {
                    result = Err(panic);
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
                if IsWindow(hwnd) != 0 && GetWindowLongPtrW(hwnd, GWLP_USERDATA) == raw as isize {
                    if let Ok(mut editor) = (*raw).try_borrow_mut() {
                        editor.modal_active = false;
                    }
                    message(
                        hwnd,
                        "编辑未完成",
                        "操作未完成，原设置不会被当作保存成功。",
                        true,
                    );
                }
                0
            }
        }
    }
}

// The modal owner's stable Box lives until this outer message dispatch returns.
// No Editor RefMut crosses the native Common Item Dialog / ChooseColor nested loop.
unsafe fn run_picker(hwnd: HWND, raw: *mut RefCell<Editor>, request: PickerRequest) {
    unsafe {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            selectors::pick(hwnd, request.kind, &request.initial)
        }))
        .unwrap_or_else(|_| Err(anyhow!("Native selector did not complete")));
        let current =
            if IsWindow(hwnd) != 0 && GetWindowLongPtrW(hwnd, GWLP_USERDATA) == raw as isize {
                Some(raw as usize)
            } else {
                None
            };
        if current.is_none() {
            return;
        }
        let mut failure = None;
        let mut close = false;
        if let Ok(mut editor) = (*raw).try_borrow_mut() {
            editor.modal_active = false;
            close = std::mem::take(&mut editor.close_after_modal);
            match result {
                Ok(selected) => {
                    if !close {
                        if let Some(selected) =
                            selectors::completion(raw as usize, current, selected)
                        {
                            let value = selected_draft(
                                &editor.value(&request.key),
                                selected,
                                request.append,
                            );
                            editor.drafts.insert(request.key, value);
                            editor.build();
                        }
                    }
                }
                Err(error) => {
                    let error = format!("{error:#}");
                    text(editor.status, &error);
                    failure = Some(error);
                }
            }
        }
        if close {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        } else if let Some(error) = failure {
            if IsWindow(hwnd) != 0 && GetWindowLongPtrW(hwnd, GWLP_USERDATA) == raw as isize {
                message(hwnd, "选择未完成", &error, true);
            }
        }
    }
}
unsafe fn run_confirmed_submit(hwnd: HWND, raw: *mut RefCell<Editor>, request: ConfirmedSubmit) {
    unsafe {
        // Product confirmation runs outside the Editor RefMut, just like native pickers.
        let accepted = confirm(hwnd, tr_static(request.message));
        if IsWindow(hwnd) == 0 || GetWindowLongPtrW(hwnd, GWLP_USERDATA) != raw as isize {
            return;
        }
        let mut close = false;
        let mut failure = None;
        if let Ok(mut editor) = (*raw).try_borrow_mut() {
            editor.modal_active = false;
            close = std::mem::take(&mut editor.close_after_modal);
            if accepted && !close {
                if let Err(error) = editor.submit(request.command, request.payload, true, None) {
                    let error = format!("{error:#}");
                    text(editor.status, &error);
                    failure = Some(error);
                }
            }
        }
        if close {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        } else if let Some(error) = failure {
            if IsWindow(hwnd) != 0 && GetWindowLongPtrW(hwnd, GWLP_USERDATA) == raw as isize {
                message(hwnd, "保存未完成", &error, true);
            }
        }
    }
}
fn first_language_payload(language: &Value) -> Result<Value> {
    let language = language
        .as_str()
        .filter(|language| ["en", "zh-Hans"].contains(language))
        .ok_or_else(|| anyhow!("{}", tr_static("请选择已提供的界面语言。")))?;
    Ok(json!({"appearance.language":language}))
}
fn startup_arguments_snapshot(bridge: &UiBridge) -> Value {
    bridge
        .model
        .lock()
        .ok()
        .and_then(|model| model.values.get("startup.arguments").cloned())
        .unwrap_or(Value::Null)
}
fn arguments_state(snapshot: &Value) -> Result<Value> {
    if !snapshot.get("minimized").is_some_and(Value::is_boolean)
        || !snapshot.get("read_only").is_some_and(Value::is_boolean)
        || !snapshot
            .get("inert_legacy_count")
            .is_some_and(Value::is_u64)
        || !snapshot
            .get("revision")
            .and_then(Value::as_str)
            .is_some_and(|revision| !revision.is_empty() && revision.len() <= 128)
    {
        bail!("{}", tr_static("启动参数回执不完整；当前内容未保存。"));
    }
    Ok(
        json!({"loaded":true,"minimized":snapshot["minimized"],"read_only":snapshot["read_only"],"inert_legacy_count":snapshot["inert_legacy_count"],"revision":snapshot["revision"]}),
    )
}
fn arguments_payload(data: &Value, drafts: &BTreeMap<String, Value>) -> Result<Value> {
    if data.get("loaded").and_then(Value::as_bool) != Some(true) {
        bail!("{}", tr_static("请先成功读取启动参数。"));
    }
    let read = |key: &str| {
        drafts
            .get(key)
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow!("{}", tr_static("启动参数选项无效。")))
    };
    let revision = data
        .get("revision")
        .and_then(Value::as_str)
        .filter(|revision| !revision.is_empty())
        .ok_or_else(|| anyhow!("{}", tr_static("启动参数版本缺失，请重新读取。")))?;
    Ok(
        json!({"minimized":read("startup.minimized")?,"read_only":read("startup.read_only")?,"expected_revision":revision}),
    )
}
fn arguments_need_confirmation(data: &Value, payload: &Value) -> bool {
    data.get("read_only").and_then(Value::as_bool) == Some(true)
        && payload.get("read_only").and_then(Value::as_bool) == Some(false)
}
fn selected_draft(current: &Value, selected: String, append: bool) -> Value {
    if !append {
        return json!(selected);
    }
    let mut items = current.as_array().cloned().unwrap_or_default();
    if !items.iter().any(|value| {
        value
            .as_str()
            .is_some_and(|text| text.eq_ignore_ascii_case(&selected))
    }) {
        items.push(json!(selected));
    }
    json!(items)
}
fn picker_kind(key: &str, action: &str) -> Option<PickerKind> {
    Some(match key {
        "script.command" => PickerKind::Open(FileFilter::Executable),
        "script.directory" | "action.working_directory" | "backup.path" => PickerKind::Folder,
        "spectrum.path" => PickerKind::Open(FileFilter::Spectrum),
        "spectrum.color" => PickerKind::Color,
        "action.path" => PickerKind::Open(match action {
            "run" => FileFilter::Executable,
            "play_sound" => FileFilter::Audio,
            "spectrum_import" => FileFilter::Spectrum,
            _ => return None,
        }),
        "/IncludedProcesses" | "/ExcludedProcesses" => PickerKind::Open(FileFilter::Executable),
        _ if key.ends_with("Color")
            || key.ends_with("/ColorOverrides")
            || key.ends_with("/TextColorOverrides") =>
        {
            PickerKind::Color
        }
        _ => return None,
    })
}
unsafe fn editor_message(e: &mut Editor, msg: u32, wp: usize, lp: isize) -> Option<isize> {
    unsafe {
        match msg {
            WM_CREATE => {
                e.build();
                SetTimer(e.hwnd, 1, 200, None);
                Some(0)
            }
            WM_SIZE => {
                if !e.rendering {
                    e.capture();
                    e.build();
                }
                Some(0)
            }
            WM_GETMINMAXINFO => {
                let info = &mut *(lp as *mut MINMAXINFO);
                info.ptMinTrackSize.x = e.s(550);
                info.ptMinTrackSize.y = e.s(430);
                Some(0)
            }
            WM_COMMAND => {
                if !e.rendering {
                    let id = (wp & 0xffff) as u16;
                    let notification = ((wp >> 16) & 0xffff) as u16;
                    if let Err(error) = e.command(id, notification) {
                        text(e.status, &format!("{error:#}"));
                        message(e.hwnd, "请检查输入", &format!("{error:#}"), true);
                    }
                }
                Some(0)
            }
            WM_HSCROLL => {
                if !e.rendering {
                    for slider in &e.sliders {
                        if slider.slider == lp as HWND {
                            let raw =
                                SendMessageW(slider.slider, TRACKBAR_GET_POSITION, 0, 0) as i32;
                            let value = slider.range.minimum
                                + (raw - slider.range.minimum) / slider.range.step.max(1)
                                    * slider.range.step.max(1);
                            text(slider.edit, &value.to_string());
                            e.drafts.insert(slider.key.clone(), json!(value));
                        }
                    }
                }
                Some(0)
            }
            WM_VSCROLL => {
                let code = (wp & 0xffff) as i32;
                let old = e.scroll;
                e.scroll = match code {
                    SB_LINEUP => old - 32,
                    SB_LINEDOWN => old + 32,
                    SB_PAGEUP => old - 220,
                    SB_PAGEDOWN => old + 220,
                    SB_THUMBPOSITION | SB_THUMBTRACK => {
                        let mut info: SCROLLINFO = zeroed();
                        info.cbSize = size_of::<SCROLLINFO>() as u32;
                        info.fMask = SIF_TRACKPOS;
                        GetScrollInfo(e.hwnd, SB_VERT, &mut info);
                        info.nTrackPos
                    }
                    _ => old,
                };
                e.position();
                Some(0)
            }
            WM_MOUSEWHEEL => {
                e.scroll -= ((wp >> 16) as u16 as i16 as i32) / 120 * 64;
                e.position();
                Some(0)
            }
            WM_TIMER => {
                e.poll();
                Some(0)
            }
            WM_DRAWITEM => {
                paint_button(&*(lp as *const DRAWITEMSTRUCT), false);
                Some(1)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
                SetTextColor(
                    wp as HDC,
                    if lp as HWND == e.status {
                        muted()
                    } else {
                        ink()
                    },
                );
                SetBkMode(wp as HDC, TRANSPARENT as i32);
                SetBkColor(wp as HDC, paper());
                Some(e.brush as isize)
            }
            WM_ERASEBKGND => {
                let mut rect: RECT = zeroed();
                GetClientRect(e.hwnd, &mut rect);
                FillRect(wp as HDC, &rect, e.brush);
                Some(1)
            }
            WM_CLOSE => {
                e.close();
                Some(0)
            }
            WM_DESTROY => {
                KillTimer(e.hwnd, 1);
                Some(0)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(e.hwnd, GWLP_USERDATA, 0);
                Some(DefWindowProcW(e.hwnd, msg, wp, lp))
            }
            _ => None,
        }
    }
}

impl Editor {
    fn s(&self, v: i32) -> i32 {
        v * self.dpi as i32 / 96
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
    fn value(&self, key: &str) -> Value {
        self.drafts.get(key).cloned().unwrap_or(Value::Null)
    }
    fn seed(&mut self) {
        let model = snapshot(&self.bridge);
        self.drafts.insert(
            "name".into(),
            self.data.get("name").cloned().unwrap_or_else(|| json!("")),
        );
        match self.mode {
            Mode::Preset => {
                for (key, _, _) in PRESET_FIELDS {
                    let value = self
                        .data
                        .get("values")
                        .and_then(|v| v.get(*key))
                        .cloned()
                        .unwrap_or_else(|| model.value(key));
                    self.drafts.insert((*key).into(), value);
                }
                self.drafts.insert(
                    "fan.full_speed".into(),
                    self.data
                        .pointer("/fan/full_speed")
                        .cloned()
                        .unwrap_or_else(|| model.value("fan.full_speed")),
                );
                if let Some(table) = self
                    .data
                    .pointer("/fan/table")
                    .or_else(|| model.values.get("fan.table"))
                    .and_then(Value::as_array)
                {
                    for (index, v) in table.iter().enumerate() {
                        self.drafts.insert(format!("fan.point.{index}"), v.clone());
                    }
                }
                for key in ["power.plan", "power.mode_ac", "power.mode_dc"] {
                    self.drafts.insert(
                        key.into(),
                        self.data
                            .get("overrides")
                            .and_then(|v| v.get(key))
                            .cloned()
                            .unwrap_or_else(|| model.value(key)),
                    );
                }
            }
            Mode::Automation => {
                self.drafts.insert(
                    "icon".into(),
                    self.data.get("icon").cloned().unwrap_or_else(|| json!("")),
                );
                self.drafts
                    .insert("catalog.action_search".into(), json!(""));
                for (key, default) in [
                    ("enabled", json!(true)),
                    ("exclusive", json!(false)),
                    ("run_on_startup", json!(false)),
                ] {
                    self.drafts
                        .insert(key.into(), self.data.get(key).cloned().unwrap_or(default));
                }
                let logic = self
                    .data
                    .get("trigger")
                    .and_then(|t| t.get("type"))
                    .and_then(Value::as_str)
                    .filter(|s| *s == "all" || *s == "any")
                    .unwrap_or("all");
                self.drafts.insert("match_logic".into(), json!(logic));
                self.load_action();
                self.load_condition();
            }
            Mode::Macro => {
                for (key, default) in [
                    ("trigger_key", json!("NumPad0")),
                    ("repeat", json!(1)),
                    ("ignore_delays", json!(false)),
                    ("interrupt_on_other_key", json!(true)),
                ] {
                    self.drafts
                        .insert(key.into(), self.data.get(key).cloned().unwrap_or(default));
                }
                for (key, value) in [
                    ("event.key", json!(65)),
                    ("event.delay_ms", json!(50)),
                    ("event.x", json!(0)),
                    ("event.y", json!(0)),
                ] {
                    self.drafts.insert(key.into(), value);
                }
                self.drafts.insert("record.mode".into(), json!("keyboard"));
            }
            Mode::SensorSettings => {
                for (key, _) in SENSOR_FIELDS {
                    self.drafts.insert((*key).into(), model.value(key));
                }
                self.data = Value::Object(
                    THERMAL_FIELDS
                        .iter()
                        .map(|key| ((*key).into(), model.value(key)))
                        .collect(),
                );
            }
            Mode::Form(ref id) => {
                if let Some(form) = forms::schema(id, &model) {
                    self.drafts.extend(forms::seed(&form, &self.data));
                }
            }
            Mode::Layout(ref id) => {
                let sensor = id == "sensor_dashboard";
                if !self.data.get("Groups").is_some_and(Value::is_array) {
                    self.steps = layout::defaults(sensor);
                }
                self.selected_step = (!self.steps.is_empty()).then_some(0);
                self.conditions = self
                    .data
                    .get("VisibleItems")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_else(|| {
                        layout::items(sensor).into_iter().map(|c| c.value).collect()
                    });
                if let Some(form) = forms::schema(id, &model) {
                    self.drafts.extend(forms::seed(&form, &self.data));
                }
                self.load_group();
            }
            Mode::Spectrum => {
                self.drafts.insert(
                    "spectrum.profile".into(),
                    self.data
                        .get("Profile")
                        .filter(|v| !v.is_null())
                        .cloned()
                        .unwrap_or_else(|| json!(1)),
                );
                self.drafts.insert("spectrum.path".into(), json!(""));
                self.selected_step = (!self.steps.is_empty()).then_some(0);
                self.load_spectrum_effect();
            }
            Mode::Hotkeys => {
                let code = model
                    .collections
                    .get("special_keys")
                    .and_then(|items| items.first())
                    .and_then(|i| i.id.parse::<i64>().ok())
                    .or_else(|| {
                        self.data
                            .get("KeyDescriptions")
                            .and_then(Value::as_object)
                            .and_then(|m| m.keys().find_map(|s| s.parse().ok()))
                    });
                self.drafts
                    .insert("key.code".into(), json!(code.unwrap_or(1)));
                self.load_hotkey(code);
            }
            Mode::Backup => {
                self.drafts.insert("backup.path".into(), json!(""));
                self.drafts
                    .insert("backup.include_macros".into(), json!(true));
                self.drafts
                    .insert("backup.include_automation".into(), json!(true));
            }
            Mode::Document(_) => {}
            Mode::FirstLanguage => {
                let language = model.value("appearance.language");
                let english = language.as_str().is_some_and(|language| {
                    ["en", "en-US", "en-GB"]
                        .iter()
                        .any(|known| known.eq_ignore_ascii_case(language))
                });
                self.drafts.insert(
                    "appearance.language".into(),
                    json!(if english { "en" } else { "zh-Hans" }),
                );
            }
            Mode::Arguments => {
                self.drafts.insert("startup.minimized".into(), json!(false));
                self.drafts.insert("startup.read_only".into(), json!(false));
                self.pending = self
                    .data
                    .get("load_request")
                    .and_then(Value::as_u64)
                    .map(|id| Pending {
                        id,
                        close: false,
                        recording: None,
                    });
            }
            Mode::Console => {
                for key in ["script.command", "script.arguments", "script.directory"] {
                    self.drafts.insert(key.into(), json!(""));
                }
            }
        }
    }
    unsafe fn place(
        &mut self,
        class: &str,
        label: &str,
        style: u32,
        id: u16,
        mut rect: [i32; 4],
        font: HFONT,
    ) -> HWND {
        unsafe {
            rect[1] += self.layout_extra;
            let hwnd = create(
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
            self.placements.push(Placement {
                hwnd,
                rect,
                clip_height: if class == "COMBOBOX" { 30 } else { rect[3] },
            });
            hwnd
        }
    }
    unsafe fn label(
        &mut self,
        label: &str,
        y: i32,
        width: i32,
        height: i32,
        heading: bool,
    ) -> HWND {
        unsafe { self.raw_label(&tr(label), y, width, height, heading) }
    }
    // Backend errors, user-defined labels and hardware names bypass translation.
    unsafe fn raw_label(
        &mut self,
        label: &str,
        y: i32,
        width: i32,
        height: i32,
        heading: bool,
    ) -> HWND {
        unsafe {
            self.place(
                "STATIC",
                label,
                SS_LEFT as u32,
                0,
                [20, y, width, height],
                if heading { self.heading } else { self.font },
            )
        }
    }
    unsafe fn button(
        &mut self,
        label: &str,
        id: u16,
        x: i32,
        y: i32,
        width: i32,
        capability: &str,
    ) -> HWND {
        unsafe {
            let available = (self.size().0 - 20).max(260);
            let width = width.min(available - 20);
            if self.flow_y != Some(y) {
                self.flow_y = Some(y);
                self.flow_next_x = x;
                self.flow_wrapped = false;
            }
            let mut actual_x = if self.flow_wrapped {
                self.flow_next_x
            } else {
                x
            };
            if actual_x + width > available {
                self.layout_extra += 42;
                actual_x = 20;
                self.flow_wrapped = true;
            }
            self.flow_next_x = actual_x + width + 12;
            let h = self.place(
                "BUTTON",
                &tr(label),
                BS_OWNERDRAW as u32 | WS_TABSTOP,
                id,
                [actual_x, y, width, 32],
                self.font,
            );
            if !capability.is_empty() {
                EnableWindow(h, live_snapshot(&self.bridge).can(capability) as i32);
                self.body_buttons.push((h, capability.into()));
            }
            h
        }
    }
    unsafe fn input(&mut self, key: &str, label: &str, kind: Kind, y: i32, width: i32) -> HWND {
        unsafe {
            let editor_x = if width < 650 { 205 } else { 240 };
            self.label(label, y + 3, editor_x - 40, 34, false);
            let id = 1000 + self.edits.len() as u16;
            let value = self.value(key);
            let picker = if matches!(&kind, Kind::Text | Kind::Lines) {
                picker_kind(key, &self.action_type)
            } else {
                None
            };
            let editor_width =
                (width - editor_x - 20 - if picker.is_some() { 84 } else { 0 }).max(96);
            let h = match &kind {
                Kind::Check => {
                    let h = self.place(
                        "BUTTON",
                        "",
                        BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
                        id,
                        [editor_x, y, editor_width, 30],
                        self.font,
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
                    h
                }
                Kind::Choice(options) => {
                    let h = self.place(
                        "COMBOBOX",
                        "",
                        CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
                        id,
                        [editor_x, y, editor_width, 300],
                        self.font,
                    );
                    set_choices(h, options, &value);
                    h
                }
                Kind::Lines => {
                    let lines = value
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("\r\n")
                        })
                        .unwrap_or_default();
                    self.place(
                        "EDIT",
                        &lines,
                        WS_BORDER
                            | WS_TABSTOP
                            | WS_VSCROLL
                            | ES_MULTILINE as u32
                            | ES_AUTOVSCROLL as u32
                            | ES_WANTRETURN as u32,
                        id,
                        [editor_x, y, editor_width, 132],
                        self.font,
                    )
                }
                Kind::MultiChoice(options) => {
                    let h = self.place(
                        "LISTBOX",
                        "",
                        WS_BORDER | WS_TABSTOP | WS_VSCROLL | LBS_EXTENDEDSEL as u32,
                        id,
                        [editor_x, y, editor_width, 160],
                        self.font,
                    );
                    for (index, option) in options.iter().enumerate() {
                        SendMessageW(h, LB_ADDSTRING, 0, wide(&option.label).as_ptr() as isize);
                        if value.as_array().is_some_and(|a| a.contains(&option.value)) {
                            SendMessageW(h, LB_SETSEL, 1, index as isize);
                        }
                    }
                    if options.is_empty() {
                        EnableWindow(h, 0);
                    }
                    h
                }
                Kind::Text | Kind::Number => {
                    let h = self.place(
                        "EDIT",
                        &scalar(&value),
                        WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32,
                        id,
                        [editor_x, y, editor_width, 30],
                        self.font,
                    );
                    SendMessageW(h, EM_SETLIMITTEXT, 65536, 0);
                    h
                }
            };
            if let Some(picker) = picker {
                let browse_id = 5000 + self.picker_buttons.len() as u16;
                let button = self.place(
                    "BUTTON",
                    tr_static("选择…"),
                    BS_OWNERDRAW as u32 | WS_TABSTOP,
                    browse_id,
                    [editor_x + editor_width + 8, y, 76, 30],
                    self.font,
                );
                EnableWindow(
                    button,
                    (!self.modal_active && self.pending.is_none()) as i32,
                );
                self.picker_buttons.insert(
                    browse_id,
                    (key.into(), picker, matches!(&kind, Kind::Lines)),
                );
            }
            self.edits.push(Edit {
                key: key.into(),
                hwnd: h,
                kind,
            });
            h
        }
    }
    unsafe fn build(&mut self) {
        unsafe {
            self.rendering = true;
            self.layout_extra = 0;
            self.flow_y = None;
            self.flow_next_x = 20;
            self.flow_wrapped = false;
            for p in self.placements.drain(..) {
                DestroyWindow(p.hwnd);
            }
            self.edits.clear();
            self.picker_buttons.clear();
            self.sliders.clear();
            self.body_buttons.clear();
            for h in [self.status, self.save, self.cancel] {
                if !h.is_null() {
                    DestroyWindow(h);
                }
            }
            let (width, height) = self.size();
            let body_width = (width - 40).max(470);
            let model = snapshot(&self.bridge);
            let mut y = 18;
            match self.mode.clone() {
                Mode::Document(id) => {
                    let document = model.documents.get(&id);
                    let title = document.map(|d| d.title.as_str()).unwrap_or("高级配置");
                    self.raw_label(title, y, body_width, 32, true);
                    y += 42;
                    self.label(
                        "高级入口：先检查内容再保存。普通预设、规则与宏请使用结构化编辑器。",
                        y,
                        body_width,
                        42,
                        false,
                    );
                    y += 48;
                    let value = self
                        .drafts
                        .get("document.text")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            serde_json::to_string_pretty(&self.data).unwrap_or_default()
                        });
                    let h = self.place(
                        "EDIT",
                        &value,
                        WS_BORDER
                            | WS_TABSTOP
                            | WS_VSCROLL
                            | WS_HSCROLL
                            | ES_MULTILINE as u32
                            | ES_AUTOVSCROLL as u32
                            | ES_AUTOHSCROLL as u32
                            | ES_WANTRETURN as u32,
                        900,
                        [20, y, body_width, (height - y - 112).max(210)],
                        self.font,
                    );
                    SendMessageW(h, EM_SETLIMITTEXT, 4 * 1024 * 1024, 0);
                    self.edits.push(Edit {
                        key: "document.text".into(),
                        hwnd: h,
                        kind: Kind::Text,
                    });
                    if let Some(d) = document {
                        if !d.writable {
                            SendMessageW(h, EM_SETREADONLY, 1, 0);
                            self.raw_label(
                                &d.reason,
                                y + (height - y - 112).max(210) + 8,
                                body_width,
                                36,
                                false,
                            );
                        }
                    }
                    y = height - 90;
                }
                Mode::Preset => {
                    y = self.preset_form(body_width, y, &model);
                }
                Mode::Automation => {
                    y = self.automation_form(body_width, y, &model);
                }
                Mode::Macro => {
                    y = self.macro_form(body_width, y, &model);
                }
                Mode::SensorSettings => {
                    y = self.sensor_form(body_width, y, &model);
                }
                Mode::Form(id) => {
                    y = self.settings_form(&id, body_width, y, &model);
                }
                Mode::Backup => {
                    y = self.backup_form(body_width, y);
                }
                Mode::Layout(id) => {
                    y = self.layout_form(&id, body_width, y, &model);
                }
                Mode::Hotkeys => {
                    y = self.hotkey_form(body_width, y, &model);
                }
                Mode::Spectrum => {
                    y = self.spectrum_form(body_width, y, &model);
                }
                Mode::Arguments => {
                    y = self.arguments_form(body_width, y);
                }
                Mode::FirstLanguage => {
                    self.label("选择语言", y, body_width, 32, true);
                    y += 42;
                    self.label(
                        "选择界面语言。取消将使用当前默认，不保存选择。",
                        y,
                        body_width,
                        55,
                        false,
                    );
                    y += 62;
                    self.input(
                        "appearance.language",
                        "语言",
                        Kind::Choice(vec![
                            UiChoice::new(tr_static("简体中文"), "zh-Hans"),
                            UiChoice::new("English", "en"),
                        ]),
                        y,
                        body_width,
                    );
                    y += 48;
                }
                Mode::Console => {
                    self.label(
                        "明确选择解释器或可执行文件；参数原样传递，不拼接命令行 shell。",
                        y,
                        body_width,
                        45,
                        false,
                    );
                    y += 52;
                    for (key, label) in [
                        ("script.command", "可执行文件完整路径"),
                        ("script.arguments", "参数"),
                        ("script.directory", "工作目录（可选）"),
                    ] {
                        self.input(key, label, Kind::Text, y, body_width);
                        y += 44;
                    }
                    self.button("停止等待", 64, 20, y, 130, "script.stop");
                    y += 44;
                    self.place(
                        "EDIT",
                        &self.script_output(),
                        WS_BORDER
                            | WS_VSCROLL
                            | ES_MULTILINE as u32
                            | ES_AUTOVSCROLL as u32
                            | ES_READONLY as u32,
                        63,
                        [20, y, body_width, 210],
                        self.font,
                    );
                    y += 222;
                }
            }
            self.content_height = y + 24 + self.layout_extra;
            self.status = create(
                self.hwnd,
                "STATIC",
                "",
                SS_LEFT as u32,
                0,
                [
                    self.s(20),
                    self.s(height - 80),
                    self.s((width - 270).max(230)),
                    self.s(64),
                ],
                self.font,
            );
            self.save = create(
                self.hwnd,
                "BUTTON",
                tr_static("保存"),
                BS_OWNERDRAW as u32 | WS_TABSTOP,
                SAVE,
                [
                    self.s(width - 236),
                    self.s(height - 52),
                    self.s(98),
                    self.s(32),
                ],
                self.font,
            );
            self.cancel = create(
                self.hwnd,
                "BUTTON",
                tr_static("取消"),
                BS_OWNERDRAW as u32 | WS_TABSTOP,
                CANCEL,
                [
                    self.s(width - 126),
                    self.s(height - 52),
                    self.s(98),
                    self.s(32),
                ],
                self.font,
            );
            if matches!(self.mode, Mode::FirstLanguage) {
                text(self.save, tr_static("应用"));
            }
            if matches!(self.mode, Mode::Console) {
                text(self.save, "运行");
                text(self.cancel, "关闭");
            }
            let capability = if matches!(self.mode, Mode::SensorSettings)
                && model.can("sensors.settings.save")
            {
                "sensors.settings.save"
            } else {
                self.save_capability()
            };
            let writable = match &self.mode {
                Mode::Document(id) => model.documents.get(id).is_some_and(|d| d.writable),
                Mode::Form(id) => forms::schema(id, &model).is_some_and(|f| {
                    model.can("settings.fields.save")
                        && model.documents.get(f.document).is_some_and(|d| d.writable)
                }),
                Mode::Layout(id) => {
                    model.can("settings.fields.save")
                        && model.documents.get(id).is_some_and(|d| d.writable)
                }
                Mode::Hotkeys => {
                    model.can("settings.fields.save")
                        && model
                            .documents
                            .get("special_keys")
                            .is_some_and(|d| d.writable)
                }
                Mode::Arguments => {
                    model.can("startup.arguments.save")
                        && self.data.get("loaded").and_then(Value::as_bool) == Some(true)
                }
                Mode::Spectrum => {
                    model.can("keyboard.effects.apply")
                        && self.data.get("Loaded").and_then(Value::as_bool) == Some(true)
                }
                Mode::Backup => false,
                _ => model.can(capability),
            };
            EnableWindow(
                self.save,
                (writable
                    && self.pending.is_none()
                    && (!matches!(self.mode, Mode::Macro)
                        || (!self.recording && !self.recording_stop.busy())))
                    as i32,
            );
            if !writable {
                text(
                    self.status,
                    &match &self.mode {
                        Mode::Document(id) => model
                            .documents
                            .get(id)
                            .map(|d| d.reason.clone())
                            .unwrap_or_else(|| "配置不可用".into()),
                        Mode::Form(id) => forms::schema(id, &model)
                            .and_then(|f| model.documents.get(f.document))
                            .filter(|d| !d.writable)
                            .map(|d| d.reason.clone())
                            .unwrap_or_else(|| model.unavailable_reason("settings.fields.save")),
                        Mode::Backup => "选择备份文件路径，再点击备份或还原。".into(),
                        Mode::Arguments => {
                            tr_static("启动参数尚未成功读取，请等待或重新读取。").into()
                        }
                        _ => model.unavailable_reason(capability),
                    },
                );
            }
            self.position();
            self.rendering = false;
        }
    }
    fn save_capability(&self) -> &'static str {
        match self.mode {
            Mode::Preset => "preset.save",
            Mode::Automation => "automation.save",
            Mode::Macro => "macro.save",
            Mode::SensorSettings => "thermal.apply",
            Mode::Document(_) => "",
            Mode::Form(_) | Mode::Layout(_) | Mode::Hotkeys => "settings.fields.save",
            Mode::Backup => "",
            Mode::Spectrum => "keyboard.effects.apply",
            Mode::Console => "script.run",
            Mode::Arguments => "startup.arguments.save",
            Mode::FirstLanguage => "app.preferences.save",
        }
    }
    unsafe fn position(&mut self) {
        unsafe {
            let (_, height) = self.size();
            let viewport = (height - 120).max(200);
            let max = (self.content_height - viewport).max(0);
            self.scroll = self.scroll.clamp(0, max);
            let si = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: self.content_height,
                nPage: viewport as u32,
                nPos: self.scroll,
                nTrackPos: 0,
            };
            SetScrollInfo(self.hwnd, SB_VERT, &si, 1);
            for p in &self.placements {
                let r = p.rect;
                let y = r[1] - self.scroll;
                SetWindowPos(
                    p.hwnd,
                    null_mut(),
                    self.s(r[0]),
                    self.s(y),
                    self.s(r[2]),
                    self.s(r[3]),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                ShowWindow(
                    p.hwnd,
                    if y >= 12 && y + p.clip_height <= height - 94 {
                        SW_SHOWNA
                    } else {
                        SW_HIDE
                    },
                );
            }
        }
    }
    unsafe fn capture(&mut self) {
        unsafe {
            for edit in &self.edits {
                if IsWindowEnabled(edit.hwnd) == 0 {
                    continue;
                }
                let value = match &edit.kind {
                    Kind::Check => {
                        json!(SendMessageW(edit.hwnd, BM_GETCHECK, 0, 0) == BST_CHECKED as isize)
                    }
                    Kind::Choice(options) => {
                        let index = SendMessageW(edit.hwnd, CB_GETCURSEL, 0, 0);
                        if index >= 0 {
                            options
                                .get(index as usize)
                                .map(|o| o.value.clone())
                                .unwrap_or(Value::Null)
                        } else {
                            Value::Null
                        }
                    }
                    Kind::Number => {
                        let value = get_text(edit.hwnd);
                        serde_json::from_str::<Value>(&value)
                            .ok()
                            .filter(Value::is_number)
                            .unwrap_or_else(|| json!(value))
                    }
                    Kind::Lines => json!(
                        get_text(edit.hwnd)
                            .lines()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                    ),
                    Kind::MultiChoice(options) => json!(
                        options
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| SendMessageW(edit.hwnd, LB_GETSEL, *index, 0) > 0)
                            .map(|(_, o)| o.value.clone())
                            .collect::<Vec<_>>()
                    ),
                    Kind::Text => json!(get_text(edit.hwnd)),
                };
                self.drafts.insert(edit.key.clone(), value);
            }
        }
    }
    unsafe fn settings_form(&mut self, id: &str, width: i32, mut y: i32, model: &UiModel) -> i32 {
        unsafe {
            let Some(form) = forms::schema(id, model) else {
                return y;
            };
            self.label(form.title, y, width, 32, true);
            y += 42;
            if form.sections.len() > 1 {
                let combo = self.place(
                    "COMBOBOX",
                    "",
                    CBS_DROPDOWNLIST as u32 | WS_TABSTOP,
                    EDITOR_TAB,
                    [20, y, width, 260],
                    self.font,
                );
                for section in &form.sections {
                    SendMessageW(
                        combo,
                        CB_ADDSTRING,
                        0,
                        wide(section.title).as_ptr() as isize,
                    );
                }
                self.tab = self.tab.min(form.sections.len() - 1);
                SendMessageW(combo, CB_SETCURSEL, self.tab, 0);
                y += 44;
            }
            self.label("保存后生效。取消将保留原设置。", y, width, 34, false);
            y += 42;
            if id == "notifications" && self.tab == 2 {
                return self.notification_form(width, y);
            }
            for f in &form.sections[self.tab.min(form.sections.len() - 1)].fields {
                let label = if let Some((min, max)) = f.range {
                    format!("{} · {}–{}", f.label, min, max)
                } else {
                    f.label.clone()
                };
                self.input(&f.path, &label, f.kind.clone(), y, width);
                y += match f.kind {
                    Kind::Lines => 146,
                    Kind::MultiChoice(_) => 174,
                    _ => 42,
                };
            }
            y
        }
    }
    unsafe fn arguments_form(&mut self, width: i32, mut y: i32) -> i32 {
        unsafe {
            self.label("启动参数", y, width, 32, true);
            y += 42;
            self.label(
                "这些选项仅在下一次普通启动时生效；不会更改当前会话的只读状态。",
                y,
                width,
                55,
                false,
            );
            y += 62;
            let loaded = self.data.get("loaded").and_then(Value::as_bool) == Some(true);
            for (key, label) in [
                ("startup.minimized", "启动时最小化"),
                ("startup.read_only", "只读启动"),
            ] {
                let control = self.input(key, label, Kind::Check, y, width);
                EnableWindow(control, (loaded && self.pending.is_none()) as i32);
                y += 44;
            }
            if let Some(count) = self.data.get("inert_legacy_count").and_then(Value::as_u64) {
                self.raw_label(
                    &format_product(
                        "已保留 {} 条未知旧参数，原文不会执行或显示。",
                        &[&count.to_string()],
                    ),
                    y,
                    width,
                    55,
                    false,
                );
            } else {
                self.label(
                    "正在读取启动参数；不会显示未知参数原文。",
                    y,
                    width,
                    55,
                    false,
                );
            }
            y += 62;
            self.label(
                "保存前会核对读取版本；如果文件已被其他程序更改，请重新读取后再编辑。",
                y,
                width,
                55,
                false,
            );
            y += 62;
            self.button("重新读取", 148, 20, y, 130, "startup.arguments.load");
            y + 48
        }
    }
    unsafe fn notification_form(&mut self, width: i32, mut y: i32) -> i32 {
        unsafe {
            let options = forms::notification_types();
            let selected = self
                .drafts
                .get("@notification.selected_type")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(42) as usize;
            self.label("通知类型", y, width, 28, false);
            y += 34;
            let combo = self.place(
                "COMBOBOX",
                "",
                CBS_DROPDOWNLIST as u32 | WS_TABSTOP,
                144,
                [20, y, width, 300],
                self.font,
            );
            set_choices(combo, &options, &json!(selected));
            y += 44;
            self.label("留空或选择继承使用通用设置。图标编号保留旧值，原生界面使用类型标记并报告兼容情况。",y,width,55,false);
            y += 62;
            for f in forms::notification_fields(selected, &self.data) {
                self.drafts.entry(f.path.clone()).or_insert(f.default);
                self.input(&f.path, &f.label, f.kind, y, width);
                y += 44;
            }
            self.button("重置此类型", 145, 20, y, 156, "");
            y + 46
        }
    }
    fn load_hotkey(&mut self, code: Option<i64>) {
        self.key_code = code;
        self.selected_step = None;
        self.selected_condition = None;
        let key = code.map(|i| i.to_string()).unwrap_or_default();
        let read = |name: &str, default: Value| {
            self.key_changes
                .get(&format!("/{name}/{key}"))
                .cloned()
                .or_else(|| self.data.get(name).and_then(|o| o.get(&key)).cloned())
                .unwrap_or(default)
        };
        self.drafts
            .insert("key.name".into(), read("KeyDescriptions", json!("")));
        self.drafts.insert(
            "key.custom".into(),
            json!(read("KeyModes", json!("Default")) == "Custom"),
        );
        let hidden = self
            .key_changes
            .get("/HiddenKeys")
            .or_else(|| self.data.get("HiddenKeys"))
            .and_then(Value::as_array)
            .is_some_and(|a| code.is_some_and(|c| a.contains(&json!(c))));
        self.drafts.insert("key.hidden".into(), json!(hidden));
        self.steps = read("KeySinglePressActions", json!([]))
            .as_array()
            .cloned()
            .unwrap_or_default();
        self.conditions = read("KeyDoublePressActions", json!([]))
            .as_array()
            .cloned()
            .unwrap_or_default();
    }
    fn stash_hotkey(&mut self) {
        let Some(code) = self.key_code else {
            return;
        };
        let key = code.to_string();
        self.key_changes
            .insert(format!("/KeyDescriptions/{key}"), self.value("key.name"));
        self.key_changes.insert(
            format!("/KeyModes/{key}"),
            json!(if self.value("key.custom").as_bool().unwrap_or(false) {
                "Custom"
            } else {
                "Default"
            }),
        );
        self.key_changes
            .insert(format!("/KeySinglePressActions/{key}"), json!(self.steps));
        self.key_changes.insert(
            format!("/KeyDoublePressActions/{key}"),
            json!(self.conditions),
        );
        let mut hidden = self
            .key_changes
            .get("/HiddenKeys")
            .or_else(|| self.data.get("HiddenKeys"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        hidden.retain(|v| v != &json!(code));
        if self.value("key.hidden").as_bool().unwrap_or(false) {
            hidden.push(json!(code));
        }
        self.key_changes.insert("/HiddenKeys".into(), json!(hidden));
    }
    unsafe fn hotkey_form(&mut self, width: i32, mut y: i32, model: &UiModel) -> i32 {
        unsafe {
            self.label("自定义 Fn 快捷键", y, width, 32, true);
            y += 42;
            self.label(
                "选择已发现的按键，设置单击、双击的快捷操作。操作按列表顺序执行。",
                y,
                width,
                48,
                false,
            );
            y += 54;
            let mut keys = std::collections::BTreeMap::<i64, String>::new();
            if let Some(items) = model.collections.get("special_keys") {
                for item in items {
                    if let Ok(code) = item.id.parse() {
                        keys.insert(code, item.label.clone());
                    }
                }
            }
            if let Some(m) = self.data.get("KeyDescriptions").and_then(Value::as_object) {
                for (k, v) in m {
                    if let Ok(code) = k.parse() {
                        keys.entry(code)
                            .or_insert_with(|| v.as_str().unwrap_or("按键").to_owned());
                    }
                }
            }
            if let Some(code) = self.key_code {
                keys.entry(code).or_insert_with(|| format!("按键 {code}"));
            }
            self.action_labels = keys.keys().map(ToString::to_string).collect();
            let list = self.place(
                "LISTBOX",
                "",
                WS_BORDER | WS_VSCROLL | WS_TABSTOP | LBS_NOTIFY as u32,
                100,
                [20, y, width, 106],
                self.font,
            );
            for (index, (code, label)) in keys.iter().enumerate() {
                SendMessageW(
                    list,
                    LB_ADDSTRING,
                    0,
                    wide(&format!("{label} · {code}")).as_ptr() as isize,
                );
                if Some(*code) == self.key_code {
                    SendMessageW(list, LB_SETCURSEL, index, 0);
                }
            }
            y += 116;
            self.button("发现按键", 110, 20, y, 120, "keys.discover.start");
            self.button("停止发现", 111, 152, y, 120, "keys.discover.stop");
            self.button("重试清理", 119, 284, y, 120, "keys.retry_cleanup");
            y += 44;
            self.input("key.code", "高级：指定键码", Kind::Number, y, width);
            y += 40;
            self.button("载入此按键", 101, 20, y, 130, "");
            y += 44;
            self.input("key.name", "按键说明", Kind::Text, y, width);
            y += 42;
            self.input("key.custom", "使用自定义操作", Kind::Check, y, width);
            y += 42;
            self.input("key.hidden", "在按键列表中隐藏", Kind::Check, y, width);
            y += 42;
            let choices = model
                .collections
                .get("automation")
                .map(|items| {
                    items
                        .iter()
                        .map(|i| UiChoice::new(i.label.clone(), i.id.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if self.value("key.pipeline").is_null() {
                self.drafts.insert(
                    "key.pipeline".into(),
                    choices
                        .first()
                        .map(|c| c.value.clone())
                        .unwrap_or(Value::Null),
                );
            }
            self.input(
                "key.pipeline",
                "添加快捷操作",
                Kind::Choice(choices),
                y,
                width,
            );
            y += 44;
            for (double, list_id, add, remove, up, down) in [
                (false, 102, 104, 105, 106, 107),
                (true, 103, 114, 115, 116, 117),
            ] {
                self.label(
                    if double {
                        "双击操作"
                    } else {
                        "单击操作"
                    },
                    y,
                    width,
                    28,
                    true,
                );
                y += 34;
                let list = self.place(
                    "LISTBOX",
                    "",
                    WS_BORDER | WS_VSCROLL | WS_TABSTOP | LBS_NOTIFY as u32,
                    list_id,
                    [20, y, width, 100],
                    self.font,
                );
                for value in if double {
                    &self.conditions
                } else {
                    &self.steps
                } {
                    let name = model
                        .collections
                        .get("automation")
                        .and_then(|a| a.iter().find(|i| value.as_str() == Some(i.id.as_str())))
                        .map(|i| i.label.clone())
                        .unwrap_or_else(|| format!("不可用的快捷操作 · {}", scalar(value)));
                    SendMessageW(list, LB_ADDSTRING, 0, wide(&name).as_ptr() as isize);
                }
                if let Some(index) = if double {
                    self.selected_condition
                } else {
                    self.selected_step
                } {
                    SendMessageW(list, LB_SETCURSEL, index, 0);
                }
                y += 110;
                for (label, id, x, w) in [
                    ("添加", add, 20, 90),
                    ("移除", remove, 122, 90),
                    ("上移", up, 224, 90),
                    ("下移", down, 326, 90),
                ] {
                    self.button(label, id, x, y, w, "");
                }
                y += 44;
            }
            y
        }
    }
    unsafe fn hotkey_command(&mut self, id: u16, notification: u16) -> Result<()> {
        unsafe {
            match id {
                100 if notification == LBN_SELCHANGE as u16 => {
                    let index = SendMessageW(GetDlgItem(self.hwnd, 100), LB_GETCURSEL, 0, 0);
                    let code = (index >= 0)
                        .then(|| self.action_labels.get(index as usize))
                        .flatten()
                        .and_then(|s| s.parse().ok());
                    self.stash_hotkey();
                    self.load_hotkey(code);
                    if let Some(c) = code {
                        self.drafts.insert("key.code".into(), json!(c));
                    }
                }
                101 => {
                    let code = self.numeric("key.code")?;
                    if !(1..=i32::MAX as i64).contains(&code) {
                        bail!(
                            "{}",
                            tr_static("键码范围为1–2147483647，保留厂商扩展Fn事件编号。")
                        );
                    }
                    self.stash_hotkey();
                    self.load_hotkey(Some(code));
                }
                102 | 103 if notification == LBN_SELCHANGE as u16 => {
                    let index = SendMessageW(GetDlgItem(self.hwnd, id as i32), LB_GETCURSEL, 0, 0);
                    let selected = (index >= 0).then_some(index.max(0) as usize);
                    if id == 102 {
                        self.selected_step = selected;
                    } else {
                        self.selected_condition = selected;
                    }
                }
                110 | 111 => {
                    self.submit(
                        if id == 110 {
                            "keys.discover.start"
                        } else {
                            "keys.discover.stop"
                        },
                        json!({}),
                        false,
                        None,
                    )?;
                    return Ok(());
                }
                104..=107 | 114..=117 => {
                    let double = id >= 114;
                    let op = if double { id - 114 } else { id - 104 };
                    let value = self.value("key.pipeline");
                    let (list, selected) = if double {
                        (&mut self.conditions, &mut self.selected_condition)
                    } else {
                        (&mut self.steps, &mut self.selected_step)
                    };
                    if op == 0 {
                        if value.as_str().unwrap_or("").is_empty() {
                            bail!("{}", tr_static("请先选择快捷操作。"));
                        }
                        list.push(value);
                        *selected = Some(list.len() - 1);
                    } else {
                        let index = selected.ok_or_else(|| {
                            anyhow!("{}", tr_static("请先选择列表中的快捷操作。"))
                        })?;
                        if index >= list.len() {
                            bail!("{}", tr_static("列表已改变，请重新选择。"));
                        }
                        if op == 1 {
                            list.remove(index);
                            *selected = None;
                        } else {
                            let target = if op == 2 {
                                index.saturating_sub(1)
                            } else {
                                (index + 1).min(list.len() - 1)
                            };
                            list.swap(index, target);
                            *selected = Some(target);
                        }
                    }
                }
                _ => return Ok(()),
            }
            self.build();
            Ok(())
        }
    }
    fn load_group(&mut self) {
        let sensor = matches!(&self.mode,Mode::Layout(id)if id=="sensor_dashboard");
        let group = self.selected_step.and_then(|i| self.steps.get(i));
        self.drafts.insert(
            "layout.group.type".into(),
            group
                .and_then(|g| g.get("Type"))
                .cloned()
                .unwrap_or_else(|| json!(if sensor { "CPU" } else { "Custom" })),
        );
        self.drafts.insert(
            "layout.group.name".into(),
            group
                .and_then(|g| g.get("CustomName"))
                .cloned()
                .unwrap_or_else(|| json!("")),
        );
        self.drafts.insert(
            "layout.item".into(),
            layout::items(sensor)
                .first()
                .map(|i| i.value.clone())
                .unwrap_or(Value::Null),
        );
        self.selected_condition = None;
    }
    unsafe fn layout_form(&mut self, id: &str, width: i32, mut y: i32, model: &UiModel) -> i32 {
        unsafe {
            let sensor = id == "sensor_dashboard";
            self.label(
                if sensor {
                    "传感器分组"
                } else {
                    "自定义控制台"
                },
                y,
                width,
                32,
                true,
            );
            y += 42;
            for (key, label, kind) in [
                ("/ShowSensors", "显示传感器", Kind::Check),
                (
                    "/SensorsRefreshIntervalSeconds",
                    "刷新间隔 · 秒",
                    Kind::Number,
                ),
            ] {
                self.input(key, label, kind, y, width);
                y += 42;
            }
            self.label(
                "选择分组后增删或排序项目。改动仅在保存后生效。",
                y,
                width,
                38,
                false,
            );
            y += 44;
            let list = self.place(
                "LISTBOX",
                "",
                WS_BORDER | WS_VSCROLL | WS_TABSTOP | LBS_NOTIFY as u32,
                80,
                [20, y, width, 120],
                self.font,
            );
            for group in &self.steps {
                SendMessageW(
                    list,
                    LB_ADDSTRING,
                    0,
                    wide(&layout::group_label(group)).as_ptr() as isize,
                );
            }
            if let Some(i) = self.selected_step {
                SendMessageW(list, LB_SETCURSEL, i, 0);
            }
            y += 130;
            for (label, id, x, w) in [
                ("新建分组", 82, 20, 108),
                ("更新分组", 83, 140, 108),
                ("删除分组", 84, 260, 108),
                ("上移", 85, 380, 80),
                ("下移", 86, 472, 80),
            ] {
                self.button(label, id, x, y, w, "");
            }
            y += 42;
            self.input(
                "layout.group.type",
                "分组类别",
                Kind::Choice(layout::group_types(sensor)),
                y,
                width,
            );
            y += 42;
            if !sensor {
                self.input("layout.group.name", "自定义分组名称", Kind::Text, y, width);
                y += 42;
            }
            let list = self.place(
                "LISTBOX",
                "",
                WS_BORDER | WS_VSCROLL | WS_TABSTOP | LBS_NOTIFY as u32,
                81,
                [20, y, width, 150],
                self.font,
            );
            if let Some(items) = self
                .selected_step
                .and_then(|i| self.steps.get(i))
                .and_then(|g| g.get("Items"))
                .and_then(Value::as_array)
            {
                for item in items {
                    let shown = !sensor || self.conditions.contains(item);
                    SendMessageW(
                        list,
                        LB_ADDSTRING,
                        0,
                        wide(&format!(
                            "{}{}",
                            if shown { "" } else { tr_static("隐藏 · ") },
                            layout::item_label(item, sensor)
                        ))
                        .as_ptr() as isize,
                    );
                }
            }
            if let Some(i) = self.selected_condition {
                SendMessageW(list, LB_SETCURSEL, i, 0);
            }
            y += 160;
            self.input(
                "layout.item",
                "添加项目",
                Kind::Choice(layout::items(sensor)),
                y,
                width,
            );
            y += 42;
            for (label, id, x, w) in [
                ("添加", 87, 20, 86),
                ("移除", 88, 118, 86),
                ("上移", 89, 216, 86),
                ("下移", 90, 314, 86),
                ("显示/隐藏", 93, 412, 126),
            ] {
                if id != 93 || sensor {
                    self.button(label, id, x, y, w, "");
                }
            }
            y += 44;
            self.button("恢复原版分组", 91, 20, y, 160, "");
            y += 44;
            let _ = model;
            y
        }
    }
    unsafe fn layout_command(&mut self, id: u16, notification: u16) -> Result<()> {
        unsafe {
            let sensor = matches!(&self.mode,Mode::Layout(v)if v=="sensor_dashboard");
            match id {
                80 if notification == LBN_SELCHANGE as u16 => {
                    let i = SendMessageW(GetDlgItem(self.hwnd, 80), LB_GETCURSEL, 0, 0);
                    self.selected_step = (i >= 0).then_some(i.max(0) as usize);
                    self.load_group();
                }
                81 if notification == LBN_SELCHANGE as u16 => {
                    let i = SendMessageW(GetDlgItem(self.hwnd, 81), LB_GETCURSEL, 0, 0);
                    self.selected_condition = (i >= 0).then_some(i.max(0) as usize);
                }
                82 => {
                    self.steps.push(json!({"Type":if sensor{"CPU"}else{"Custom"},"CustomName":"新分组","Items":[]}));
                    self.selected_step = Some(self.steps.len() - 1);
                    self.load_group();
                }
                83 => {
                    let index = self
                        .selected_step
                        .ok_or_else(|| anyhow!("{}", tr_static("请先选择分组。")))?;
                    self.steps[index]["Type"] = self.value("layout.group.type");
                    self.steps[index]["CustomName"] = self.value("layout.group.name");
                }
                84 => {
                    let index = self
                        .selected_step
                        .ok_or_else(|| anyhow!("{}", tr_static("请先选择分组。")))?;
                    if confirm(self.hwnd, "移除此分组？保存前仍可取消。") {
                        self.steps.remove(index);
                        self.selected_step = (!self.steps.is_empty())
                            .then_some(index.min(self.steps.len().saturating_sub(1)));
                        self.load_group();
                    }
                }
                85 | 86 => {
                    let i = self
                        .selected_step
                        .ok_or_else(|| anyhow!("{}", tr_static("请先选择分组。")))?;
                    let target = if id == 85 {
                        i.saturating_sub(1)
                    } else {
                        (i + 1).min(self.steps.len() - 1)
                    };
                    self.steps.swap(i, target);
                    self.selected_step = Some(target);
                }
                87 => {
                    let i = self
                        .selected_step
                        .ok_or_else(|| anyhow!("{}", tr_static("请先选择分组。")))?;
                    let item = self.value("layout.item");
                    let items = self.steps[i]["Items"]
                        .as_array_mut()
                        .ok_or_else(|| anyhow!("{}", tr_static("分组项目无效。")))?;
                    if !items.contains(&item) {
                        items.push(item.clone());
                    }
                    if sensor && !self.conditions.contains(&item) {
                        self.conditions.push(item);
                    }
                }
                88 | 89 | 90 | 93 => {
                    let i = self
                        .selected_step
                        .ok_or_else(|| anyhow!("{}", tr_static("请先选择分组。")))?;
                    let item_index = self
                        .selected_condition
                        .ok_or_else(|| anyhow!("{}", tr_static("请先选择项目。")))?;
                    let items = self.steps[i]["Items"]
                        .as_array_mut()
                        .ok_or_else(|| anyhow!("{}", tr_static("分组项目无效。")))?;
                    if item_index >= items.len() {
                        bail!("{}", tr_static("项目已改变，请重新选择。"));
                    }
                    if id == 88 {
                        items.remove(item_index);
                        self.selected_condition = None;
                    } else if id == 93 {
                        let item = &items[item_index];
                        if let Some(i) = self.conditions.iter().position(|v| v == item) {
                            self.conditions.remove(i);
                        } else {
                            self.conditions.push(item.clone());
                        }
                    } else {
                        let target = if id == 89 {
                            item_index.saturating_sub(1)
                        } else {
                            (item_index + 1).min(items.len() - 1)
                        };
                        items.swap(item_index, target);
                        self.selected_condition = Some(target);
                    }
                }
                91 => {
                    if confirm(self.hwnd, "恢复原版分组顺序？保存前仍可取消。") {
                        self.steps = layout::defaults(sensor);
                        self.selected_step = Some(0);
                        self.conditions =
                            layout::items(sensor).into_iter().map(|c| c.value).collect();
                        self.load_group();
                    }
                }
                _ => return Ok(()),
            }
            self.build();
            Ok(())
        }
    }
    unsafe fn backup_form(&mut self, width: i32, mut y: i32) -> i32 {
        unsafe {
            self.label("备份与还原设置", y, width, 32, true);
            y += 42;
            self.label(
                "指定备份文件路径。还原前会先保留当前设置；原设备控制不会因导入而自动开启。",
                y,
                width,
                60,
                false,
            );
            y += 66;
            self.input(
                "backup.path",
                "备份目标目录 / 还原文件或旧备份目录",
                Kind::Text,
                y,
                width,
            );
            y += 44;
            self.button("选择还原文件…", 146, 20, y, 170, "");
            y += 44;
            self.input("backup.include_macros", "包含宏", Kind::Check, y, width);
            y += 42;
            self.input(
                "backup.include_automation",
                "包含自动化规则",
                Kind::Check,
                y,
                width,
            );
            y += 48;
            self.button("创建备份", 70, 20, y, 140, "settings.backup");
            self.button("还原备份…", 71, 174, y, 140, "settings.restore");
            y + 50
        }
    }
    unsafe fn preset_form(&mut self, width: i32, mut y: i32, model: &UiModel) -> i32 {
        unsafe {
            self.label("自定义模式设置", y, width, 32, true);
            y += 42;
            self.input("name", "预设名称", Kind::Text, y, width);
            y += 42;
            let tabs = ["CPU", "显卡", "风扇", "Windows 电源映射"];
            let combo = self.place(
                "COMBOBOX",
                "",
                CBS_DROPDOWNLIST as u32 | WS_TABSTOP,
                EDITOR_TAB,
                [20, y, width, 220],
                self.font,
            );
            for tab in tabs {
                SendMessageW(combo, CB_ADDSTRING, 0, wide(&tr(tab)).as_ptr() as isize);
            }
            SendMessageW(combo, CB_SETCURSEL, self.tab, 0);
            y += 48;
            self.label(
                "滑块范围来自本机能力；范围未知或不支持的值不会允许保存为硬件参数。",
                y,
                width,
                42,
                false,
            );
            y += 48;
            match self.tab {
                0 | 1 => {
                    for (key, label, tab) in PRESET_FIELDS {
                        if *tab != self.tab {
                            continue;
                        }
                        if let Some(options) = model.options.get(*key).filter(|options| {
                            !options.is_empty()
                                && options.iter().all(|o| o.value.as_i64().is_some())
                        }) {
                            self.input(key, label, Kind::Choice(options.clone()), y, width);
                            y += 46;
                            continue;
                        }
                        let range = model.ranges.get(*key).cloned();
                        let value = self.value(key);
                        let label = range
                            .as_ref()
                            .map(|r| {
                                format!("{} · {}–{} {}", tr(label), r.minimum, r.maximum, r.unit)
                            })
                            .unwrap_or_else(|| {
                                format!("{} · {}", tr(label), tr_static("范围未知"))
                            });
                        let editor_x = if width < 650 { 220 } else { 270 };
                        self.label(&label, y + 3, editor_x - 40, 34, false);
                        let edit = self.place(
                            "EDIT",
                            &scalar(&value),
                            WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32,
                            1000 + self.edits.len() as u16,
                            [editor_x, y, 100, 30],
                            self.font,
                        );
                        self.edits.push(Edit {
                            key: (*key).into(),
                            hwnd: edit,
                            kind: Kind::Number,
                        });
                        if let Some(range) = range.filter(|r| r.step > 0 && r.maximum >= r.minimum)
                        {
                            let slider = self.place(
                                "msctls_trackbar32",
                                "",
                                TBS_HORZ | TBS_AUTOTICKS | WS_TABSTOP,
                                1800 + self.sliders.len() as u16,
                                if width < 750 {
                                    [20, y + 36, width, 30]
                                } else {
                                    [editor_x + 116, y, (width - editor_x - 136).max(120), 30]
                                },
                                self.font,
                            );
                            SendMessageW(slider, TBM_SETRANGEMIN, 0, range.minimum as isize);
                            SendMessageW(slider, TBM_SETRANGEMAX, 1, range.maximum as isize);
                            SendMessageW(slider, TBM_SETPAGESIZE, 0, range.step.max(1) as isize);
                            SendMessageW(
                                slider,
                                TBM_SETPOS,
                                1,
                                value.as_i64().unwrap_or(range.minimum as i64) as isize,
                            );
                            self.sliders.push(Slider {
                                key: (*key).into(),
                                slider,
                                edit,
                                range,
                            });
                        } else {
                            EnableWindow(edit, 0);
                        }
                        y += if width < 750 { 80 } else { 46 };
                    }
                }
                2 => {
                    self.input("fan.full_speed", "风扇全速", Kind::Check, y, width);
                    y += 46;
                    let count = self
                        .drafts
                        .keys()
                        .filter(|k| k.starts_with("fan.point."))
                        .count();
                    if count == 0 {
                        self.label(
                            "尚无已验证的风扇曲线数据；保留固件当前风扇行为。",
                            y,
                            width,
                            48,
                            false,
                        );
                        y += 56;
                    } else {
                        self.label(
                            "按原曲线节点顺序编辑，后台校验节点数、范围与合法组合。",
                            y,
                            width,
                            42,
                            false,
                        );
                        y += 48;
                        for index in 0..count {
                            self.input(
                                &format!("fan.point.{index}"),
                                &tr_static("节点 {}").replacen("{}", &(index + 1).to_string(), 1),
                                Kind::Number,
                                y,
                                width,
                            );
                            y += 40;
                        }
                    }
                }
                _ => {
                    for (key, label) in [
                        ("power.plan", "Windows 电源计划"),
                        ("power.mode_ac", "插电电源模式"),
                        ("power.mode_dc", "离电电源模式"),
                    ] {
                        self.input(
                            key,
                            label,
                            Kind::Choice(model.options.get(key).cloned().unwrap_or_default()),
                            y,
                            width,
                        );
                        y += 44;
                    }
                }
            }
            y
        }
    }
    unsafe fn sensor_form(&mut self, width: i32, mut y: i32, model: &UiModel) -> i32 {
        unsafe {
            self.label("传感器设置", y, width, 32, true);
            y += 42;
            let shadow = self.input("thermal.shadow", "CPU 90°C 控温观察", Kind::Check, y, width);
            EnableWindow(shadow, model.can("thermal.apply") as i32);
            y += 42;
            let thermal = self.input(
                "thermal.enabled",
                "本机 CPU 90°C 温控",
                Kind::Check,
                y,
                width,
            );
            EnableWindow(thermal, model.can("thermal.apply") as i32);
            y += 42;
            let status = model
                .values
                .get("thermal.status")
                .map(scalar)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "尚未启用；运行状态以后台确认结果为准。".into());
            self.raw_label(&status, y, width, 52, false);
            y += 58;
            let definitions = [
                (
                    "sensors.cpu_frequency",
                    "CPU 频率",
                    vec![
                        UiChoice::new(tr_static("最高频率"), "maximum"),
                        UiChoice::new(tr_static("平均频率"), "average"),
                    ],
                ),
                (
                    "sensors.temperature_source",
                    "CPU 温度来源",
                    vec![
                        UiChoice::new(tr_static("自动"), "auto"),
                        UiChoice::new(tr_static("平均"), "average"),
                        UiChoice::new(tr_static("封装"), "package"),
                        UiChoice::new(tr_static("核心最高"), "core_max"),
                    ],
                ),
                (
                    "sensors.voltage_mode",
                    "电压显示模式",
                    vec![
                        UiChoice::new(tr_static("平均"), "average"),
                        UiChoice::new(tr_static("最大"), "maximum"),
                        UiChoice::new(tr_static("核心"), "core"),
                    ],
                ),
                (
                    "sensors.gpu",
                    "首选显卡",
                    vec![
                        UiChoice::new(tr_static("独显"), "discrete"),
                        UiChoice::new(tr_static("核显"), "integrated"),
                    ],
                ),
                (
                    "sensors.memory_mode",
                    "内存显示",
                    vec![
                        UiChoice::new(tr_static("百分比"), "percent"),
                        UiChoice::new(tr_static("已用 / 总量 GB"), "gigabytes"),
                    ],
                ),
                (
                    "sensors.temperature_unit",
                    "温度单位",
                    vec![
                        UiChoice::new(tr_static("°C"), "C"),
                        UiChoice::new(tr_static("°F"), "F"),
                    ],
                ),
            ];
            for (key, label, fallback) in definitions {
                self.input(
                    key,
                    label,
                    Kind::Choice(model.options.get(key).cloned().unwrap_or(fallback)),
                    y,
                    width,
                );
                y += 42;
                if key == "sensors.voltage_mode" {
                    self.input(
                        "sensors.voltage_core",
                        "电压核心索引",
                        Kind::Number,
                        y,
                        width,
                    );
                    y += 42;
                }
            }
            self.label("高级温控参数", y, width, 30, true);
            y += 38;
            self.input(
                "thermal.sampling_ms",
                "采样间隔 · ms",
                Kind::Number,
                y,
                width,
            );
            y += 42;
            self.input(
                "thermal.firmware_limit_c",
                "固件温限 · °C",
                Kind::Number,
                y,
                width,
            );
            y += 44;
            self.button("恢复默认显示设置", SENSOR_DEFAULTS, 20, y, 190, "");
            y += 44;
            y
        }
    }
    unsafe fn automation_form(&mut self, width: i32, mut y: i32, model: &UiModel) -> i32 {
        unsafe {
            self.label("自动化规则 / 快捷操作", y, width, 32, true);
            y += 42;
            self.input("name", "名称", Kind::Text, y, width);
            y += 42;
            self.input(
                "icon",
                "快捷操作图标",
                Kind::Choice(quick_action_icons(&self.value("icon"))),
                y,
                width,
            );
            y += 44;
            for (key, label) in [
                ("enabled", "启用此规则"),
                ("exclusive", "独占执行"),
                ("run_on_startup", "启动后运行"),
            ] {
                self.input(key, label, Kind::Check, y, width);
                y += 36;
            }
            self.label("触发条件", y, width, 30, true);
            y += 38;
            let trigger_options = AUTOMATION_TRIGGERS
                .iter()
                .map(|(id, label)| UiChoice::new(tr(label).into_owned(), *id))
                .collect::<Vec<_>>();
            let h = self.place(
                "COMBOBOX",
                "",
                CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
                TRIGGER_TYPE,
                [20, y, width, 400],
                self.font,
            );
            set_choices(h, &trigger_options, &json!(self.trigger_type));
            self.trigger_labels = trigger_options.iter().map(|c| scalar(&c.value)).collect();
            y += 42;
            self.input(
                "match_logic",
                "多个条件的关系",
                Kind::Choice(vec![
                    UiChoice::new(tr_static("匹配全部"), "all"),
                    UiChoice::new(tr_static("匹配任意"), "any"),
                ]),
                y,
                width,
            );
            y += 42;
            let specs = trigger_parameters(&self.trigger_type, model);
            for (key, label, kind) in specs {
                self.input(&format!("condition.{key}"), &label, kind, y, width);
                y += 40;
            }
            let list = self.place(
                "LISTBOX",
                "",
                WS_BORDER | WS_VSCROLL | WS_TABSTOP | LBS_NOTIFY as u32,
                CONDITION_LIST,
                [20, y, width, 96],
                self.font,
            );
            for condition in &self.conditions {
                let text = describe_condition(condition);
                SendMessageW(list, LB_ADDSTRING, 0, wide(&text).as_ptr() as isize);
            }
            if let Some(index) = self.selected_condition {
                SendMessageW(list, LB_SETCURSEL, index, 0);
            }
            y += 104;
            self.button("添加 / 更新条件", CONDITION_ADD, 20, y, 164, "");
            self.button("移除条件", CONDITION_DELETE, 196, y, 112, "");
            y += 46;
            self.label("按顺序执行的动作", y, width, 30, true);
            y += 38;
            self.label("搜索动作目录", y, width, 28, false);
            y += 32;
            let search = self.place(
                "EDIT",
                self.value("catalog.action_search").as_str().unwrap_or(""),
                WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32,
                149,
                [20, y, width, 30],
                self.font,
            );
            SendMessageW(search, EM_SETLIMITTEXT, 128, 0);
            y += 38;
            self.place(
                "LISTBOX",
                "",
                WS_BORDER | WS_TABSTOP | WS_VSCROLL | LBS_EXTENDEDSEL as u32 | LBS_NOTIFY as u32,
                150,
                [20, y, width, 100],
                self.font,
            );
            self.refresh_action_palette();
            y += 108;
            self.button("批量添加所选模板", 151, 20, y, 205, "");
            EnableWindow(GetDlgItem(self.hwnd, 151), 0);
            y += 42;
            self.label(
                "按目录顺序添加；请在下方逐项填写参数，可调整顺序。",
                y,
                width,
                38,
                false,
            );
            y += 44;

            let options = AUTOMATION_ACTIONS
                .iter()
                .map(|(id, label)| UiChoice::new(tr(label).into_owned(), *id))
                .collect::<Vec<_>>();
            let h = self.place(
                "COMBOBOX",
                "",
                CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
                ACTION_TYPE,
                [20, y, width, 400],
                self.font,
            );
            set_choices(h, &options, &json!(self.action_type));
            self.action_labels = options.iter().map(|c| scalar(&c.value)).collect();
            y += 42;
            let cap = format!("automation.step.{}", self.action_type);
            if !model.can(&cap) {
                self.raw_label(
                    &format!(
                        "{}{}",
                        tr_static("此动作："),
                        model.unavailable_reason(&cap)
                    ),
                    y,
                    width,
                    42,
                    false,
                );
                y += 48;
            }
            for (key, label, kind) in action_parameters(&self.action_type, model) {
                self.input(&format!("action.{key}"), &label, kind, y, width);
                y += 40;
            }
            let list = self.place(
                "LISTBOX",
                "",
                WS_BORDER | WS_VSCROLL | WS_TABSTOP | LBS_NOTIFY as u32,
                STEP_LIST,
                [20, y, width, 140],
                self.font,
            );
            for (index, step) in self.steps.iter().enumerate() {
                SendMessageW(
                    list,
                    LB_ADDSTRING,
                    0,
                    wide(&format!("{}. {}", index + 1, describe_step(step))).as_ptr() as isize,
                );
            }
            if let Some(index) = self.selected_step {
                SendMessageW(list, LB_SETCURSEL, index, 0);
            }
            y += 148;
            for (label, id, x, w) in [
                ("添加动作", STEP_ADD, 20, 112),
                ("更新所选", STEP_UPDATE, 144, 112),
                ("删除", STEP_DELETE, 268, 86),
                ("上移", STEP_UP, 366, 86),
                ("下移", STEP_DOWN, 464, 86),
            ] {
                self.button(label, id, x, y, w, "");
            }
            y += 46;
            y
        }
    }
    unsafe fn refresh_action_palette(&mut self) {
        unsafe {
            let list = GetDlgItem(self.hwnd, 150);
            if list.is_null() {
                return;
            }
            SendMessageW(list, LB_RESETCONTENT, 0, 0);
            let choices = filtered_action_choices(
                self.drafts
                    .get("catalog.action_search")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            self.action_palette_ids = choices
                .iter()
                .map(|choice| choice.value.as_str().unwrap_or("").to_owned())
                .collect();
            for choice in choices {
                SendMessageW(list, LB_ADDSTRING, 0, wide(&choice.label).as_ptr() as isize);
            }
            let button = GetDlgItem(self.hwnd, 151);
            if !button.is_null() {
                EnableWindow(button, 0);
            }
        }
    }
    unsafe fn macro_form(&mut self, width: i32, mut y: i32, model: &UiModel) -> i32 {
        unsafe {
            self.label("键鼠宏", y, width, 32, true);
            y += 42;
            self.input("name", "名称", Kind::Text, y, width);
            y += 42;
            self.input(
                "trigger_key",
                "触发键",
                Kind::Choice(
                    (0..10)
                        .map(|n| {
                            UiChoice::new(
                                tr_static("数字小键盘 {n}").replace("{n}", &n.to_string()),
                                format!("NumPad{n}"),
                            )
                        })
                        .collect(),
                ),
                y,
                width,
            );
            y += 42;
            self.input("repeat", "重复次数", Kind::Number, y, width);
            y += 40;
            self.input("ignore_delays", "忽略延时", Kind::Check, y, width);
            y += 36;
            self.input(
                "interrupt_on_other_key",
                "按其它键中断",
                Kind::Check,
                y,
                width,
            );
            y += 42;
            self.input(
                "record.mode",
                "录制选项",
                Kind::Choice(vec![
                    UiChoice::new(tr_static("键盘"), "keyboard"),
                    UiChoice::new(tr_static("键盘 + 鼠标"), "keyboard_mouse"),
                    UiChoice::new(tr_static("键盘 + 鼠标 + 移动"), "keyboard_mouse_movement"),
                ]),
                y,
                width,
            );
            y += 42;
            self.button("开始录制", RECORD_START, 20, y, 120, "macro.record.start");
            self.button("停止录制", RECORD_STOP, 152, y, 120, "macro.record.stop");
            y += 44;
            self.label(
                if self.recording_stop.cleanup_uncertain {
                    "录制清理尚未确认，请重试停止；窗口和内容会保留。"
                } else if self.recording {
                    "正在录制；按停止录制完成。关闭编辑器前会先停止。"
                } else {
                    "也可以手动添加按下、抬起、鼠标和延时事件。"
                },
                y,
                width,
                40,
                false,
            );
            y += 46;
            let kinds = event_kinds();
            let h = self.place(
                "COMBOBOX",
                "",
                CBS_DROPDOWNLIST as u32 | WS_TABSTOP,
                EVENT_KIND,
                [20, y, width, 260],
                self.font,
            );
            set_choices(h, &kinds, &json!(self.event_kind));
            y += 42;
            for (key, label) in [
                ("event.key", "虚拟键码 / 鼠标按钮"),
                ("event.delay_ms", "延时 · ms"),
                ("event.x", "X 坐标 / 滚轮量"),
                ("event.y", "Y 坐标"),
            ] {
                self.input(key, label, Kind::Number, y, width);
                y += 40;
            }
            let list = self.place(
                "LISTBOX",
                "",
                WS_BORDER | WS_VSCROLL | WS_TABSTOP | LBS_NOTIFY as u32,
                STEP_LIST,
                [20, y, width, 160],
                self.font,
            );
            for (index, event) in self.steps.iter().enumerate() {
                SendMessageW(
                    list,
                    LB_ADDSTRING,
                    0,
                    wide(&format!("{}. {}", index + 1, describe_event(event))).as_ptr() as isize,
                );
            }
            if let Some(index) = self.selected_step {
                SendMessageW(list, LB_SETCURSEL, index, 0);
            }
            y += 168;
            for (label, id, x, w) in [
                ("添加事件", EVENT_ADD, 20, 108),
                ("更新所选", EVENT_UPDATE, 140, 108),
                ("删除", EVENT_DELETE, 260, 80),
                ("上移", EVENT_UP, 352, 80),
                ("下移", EVENT_DOWN, 444, 80),
                ("清空", EVENT_CLEAR, 536, 80),
            ] {
                self.button(label, id, x, y, w, "");
            }
            y += 46;
            let _ = model;
            y
        }
    }
    fn load_action(&mut self) {
        self.drafts.retain(|k, _| !k.starts_with("action."));
        if let Some(step) = self.selected_step.and_then(|i| self.steps.get(i)) {
            if let Some(kind) = step.get("type").and_then(Value::as_str) {
                self.action_type = kind.into();
            }
            if let Some(parameters) = step.get("parameters").and_then(Value::as_object) {
                for (k, v) in parameters {
                    self.drafts.insert(format!("action.{k}"), v.clone());
                }
            }
        }
    }
    fn load_condition(&mut self) {
        self.drafts.retain(|k, _| !k.starts_with("condition."));
        if let Some(condition) = self.selected_condition.and_then(|i| self.conditions.get(i)) {
            if let Some(kind) = condition.get("type").and_then(Value::as_str) {
                self.trigger_type = kind.into();
            }
            if let Some(parameters) = condition.get("parameters").and_then(Value::as_object) {
                for (k, v) in parameters {
                    self.drafts.insert(format!("condition.{k}"), v.clone());
                }
            }
        }
    }
    fn numeric(&self, key: &str) -> Result<i64> {
        self.value(key).as_i64().ok_or_else(|| {
            anyhow!(
                "{}",
                format_product("{} 需要有效整数。", &[&friendly_key(key)])
            )
        })
    }
    fn parameter_values(&self, prefix: &str) -> Value {
        Value::Object(
            self.drafts
                .iter()
                .filter_map(|(k, v)| {
                    k.strip_prefix(prefix)
                        .map(|name| (name.to_string(), v.clone()))
                })
                .collect(),
        )
    }
    fn make_step(&self) -> Result<Value> {
        let model = live_snapshot(&self.bridge);
        let cap = format!("automation.step.{}", self.action_type);
        if !model.can(&cap) {
            bail!("此动作当前不可用：{}", model.unavailable_reason(&cap));
        }
        Ok(json!({"type":self.action_type,"parameters":self.parameter_values("action.")}))
    }
    fn make_condition(&self) -> Result<Value> {
        if self.trigger_type == "its_mode" {
            bail!("{}", tr_static("ITS 是其他机型的接口，本机不适用。"));
        }
        Ok(json!({"type":self.trigger_type,"parameters":self.parameter_values("condition.")}))
    }
    fn make_event(&self) -> Result<Value> {
        let delay = self.numeric("event.delay_ms")?;
        if !(0..=3_600_000).contains(&delay) {
            bail!("{}", tr_static("事件延时应为0–3600000 ms。"));
        }
        Ok(
            json!({"kind":self.event_kind,"key":self.numeric("event.key")?,"delay_ms":delay,"x":self.numeric("event.x")?,"y":self.numeric("event.y")?}),
        )
    }
    unsafe fn command(&mut self, id: u16, notification: u16) -> Result<()> {
        unsafe {
            if id == CANCEL {
                self.close();
                return Ok(());
            }
            if id == SAVE {
                return self.save();
            }
            if id == 64 && matches!(self.mode, Mode::Console) {
                let id = request_id();
                send(
                    &self.bridge,
                    UiAction::Execute {
                        request_id: id,
                        command: "script.stop".into(),
                        payload: json!({}),
                    },
                )?;
                return Ok(());
            }
            if id == 119 && matches!(self.mode, Mode::Hotkeys) {
                let id = request_id();
                send(
                    &self.bridge,
                    UiAction::Execute {
                        request_id: id,
                        command: "keys.retry_cleanup".into(),
                        payload: json!({}),
                    },
                )?;
                return Ok(());
            }
            self.capture();
            if id == 148 && matches!(self.mode, Mode::Arguments) {
                self.submit("startup.arguments.load", json!({}), false, None)?;
                self.data["loaded"] = json!(false);
                self.build();
                return Ok(());
            }
            if let Some((key, kind, append)) = self.picker_buttons.get(&id).cloned() {
                return self.queue_picker(&key, kind, append);
            }
            if id == 146 && matches!(self.mode, Mode::Backup) {
                return self.queue_picker(
                    "backup.path",
                    PickerKind::Open(FileFilter::Backup),
                    false,
                );
            }
            if id == 147 && matches!(self.mode, Mode::Spectrum) {
                return self.queue_picker(
                    "spectrum.path",
                    PickerKind::Save(FileFilter::Spectrum),
                    false,
                );
            }
            if matches!(self.mode, Mode::Spectrum) && id >= 120 && id <= 143 {
                return self.spectrum_command(id, notification);
            }
            if matches!(self.mode, Mode::Hotkeys) && id >= 100 && id <= 117 {
                return self.hotkey_command(id, notification);
            }
            if matches!(self.mode, Mode::Layout(_)) && id >= 80 && id <= 93 {
                return self.layout_command(id, notification);
            }
            match id {
                149 if notification == EN_CHANGE as u16
                    && matches!(self.mode, Mode::Automation) =>
                {
                    self.drafts.insert(
                        "catalog.action_search".into(),
                        json!(get_text(GetDlgItem(self.hwnd, 149))),
                    );
                    self.refresh_action_palette();
                }
                150 if notification == LBN_SELCHANGE as u16
                    && matches!(self.mode, Mode::Automation) =>
                {
                    EnableWindow(
                        GetDlgItem(self.hwnd, 151),
                        (SendMessageW(GetDlgItem(self.hwnd, 150), LB_GETSELCOUNT, 0, 0) > 0) as i32,
                    );
                }
                151 if matches!(self.mode, Mode::Automation) => {
                    let selected = self
                        .action_palette_ids
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| {
                            SendMessageW(GetDlgItem(self.hwnd, 150), LB_GETSEL, *index, 0) > 0
                        })
                        .map(|(_, id)| id.clone())
                        .collect::<Vec<_>>();
                    let templates = action_templates(&selected, &live_snapshot(&self.bridge))?;
                    let first = self.steps.len();
                    self.steps.extend(templates);
                    self.selected_step = Some(first);
                    self.load_action();
                    self.build();
                }
                144 if notification == CBN_SELCHANGE as u16
                    && matches!(&self.mode,Mode::Form(id) if id=="notifications") =>
                {
                    let selected =
                        SendMessageW(GetDlgItem(self.hwnd, 144), CB_GETCURSEL, 0, 0).clamp(0, 42);
                    self.drafts
                        .insert("@notification.selected_type".into(), json!(selected));
                    self.scroll = 0;
                    self.build();
                }
                145 if matches!(&self.mode,Mode::Form(id) if id=="notifications") => {
                    let selected = self
                        .drafts
                        .get("@notification.selected_type")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        .min(42) as usize;
                    forms::reset_notification_draft(selected, &mut self.drafts);
                    self.build();
                }
                EDITOR_TAB if notification == CBN_SELCHANGE as u16 => {
                    self.tab =
                        SendMessageW(GetDlgItem(self.hwnd, EDITOR_TAB as i32), CB_GETCURSEL, 0, 0)
                            .max(0) as usize;
                    self.scroll = 0;
                    self.build();
                }
                ACTION_TYPE if notification == CBN_SELCHANGE as u16 => {
                    let index = SendMessageW(
                        GetDlgItem(self.hwnd, ACTION_TYPE as i32),
                        CB_GETCURSEL,
                        0,
                        0,
                    )
                    .max(0) as usize;
                    if let Some(kind) = self.action_labels.get(index) {
                        self.action_type = kind.clone();
                        self.drafts.retain(|k, _| !k.starts_with("action."));
                    }
                    self.build();
                }
                TRIGGER_TYPE if notification == CBN_SELCHANGE as u16 => {
                    let index = SendMessageW(
                        GetDlgItem(self.hwnd, TRIGGER_TYPE as i32),
                        CB_GETCURSEL,
                        0,
                        0,
                    )
                    .max(0) as usize;
                    if let Some(kind) = self.trigger_labels.get(index) {
                        self.trigger_type = kind.clone();
                        self.drafts.retain(|k, _| !k.starts_with("condition."));
                    }
                    self.selected_condition = None;
                    self.build();
                }
                EVENT_KIND if notification == CBN_SELCHANGE as u16 => {
                    let index =
                        SendMessageW(GetDlgItem(self.hwnd, EVENT_KIND as i32), CB_GETCURSEL, 0, 0)
                            .max(0) as usize;
                    if let Some(choice) = event_kinds().get(index) {
                        self.event_kind = scalar(&choice.value);
                    }
                    self.build();
                }
                STEP_LIST if notification == LBN_SELCHANGE as u16 => {
                    let index =
                        SendMessageW(GetDlgItem(self.hwnd, STEP_LIST as i32), LB_GETCURSEL, 0, 0);
                    self.selected_step = (index >= 0).then_some(index.max(0) as usize);
                    if matches!(self.mode, Mode::Automation) {
                        self.load_action();
                    } else if let Some(event) = self.selected_step.and_then(|i| self.steps.get(i)) {
                        self.event_kind = event
                            .get("kind")
                            .and_then(Value::as_str)
                            .unwrap_or("key_down")
                            .into();
                        for key in ["key", "delay_ms", "x", "y"] {
                            self.drafts.insert(
                                format!("event.{key}"),
                                event.get(key).cloned().unwrap_or_else(|| json!(0)),
                            );
                        }
                    }
                    self.build();
                }
                CONDITION_LIST if notification == LBN_SELCHANGE as u16 => {
                    let index = SendMessageW(
                        GetDlgItem(self.hwnd, CONDITION_LIST as i32),
                        LB_GETCURSEL,
                        0,
                        0,
                    );
                    self.selected_condition = (index >= 0).then_some(index.max(0) as usize);
                    self.load_condition();
                    self.build();
                }
                STEP_ADD => {
                    let step = self.make_step()?;
                    self.steps.push(step);
                    self.selected_step = Some(self.steps.len() - 1);
                    self.build();
                }
                STEP_UPDATE => {
                    let index = self
                        .selected_step
                        .ok_or_else(|| anyhow!("{}", tr_static("请先选择要更新的动作。")))?;
                    let mut step = self.make_step()?;
                    retain_legacy(&self.steps[index], &mut step, "type");
                    self.steps[index] = step;
                    self.build();
                }
                STEP_DELETE | EVENT_DELETE => {
                    let index = self
                        .selected_step
                        .ok_or_else(|| anyhow!("{}", tr_static("请先选择列表项目。")))?;
                    self.steps.remove(index);
                    self.selected_step = None;
                    self.build();
                }
                STEP_UP | EVENT_UP => {
                    if let Some(index) = self.selected_step {
                        if index > 0 {
                            self.steps.swap(index, index - 1);
                            self.selected_step = Some(index - 1);
                            self.build();
                        }
                    }
                }
                STEP_DOWN | EVENT_DOWN => {
                    if let Some(index) = self.selected_step {
                        if index + 1 < self.steps.len() {
                            self.steps.swap(index, index + 1);
                            self.selected_step = Some(index + 1);
                            self.build();
                        }
                    }
                }
                CONDITION_ADD => {
                    if self.trigger_type == "manual" {
                        self.conditions.clear();
                        self.selected_condition = None;
                    } else {
                        let mut condition = self.make_condition()?;
                        if let Some(index) = self.selected_condition {
                            retain_legacy(&self.conditions[index], &mut condition, "type");
                            self.conditions[index] = condition;
                        } else {
                            self.conditions.push(condition);
                            self.selected_condition = Some(self.conditions.len() - 1);
                        }
                    }
                    self.build();
                }
                CONDITION_DELETE => {
                    let index = self
                        .selected_condition
                        .ok_or_else(|| anyhow!("{}", tr_static("请先选择要移除的触发条件。")))?;
                    self.conditions.remove(index);
                    self.selected_condition = None;
                    self.build();
                }
                EVENT_ADD => {
                    let event = self.make_event()?;
                    self.steps.push(event);
                    self.selected_step = Some(self.steps.len() - 1);
                    self.build();
                }
                EVENT_UPDATE => {
                    let index = self
                        .selected_step
                        .ok_or_else(|| anyhow!("{}", tr_static("请先选择要更新的事件。")))?;
                    let mut event = self.make_event()?;
                    retain_legacy(&self.steps[index], &mut event, "kind");
                    self.steps[index] = event;
                    self.build();
                }
                EVENT_CLEAR => {
                    if confirm(
                        self.hwnd,
                        "清空此宏正在编辑的全部事件？保存前不会改写原宏。",
                    ) {
                        self.steps.clear();
                        self.selected_step = None;
                        self.build();
                    }
                }
                RECORD_START => {
                    let baseline = live_snapshot(&self.bridge)
                        .value("macro.recording.revision")
                        .as_u64()
                        .unwrap_or(0);
                    let payload = json!({"id":self.data.get("id").cloned().unwrap_or(Value::Null),"mode":self.value("record.mode")});
                    self.submit("macro.record.start", payload, false, Some(true))?;
                    self.recording_stop = RecordingStop::default();
                    self.recording_revision = baseline;
                }
                RECORD_STOP => {
                    self.request_recording_stop(false)?;
                }
                70 | 71 => {
                    let path = self
                        .value("backup.path")
                        .as_str()
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    if path.is_empty() {
                        bail!("{}", tr_static("请填写备份文件路径。"));
                    }
                    if id == 71
                        && !confirm(
                            self.hwnd,
                            "还原此备份？当前设置会先备份；温控和自动化不会因导入而自动运行。",
                        )
                    {
                        return Ok(());
                    }
                    self.submit(if id==70{"settings.backup"}else{"settings.restore"},json!({"path":path,"include_macros":self.value("backup.include_macros"),"include_automation":self.value("backup.include_automation")}),false,None)?;
                }
                SENSOR_DEFAULTS => {
                    for (key, value) in [
                        ("sensors.cpu_frequency", json!("maximum")),
                        ("sensors.temperature_source", json!("auto")),
                        ("sensors.voltage_mode", json!("average")),
                        ("sensors.voltage_core", json!(0)),
                        ("sensors.gpu", json!("discrete")),
                        ("sensors.memory_mode", json!("percent")),
                        ("sensors.temperature_unit", json!("C")),
                    ] {
                        self.drafts.insert(key.into(), value);
                    }
                    self.build();
                }
                _ => {}
            }
            Ok(())
        }
    }
    fn start_recording_pending(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.recording == Some(true))
    }
    fn request_recording_stop(&mut self, close: bool) -> Result<()> {
        self.recording_stop.close_requested |= close;
        if self.recording_stop.inflight.is_some()
            || self.recording_stop.confirmed && !self.recording
        {
            return Ok(());
        }
        let id = request_id();
        send(
            &self.bridge,
            UiAction::Execute {
                request_id: id,
                command: "macro.record.stop".into(),
                payload: json!({}),
            },
        )?;
        self.recording_stop.requested = true;
        self.recording_stop.inflight = Some(id);
        Ok(())
    }
    unsafe fn final_recorded_events(&mut self) {
        unsafe {
            if let Some((revision, events, reason)) =
                recorded_events(&self.bridge, self.recording_revision)
            {
                self.capture();
                self.steps = events;
                self.recording = false;
                self.recording_revision = revision;
                self.recording_stop.final_events_loaded = true;
                // A fresh final report is also the actual completion for ESC, which has no Stop RPC.
                // Explicit Stop failures remain a retry gate even if a partial report was published.
                if !self.recording_stop.requested {
                    self.recording_stop.confirmed = true;
                }
                self.build();
                text(self.status, &reason);
            } else if self.recording_stop.needs_final_events() {
                text(
                    self.status,
                    tr_static("录制已停止，正在等待最后一批事件；编辑内容会保留。"),
                );
            }
        }
    }
    fn queue_picker(&mut self, key: &str, kind: PickerKind, append: bool) -> Result<()> {
        if self.pending.is_some() || self.modal_active {
            bail!("{}", tr_static("请等待当前操作结束后再选择。"));
        }
        self.modal_active = true;
        self.deferred_picker = Some(PickerRequest {
            key: key.into(),
            kind,
            initial: if append {
                String::new()
            } else {
                scalar(&self.value(key))
            },
            append,
        });
        Ok(())
    }
    unsafe fn submit(
        &mut self,
        command: &str,
        payload: Value,
        close: bool,
        recording: Option<bool>,
    ) -> Result<()> {
        unsafe {
            if self.pending.is_some() {
                bail!("{}", tr_static("上一项操作仍在等待确认，请稍候。"));
            }
            let model = live_snapshot(&self.bridge);
            if matches!(self.mode, Mode::Hotkeys) {
                if model.metadata_revision != self.recording_revision {
                    self.recording_revision = model.metadata_revision;
                    self.capture();
                    self.build();
                }
                let status = scalar(&model.value("keys.status"));
                if !status.is_empty() {
                    text(self.status, &status);
                }
            }
            if !model.can(command) {
                bail!("{}", model.unavailable_reason(command));
            }
            let id = request_id();
            if ["keyboard.effects.import", "keyboard.effects.export"].contains(&command) {
                self.drafts.insert(
                    "spectrum.file_request".into(),
                    json!({"id":id,"kind":command}),
                );
            }
            send(
                &self.bridge,
                UiAction::Execute {
                    request_id: id,
                    command: command.into(),
                    payload,
                },
            )?;
            self.pending = Some(Pending {
                id,
                close,
                recording,
            });
            EnableWindow(self.save, 0);
            text(self.status, &tr("正在处理；结果返回前保留窗口和内容。"));
            Ok(())
        }
    }
    unsafe fn save(&mut self) -> Result<()> {
        unsafe {
            self.capture();
            if self.pending.is_some() {
                bail!("{}", tr_static("仍在等待上一项保存结果。"));
            }
            match self.mode.clone() {
                Mode::Console => {
                    self.submit("script.run", json!({"script.command":self.value("script.command"),"script.arguments":self.value("script.arguments"),"script.directory":self.value("script.directory")}), false, None)?;
                }
                Mode::FirstLanguage => {
                    self.submit(
                        "app.preferences.save",
                        first_language_payload(&self.value("appearance.language"))?,
                        true,
                        None,
                    )?;
                }
                Mode::Arguments => {
                    let payload = arguments_payload(&self.data, &self.drafts)?;
                    if arguments_need_confirmation(&self.data, &payload) {
                        if self.modal_active {
                            bail!("{}", tr_static("请等待当前操作结束后再选择。"));
                        }
                        self.modal_active = true;
                        self.deferred_submit = Some(ConfirmedSubmit {
                            message: "关闭下次启动的只读模式？下次普通启动将允许运行已保存的功能；当前会话不受影响。",
                            command: "startup.arguments.save",
                            payload,
                        });
                    } else {
                        self.submit("startup.arguments.save", payload, true, None)?;
                    }
                }
                Mode::Document(document) => {
                    let input = self.value("document.text");
                    let source = input
                        .as_str()
                        .ok_or_else(|| anyhow!("{}", tr_static("配置内容为空。")))?;
                    let value: Value = serde_json::from_str(source).map_err(|e| {
                        anyhow!("配置格式错误：第{}行第{}列：{}", e.line(), e.column(), e)
                    })?;
                    let model = snapshot(&self.bridge);
                    if !model.documents.get(&document).is_some_and(|d| d.writable) {
                        bail!("{}", tr_static("此配置当前只读。"));
                    }
                    let id = request_id();
                    send(
                        &self.bridge,
                        UiAction::SaveDocument {
                            request_id: id,
                            document,
                            value,
                        },
                    )?;
                    self.pending = Some(Pending {
                        id,
                        close: true,
                        recording: None,
                    });
                    EnableWindow(self.save, 0);
                    text(self.status, &tr("保存中；失败时保留窗口和内容。"));
                }
                Mode::Form(form_id) => {
                    let model = snapshot(&self.bridge);
                    let form = forms::schema(&form_id, &model)
                        .ok_or_else(|| anyhow!("{}", tr_static("设置表单不可用。")))?;
                    if !model
                        .documents
                        .get(form.document)
                        .is_some_and(|d| d.writable)
                    {
                        bail!("{}", tr_static("此配置当前只读。"));
                    }
                    let mut fields = forms::validate(&form, &self.drafts)?;
                    if form_id == "notifications" {
                        if let (Some(fields), Some(patch)) = (
                            fields.as_object_mut(),
                            forms::notification_patch(&self.data, &self.drafts)?.as_object(),
                        ) {
                            fields.extend(patch.clone());
                        }
                    }
                    self.submit(
                        "settings.fields.save",
                        json!({"document":form.document,"fields":fields}),
                        true,
                        None,
                    )?;
                }
                Mode::Layout(document) => {
                    let model = snapshot(&self.bridge);
                    if !model.documents.get(&document).is_some_and(|d| d.writable) {
                        bail!("{}", tr_static("此配置当前只读。"));
                    }
                    layout::validate(&self.steps, document == "sensor_dashboard")?;
                    let form = forms::schema(&document, &model)
                        .ok_or_else(|| anyhow!("{}", tr_static("分组设置不可用。")))?;
                    let mut fields = forms::validate(&form, &self.drafts)?;
                    fields["/Groups"] = json!(self.steps);
                    if document == "sensor_dashboard" {
                        fields["/VisibleItems"] = json!(self.conditions);
                    }
                    self.submit(
                        "settings.fields.save",
                        json!({"document":document,"fields":fields}),
                        true,
                        None,
                    )?;
                }
                Mode::Spectrum => {
                    let payload = self.spectrum_payload()?;
                    self.submit("keyboard.effects.apply", payload, true, None)?;
                }
                Mode::Hotkeys => {
                    if self.key_code.is_none() {
                        bail!("{}", tr_static("请先选择或发现按键。"));
                    }
                    self.stash_hotkey();
                    let model = snapshot(&self.bridge);
                    if !model
                        .documents
                        .get("special_keys")
                        .is_some_and(|d| d.writable)
                    {
                        bail!("{}", tr_static("此配置当前只读。"));
                    }
                    self.submit(
                        "settings.fields.save",
                        json!({"document":"special_keys","fields":self.key_changes}),
                        true,
                        None,
                    )?;
                }
                Mode::Backup => {
                    bail!("{}", tr_static("请选择创建备份或还原备份。"));
                }
                Mode::Preset => {
                    let name = self.value("name").as_str().unwrap_or("").trim().to_owned();
                    if name.is_empty() {
                        bail!("{}", tr_static("请填写预设名称。"));
                    }
                    let model = snapshot(&self.bridge);
                    let mut values = serde_json::Map::new();
                    for (key, _, _) in PRESET_FIELDS {
                        let value = self.value(key);
                        if value.is_null() || value.as_str() == Some("") {
                            continue;
                        }
                        let number = value.as_i64().ok_or_else(|| {
                            anyhow!("{}", format_product("{} 需要整数。", &[&friendly_key(key)]))
                        })?;
                        if let Some(options) = model.options.get(*key).filter(|o| !o.is_empty()) {
                            if !options.iter().any(|option| option.value == value) {
                                bail!(
                                    "{}",
                                    format_product(
                                        "{}必须从本机合法选项中选择。",
                                        &[&friendly_key(key)]
                                    )
                                );
                            }
                            values.insert((*key).into(), json!(number));
                            continue;
                        }
                        if !model.ranges.contains_key(*key) {
                            continue;
                        }
                        let range = model
                            .ranges
                            .get(*key)
                            .filter(|r| r.step > 0 && r.maximum >= r.minimum)
                            .ok_or_else(|| {
                                anyhow!(
                                    "{}",
                                    format_product(
                                        "{} 的本机合法范围尚不可用。",
                                        &[&friendly_key(key)]
                                    )
                                )
                            })?;
                        if number < range.minimum as i64
                            || number > range.maximum as i64
                            || (number - range.minimum as i64) % range.step as i64 != 0
                        {
                            bail!(
                                "{} 必须为{}–{}，步长{}。",
                                friendly_key(key),
                                range.minimum,
                                range.maximum,
                                range.step
                            );
                        }
                        values.insert((*key).into(), json!(number));
                    }
                    for key in ["cpu.pl1", "cpu.pl2", "cpu.temperature"] {
                        if !values.contains_key(key) {
                            bail!("缺少{}的已验证数值。", friendly_key(key));
                        }
                    }
                    if values["cpu.pl1"].as_i64() > values["cpu.pl2"].as_i64() {
                        bail!("{}", tr_static("PL1 不能高于 PL2。"));
                    }
                    let count = self
                        .drafts
                        .keys()
                        .filter(|k| k.starts_with("fan.point."))
                        .count();
                    let mut table = vec![];
                    for index in 0..count {
                        let n = self.numeric(&format!("fan.point.{index}"))?;
                        if !(0..=10).contains(&n) {
                            bail!(
                                "{}",
                                tr_static("风扇节点必须为0–10；后台还会校验本机最小值。")
                            );
                        }
                        table.push(n);
                    }
                    let payload = json!({"id":self.data.get("id").cloned().unwrap_or(Value::Null),"name":name,"values":values,"fan":{"full_speed":self.value("fan.full_speed"),"table":if table.is_empty(){Value::Null}else{json!(table)}},"overrides":{"power.plan":self.value("power.plan"),"power.mode_ac":self.value("power.mode_ac"),"power.mode_dc":self.value("power.mode_dc")}});
                    self.submit("preset.save", payload, true, None)?;
                }
                Mode::Automation => {
                    let name = self.value("name").as_str().unwrap_or("").trim().to_owned();
                    if name.is_empty() {
                        bail!("{}", tr_static("请填写规则名称。"));
                    }
                    if self.steps.is_empty() {
                        bail!("{}", tr_static("请至少添加一个动作。"));
                    }
                    let trigger = if self.trigger_type == "manual" && self.conditions.is_empty() {
                        Value::Null
                    } else if self.conditions.is_empty() {
                        self.make_condition()?
                    } else if self.conditions.len() == 1 {
                        self.conditions[0].clone()
                    } else {
                        json!({"type":self.value("match_logic"),"conditions":self.conditions})
                    };
                    let payload = json!({"id":self.data.get("id").cloned().unwrap_or(Value::Null),"name":name,"enabled":self.value("enabled"),"exclusive":self.value("exclusive"),"run_on_startup":self.value("run_on_startup"),"trigger":trigger,"steps":self.steps,"icon":self.value("icon")});
                    self.submit("automation.save", payload, true, None)?;
                }
                Mode::Macro => {
                    if self.recording
                        || self.recording_stop.busy()
                        || self.start_recording_pending()
                    {
                        bail!("{}", tr_static("请先停止录制，再保存宏。"));
                    }
                    let name = self.value("name").as_str().unwrap_or("").trim().to_owned();
                    if name.is_empty() {
                        bail!("{}", tr_static("请填写宏名称。"));
                    }
                    let repeat = self.numeric("repeat")?;
                    if !(1..=10).contains(&repeat) {
                        bail!("{}", tr_static("重复次数应为1–10。"));
                    }
                    let payload = json!({"id":self.data.get("id").cloned().unwrap_or(Value::Null),"name":name,"trigger_key":self.value("trigger_key"),"repeat":repeat,"ignore_delays":self.value("ignore_delays"),"interrupt_on_other_key":self.value("interrupt_on_other_key"),"events":self.steps});
                    self.submit("macro.save", payload, true, None)?;
                }
                Mode::SensorSettings => {
                    let mut map = serde_json::Map::new();
                    for (key, _) in SENSOR_FIELDS {
                        let value = self.value(key);
                        if !value.is_null() {
                            map.insert((*key).into(), value);
                        }
                    }
                    let model = snapshot(&self.bridge);
                    let (command, payload) =
                        sensor_save_payload(&model, &self.data, Value::Object(map))?;
                    self.submit(command, payload, true, None)?;
                }
            }
            Ok(())
        }
    }
    fn script_output(&self) -> String {
        collection_text(&self.bridge, "script.output")
    }
    unsafe fn poll(&mut self) {
        unsafe {
            if self.bridge.smoke_exercise_reentrancy
                && self.started.elapsed() >= std::time::Duration::from_millis(250)
                && self.pending.is_none()
            {
                self.close();
                return;
            }
            let model = live_snapshot(&self.bridge);
            if matches!(self.mode, Mode::Console) {
                let revision = model.value("script.output_revision");
                if self.drafts.get("script.output_revision") != Some(&revision) {
                    let output = self.script_output();
                    text(GetDlgItem(self.hwnd, 63), &output);
                    self.drafts
                        .insert("script.output_revision".into(), revision);
                }
            }
            if let Some(pending) = &self.pending {
                if let Some(result) = model.result(pending.id) {
                    let pending_id = pending.id;
                    let close = pending.close;
                    let recording = pending.recording;
                    let ok = result.ok;
                    let status_message = result.message.clone();
                    self.pending = None;
                    if ok {
                        if let Some(recording) = recording {
                            self.recording = if recording {
                                self.recording_stop.acknowledge_start(true)
                            } else {
                                false
                            };
                        }
                        if close {
                            self.deferred_destroy = true;
                            return;
                        }
                    }
                    EnableWindow(
                        self.save,
                        (!matches!(self.mode, Mode::Backup)
                            && (!matches!(self.mode, Mode::Macro)
                                || (!self.recording && !self.recording_stop.busy())))
                            as i32,
                    );
                    text(self.status, &status_message);
                    if !ok {
                        if !(recording == Some(true) && self.recording_stop.requested) {
                            message(self.hwnd, "操作未完成", &status_message, true);
                        }
                    } else if matches!(self.mode, Mode::Spectrum) {
                        let file_kind = self
                            .drafts
                            .get("spectrum.file_request")
                            .filter(|value| {
                                value.get("id").and_then(Value::as_u64) == Some(pending_id)
                            })
                            .and_then(|value| value.get("kind"))
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        if file_kind.as_deref() == Some("keyboard.effects.export") {
                            self.drafts.remove("spectrum.file_request");
                            return;
                        }
                        if file_kind.as_deref() == Some("keyboard.effects.import") {
                            if let Some(effects) = imported_spectrum_draft(&self.bridge, pending_id)
                            {
                                self.steps = effects;
                                self.selected_step = (!self.steps.is_empty()).then_some(0);
                                self.load_spectrum_effect();
                                let _ = self.bridge.actions.try_send(UiAction::Execute {
                                    request_id: request_id(),
                                    command: "keyboard.effects.imported.ack".into(),
                                    payload: json!({"request_id":pending_id}),
                                });
                                self.build();
                            } else {
                                text(self.status, &tr("导入回执缺少对应草稿；当前编辑内容保留"));
                            }
                            self.drafts.remove("spectrum.file_request");
                            return;
                        }
                        let model = snapshot(&self.bridge);
                        if let Some(effects) = spectrum_editor::cached(&model) {
                            self.steps = effects;
                            self.data["Loaded"] = json!(true);
                            self.data["Profile"] = self.value("spectrum.profile");
                            self.selected_step = (!self.steps.is_empty()).then_some(0);
                            self.load_spectrum_effect();
                        }
                        self.build();
                        text(self.status, &status_message);
                    } else if matches!(self.mode, Mode::Arguments) {
                        match arguments_state(&startup_arguments_snapshot(&self.bridge)) {
                            Ok(state) => {
                                self.drafts
                                    .insert("startup.minimized".into(), state["minimized"].clone());
                                self.drafts
                                    .insert("startup.read_only".into(), state["read_only"].clone());
                                self.data = state;
                                self.build();
                                text(self.status, &status_message);
                            }
                            Err(error) => {
                                self.data["loaded"] = json!(false);
                                self.build();
                                text(self.status, &error.to_string());
                            }
                        }
                    } else if recording.is_some() {
                        self.capture();
                        self.build();
                        text(self.status, &status_message);
                    }
                }
            }
            if matches!(self.mode, Mode::Macro) {
                if let Some(result) = self.recording_stop.inflight.and_then(|id| model.result(id)) {
                    let ok = result.ok;
                    let status = result.message.clone();
                    self.recording_stop.acknowledge(ok);
                    if ok {
                        self.recording = false;
                        if !self.recording_stop.needs_final_events() {
                            self.capture();
                            self.build();
                        }
                    } else {
                        self.capture();
                        self.build();
                    }
                    text(self.status, &status);
                    // Error stays visible in status and the cleanup-required form;
                    // no additional modal loop runs under the Editor borrow.
                }
                if self.recording_stop.had_started && !self.recording_stop.final_events_loaded {
                    self.final_recorded_events();
                }
                let start_pending = self.start_recording_pending();
                let stop_pending = self.recording_stop.busy();
                EnableWindow(
                    GetDlgItem(self.hwnd, RECORD_STOP as i32),
                    (start_pending || self.recording || self.recording_stop.cleanup_uncertain)
                        as i32,
                );
                EnableWindow(
                    GetDlgItem(self.hwnd, RECORD_START as i32),
                    (!self.recording
                        && !start_pending
                        && !stop_pending
                        && self.pending.is_none()
                        && model.can("macro.record.start")) as i32,
                );
                EnableWindow(
                    self.save,
                    (!self.recording
                        && !start_pending
                        && !stop_pending
                        && self.pending.is_none()
                        && model.can("macro.save")) as i32,
                );
                if self
                    .recording_stop
                    .can_close(self.pending.is_some(), self.recording)
                {
                    self.deferred_destroy = true;
                    return;
                }
                if start_pending
                    && model.value("macro.preparing").as_bool() == Some(true)
                    && !self.recording_stop.requested
                {
                    if let Some(remaining) = model.value("macro.prepare_remaining_seconds").as_u64()
                    {
                        text(
                            self.status,
                            &format_product("准备录制：{} 秒…", &[&remaining.to_string()]),
                        );
                    } else {
                        text(self.status, tr_static("正在准备录制…"));
                    }
                } else if self.recording_stop.inflight.is_some() {
                    text(self.status, tr_static("正在停止录制，等待实际确认…"));
                }
            }
            if matches!(self.mode, Mode::Macro) && self.recording && !self.recording_stop.requested
            {
                if let Some(count) = model.value("macro.recording.count").as_u64() {
                    if self
                        .drafts
                        .get("recording.last_count")
                        .and_then(Value::as_u64)
                        != Some(count)
                    {
                        self.drafts
                            .insert("recording.last_count".into(), json!(count));
                        text(
                            self.status,
                            &format_product(
                                "正在录制：已记录 {} 个事件；请按停止完成。",
                                &[&count.to_string()],
                            ),
                        );
                    }
                }
            }
        }
    }
    unsafe fn close(&mut self) {
        unsafe {
            if self.modal_active {
                self.close_after_modal = true;
                return;
            }
            if matches!(self.mode, Mode::Macro)
                && (self.start_recording_pending()
                    || self.recording
                    || self.recording_stop.busy()
                    || self.recording_stop.requested)
            {
                if let Err(error) = self.request_recording_stop(true) {
                    text(self.status, &error.to_string());
                }
                return;
            }
            if self.pending.is_some() {
                message(
                    self.hwnd,
                    "仍在等待确认",
                    "后台操作尚未返回，窗口和编辑内容会保留。",
                    false,
                );
                return;
            }
            if self.recording {
                if let Err(error) = self.submit("macro.record.stop", json!({}), true, Some(false)) {
                    message(self.hwnd, "请先停止录制", &error.to_string(), true);
                }
                return;
            }
            self.deferred_destroy = true;
        }
    }
}

const PRESET_FIELDS: &[(&str, &str, usize)] = &[
    ("cpu.pl1", "长时负载功耗 PL1", 0),
    ("cpu.pl2", "短时睿频功耗 PL2", 0),
    ("cpu.peak", "峰值功耗", 0),
    ("cpu.cross_loading", "Cross Loading 功耗", 0),
    ("cpu.pl2_tau", "PL2 时间常数", 0),
    ("cpu.sppt", "APU sPPT", 0),
    ("cpu.temperature", "CPU 温度限制", 0),
    ("gpu.dynamic_boost", "动态加速功率", 1),
    ("gpu.tgp", "可配置 TGP", 1),
    ("gpu.temperature", "GPU 温度限制", 1),
    ("gpu.total_power_target", "插电总功耗目标", 1),
    ("gpu.to_cpu_boost", "GPU 至 CPU Dynamic Boost", 1),
];
const SENSOR_FIELDS: &[(&str, &str)] = &[
    ("thermal.shadow", "观察"),
    ("thermal.enabled", "本机温控"),
    ("sensors.cpu_frequency", "CPU频率"),
    ("sensors.temperature_source", "温度来源"),
    ("sensors.voltage_mode", "电压模式"),
    ("sensors.voltage_core", "电压核心"),
    ("sensors.gpu", "首选显卡"),
    ("sensors.memory_mode", "内存模式"),
    ("sensors.temperature_unit", "温度单位"),
    ("thermal.sampling_ms", "采样间隔"),
    ("thermal.firmware_limit_c", "固件温限"),
];
const THERMAL_FIELDS: &[&str] = jiks_runtime::config_actions::THERMAL_EDITOR_FIELDS;
fn recorded_events(bridge: &UiBridge, after_revision: u64) -> Option<(u64, Vec<Value>, String)> {
    let model = bridge.model.lock().ok()?;
    let revision = model.values.get("macro.recording.revision")?.as_u64()?;
    if revision <= after_revision {
        return None;
    }
    let item = model.collections.get("macro.recording")?.first()?;
    if item.id != format!("recording-{revision}") {
        return None;
    }
    Some((
        revision,
        item.value.as_array()?.clone(),
        item.detail.clone(),
    ))
}
fn collection_text(bridge: &UiBridge, key: &str) -> String {
    bridge
        .model
        .lock()
        .ok()
        .and_then(|model| {
            model
                .collections
                .get(key)
                .and_then(|items| items.first())
                .and_then(|item| item.value.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default()
}
fn imported_spectrum_draft(bridge: &UiBridge, request: u64) -> Option<Vec<Value>> {
    bridge.model.lock().ok().and_then(|model| {
        model
            .collections
            .get("keyboard.effects.imported")
            .and_then(|items| items.first())
            .map(|item| &item.value)
            .filter(|value| value.get("request_id").and_then(Value::as_u64) == Some(request))
            .and_then(|value| value.get("effects").and_then(Value::as_array))
            .cloned()
    })
}
fn sensor_save_payload(
    model: &UiModel,
    baseline: &Value,
    payload: Value,
) -> Result<(&'static str, Value)> {
    let current = Value::Object(
        THERMAL_FIELDS
            .iter()
            .map(|key| ((*key).into(), model.value(key)))
            .collect(),
    );
    jiks_runtime::config_actions::sensor_editor_command(
        baseline,
        &current,
        payload,
        model.can("thermal.apply"),
    )
}
#[cfg(test)]
mod sensor_save_tests {
    use super::*;
    fn model() -> UiModel {
        let mut model = UiModel::default();
        for (key, value) in [
            ("thermal.enabled", json!(false)),
            ("thermal.shadow", json!(false)),
            ("thermal.sampling_ms", json!(250)),
            ("thermal.firmware_limit_c", json!(88)),
        ] {
            model.values.insert(key.into(), value);
        }
        model.set_capability("thermal.apply", true, "");
        model.set_capability("sensors.settings.save", true, "");
        model
    }
    #[test]
    fn display_save_omits_all_control_fields_even_when_controller_changed_elsewhere() {
        let mut model = model();
        let baseline = json!({"thermal.enabled":false,"thermal.shadow":false,"thermal.sampling_ms":250,"thermal.firmware_limit_c":88});
        let mut payload = baseline.clone();
        payload["sensors.temperature_unit"] = json!("fahrenheit");
        model.values.insert("thermal.enabled".into(), json!(true));
        let (command, payload) = sensor_save_payload(&model, &baseline, payload).unwrap();
        assert_eq!(command, "sensors.settings.save");
        assert!(THERMAL_FIELDS.iter().all(|key| payload.get(*key).is_none()));
        assert_eq!(payload["sensors.temperature_unit"], "fahrenheit");
    }
    #[test]
    fn enabling_stopping_and_policy_changes_use_dedicated_route() {
        let model = model();
        let baseline = Value::Object(model.values.clone().into_iter().collect());
        for (key, value) in [
            ("thermal.enabled", json!(true)),
            ("thermal.shadow", json!(true)),
            ("thermal.sampling_ms", json!(500)),
            ("thermal.firmware_limit_c", json!(87)),
        ] {
            let mut payload = baseline.clone();
            payload[key] = value;
            assert_eq!(
                sensor_save_payload(&model, &baseline, payload).unwrap().0,
                "thermal.apply"
            );
        }
    }
    #[test]
    fn external_contract_change_does_not_overwrite_concurrent_control() {
        let mut model = model();
        let baseline = Value::Object(model.values.clone().into_iter().collect());
        model.values.insert("thermal.enabled".into(), json!(true));
        let mut payload = baseline.clone();
        payload["thermal.shadow"] = json!(true);
        assert!(sensor_save_payload(&model, &baseline, payload).is_err());
    }
}
fn friendly_key(key: &str) -> String {
    PRESET_FIELDS
        .iter()
        .find(|(id, _, _)| *id == key)
        .map(|(_, label, _)| tr(label).into_owned())
        .unwrap_or_else(|| key.into())
}
fn choice(model: &UiModel, key: &str, fallback: Vec<UiChoice>) -> Kind {
    Kind::Choice(model.options.get(key).cloned().unwrap_or(fallback))
}
fn on_off() -> Vec<UiChoice> {
    vec![
        UiChoice::new(tr_static("关闭"), false),
        UiChoice::new(tr_static("开启"), true),
    ]
}
fn action_parameters(id: &str, model: &UiModel) -> Vec<(String, String, Kind)> {
    let one = |key: &str, label: &str, kind: Kind| vec![(key.into(), label.into(), kind)];
    match id {
        "delay" => one("milliseconds", "等待时间 · ms", Kind::Number),
        "run" => vec![
            ("path".into(), "程序路径".into(), Kind::Text),
            ("arguments".into(), "参数".into(), Kind::Text),
            ("working_directory".into(), "工作目录".into(), Kind::Text),
            ("wait".into(), "等待完成".into(), Kind::Check),
        ],
        "notification" => vec![
            ("title".into(), "通知标题".into(), Kind::Text),
            ("message".into(), "通知内容".into(), Kind::Text),
        ],
        "play_sound" | "spectrum_import" => one("path", "文件路径", Kind::Text),
        "display_brightness" | "volume" => one("value", "数值 · %", Kind::Number),
        "dpi" => one("value", "缩放比例 · %", Kind::Number),
        "refresh_rate" => one(
            "value",
            "刷新率",
            choice(model, "display.refresh_rate", vec![]),
        ),
        "resolution" => one(
            "value",
            "分辨率",
            choice(model, "display.resolution", vec![]),
        ),
        "power_mode" => one(
            "value",
            "性能模式",
            choice(
                model,
                "power.mode",
                vec![
                    UiChoice::new(tr_static("安静"), 1),
                    UiChoice::new(tr_static("均衡"), 2),
                    UiChoice::new(tr_static("性能"), 3),
                    UiChoice::new(tr_static("自定义"), 255),
                ],
            ),
        ),
        "hybrid" => one(
            "value",
            "显卡工作模式",
            choice(model, "gpu.hybrid_mode", vec![]),
        ),
        "battery" => one("value", "充电模式", choice(model, "battery.mode", vec![])),
        "macro" => one(
            "value",
            "宏总开关",
            Kind::Choice(vec![
                UiChoice::new(tr_static("关闭"), "off"),
                UiChoice::new(tr_static("开启"), "on"),
                UiChoice::new(tr_static("切换"), "toggle"),
            ]),
        ),
        "godmode_preset" | "quick_action" => {
            let collection = match id {
                "godmode_preset" => "presets",
                _ => "automation",
            };
            one(
                "id",
                "目标项目",
                Kind::Choice(
                    model
                        .collections
                        .get(collection)
                        .map(|items| {
                            items
                                .iter()
                                .map(|i| UiChoice::new(i.label.clone(), i.id.clone()))
                                .collect()
                        })
                        .unwrap_or_default(),
                ),
            )
        }
        "spectrum_profile" | "rgb_keyboard" => one("value", "预设编号", Kind::Number),
        "spectrum_brightness" | "white_keyboard" => one("value", "亮度级别", Kind::Number),
        "close_app" | "show_app" | "monitors_off" | "cycle_preset" => vec![],
        "its_mode" => one("value", "本机不适用", Kind::Text),
        _ => one("value", "目标状态", Kind::Choice(on_off())),
    }
}
fn trigger_parameters(id: &str, model: &UiModel) -> Vec<(String, String, Kind)> {
    let one = |key: &str, label: &str, kind: Kind| vec![(key.into(), label.into(), kind)];
    match id {
        "battery_percentage" => vec![
            (
                "comparison".into(),
                "条件".into(),
                Kind::Choice(vec![
                    UiChoice::new(tr_static("不高于"), "le"),
                    UiChoice::new(tr_static("不低于"), "ge"),
                ]),
            ),
            ("percentage".into(), "电量 · %".into(), Kind::Number),
        ],
        "power_mode" => one("value", "性能模式", choice(model, "power.mode", vec![])),
        "hybrid" => one(
            "value",
            "显卡模式",
            choice(model, "gpu.hybrid_mode", vec![]),
        ),
        "godmode_preset" => one(
            "id",
            "预设",
            Kind::Choice(
                model
                    .collections
                    .get("presets")
                    .map(|items| {
                        items
                            .iter()
                            .map(|i| UiChoice::new(i.label.clone(), i.id.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
        ),
        "process_running" | "process_stopped" => one("processes", "进程名（逗号分隔）", Kind::Text),
        "device_connected" | "device_disconnected" => {
            one("devices", "设备标识（逗号分隔）", Kind::Text)
        }
        "wifi_connected" => one("ssids", "Wi-Fi 名称（留空为任意）", Kind::Text),
        "time" => vec![
            (
                "time".into(),
                "时间 HH:MM:SS / sunrise / sunset".into(),
                Kind::Text,
            ),
            ("days".into(), "星期 0–6（逗号分隔）".into(), Kind::Text),
        ],
        "time_range" => vec![
            ("start".into(), "开始 HH:MM:SS / sunrise".into(), Kind::Text),
            ("end".into(), "结束 HH:MM:SS / sunset".into(), Kind::Text),
            ("days".into(), "星期 0–6（逗号分隔）".into(), Kind::Text),
        ],
        "periodic" => one("seconds", "间隔 · 秒", Kind::Number),
        "user_idle" => one("seconds", "空闲时长 · 秒", Kind::Number),
        _ => vec![],
    }
}
fn event_kinds() -> Vec<UiChoice> {
    [
        ("按下键盘键", "key_down"),
        ("抬起键盘键", "key_up"),
        ("延时", "delay"),
        ("按下鼠标按钮", "mouse_down"),
        ("抬起鼠标按钮", "mouse_up"),
        ("鼠标移动", "mouse_move"),
        ("滚轮", "wheel"),
        ("水平滚轮", "horizontal_wheel"),
    ]
    .iter()
    .map(|(label, id)| UiChoice::new(tr(label).into_owned(), *id))
    .collect()
}
fn filtered_action_choices(query: &str) -> Vec<UiChoice> {
    let query = query.trim().to_lowercase();
    AUTOMATION_ACTIONS
        .iter()
        .filter(|(_, label)| {
            query.is_empty()
                || label.to_lowercase().contains(&query)
                || tr(label).to_lowercase().contains(&query)
        })
        .map(|(id, label)| UiChoice::new(tr(label).into_owned(), *id))
        .collect()
}
fn quick_action_icons(current: &Value) -> Vec<UiChoice> {
    let mut choices = [
        ("默认", ""),
        ("播放", "Play"),
        ("电源", "Power"),
        ("设置", "Settings"),
        ("星标", "Star"),
        ("键盘", "Keyboard"),
        ("显示器", "Display"),
        ("闪电", "Lightning"),
    ]
    .iter()
    .map(|(label, value)| UiChoice::new(tr_static(label), *value))
    .collect::<Vec<_>>();
    if let Some(icon) = current.as_str() {
        if !choices.iter().any(|choice| choice.value == icon) {
            choices.push(UiChoice::new(tr_static("保留原有图标"), icon));
        }
    }
    choices
}
fn action_templates(selected: &[String], model: &UiModel) -> Result<Vec<Value>> {
    if selected.is_empty() {
        bail!("{}", tr_static("请先选择一个或多个动作模板。"));
    }
    let mut templates = Vec::new();
    for id in selected {
        if !AUTOMATION_ACTIONS.iter().any(|(known, _)| known == id) {
            bail!("Unknown action template");
        }
        let capability = format!("automation.step.{id}");
        if !model.can(&capability) {
            bail!("{}", model.unavailable_reason(&capability));
        }
        templates.push(json!({"type":id,"parameters":{}}));
    }
    Ok(templates)
}
fn describe_step(value: &Value) -> String {
    let id = value.get("type").and_then(Value::as_str).unwrap_or("");
    let label = AUTOMATION_ACTIONS
        .iter()
        .find(|(key, _)| *key == id)
        .map(|(_, label)| *label)
        .unwrap_or("已有动作（需兼容适配）");
    format!(
        "{}    {}",
        tr(label),
        parameter_summary(value.get("parameters"))
    )
}
fn describe_condition(value: &Value) -> String {
    let id = value.get("type").and_then(Value::as_str).unwrap_or("");
    let label = AUTOMATION_TRIGGERS
        .iter()
        .find(|(key, _)| *key == id)
        .map(|(_, label)| *label)
        .unwrap_or("已有条件（需兼容适配）");
    format!(
        "{}    {}",
        tr(label),
        parameter_summary(value.get("parameters"))
    )
}
fn parameter_summary(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_object)
        .map(|p| {
            p.values()
                .map(scalar)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .unwrap_or_default()
}
fn describe_event(value: &Value) -> String {
    let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
    let label = event_kinds()
        .into_iter()
        .find(|c| c.value.as_str() == Some(kind))
        .map(|c| c.label)
        .unwrap_or_else(|| tr_static("已有事件").into());
    format!(
        "{}    {} {}    {} {} ms",
        label,
        tr_static("键"),
        scalar(value.get("key").unwrap_or(&Value::Null)),
        tr_static("延时"),
        scalar(value.get("delay_ms").unwrap_or(&Value::Null))
    )
}

#[cfg(test)]
mod edit_compatibility_tests {
    use super::*;
    #[test]
    fn same_kind_edit_retains_opaque_legacy_after_reordering() {
        let original = json!({"type":"delay","parameters":{"milliseconds":1000},"legacy":{"Future":{"precise":"1e9999"}}});
        let mut updated = json!({"type":"delay","parameters":{"milliseconds":1250}});
        retain_legacy(&original, &mut updated, "type");
        assert_eq!(updated["legacy"], original["legacy"]);
        assert_eq!(updated["parameters"]["milliseconds"], 1250);
    }
    #[test]
    fn changing_kind_does_not_reuse_unrelated_legacy() {
        let original = json!({"kind":"key_down","legacy":{"State":{"Key":65}}});
        let mut updated = json!({"kind":"delay","delay_ms":80});
        retain_legacy(&original, &mut updated, "kind");
        assert!(updated.get("legacy").is_none());
    }
}

#[cfg(test)]
mod static_payload_tests {
    use super::*;
    #[test]
    fn large_outputs_and_imports_use_only_static_collections_and_match_request_identity() {
        use std::sync::{Arc, Mutex, mpsc::sync_channel};
        let mut model = UiModel::default();
        model
            .values
            .insert("script.output".into(), json!("stale live scalar"));
        model.collections.insert(
            "script.output".into(),
            vec![UiItem {
                value: json!("用户 stdout: 设置\n"),
                ..Default::default()
            }],
        );
        model.collections.insert(
            "keyboard.effects.imported".into(),
            vec![UiItem {
                value: json!({"request_id":42,"effects":[{"Type":"Always","unknown":17}]}),
                ..Default::default()
            }],
        );
        let (tx, _rx) = sync_channel(1);
        let bridge = UiBridge::new(Arc::new(Mutex::new(model)), tx);
        let _restore = crate::ui::i18n::TestLanguage::new("en");
        assert_eq!(
            collection_text(&bridge, "script.output"),
            "用户 stdout: 设置\n"
        );
        assert!(imported_spectrum_draft(&bridge, 41).is_none());
        assert_eq!(
            imported_spectrum_draft(&bridge, 42).unwrap()[0]["unknown"],
            17
        );
        assert!(collection_text(&bridge, "absent").is_empty());
    }
}

#[cfg(test)]
mod picker_binding_tests {
    use super::*;
    #[test]
    fn path_and_color_fields_use_semantic_selectors_not_arbitrary_text_fields() {
        assert_eq!(
            picker_kind("action.path", "run"),
            Some(PickerKind::Open(FileFilter::Executable))
        );
        assert_eq!(
            picker_kind("action.path", "play_sound"),
            Some(PickerKind::Open(FileFilter::Audio))
        );
        assert_eq!(picker_kind("backup.path", ""), Some(PickerKind::Folder));
        assert_eq!(
            picker_kind("@notification/5/ColorOverrides", ""),
            Some(PickerKind::Color)
        );
        assert_eq!(picker_kind("name", ""), None);
        assert_eq!(picker_kind("script.arguments", ""), None);
    }
    #[test]
    fn multi_line_process_lists_append_without_rewriting_other_entries() {
        let current = json!(["game.exe", "D:\\Games\\First.exe"]);
        let selected = selected_draft(&current, "new.exe".into(), true);
        assert_eq!(
            selected,
            json!(["game.exe", "D:\\Games\\First.exe", "new.exe"])
        );
        assert_eq!(selected_draft(&selected, "NEW.exe".into(), true), selected);
        assert_eq!(
            selected_draft(&current, "用户 新路径".into(), false),
            json!("用户 新路径")
        );
    }
}

#[cfg(test)]
mod arguments_editor_tests {
    use super::*;
    #[test]
    fn load_requires_actual_complete_snapshot_and_never_copies_legacy_lines() {
        assert!(arguments_state(&json!({"minimized":true})).is_err());
        let state=arguments_state(&json!({"minimized":true,"read_only":true,"inert_legacy_count":3,"revision":"hash","secret_legacy":"private"})).unwrap();
        assert_eq!(state["inert_legacy_count"], 3);
        assert!(state.get("secret_legacy").is_none());
        assert_eq!(state["loaded"], true);
        use std::sync::{Arc, Mutex, mpsc::sync_channel};
        let mut model = UiModel::default();
        model.values.insert(
            "startup.arguments".into(),
            json!({"minimized":true,"read_only":true,"inert_legacy_count":3,"revision":"hash"}),
        );
        assert!(
            model
                .realtime_snapshot()
                .value("startup.arguments")
                .is_null()
        );
        let (tx, _rx) = sync_channel(1);
        let bridge = UiBridge::new(Arc::new(Mutex::new(model)), tx);
        assert_eq!(
            arguments_state(&startup_arguments_snapshot(&bridge)).unwrap(),
            state
        );
    }
    #[test]
    fn save_keeps_original_revision_and_only_two_preferences_and_confirms_readonly_to_normal() {
        let state=arguments_state(&json!({"minimized":false,"read_only":true,"inert_legacy_count":0,"revision":"original"})).unwrap();
        let drafts = BTreeMap::from([
            ("startup.minimized".into(), json!(true)),
            ("startup.read_only".into(), json!(false)),
            ("revision".into(), json!("forged")),
            ("unknown".into(), json!(17)),
        ]);
        let payload = arguments_payload(&state, &drafts).unwrap();
        assert_eq!(
            payload,
            json!({"minimized":true,"read_only":false,"expected_revision":"original"})
        );
        assert!(arguments_need_confirmation(&state, &payload));
        assert!(!arguments_need_confirmation(
            &state,
            &json!({"read_only":true})
        ));
        assert!(arguments_payload(&json!({"loaded":false}), &drafts).is_err());
    }
}

#[cfg(test)]
mod recording_cancel_tests {
    use super::*;
    #[test]
    fn stop_ack_before_late_start_cannot_reopen_recording_or_close_early() {
        let mut stop = RecordingStop {
            inflight: Some(9),
            requested: true,
            close_requested: true,
            ..Default::default()
        };
        assert!(!stop.can_close(true, false));
        stop.acknowledge(true);
        assert!(!stop.acknowledge_start(true));
        assert!(!stop.can_close(true, false));
        assert!(stop.needs_final_events());
        assert!(!stop.can_close(false, false));
        stop.final_events_loaded = true;
        assert!(stop.can_close(false, false));
    }
    #[test]
    fn failed_actual_cleanup_keeps_retry_gate_and_never_confirms_close() {
        let mut stop = RecordingStop {
            inflight: Some(9),
            requested: true,
            close_requested: true,
            ..Default::default()
        };
        stop.acknowledge(false);
        assert!(stop.busy() && stop.cleanup_uncertain);
        assert!(!stop.can_close(false, false));
        assert!(stop.acknowledge_start(true));
        stop.inflight = Some(10);
        assert!(!stop.can_close(false, false));
        stop.acknowledge(true);
        assert!(stop.needs_final_events());
        stop.final_events_loaded = true;
        assert!(!stop.busy());
        assert!(stop.can_close(false, false));
    }
    #[test]
    fn prep_cancel_does_not_read_or_clear_an_existing_macro_draft() {
        let mut stop = RecordingStop {
            requested: true,
            close_requested: true,
            ..Default::default()
        };
        assert!(!stop.acknowledge_start(false));
        stop.acknowledge(true);
        assert!(!stop.needs_final_events());
        assert!(stop.can_close(false, false));
    }
    #[test]
    fn admission_and_cancelled_start_are_not_actual_stop_acknowledgments() {
        let mut stop = RecordingStop {
            inflight: Some(9),
            requested: true,
            close_requested: true,
            ..Default::default()
        };
        assert!(!stop.acknowledge_start(false));
        assert!(!stop.confirmed);
        assert!(!stop.can_close(false, false));
    }
}

#[cfg(test)]
mod recording_collection_tests {
    use super::*;
    use std::sync::{Arc, Mutex, mpsc::sync_channel};
    #[test]
    fn final_events_require_a_new_coherent_generation_and_keep_legacy_nodes() {
        let (tx, _rx) = sync_channel(1);
        let mut model = UiModel::default();
        model
            .values
            .insert("macro.recording.revision".into(), json!(2));
        model.collections.insert(
            "macro.recording".into(),
            vec![UiItem {
                id: "recording-1".into(),
                value: json!([{"kind":"keyboard_down","legacy":{"Future":17}}]),
                detail: "ESC".into(),
                ..Default::default()
            }],
        );
        let bridge = UiBridge::new(Arc::new(Mutex::new(model)), tx);
        assert!(recorded_events(&bridge, 1).is_none());
        bridge
            .model
            .lock()
            .unwrap()
            .collections
            .get_mut("macro.recording")
            .unwrap()[0]
            .id = "recording-2".into();
        let (revision, events, reason) = recorded_events(&bridge, 1).unwrap();
        assert_eq!(revision, 2);
        assert_eq!(events[0]["legacy"]["Future"], 17);
        assert_eq!(reason, "ESC");
        assert!(recorded_events(&bridge, revision).is_none());
    }
}

#[cfg(test)]
mod automation_convenience_tests {
    use super::*;
    #[test]
    fn searches_only_catalog_labels_and_keeps_stable_ids() {
        let _locale = crate::ui::i18n::TestLanguage::new("en");
        assert!(filtered_action_choices("power_mode").is_empty());
        assert!(
            filtered_action_choices("power mode")
                .iter()
                .any(|choice| choice.value == "power_mode")
        );
        assert!(
            filtered_action_choices("性能模式")
                .iter()
                .any(|choice| choice.value == "power_mode")
        );
    }
    #[test]
    fn batch_templates_validate_every_capability_before_returning_and_preserve_order() {
        let mut model = UiModel::default();
        model.capabilities.insert(
            "automation.step.delay".into(),
            UiCapability {
                available: true,
                ..Default::default()
            },
        );
        assert!(action_templates(&["delay".into(), "monitors_off".into()], &model).is_err());
        model.capabilities.insert(
            "automation.step.monitors_off".into(),
            UiCapability {
                available: true,
                ..Default::default()
            },
        );
        let templates = action_templates(&["monitors_off".into(), "delay".into()], &model).unwrap();
        assert_eq!(templates[0]["type"], "monitors_off");
        assert_eq!(templates[1]["type"], "delay");
        assert_eq!(templates[1]["parameters"], json!({}));
    }
    #[test]
    fn legacy_icon_names_are_retained_without_claiming_a_wpf_glyph_mapping() {
        let choices = quick_action_icons(&json!("OriginalCustomSymbol24"));
        assert!(
            choices
                .iter()
                .any(|choice| choice.value == "OriginalCustomSymbol24")
        );
        assert!(choices.iter().any(|choice| choice.value == ""));
        assert!(choices.iter().any(|choice| choice.value == "Lightning"));
    }
}

#[cfg(test)]
mod first_language_tests {
    use super::*;
    #[test]
    fn first_launch_save_contains_only_the_explicit_supported_selection() {
        assert_eq!(
            first_language_payload(&json!("en")).unwrap(),
            json!({"appearance.language":"en"})
        );
        assert_eq!(
            first_language_payload(&json!("zh-Hans")).unwrap(),
            json!({"appearance.language":"zh-Hans"})
        );
        assert!(first_language_payload(&json!("en-AU")).is_err());
        assert!(first_language_payload(&Value::Null).is_err());
    }
}
