//! A normal local inventory dialog. It consumes one cached metadata document;
//! no Win32 message path performs a device query or filesystem operation.
use super::native::*;
use crate::ui_model::*;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    mem::zeroed,
    ptr::{null, null_mut},
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::*, HiDpi::GetDpiForWindow, Input::KeyboardAndMouse::EnableWindow,
        WindowsAndMessaging::*,
    },
};

const GROUPS: i32 = 20;
const BODY: i32 = 21;
const PRIVATE: i32 = 22;
const REFRESH: i32 = 23;
const COPY: i32 = 24;
const COPY_ALL: i32 = 25;
const CLOSE: i32 = 2;
struct Dialog {
    bridge: UiBridge,
    hwnd: HWND,
    font: HFONT,
    brush: HBRUSH,
    dpi: i32,
    groups: Vec<Value>,
    selected: usize,
    show_private: bool,
    revision: u64,
    pending: Option<u64>,
    status: String,
    destroy: bool,
    started: Instant,
}

pub(super) fn format_group(group: &Value, show_private: bool) -> String {
    let mut output = super::i18n::tr(
        group
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("设备信息"),
    )
    .into_owned();
    output.push_str("\r\n\r\n");
    if let Some(error) = group.get("error").and_then(Value::as_str) {
        output.push_str(&if super::i18n::language() == "en" {
            format!("Read failed: {error}\r\n")
        } else {
            format!("读取失败：{error}\r\n")
        });
    }
    if let Some(rows) = group.get("rows").and_then(Value::as_array) {
        for row in rows {
            let label = row.get("label").and_then(Value::as_str).unwrap_or("");
            let value =
                if row.get("sensitive").and_then(Value::as_bool) == Some(true) && !show_private {
                    "••••••（已隐藏）"
                } else {
                    row.get("value").and_then(Value::as_str).unwrap_or("—")
                };
            if row.get("heading").and_then(Value::as_bool) == Some(true) {
                output.push_str(&format!("\r\n{label}\r\n"));
            } else {
                output.push_str(&format!("{}: {value}\r\n", super::i18n::tr(label)));
            }
        }
    }
    output
}
pub(super) fn open(parent: HWND, bridge: UiBridge) -> Result<()> {
    unsafe {
        let class = wide("Jiks.Native.DeviceInformation");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: GetModuleHandleW(null()),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: GetStockObject(WHITE_BRUSH) as HBRUSH,
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            bail!("设备信息窗口注册失败：{}", GetLastError());
        }
        let dpi = GetDpiForWindow(parent).max(96);
        let state = Box::new(RefCell::new(Dialog {
            bridge,
            hwnd: null_mut(),
            font: font(dpi, 15, false),
            brush: CreateSolidBrush(paper()),
            dpi: dpi as i32,
            groups: vec![],
            selected: 0,
            show_private: false,
            revision: u64::MAX,
            pending: None,
            status: "正在读取本机设备信息…".into(),
            destroy: false,
            started: Instant::now(),
        }));
        let mut rect: RECT = zeroed();
        GetWindowRect(parent, &mut rect);
        let width = (950 * dpi as i32 / 96)
            .min(GetSystemMetrics(SM_CXSCREEN) - 40)
            .max(550);
        let height = (720 * dpi as i32 / 96)
            .min(GetSystemMetrics(SM_CYSCREEN) - 70)
            .max(420);
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class.as_ptr(),
            wide(&super::i18n::tr("设备信息 · 机控")).as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            (rect.left + (rect.right - rect.left - width) / 2).max(0),
            (rect.top + (rect.bottom - rect.top - height) / 2).max(0),
            width,
            height,
            parent,
            null_mut(),
            GetModuleHandleW(null()),
            (&*state as *const RefCell<Dialog>).cast(),
        );
        if hwnd.is_null() {
            bail!("无法打开设备信息：{}", GetLastError());
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
        drop(state);
        Ok(())
    }
}
impl Drop for Dialog {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.font);
            DeleteObject(self.brush);
        }
    }
}
impl Dialog {
    fn s(&self, n: i32) -> i32 {
        n * self.dpi / 96
    }
    unsafe fn child(&self, class: &str, title: &str, style: u32, id: i32) -> HWND {
        unsafe {
            let hwnd = CreateWindowExW(
                0,
                wide(class).as_ptr(),
                wide(&if class == "BUTTON" {
                    super::i18n::tr(title)
                } else {
                    std::borrow::Cow::Borrowed(title)
                })
                .as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                0,
                0,
                0,
                0,
                self.hwnd,
                id as usize as _,
                GetModuleHandleW(null()),
                null(),
            );
            SendMessageW(hwnd, WM_SETFONT, self.font as usize, 1);
            hwnd
        }
    }
    unsafe fn create(&mut self) {
        unsafe {
            self.child(
                "LISTBOX",
                "",
                WS_TABSTOP | WS_VSCROLL | LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32,
                GROUPS,
            );
            self.child(
                "EDIT",
                "",
                WS_TABSTOP
                    | WS_VSCROLL
                    | ES_MULTILINE as u32
                    | ES_READONLY as u32
                    | ES_AUTOVSCROLL as u32,
                BODY,
            );
            self.child(
                "BUTTON",
                "显示序列号与 MAC 地址",
                WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                PRIVATE,
            );
            self.child("BUTTON", "刷新", WS_TABSTOP, REFRESH);
            self.child("BUTTON", "复制本页", WS_TABSTOP, COPY);
            self.child("BUTTON", "复制全部", WS_TABSTOP, COPY_ALL);
            self.child(
                "BUTTON",
                "关闭",
                WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
                CLOSE,
            );
            self.child("STATIC", "", SS_LEFT as u32, 26);
            self.layout();
            self.refresh();
            SetTimer(self.hwnd, 1, 200, None);
        }
    }
    unsafe fn layout(&self) {
        unsafe {
            let mut rect: RECT = zeroed();
            GetClientRect(self.hwnd, &mut rect);
            let w = rect.right * 96 / self.dpi;
            let h = rect.bottom * 96 / self.dpi;
            let nav = if w < 680 { 120 } else { 165 };
            let positions = [
                (GROUPS, 16, 18, nav, h - 136),
                (BODY, nav + 30, 18, (w - nav - 46).max(100), h - 136),
                (PRIVATE, 16, h - 103, w - 32, 26),
                (REFRESH, 16, h - 69, 75, 30),
                (COPY, 99, h - 69, 94, 30),
                (COPY_ALL, 201, h - 69, 94, 30),
                (CLOSE, w - 94, h - 69, 78, 30),
                (26, 16, h - 31, w - 32, 24),
            ];
            for (id, x, y, width, height) in positions {
                MoveWindow(
                    GetDlgItem(self.hwnd, id),
                    self.s(x),
                    self.s(y),
                    self.s(width),
                    self.s(height),
                    1,
                );
            }
        }
    }
    unsafe fn refresh(&mut self) {
        unsafe {
            if self.pending.is_some() {
                return;
            }
            let id = request_id();
            match send(
                &self.bridge,
                UiAction::Execute {
                    request_id: id,
                    command: "device.information.refresh".into(),
                    payload: json!({}),
                },
            ) {
                Ok(()) => {
                    self.pending = Some(id);
                    self.status = "正在读取本机元数据；不采样温度、不上传信息。".into();
                }
                Err(error) => self.status = error.to_string(),
            }
            EnableWindow(
                GetDlgItem(self.hwnd, REFRESH),
                self.pending.is_none() as i32,
            );
            text(GetDlgItem(self.hwnd, 26), &self.status);
        }
    }
    unsafe fn render_body(&self) {
        unsafe {
            let body = self
                .groups
                .get(self.selected)
                .map(|g| format_group(g, self.show_private))
                .unwrap_or_else(|| "设备信息尚未读取。请稍候，或点击刷新重试。".into());
            text(GetDlgItem(self.hwnd, BODY), &body);
        }
    }
    unsafe fn tick(&mut self) {
        unsafe {
            let (groups, result, revision, shutdown) = {
                let model = self.bridge.model.lock().unwrap_or_else(|p| p.into_inner());
                let groups = if model.metadata_revision != self.revision {
                    Some(
                        model
                            .collections
                            .get("device.information")
                            .and_then(|items| items.first())
                            .and_then(|i| i.value.get("groups"))
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default(),
                    )
                } else {
                    None
                };
                (
                    groups,
                    self.pending
                        .and_then(|id| model.results.iter().find(|r| r.request_id == id).cloned()),
                    model.metadata_revision,
                    model.shutdown_ready,
                )
            };
            if let Some(groups) = groups {
                self.revision = revision;
                if self.groups != groups {
                    self.groups = groups;
                    self.selected = self.selected.min(self.groups.len().saturating_sub(1));
                    let list = GetDlgItem(self.hwnd, GROUPS);
                    SendMessageW(list, LB_RESETCONTENT, 0, 0);
                    for group in &self.groups {
                        SendMessageW(
                            list,
                            LB_ADDSTRING,
                            0,
                            wide(&super::i18n::tr(
                                group.get("title").and_then(Value::as_str).unwrap_or("设备"),
                            ))
                            .as_ptr() as isize,
                        );
                    }
                    SendMessageW(list, LB_SETCURSEL, self.selected, 0);
                    self.render_body();
                }
            }
            if let Some(result) = result {
                self.pending = None;
                self.status = if result.ok {
                    "仅保留在本机。复制内容遵循当前隐私显示开关。".into()
                } else {
                    result.message
                };
                EnableWindow(GetDlgItem(self.hwnd, REFRESH), 1);
                text(GetDlgItem(self.hwnd, 26), &self.status);
            }
            if shutdown
                || (self.bridge.smoke_exercise_reentrancy
                    && self.started.elapsed().as_millis() > 200)
            {
                self.destroy = true;
            }
        }
    }
    unsafe fn copy(&self, all: bool) {
        unsafe {
            let value = if all {
                self.groups
                    .iter()
                    .map(|g| format_group(g, self.show_private))
                    .collect::<Vec<_>>()
                    .join("\r\n")
            } else {
                self.groups
                    .get(self.selected)
                    .map(|g| format_group(g, self.show_private))
                    .unwrap_or_default()
            };
            match copy_text(self.hwnd, &value) {
                Ok(()) => text(
                    GetDlgItem(self.hwnd, 26),
                    &super::i18n::tr("已复制所选项目"),
                ),
                Err(error) => text(
                    GetDlgItem(self.hwnd, 26),
                    &format!("{}：{error:#}", super::i18n::tr("复制未完成")),
                ),
            }
        }
    }
}
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    unsafe {
        if msg == WM_NCCREATE {
            let create = &*(lp as *const CREATESTRUCTW);
            let raw = create.lpCreateParams as *mut RefCell<Dialog>;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
            (*raw).borrow_mut().hwnd = hwnd;
        }
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RefCell<Dialog>;
        if raw.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        if msg == WM_NCDESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let destroy = {
            let Ok(mut d) = (*raw).try_borrow_mut() else {
                return DefWindowProcW(hwnd, msg, wp, lp);
            };
            match msg {
                WM_CREATE => d.create(),
                WM_SIZE => d.layout(),
                WM_TIMER => d.tick(),
                WM_GETMINMAXINFO => {
                    let info = &mut *(lp as *mut MINMAXINFO);
                    info.ptMinTrackSize.x = d.s(550);
                    info.ptMinTrackSize.y = d.s(420);
                }
                WM_CLOSE => d.destroy = true,
                WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
                    SetTextColor(wp as HDC, ink());
                    SetBkColor(wp as HDC, paper());
                    return d.brush as isize;
                }
                WM_COMMAND => match (wp & 0xffff) as i32 {
                    GROUPS if ((wp >> 16) & 0xffff) == LBN_SELCHANGE as usize => {
                        let index = SendMessageW(GetDlgItem(hwnd, GROUPS), LB_GETCURSEL, 0, 0);
                        if index >= 0 {
                            d.selected = index as usize;
                            d.render_body();
                        }
                    }
                    PRIVATE => {
                        d.show_private = SendMessageW(GetDlgItem(hwnd, PRIVATE), BM_GETCHECK, 0, 0)
                            == BST_CHECKED as isize;
                        d.render_body();
                    }
                    REFRESH => d.refresh(),
                    COPY => d.copy(false),
                    COPY_ALL => d.copy(true),
                    CLOSE => d.destroy = true,
                    _ => return DefWindowProcW(hwnd, msg, wp, lp),
                },
                _ => return DefWindowProcW(hwnd, msg, wp, lp),
            }
            d.destroy
        };
        if destroy && IsWindow(hwnd) != 0 {
            DestroyWindow(hwnd);
        }
        0
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copied_inventory_masks_private_values_until_explicitly_revealed() {
        let group = json!({"title":"磁盘","rows":[{"label":"型号","value":"NVMe"},{"label":"序列号","value":"SECRET123","sensitive":true}]});
        let masked = format_group(&group, false);
        assert!(masked.contains("NVMe"));
        assert!(!masked.contains("SECRET123"));
        assert!(format_group(&group, true).contains("SECRET123"));
    }
}
