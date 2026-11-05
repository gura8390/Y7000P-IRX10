//! Compact tray-only view of the same completed telemetry used by the dashboard.
use super::{UiModel, actual_pstate, clean, fresh, label, power_mode, raw_mode, scalar, wide};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::{
    mem::zeroed,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*, Graphics::Gdi::*, System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::*,
};

const WIDTH: i32 = 300;
const MARGIN: i32 = 16;
const SECONDARY_HEIGHT: i32 = 26;
const SECTION_HEIGHT: i32 = 33;
const ID: i32 = 560;
const CENTER_IMAGE: u32 = 0x0200;
const RIGHT: u32 = 2;
const NO_PREFIX: u32 = 0x0080;
const ELLIPSIS: u32 = 0x4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Mode,
    Preset,
    Cpu,
    CpuClockTemp,
    CpuFanPower,
    Gpu,
    GpuPstate,
    GpuClockTemp,
    GpuFanPower,
    Fan,
    Battery,
    BatteryMode,
    BatteryRate,
    BatteryMinimum,
    BatteryMaximum,
    Update,
}

const ROWS: [Row; 16] = [
    Row::Mode,
    Row::Preset,
    Row::Cpu,
    Row::CpuClockTemp,
    Row::CpuFanPower,
    Row::Gpu,
    Row::GpuPstate,
    Row::GpuClockTemp,
    Row::GpuFanPower,
    Row::Fan,
    Row::Battery,
    Row::BatteryMode,
    Row::BatteryRate,
    Row::BatteryMinimum,
    Row::BatteryMaximum,
    Row::Update,
];

impl Row {
    fn primary(self) -> bool {
        matches!(
            self,
            Self::Mode | Self::Cpu | Self::Gpu | Self::Fan | Self::Battery | Self::Update
        )
    }
    fn height(self) -> i32 {
        if self.primary() {
            SECTION_HEIGHT
        } else {
            SECONDARY_HEIGHT
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DisplayRow {
    title: &'static str,
    value: String,
    dot: Option<COLORREF>,
}

fn metric<'a>(model: &'a UiModel, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        model
            .metrics
            .iter()
            .find(|item| {
                item.key == *key
                    && !item.value.trim().is_empty()
                    && !matches!(item.value.trim(), "—" | "-")
            })
            .map(|item| item.value.as_str())
    })
}

fn pair(a: Option<&str>, b: Option<&str>) -> String {
    let compact = |value: Option<&str>| {
        let value = value.unwrap_or("—");
        if let Some((number, unit)) = value.split_once(' ') {
            if let Ok(number) = number.parse::<f64>() {
                if number.is_finite() && matches!(unit, "MHz" | "°C" | "°F" | "W" | "RPM") {
                    let unit = if unit == "RPM" {
                        label("转", " RPM")
                    } else {
                        unit
                    };
                    return format!("{number:.0}{unit}");
                }
            }
        }
        value.into()
    };
    format!("{} | {}", compact(a), compact(b))
}

fn watts(model: &UiModel, key: &str) -> Option<String> {
    let value = model.values.get(key)?.as_f64()?;
    let watts = if key == "battery.rate" {
        match model.values.get("battery.rate_unit")?.as_str()? {
            "mW" => value / 1000.0,
            "W" => value,
            _ => return None,
        }
    } else {
        value
    };
    Some(if watts == 0.0 {
        "0.00 W".into()
    } else {
        format!("{watts:+.2} W")
    })
}

fn rows(model: &UiModel, now: u64) -> [Option<DisplayRow>; ROWS.len()] {
    let mut result = std::array::from_fn(|_| None);
    let mut put = |row: Row, title: &'static str, value: String, dot| {
        result[row as usize] = Some(DisplayRow { title, value, dot });
    };
    let values = &model.values;
    let mode = values.get("power.mode");
    put(
        Row::Mode,
        label("性能模式", "Power mode"),
        power_mode(mode).into(),
        match raw_mode(mode) {
            Some(1) => Some(0x00f27b35),
            Some(2) => Some(0x00a0a0a0),
            Some(3) => Some(0x003333d4),
            Some(224 | 255) => Some(0x00e33463),
            _ => None,
        },
    );
    if raw_mode(mode) == Some(255) {
        put(
            Row::Preset,
            label("预设", "Preset"),
            scalar(values.get("preset.active_name")),
            None,
        );
    }
    let sensors = values.get("sensors.enabled").and_then(Value::as_bool) != Some(false);
    let frequency_temperature = label("频率 / 温度", "Clock / temperature");
    let fan_power = label("风扇 / 功率", "Fan / power");
    if sensors {
        put(Row::Cpu, "CPU", String::new(), None);
        put(
            Row::CpuClockTemp,
            frequency_temperature,
            pair(
                metric(model, &["CpuFrequency", "CpuMaxFrequency"]),
                metric(model, &["CpuTemperature"]),
            ),
            None,
        );
        put(
            Row::CpuFanPower,
            fan_power,
            pair(
                metric(model, &["CpuFanSpeed"]),
                metric(model, &["CpuPower"]),
            ),
            None,
        );
    }

    let state = fresh(model, "gpu.state_at_tick_ms", now)
        .then(|| values.get("gpu.state").and_then(Value::as_str))
        .flatten();
    let (status, dot) = match state {
        Some("Active") => (label("正在使用", "Active"), Some(0x004aaa61)),
        Some("Inactive") => (label("空闲", "Inactive"), Some(0x0041a5f2)),
        Some("PoweredOff") => (label("已断电", "Powered off"), Some(0x004f55c9)),
        // Direct/display-connected mode uses the detail row, as in the standard card.
        Some("MonitorConnected") => ("", None),
        _ => ("—", None),
    };
    // This project targets the single 83NN configuration with a discrete GPU.
    put(
        Row::Gpu,
        label("独立显卡", "Discrete GPU"),
        status.into(),
        dot,
    );
    if state != Some("PoweredOff") {
        let pstate = actual_pstate(model, now);
        let enabled = label("启用", "Enabled");
        let observed = match (state, pstate.as_str()) {
            (Some("Active" | "Inactive" | "MonitorConnected"), "—") => enabled.into(),
            (Some("Active" | "Inactive" | "MonitorConnected"), _) => format!("{enabled}, {pstate}"),
            _ => pstate,
        };
        put(Row::GpuPstate, label("性能状态", "P-state"), observed, None);
        if sensors {
            put(
                Row::GpuClockTemp,
                frequency_temperature,
                pair(
                    metric(model, &["GpuFrequency"]),
                    metric(model, &["GpuCoreTemperature"]),
                ),
                None,
            );
            put(
                Row::GpuFanPower,
                fan_power,
                pair(
                    metric(model, &["GpuFanSpeed"]),
                    metric(model, &["GpuPower", "GpuChipPower"]),
                ),
                None,
            );
        }
    }
    if sensors {
        // A successful GetFeatureValue returning zero does not prove there
        // is a third fan. The standard version requires a PCH fan endpoint.
        let fan = metric(model, &["PchFanSpeed", "SystemFanSpeed"]).filter(|value| {
            value
                .strip_suffix(" RPM")
                .and_then(|rpm| rpm.parse::<u32>().ok())
                .is_some_and(|rpm| rpm > 0)
        });
        if let Some(speed) = fan {
            put(
                Row::Fan,
                label("系统风扇", "System fan"),
                speed.into(),
                None,
            );
        }
    }
    put(
        Row::Battery,
        label("电池", "Battery"),
        metric(model, &["BatteryLevel"]).unwrap_or("—").into(),
        None,
    );
    let charge_mode = match values.get("battery.mode").and_then(Value::as_str) {
        Some("Normal") => label("正常", "Normal"),
        Some("Rapid") => label("快充", "Rapid charge"),
        Some("Conservation") => label("养护充电", "Conservation"),
        _ => "—",
    };
    put(
        Row::BatteryMode,
        label("充电模式", "Charge mode"),
        charge_mode.into(),
        None,
    );
    put(
        Row::BatteryRate,
        label("充放电功率", "Charge rate"),
        watts(model, "battery.rate").unwrap_or_else(|| "—".into()),
        None,
    );
    for (row, title, key) in [
        (
            Row::BatteryMinimum,
            label("最低功率", "Minimum rate"),
            "battery.minimum_rate_w",
        ),
        (
            Row::BatteryMaximum,
            label("最高功率", "Maximum rate"),
            "battery.maximum_rate_w",
        ),
    ] {
        put(
            row,
            title,
            watts(model, key).unwrap_or_else(|| "—".into()),
            None,
        );
    }
    if values.get("update.available").and_then(Value::as_bool) == Some(true) {
        put(
            Row::Update,
            label("有可用更新", "Update available"),
            String::new(),
            None,
        );
    }
    result
}

#[derive(Default)]
struct Line {
    label: HWND,
    value: HWND,
}

pub(super) struct Panel {
    title: HWND,
    lines: [Line; ROWS.len()],
    current: [Option<DisplayRow>; ROWS.len()],
    fonts: [HFONT; 3],
    dpi: u32,
}

impl Panel {
    pub(super) unsafe fn create(parent: HWND, dpi: u32) -> Result<Self> {
        unsafe {
            let mut panel = Self {
                title: static_control(parent, ID, false)?,
                lines: std::array::from_fn(|_| Line::default()),
                current: std::array::from_fn(|_| None),
                fonts: [null_mut(); 3],
                dpi: 0,
            };
            super::a11y::annotate(parent, "status-popup.hover", None, None);
            super::a11y::annotate(panel.title, "status-popup.hover.title", None, None);
            SetWindowTextW(panel.title, wide("拯救者工具箱").as_ptr());
            for row in ROWS {
                let index = row as usize;
                let label = static_control(parent, ID + 1 + 2 * index as i32, false)?;
                let value = static_control(parent, ID + 2 + 2 * index as i32, true)?;
                let role = format!("status-popup.hover.{row:?}").to_ascii_lowercase();
                super::a11y::annotate(label, &format!("{role}.label"), None, None);
                super::a11y::annotate(value, &format!("{role}.value"), None, None);
                panel.lines[index] = Line { label, value };
            }
            panel.apply_fonts(dpi)?;
            ShowWindow(panel.title, SW_SHOWNOACTIVATE);
            Ok(panel)
        }
    }

    pub(super) unsafe fn apply_fonts(&mut self, dpi: u32) -> Result<()> {
        if self.dpi == dpi {
            return Ok(());
        }
        unsafe {
            let next = [
                super::font(dpi, 12, false),
                super::font(dpi, 14, true),
                super::font(dpi, 13, false),
            ];
            if next.iter().any(|font| font.is_null()) {
                delete_fonts(next);
                anyhow::bail!("Cannot create tray status fonts");
            }
            SendMessageW(self.title, WM_SETFONT, next[0] as usize, 0);
            for row in ROWS {
                let line = &self.lines[row as usize];
                let font = next[if row.primary() { 1 } else { 2 }] as usize;
                SendMessageW(line.label, WM_SETFONT, font, 0);
                SendMessageW(line.value, WM_SETFONT, font, 0);
            }
            delete_fonts(std::mem::replace(&mut self.fonts, next));
            self.dpi = dpi;
            Ok(())
        }
    }

    // Values are individual native controls. Static rows, surrounding chrome,
    // and the anchor are left alone on regular sensor samples.
    pub(super) unsafe fn update(&mut self, model: &UiModel, now: u64) -> bool {
        let next = rows(model, now);
        let mut relayout = false;
        for row in ROWS {
            let index = row as usize;
            let old = self.current[index].as_ref();
            let new = next[index].as_ref();
            if old == new {
                continue;
            }
            let line = &self.lines[index];
            unsafe {
                if let Some(new) = new {
                    if old.map(|item| item.title) != Some(new.title) {
                        SetWindowTextW(line.label, wide(new.title).as_ptr());
                    }
                    if old.map(|item| item.value.as_str()) != Some(new.value.as_str()) {
                        SetWindowTextW(line.value, wide(&clean(&new.value, 90)).as_ptr());
                    }
                }
            }
            relayout |= old.is_some() != new.is_some()
                || ((old.and_then(|item| item.dot).is_some()
                    || new.and_then(|item| item.dot).is_some())
                    && old != new);
        }
        self.current = next;
        relayout
    }

    pub(super) fn size(&self) -> (i32, i32) {
        let height = ROWS
            .into_iter()
            .filter(|row| self.current[*row as usize].is_some())
            .map(Row::height)
            .sum::<i32>();
        let width = if super::i18n::language() == "en" {
            330
        } else {
            WIDTH
        };
        (width, 60 + height)
    }

    fn s(&self, n: i32) -> i32 {
        n * self.dpi as i32 / 96
    }

    fn visible_rows(&self) -> impl Iterator<Item = (Row, i32, &DisplayRow)> {
        let mut y = 43;
        ROWS.into_iter().filter_map(move |row| {
            let data = self.current[row as usize].as_ref()?;
            let top = y;
            y += row.height();
            Some((row, top, data))
        })
    }

    pub(super) unsafe fn layout(&self, width: i32) {
        let margin = self.s(MARGIN);
        let available = (width - 2 * margin).max(1);
        let label_x = margin + self.s(28);
        let label_width = self.s(109).min((available / 2).max(1));
        let value_x = label_x + label_width + self.s(4);
        unsafe {
            position(self.title, margin, self.s(12), available, self.s(23), true);
            let parent = GetParent(self.title);
            let dc = GetDC(parent);
            for row in ROWS {
                if self.current[row as usize].is_none() {
                    ShowWindow(self.lines[row as usize].label, SW_HIDE);
                    ShowWindow(self.lines[row as usize].value, SW_HIDE);
                }
            }
            for (row, y, data) in self.visible_rows() {
                let line = &self.lines[row as usize];
                let height = self.s(y + row.height()) - self.s(y);
                position(line.label, label_x, self.s(y), label_width, height, true);
                let right = width - margin;
                let left = if data.dot.is_some() {
                    self.dot_text_left(dc, &data.value, right).max(value_x)
                } else {
                    value_x
                };
                position(
                    line.value,
                    left,
                    self.s(y),
                    (right - left).max(1),
                    height,
                    !data.value.is_empty(),
                );
            }
            if !dc.is_null() {
                ReleaseDC(parent, dc);
            }
        }
    }

    unsafe fn dot_text_left(&self, dc: HDC, value: &str, right: i32) -> i32 {
        if dc.is_null() {
            return right - self.s(92);
        }
        unsafe {
            let old = SelectObject(dc, self.fonts[1]);
            let value = wide(value);
            let mut size = zeroed::<SIZE>();
            GetTextExtentPoint32W(dc, value.as_ptr(), value.len() as i32 - 1, &mut size);
            SelectObject(dc, old);
            right - size.cx - self.s(2)
        }
    }

    pub(super) unsafe fn paint(&self, dc: HDC, width: i32, ink: COLORREF) {
        unsafe {
            let pen = CreatePen(PS_SOLID, self.s(1).max(1), ink);
            if pen.is_null() {
                return;
            }
            let old_pen = SelectObject(dc, pen);
            let old_brush = SelectObject(dc, GetStockObject(NULL_BRUSH));
            for (row, y, data) in self.visible_rows() {
                if row.primary() {
                    self.paint_icon(dc, row, MARGIN, y + 9);
                }
                if let Some(color) = data.dot {
                    let brush = CreateSolidBrush(color);
                    if !brush.is_null() {
                        let right = width - self.s(MARGIN);
                        let x = self.dot_text_left(dc, &data.value, right) - self.s(17);
                        let y = self.s(y + 11);
                        let previous_brush = SelectObject(dc, brush);
                        let previous_pen = SelectObject(dc, GetStockObject(NULL_PEN));
                        Ellipse(dc, x, y, x + self.s(11), y + self.s(11));
                        SelectObject(dc, previous_pen);
                        SelectObject(dc, previous_brush);
                        DeleteObject(brush);
                    }
                }
            }
            SelectObject(dc, old_pen);
            SelectObject(dc, old_brush);
            DeleteObject(pen);
        }
    }

    unsafe fn paint_icon(&self, dc: HDC, row: Row, x: i32, y: i32) {
        let rect = |l, t, r, b| (self.s(x + l), self.s(y + t), self.s(x + r), self.s(y + b));
        let path = |points: &[(i32, i32)]| unsafe {
            let pixels = points
                .iter()
                .map(|&(a, b)| POINT {
                    x: self.s(x + a),
                    y: self.s(y + b),
                })
                .collect::<Vec<_>>();
            Polyline(dc, pixels.as_ptr(), pixels.len() as i32);
        };
        let box_at = |l, t, r, b| unsafe {
            let (l, t, r, b) = rect(l, t, r, b);
            Rectangle(dc, l, t, r, b);
        };
        unsafe {
            match row {
                Row::Mode => {
                    path(&[(1, 12), (1, 8), (4, 4), (8, 2), (12, 4), (15, 8), (15, 12)]);
                    path(&[(1, 12), (15, 12)]);
                    path(&[(7, 10), (11, 6)]);
                }
                Row::Cpu => {
                    box_at(3, 3, 14, 14);
                    box_at(6, 6, 11, 11);
                    for n in [5, 8, 11] {
                        path(&[(0, n), (3, n)]);
                        path(&[(13, n), (16, n)]);
                        path(&[(n, 0), (n, 3)]);
                        path(&[(n, 13), (n, 16)]);
                    }
                }
                Row::Gpu => {
                    box_at(1, 4, 16, 13);
                    let (l, t, r, b) = rect(4, 6, 10, 12);
                    Ellipse(dc, l, t, r, b);
                    path(&[(4, 13), (4, 15), (13, 15), (13, 13)]);
                }
                Row::Fan => {
                    let (l, t, r, b) = rect(1, 1, 16, 16);
                    Ellipse(dc, l, t, r, b);
                    path(&[(4, 5), (8, 8), (8, 3)]);
                    path(&[(13, 5), (8, 8), (13, 10)]);
                    path(&[(9, 13), (8, 8), (4, 12)]);
                }
                Row::Battery => {
                    box_at(1, 4, 14, 13);
                    path(&[(14, 6), (16, 6), (16, 10), (14, 10)]);
                    path(&[(4, 8), (11, 8)]);
                }
                Row::Update => {
                    path(&[(8, 2), (8, 12)]);
                    path(&[(4, 6), (8, 2), (12, 6)]);
                    path(&[(2, 13), (14, 13)]);
                }
                _ => {}
            }
        }
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        unsafe { delete_fonts(self.fonts) }
    }
}

pub(super) fn muted_control(id: i32) -> bool {
    if id == ID {
        return true;
    }
    let index = id - ID - 1;
    index >= 0
        && ROWS
            .get(index as usize / 2)
            .is_some_and(|row| !row.primary())
}

unsafe fn static_control(parent: HWND, id: i32, right: bool) -> Result<HWND> {
    unsafe {
        let hwnd = CreateWindowExW(
            0,
            wide("STATIC").as_ptr(),
            null(),
            WS_CHILD | CENTER_IMAGE | NO_PREFIX | ELLIPSIS | if right { RIGHT } else { 0 },
            0,
            0,
            1,
            1,
            parent,
            id as usize as HMENU,
            GetModuleHandleW(null()),
            null(),
        );
        ensure!(!hwnd.is_null(), "Cannot create tray status line");
        Ok(hwnd)
    }
}

unsafe fn position(hwnd: HWND, x: i32, y: i32, width: i32, height: i32, visible: bool) {
    unsafe {
        SetWindowPos(
            hwnd,
            null_mut(),
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE
                | SWP_NOZORDER
                | if visible {
                    SWP_SHOWWINDOW
                } else {
                    SWP_HIDEWINDOW
                },
        );
    }
}

unsafe fn delete_fonts(fonts: [HFONT; 3]) {
    for font in fonts.into_iter().filter(|font| !font.is_null()) {
        unsafe { DeleteObject(font) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_model::UiMetric;
    use serde_json::json;

    #[test]
    fn compact_uses_real_values_and_hides_powered_off_details() {
        let _language = super::super::i18n::TestLanguage::new("en");
        let mut model = UiModel::default();
        model.metrics.push(UiMetric {
            key: "CpuTemperature".into(),
            label: "CPU".into(),
            value: "83 °C".into(),
            detail: String::new(),
        });
        model.metrics.push(UiMetric {
            key: "PchFanSpeed".into(),
            label: "System fan".into(),
            value: "0 RPM".into(),
            detail: String::new(),
        });
        for (key, value) in [
            ("power.mode", json!(255)),
            ("preset.active_name", json!("Gaming")),
            ("gpu.state", json!("PoweredOff")),
            ("gpu.state_at_tick_ms", json!(1000)),
            ("gpu.current_pstate", json!(0)),
            ("gpu.current_pstate_at_tick_ms", json!(1000)),
            ("battery.rate", json!(-4300)),
            ("battery.rate_unit", json!("mW")),
            ("battery.mode", json!("Conservation")),
        ] {
            model.values.insert(key.into(), value);
        }
        let current = rows(&model, 1010);
        let value = |row: Row| current[row as usize].as_ref().unwrap().value.as_str();
        assert_eq!(value(Row::Preset), "Gaming");
        assert_eq!(value(Row::CpuClockTemp), "— | 83°C");
        assert_eq!(value(Row::Gpu), "Powered off");
        assert!(current[Row::GpuPstate as usize].is_none());
        assert_eq!(value(Row::BatteryRate), "-4.30 W");
        assert_eq!(value(Row::BatteryMode), "Conservation");
        assert!(current[Row::Fan as usize].is_none());

        model.metrics[1].value = "1400 RPM".into();
        model
            .values
            .insert("gpu.state".into(), json!("MonitorConnected"));
        let current = rows(&model, 1011);
        assert_eq!(
            current[Row::Fan as usize].as_ref().unwrap().value,
            "1400 RPM"
        );
        assert!(
            current[Row::Gpu as usize]
                .as_ref()
                .unwrap()
                .value
                .is_empty()
        );
        assert_eq!(
            current[Row::GpuPstate as usize].as_ref().unwrap().value,
            "Enabled, P0"
        );

        model
            .values
            .insert("battery.rate_unit".into(), json!("relative/hour"));
        model.values.insert("power.mode".into(), json!(3));
        model.values.insert("sensors.enabled".into(), json!(false));
        let current = rows(&model, 4001);
        assert!(current[Row::Preset as usize].is_none());
        assert!(current[Row::Cpu as usize].is_none());
        assert_eq!(
            current[Row::GpuPstate as usize].as_ref().unwrap().value,
            "—"
        );
        assert_eq!(
            current[Row::BatteryRate as usize].as_ref().unwrap().value,
            "—"
        );
    }

    #[test]
    fn new_sensor_sample_changes_only_the_hover_value_and_preserves_native_controls() {
        use super::super::{Bounds, Owned, a11y};
        use crate::ui::native::get_text;

        let _language = super::super::i18n::TestLanguage::new("zh-Hans");
        let _accessibility = a11y::UiThread::enter().unwrap();
        let mut window = Owned::new();
        unsafe {
            window
                .create(
                    null_mut(),
                    144,
                    false,
                    Bounds {
                        left: 0,
                        top: 0,
                        right: 450,
                        bottom: 640,
                    },
                    false,
                )
                .unwrap();
        }
        let mut model = UiModel::default();
        model.metrics.push(UiMetric {
            key: "CpuTemperature".into(),
            label: "CPU".into(),
            value: "71 °C".into(),
            detail: String::new(),
        });
        let (label, value) = {
            let mut state = window.callback.state.borrow_mut();
            assert!(unsafe { state.update(&model, 1000) }.unwrap());
            let panel = state.compact.as_ref().unwrap();
            let line = &panel.lines[Row::CpuClockTemp as usize];
            (line.label, line.value)
        };
        assert_eq!(unsafe { get_text(value) }, "— | 71°C");
        assert_eq!(unsafe { IsWindowVisible(window.hwnd) }, 0);

        model.revision += 1;
        model.metrics[0].value = "79.0 °C".into();
        {
            let mut state = window.callback.state.borrow_mut();
            assert!(!unsafe { state.update(&model, 1001) }.unwrap());
            let panel = state.compact.as_ref().unwrap();
            let line = &panel.lines[Row::CpuClockTemp as usize];
            assert_eq!((line.label, line.value), (label, value));
        }
        assert_eq!(unsafe { get_text(value) }, "— | 79°C");
        let semantics = a11y::read_for_test(&[window.hwnd, value]).unwrap();
        assert_eq!(semantics[0].id, "status-popup.hover");
        assert_eq!(semantics[1].id, "status-popup.hover.cpuclocktemp.value");
        assert_eq!(semantics[1].name, "— | 79°C");
        window.close().unwrap();
        assert_eq!(unsafe { IsWindow(value) }, 0);
    }
}
