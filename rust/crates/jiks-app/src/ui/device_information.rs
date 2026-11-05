//! A normal local inventory dialog. It consumes one cached metadata document;
//! no Win32 message path performs a device query or filesystem operation.
use super::{a11y, native::*};
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
    heading: HFONT,
    brush: HBRUSH,
    surface_brush: HBRUSH,
    overview: bool,
    overview_cards: Vec<(HWND, HWND)>,
    scroll: i32,
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
fn summary_group(group: &Value, show_private: bool) -> String {
    let Some(rows) = group.get("rows").and_then(Value::as_array) else {
        return super::i18n::tr("暂无数据").into_owned();
    };
    if let Some(error) = group.get("error").and_then(Value::as_str) {
        return format!("{}: {error}", super::i18n::tr("读取失败"));
    }
    rows.iter()
        .filter(|row| row.get("heading").and_then(Value::as_bool) != Some(true))
        .filter(|row| {
            !row.get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
        })
        .take(2)
        .map(|row| {
            let label = row.get("label").and_then(Value::as_str).unwrap_or("");
            let value =
                if row.get("sensitive").and_then(Value::as_bool) == Some(true) && !show_private {
                    "••••••"
                } else {
                    row.get("value").and_then(Value::as_str).unwrap_or("—")
                };
            let value = if value.chars().count() > 80 {
                format!("{}…", value.chars().take(79).collect::<String>())
            } else {
                value.to_owned()
            };
            format!("{}：{value}", super::i18n::tr(label))
        })
        .collect::<Vec<_>>()
        .join("\r\n\r\n")
}
pub(super) fn open(parent: HWND, bridge: UiBridge) -> Result<()> {
    let _accessibility = a11y::UiThread::enter()?;
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
            font: font(dpi, 14, false),
            heading: font(dpi, 17, true),
            brush: CreateSolidBrush(paper()),
            surface_brush: CreateSolidBrush(surface()),
            overview: true,
            overview_cards: vec![],
            scroll: 0,
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
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_VSCROLL,
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
            DeleteObject(self.heading);
            DeleteObject(self.brush);
            DeleteObject(self.surface_brush);
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
            if class == "STATIC" {
                install_label_paint(hwnd);
            }
            let en = super::i18n::language() == "en";
            let label = |zh, english| if en { english } else { zh };
            let (semantic_id, name, help) = match id {
                GROUPS => (
                    "device-information.groups",
                    Some(label("设备信息分类", "Device information categories")),
                    None,
                ),
                BODY => (
                    "device-information.body",
                    Some(label("设备信息内容", "Device information details")),
                    Some(label(
                        "只读。序列号与 MAC 地址默认隐藏；复制内容遵循隐私显示开关。",
                        "Read-only. Serial numbers and MAC addresses are hidden by default; copied content follows the privacy toggle.",
                    )),
                ),
                PRIVATE => (
                    "device-information.reveal-private",
                    Some(label(
                        "显示序列号与 MAC 地址",
                        "Show serial numbers and MAC addresses",
                    )),
                    Some(label(
                        "勾选后，设备信息正文与复制内容将包含这些隐私值。",
                        "When checked, the device details and copied content include these private values.",
                    )),
                ),
                REFRESH => (
                    "device-information.refresh",
                    Some(label("刷新设备信息", "Refresh device information")),
                    None,
                ),
                COPY => (
                    "device-information.copy-current",
                    Some(label(
                        "复制本页设备信息",
                        "Copy current device information page",
                    )),
                    None,
                ),
                COPY_ALL => (
                    "device-information.copy-all",
                    Some(label("复制全部设备信息", "Copy all device information")),
                    None,
                ),
                CLOSE => (
                    "device-information.close",
                    Some(label("关闭设备信息窗口", "Close device information window")),
                    None,
                ),
                26 => (
                    "device-information.status",
                    None,
                    Some(label(
                        "设备信息读取或复制状态",
                        "Device information read or copy status",
                    )),
                ),
                _ => return hwnd,
            };
            // Keep inventory text in the native read-only EDIT Value provider.
            // No raw or unmasked metadata is copied into Name/HelpText.
            a11y::annotate(hwnd, semantic_id, name, help);
            if id == BODY {
                presentation_flag(hwnd, "Jiks.Surface", true);
            }
            if id == CLOSE {
                presentation_flag(hwnd, "Jiks.Primary", true);
            }
            if id == 26 {
                presentation_flag(hwnd, "Jiks.Muted", true);
            }
            hwnd
        }
    }
    unsafe fn create(&mut self) {
        unsafe {
            a11y::annotate(self.hwnd, "device-information.root", None, None);
            self.child(
                "COMBOBOX",
                "",
                WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
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
            self.child("BUTTON", "刷新", WS_TABSTOP | BS_OWNERDRAW as u32, REFRESH);
            self.child("BUTTON", "复制本页", WS_TABSTOP | BS_OWNERDRAW as u32, COPY);
            self.child(
                "BUTTON",
                "复制全部",
                WS_TABSTOP | BS_OWNERDRAW as u32,
                COPY_ALL,
            );
            self.child("BUTTON", "关闭", WS_TABSTOP | BS_OWNERDRAW as u32, CLOSE);
            self.child("STATIC", "", SS_LEFT as u32, 26);
            let heading = self.child("STATIC", &super::i18n::tr("设备信息"), SS_LEFT as u32, 30);
            SendMessageW(heading, WM_SETFONT, self.heading as usize, 0);
            a11y::annotate(heading, "device-information.title", None, None);
            self.child(
                "STATIC",
                &super::i18n::tr("设备信息分类"),
                SS_LEFT as u32,
                31,
            );
            for index in 0..6 {
                let title = self.child("STATIC", "", SS_LEFT as u32, 100 + index * 2);
                SendMessageW(title, WM_SETFONT, self.heading as usize, 0);
                let body = self.child("STATIC", "", SS_LEFT as u32, 101 + index * 2);
                a11y::annotate(
                    title,
                    &format!("device-information.overview.{}.title", index),
                    None,
                    None,
                );
                a11y::annotate(
                    body,
                    &format!("device-information.overview.{}.body", index),
                    None,
                    None,
                );
                presentation_flag(title, "Jiks.Surface", true);
                presentation_flag(body, "Jiks.Surface", true);
                self.overview_cards.push((title, body));
            }
            self.layout();
            self.refresh();
            SetTimer(self.hwnd, 1, 200, None);
        }
    }
    unsafe fn layout(&mut self) {
        unsafe {
            let mut rect: RECT = zeroed();
            GetClientRect(self.hwnd, &mut rect);
            let w = rect.right * 96 / self.dpi;
            let h = rect.bottom * 96 / self.dpi;
            let positions = [
                (30, 20, 18, w - 40, 30),
                (31, 20, 58, 140, 26),
                (GROUPS, 166, 54, (w - 186).max(100), 300),
                (BODY, 32, 108, (w - 64).max(100), (h - 250).max(80)),
                (PRIVATE, 20, h - 103, w - 40, 26),
                (REFRESH, 20, h - 69, 75, 32),
                (COPY, 103, h - 69, 94, 32),
                (COPY_ALL, 205, h - 69, 94, 32),
                (CLOSE, w - 98, h - 69, 78, 32),
                (26, 20, h - 31, w - 40, 24),
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
            let columns = if w >= 800 { 3 } else { 2 };
            let rows = (self.overview_cards.len() as i32 + columns - 1) / columns;
            let card_width = (w - 40 - (columns - 1) * 12) / columns;
            let viewport = (h - 226).max(80);
            let card_height = 204;
            let content = rows * (card_height + 12) - 12;
            let maximum = if self.overview {
                (content - viewport).max(0)
            } else {
                0
            };
            self.scroll = self.scroll.clamp(0, maximum);
            let info = SCROLLINFO {
                cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: if self.overview { content } else { 0 },
                nPage: viewport as u32,
                nPos: self.scroll,
                nTrackPos: 0,
            };
            SetScrollInfo(self.hwnd, SB_VERT, &info, 1);
            for (index, (title, body)) in self.overview_cards.iter().enumerate() {
                let x = 20 + (index as i32 % columns) * (card_width + 12);
                let y = 96 + (index as i32 / columns) * (card_height + 12) - self.scroll;
                for (hwnd, offset, height) in [(*title, 16, 28), (*body, 54, 136)] {
                    MoveWindow(
                        hwnd,
                        self.s(x + 16),
                        self.s(y + offset),
                        self.s(card_width - 32),
                        self.s(height),
                        1,
                    );
                    let visible = clip_to_viewport(
                        hwnd,
                        y + offset,
                        card_width - 32,
                        height,
                        90,
                        h - 126,
                        self.dpi as u32,
                    );
                    ShowWindow(
                        hwnd,
                        if self.overview && visible {
                            SW_SHOWNA
                        } else {
                            SW_HIDE
                        },
                    );
                }
            }
            ShowWindow(
                GetDlgItem(self.hwnd, BODY),
                if self.overview { SW_HIDE } else { SW_SHOWNA },
            );
            InvalidateRect(self.hwnd, null(), 1);
        }
    }
    unsafe fn paint(&self, dc: HDC) {
        unsafe {
            let mut rect: RECT = zeroed();
            GetClientRect(self.hwnd, &mut rect);
            let w = rect.right * 96 / self.dpi;
            let h = rect.bottom * 96 / self.dpi;
            let saved = SaveDC(dc);
            FillRect(dc, &rect, self.brush);
            IntersectClipRect(dc, self.s(16), self.s(90), self.s(w - 16), self.s(h - 126));
            if self.overview {
                let columns = if w >= 800 { 3 } else { 2 };
                let card_width = (w - 40 - (columns - 1) * 12) / columns;
                for index in 0..self.overview_cards.len() as i32 {
                    let x = 20 + (index % columns) * (card_width + 12);
                    let y = 96 + (index / columns) * 216 - self.scroll;
                    paint_card(
                        dc,
                        &RECT {
                            left: self.s(x),
                            top: self.s(y),
                            right: self.s(x + card_width),
                            bottom: self.s(y + 204),
                        },
                        self.dpi as u32,
                    );
                }
            } else {
                paint_card(
                    dc,
                    &RECT {
                        left: self.s(20),
                        top: self.s(96),
                        right: self.s(w - 20),
                        bottom: self.s(h - 130),
                    },
                    self.dpi as u32,
                );
            }
            RestoreDC(dc, saved);
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
    unsafe fn render_body(&mut self) {
        unsafe {
            let body = self
                .groups
                .get(self.selected)
                .map(|g| format_group(g, self.show_private))
                .unwrap_or_else(|| "设备信息尚未读取。请稍候，或点击刷新重试。".into());
            text(GetDlgItem(self.hwnd, BODY), &body);
            for (index, key) in ["device", "cpu", "gpu", "memory", "disks", "bios"]
                .iter()
                .enumerate()
            {
                if let Some((title, body)) = self.overview_cards.get(index) {
                    let group = self
                        .groups
                        .iter()
                        .find(|group| group.get("id").and_then(Value::as_str) == Some(*key));
                    let caption = group
                        .and_then(|group| group.get("title"))
                        .and_then(Value::as_str)
                        .unwrap_or(["设备", "CPU", "显卡", "内存", "磁盘", "BIOS"][index]);
                    text(*title, &super::i18n::tr(caption));
                    text(
                        *body,
                        &group
                            .map(|group| summary_group(group, self.show_private))
                            .unwrap_or_else(|| super::i18n::tr("暂无数据").into_owned()),
                    );
                }
            }
            self.layout();
            #[cfg(debug_assertions)]
            super::capture_fixture::queue(self.hwnd, &self.bridge, "device-information");
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
                    SendMessageW(list, CB_RESETCONTENT, 0, 0);
                    SendMessageW(
                        list,
                        CB_ADDSTRING,
                        0,
                        wide(&super::i18n::tr("概览")).as_ptr() as isize,
                    );
                    for group in &self.groups {
                        SendMessageW(
                            list,
                            CB_ADDSTRING,
                            0,
                            wide(&super::i18n::tr(
                                group.get("title").and_then(Value::as_str).unwrap_or("设备"),
                            ))
                            .as_ptr() as isize,
                        );
                    }
                    SendMessageW(
                        list,
                        CB_SETCURSEL,
                        if self.overview { 0 } else { self.selected + 1 },
                        0,
                    );
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
            let value = if all || self.overview {
                self.groups
                    .iter()
                    .filter(|g| {
                        all || ["device", "cpu", "gpu", "memory", "disks", "bios"]
                            .contains(&g.get("id").and_then(Value::as_str).unwrap_or(""))
                    })
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
        if let Some(result) = presentation_message(msg, wp, lp) {
            return result;
        }
        // DefWindowProc handles WM_PRINT by synchronously sending this window
        // WM_ERASEBKGND and WM_PRINTCLIENT. Keep Dialog unborrowed so those
        // callbacks can paint the actual overview surfaces.
        if msg == WM_PRINT {
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
                WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
                    let on_surface = lp as HWND == GetDlgItem(hwnd, BODY)
                        || d.overview_cards
                            .iter()
                            .any(|(title, body)| *title == lp as HWND || *body == lp as HWND);
                    SetTextColor(
                        wp as HDC,
                        if lp as HWND == GetDlgItem(hwnd, 26) {
                            muted()
                        } else {
                            ink()
                        },
                    );
                    SetBkMode(wp as HDC, TRANSPARENT as i32);
                    SetBkColor(wp as HDC, if on_surface { surface() } else { paper() });
                    return (if on_surface { d.surface_brush } else { d.brush }) as isize;
                }
                WM_ERASEBKGND => {
                    let mut rect: RECT = zeroed();
                    GetClientRect(hwnd, &mut rect);
                    FillRect(wp as HDC, &rect, d.brush);
                    d.paint(wp as HDC);
                    return 1;
                }
                WM_PAINT => {
                    let mut ps: PAINTSTRUCT = zeroed();
                    let dc = BeginPaint(hwnd, &mut ps);
                    d.paint(dc);
                    EndPaint(hwnd, &ps);
                    return 0;
                }
                WM_PRINTCLIENT => {
                    d.paint(wp as HDC);
                    return 0;
                }
                WM_DRAWITEM => {
                    let item = &*(lp as *const DRAWITEMSTRUCT);
                    if item.CtlID == CLOSE as u32 {
                        paint_primary_button(item);
                    } else {
                        paint_button(item, false);
                    }
                    return 1;
                }
                WM_MOUSEWHEEL => {
                    d.scroll -= ((wp >> 16) as u16 as i16 as i32) / 120 * 64;
                    d.layout();
                }
                WM_VSCROLL => {
                    d.scroll = match (wp & 0xffff) as i32 {
                        SB_LINEUP => d.scroll - 32,
                        SB_LINEDOWN => d.scroll + 32,
                        SB_PAGEUP => d.scroll - 180,
                        SB_PAGEDOWN => d.scroll + 180,
                        SB_THUMBTRACK | SB_THUMBPOSITION => {
                            let mut si: SCROLLINFO = zeroed();
                            si.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
                            si.fMask = SIF_TRACKPOS;
                            GetScrollInfo(hwnd, SB_VERT, &mut si);
                            si.nTrackPos
                        }
                        _ => d.scroll,
                    };
                    d.layout();
                }
                WM_COMMAND => match (wp & 0xffff) as i32 {
                    GROUPS if ((wp >> 16) & 0xffff) == CBN_SELCHANGE as usize => {
                        let index = SendMessageW(GetDlgItem(hwnd, GROUPS), CB_GETCURSEL, 0, 0);
                        if index >= 0 {
                            d.overview = index == 0;
                            if index > 0 {
                                d.selected = index as usize - 1;
                            }
                            d.scroll = 0;
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
    #[test]
    fn overview_summary_obeys_the_same_privacy_toggle_as_copy_and_details() {
        let group = json!({"id":"bios","title":"BIOS","rows":[
            {"label":"设备序列号","value":"PRIVATE-OVERVIEW-SERIAL","sensitive":true},
            {"label":"BIOS 版本","value":"S9CN15WW"},
            {"label":"插槽信息","heading":true,"value":"ignored heading"}
        ]});
        let masked = summary_group(&group, false);
        assert!(!masked.contains("PRIVATE-OVERVIEW-SERIAL"));
        assert!(masked.contains("••••••") && masked.contains("S9CN15WW"));
        assert!(!masked.contains("ignored heading"));
        assert!(summary_group(&group, true).contains("PRIVATE-OVERVIEW-SERIAL"));
    }
    #[test]
    fn actual_device_wndproc_print_keeps_overview_cards_through_default_reentry() {
        let _accessibility = a11y::UiThread::enter().unwrap();
        let (actions, _receiver) = std::sync::mpsc::sync_channel(4);
        let model = std::sync::Arc::new(std::sync::Mutex::new(UiModel::default()));
        struct Fixture {
            hwnd: HWND,
            class: Vec<u16>,
            _state: Box<RefCell<Dialog>>,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                unsafe {
                    if IsWindow(self.hwnd) != 0 {
                        DestroyWindow(self.hwnd);
                    }
                    UnregisterClassW(self.class.as_ptr(), GetModuleHandleW(null()));
                }
            }
        }
        struct Canvas {
            dc: HDC,
            bitmap: HBITMAP,
            old: HGDIOBJ,
        }
        impl Drop for Canvas {
            fn drop(&mut self) {
                unsafe {
                    SelectObject(self.dc, self.old);
                    DeleteObject(self.bitmap);
                    DeleteDC(self.dc);
                }
            }
        }
        unsafe {
            let class = wide("Jiks.Test.DeviceInformationPrint");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: GetModuleHandleW(null()),
                hbrBackground: GetStockObject(WHITE_BRUSH) as HBRUSH,
                lpszClassName: class.as_ptr(),
                ..zeroed()
            };
            assert_ne!(RegisterClassW(&wc), 0);
            let state = Box::new(RefCell::new(Dialog {
                bridge: UiBridge::new(model, actions),
                hwnd: null_mut(),
                font: font(96, 14, false),
                heading: font(96, 17, true),
                brush: CreateSolidBrush(paper()),
                surface_brush: CreateSolidBrush(surface()),
                overview: true,
                overview_cards: vec![],
                scroll: 0,
                dpi: 96,
                groups: vec![],
                selected: 0,
                show_private: false,
                revision: u64::MAX,
                pending: None,
                status: String::new(),
                destroy: false,
                started: Instant::now(),
            }));
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                wide("Synthetic never-visible device print").as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_VSCROLL,
                0,
                0,
                950,
                720,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                (&*state as *const RefCell<Dialog>).cast(),
            );
            let fixture = Fixture {
                hwnd,
                class,
                _state: state,
            };
            assert!(!fixture.hwnd.is_null());
            assert_eq!(IsWindowVisible(fixture.hwnd), 0);
            let mut client: RECT = zeroed();
            GetClientRect(fixture.hwnd, &mut client);
            let screen = GetDC(null_mut());
            let dc = CreateCompatibleDC(screen);
            let bitmap = CreateCompatibleBitmap(screen, client.right, client.bottom);
            ReleaseDC(null_mut(), screen);
            assert!(!dc.is_null() && !bitmap.is_null());
            let old = SelectObject(dc, bitmap);
            assert!(!old.is_null() && old as isize != -1);
            let canvas = Canvas { dc, bitmap, old };
            let points = [(12, 160), (24, 160), (20, 160)];
            fill(canvas.dc, &client, 0x00ff00ff);
            SendMessageW(fixture.hwnd, WM_PRINTCLIENT, canvas.dc as usize, 0);
            let direct = points.map(|(x, y)| GetPixel(canvas.dc, x, y));
            assert_eq!(direct, [paper(), surface(), line()]);
            fill(canvas.dc, &client, 0x00ff00ff);
            SendMessageW(
                fixture.hwnd,
                WM_PRINT,
                canvas.dc as usize,
                (PRF_CLIENT | PRF_ERASEBKGND | PRF_CHILDREN) as isize,
            );
            let delegated = points.map(|(x, y)| GetPixel(canvas.dc, x, y));
            assert_eq!(
                delegated, direct,
                "DefWindowProc WM_PRINT must reenter the actual device WM_PRINTCLIENT without a held Dialog borrow"
            );
            assert_eq!(IsWindowVisible(fixture.hwnd), 0);
        }
    }
    #[test]
    fn never_visible_controls_expose_semantics_without_unmasking_native_inventory_value() {
        let _language = super::super::i18n::TestLanguage::new("en");
        let _accessibility = a11y::UiThread::enter().unwrap();
        let (actions, _receiver) = std::sync::mpsc::sync_channel(4);
        let model = std::sync::Arc::new(std::sync::Mutex::new(UiModel::default()));
        let bridge = UiBridge::new(model.clone(), actions);
        // Only a never-shown test-owned parent; production child creation and
        // presentation run against an inert bridge, not a device coordinator.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                wide("Synthetic device information").as_ptr(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                800,
                600,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            )
        };
        assert!(!hwnd.is_null());
        struct Hidden(Dialog);
        impl Drop for Hidden {
            fn drop(&mut self) {
                unsafe {
                    if IsWindow(self.0.hwnd) != 0 {
                        DestroyWindow(self.0.hwnd);
                    }
                }
            }
        }
        let mut fixture = Hidden(Dialog {
            bridge,
            hwnd,
            font: unsafe { font(96, 14, false) },
            heading: unsafe { font(96, 17, true) },
            brush: unsafe { CreateSolidBrush(paper()) },
            surface_brush: unsafe { CreateSolidBrush(surface()) },
            overview: true,
            overview_cards: vec![],
            scroll: 0,
            dpi: 96,
            groups: vec![
                json!({"title":"Disk","rows":[{"label":"Serial","value":"SYNTHETIC_PRIVATE_SERIAL","sensitive":true}]}),
            ],
            selected: 0,
            show_private: false,
            revision: u64::MAX,
            pending: None,
            status: String::new(),
            destroy: false,
            started: Instant::now(),
        });
        unsafe {
            fixture.0.create();
            fixture.0.render_body();
        }
        let ids = [GROUPS, BODY, PRIVATE, REFRESH, COPY, COPY_ALL, CLOSE, 26];
        let children = ids.map(|id| unsafe { GetDlgItem(hwnd, id) });
        let initial = a11y::read_for_test(&children).unwrap();
        assert_eq!(
            a11y::read_for_test(&[hwnd]).unwrap()[0].id,
            "device-information.root"
        );
        assert_eq!(unsafe { IsWindowVisible(hwnd) }, 0);
        assert_eq!(initial[0].id, "device-information.groups");
        assert_eq!(initial[0].name, "Device information categories");
        assert_eq!(initial[1].id, "device-information.body");
        assert_eq!(initial[1].name, "Device information details");
        assert_eq!(
            initial[1].control_type,
            windows::Win32::UI::Accessibility::UIA_DocumentControlTypeId.0
        );
        assert_eq!(initial[1].read_only, Some(true));
        assert!(
            !initial[1]
                .value
                .as_ref()
                .unwrap()
                .contains("SYNTHETIC_PRIVATE_SERIAL")
        );
        assert!(initial[1].value.as_ref().unwrap().contains("••••••"));
        assert_eq!(initial[2].id, "device-information.reveal-private");
        assert_eq!(initial[2].toggle, Some(0));
        assert_eq!(
            initial[2].control_type,
            windows::Win32::UI::Accessibility::UIA_CheckBoxControlTypeId.0
        );
        assert!(
            !initial[3].enabled,
            "pending refresh remains natively disabled"
        );
        assert_eq!(initial[3].id, "device-information.refresh");
        assert!(initial[4].invoke && initial[5].invoke && initial[6].invoke);
        assert_eq!(initial[4].id, "device-information.copy-current");
        assert_eq!(initial[5].id, "device-information.copy-all");
        assert_eq!(initial[6].id, "device-information.close");
        assert_eq!(initial[7].id, "device-information.status");
        assert!(
            children
                .iter()
                .all(|child| unsafe { IsWindowVisible(*child) } == 0)
        );
        let pending = fixture.0.pending.unwrap();
        model
            .lock()
            .unwrap()
            .finish(pending, true, "Synthetic metadata complete");
        unsafe {
            fixture.0.tick();
        }
        fixture.0.groups = vec![
            json!({"title":"Disk","rows":[{"label":"Serial","value":"SYNTHETIC_PRIVATE_SERIAL","sensitive":true}]}),
        ];
        unsafe {
            SendMessageW(children[2], BM_SETCHECK, BST_CHECKED as usize, 0);
            fixture.0.show_private =
                SendMessageW(children[2], BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
            fixture.0.render_body();
            text(children[7], "Synthetic status changed");
        }
        let updated =
            a11y::read_for_test(&[children[1], children[3], children[7], children[2]]).unwrap();
        assert_eq!(updated[0].name, "Device information details");
        assert!(
            updated[0]
                .value
                .as_ref()
                .unwrap()
                .contains("SYNTHETIC_PRIVATE_SERIAL")
        );
        assert!(
            !updated[0].name.contains("SYNTHETIC_PRIVATE_SERIAL")
                && !updated[0].help.contains("SYNTHETIC_PRIVATE_SERIAL")
        );
        assert!(
            updated[1].enabled,
            "native enabled state follows the fake backend result"
        );
        assert_eq!(
            updated[2].name, "Synthetic status changed",
            "status Name must follow actual text rather than a static override"
        );
        assert_eq!(updated[3].toggle, Some(1));
        assert_eq!(unsafe { IsWindowVisible(hwnd) }, 0);
    }
}
