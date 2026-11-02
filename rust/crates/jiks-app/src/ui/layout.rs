//! Shared identifiers for the original control and sensor group editors.
use crate::ui::i18n::{tr, tr_static};
use crate::ui_model::UiChoice;
use anyhow::{Result, bail};
use serde_json::{Value, json};
pub(super) const DASHBOARD_ITEMS: &[(&str, &str)] = &[
    ("PowerMode", "性能模式"),
    ("BatteryMode", "充电模式"),
    ("BatteryNightChargeMode", "夜间充电"),
    ("ChargeThreshold", "充电阈值"),
    ("AlwaysOnUsb", "关机 USB 供电"),
    ("InstantBoot", "接电开机"),
    ("FlipToStart", "翻盖开机"),
    ("HybridMode", "显卡工作模式"),
    ("DiscreteGpu", "独显状态"),
    ("OverclockDiscreteGpu", "独显超频"),
    ("Resolution", "分辨率"),
    ("RefreshRate", "刷新率"),
    ("DpiScale", "显示缩放"),
    ("Hdr", "HDR"),
    ("AutoColorManagement", "自动色彩管理"),
    ("OverDrive", "OverDrive"),
    ("TurnOffMonitors", "关闭显示器"),
    ("Microphone", "麦克风"),
    ("WhiteKeyboardBacklight", "白光背光"),
    ("PanelLogoBacklight", "Logo 背光"),
    ("PortsBacklight", "接口背光"),
    ("PowerButtonLed", "电源指示灯"),
    ("TouchpadLock", "触摸板锁定"),
    ("FnLock", "Fn 锁定"),
    ("WinKeyLock", "Win 锁定"),
];
pub(super) const SENSOR_ITEMS: &[(&str, &str)] = &[
    ("CpuUtilization", "CPU 占用率"),
    ("CpuFrequency", "CPU 频率"),
    ("CpuMaxFrequency", "CPU 最高频率"),
    ("CpuAverageFrequency", "CPU 平均频率"),
    ("CpuPMaxFrequency", "P 核最高频率"),
    ("CpuPAverageFrequency", "P 核平均频率"),
    ("CpuEMaxFrequency", "E 核最高频率"),
    ("CpuEAverageFrequency", "E 核平均频率"),
    ("CpuFanSpeed", "CPU 风扇"),
    ("CpuTemperature", "CPU 温度"),
    ("CpuPower", "CPU 功耗"),
    ("CpuVoltage", "CPU 电压"),
    ("GpuUtilization", "GPU 占用率"),
    ("GpuFrequency", "GPU 频率"),
    ("GpuFanSpeed", "GPU 风扇"),
    ("GpuCoreTemperature", "GPU 核心温度"),
    ("GpuVramTemperature", "GPU 显存温度"),
    ("GpuTemperatures", "GPU 温度"),
    ("GpuPower", "GPU 功耗"),
    ("GpuVramUsed", "显存用量"),
    ("GpuVramTotal", "显存总量"),
    ("GpuVramUtilization", "显存占用率"),
    ("PchFanSpeed", "PCH 风扇"),
    ("PchTemperature", "PCH 温度"),
    ("BatteryState", "电池状态"),
    ("BatteryLevel", "电池电量"),
    ("MemoryUtilization", "内存占用率"),
    ("MemoryUsed", "内存用量"),
    ("MemoryTotal", "内存总量"),
    ("MemoryTemperature", "内存温度"),
    ("Disk1Temperature", "硬盘 1 温度"),
    ("Disk2Temperature", "硬盘 2 温度"),
];
pub(super) fn items(sensor: bool) -> Vec<UiChoice> {
    (if sensor {
        SENSOR_ITEMS
    } else {
        DASHBOARD_ITEMS
    })
    .iter()
    .map(|(id, label)| UiChoice::new(tr(label).into_owned(), *id))
    .collect()
}
pub(super) fn item_label(value: &Value, sensor: bool) -> String {
    let key = value.as_str().unwrap_or("");
    (if sensor {
        SENSOR_ITEMS
    } else {
        DASHBOARD_ITEMS
    })
    .iter()
    .find(|(id, _)| *id == key)
    .map(|(_, l)| tr(l).into_owned())
    .unwrap_or_else(|| key.into())
}
pub(super) fn group_types(sensor: bool) -> Vec<UiChoice> {
    (if sensor {
        vec![
            ("CPU", "CPU"),
            ("GPU", "GPU"),
            ("主板", "Motherboard"),
            ("电池", "Battery"),
            ("内存", "Memory"),
            ("磁盘", "Disk"),
        ]
    } else {
        vec![
            ("电源", "Power"),
            ("显卡", "Graphics"),
            ("显示", "Display"),
            ("其他", "Other"),
            ("自定义", "Custom"),
        ]
    })
    .iter()
    .map(|(label, id)| UiChoice::new(tr(label).into_owned(), *id))
    .collect()
}
pub(super) fn group_label(group: &Value) -> String {
    let ty = group
        .get("Type")
        .and_then(Value::as_str)
        .unwrap_or("Custom");
    if ty == "Custom" {
        return group
            .get("CustomName")
            .and_then(Value::as_str)
            .unwrap_or_else(|| tr_static("自定义分组"))
            .into();
    }
    group_types(false)
        .into_iter()
        .chain(group_types(true))
        .find(|c| c.value == ty)
        .map(|c| c.label)
        .unwrap_or_else(|| ty.into())
}
pub(super) fn defaults(sensor: bool) -> Vec<Value> {
    let groups = if sensor {
        vec![
            (
                "CPU",
                vec![
                    "CpuUtilization",
                    "CpuFrequency",
                    "CpuFanSpeed",
                    "CpuTemperature",
                    "CpuPower",
                ],
            ),
            (
                "GPU",
                vec![
                    "GpuUtilization",
                    "GpuVramUtilization",
                    "GpuFrequency",
                    "GpuFanSpeed",
                    "GpuCoreTemperature",
                    "GpuVramTemperature",
                    "GpuTemperatures",
                    "GpuPower",
                ],
            ),
            ("Motherboard", vec!["PchFanSpeed", "PchTemperature"]),
            ("Battery", vec!["BatteryState", "BatteryLevel"]),
            ("Memory", vec!["MemoryUtilization", "MemoryTemperature"]),
            ("Disk", vec!["Disk1Temperature", "Disk2Temperature"]),
        ]
    } else {
        vec![
            (
                "Power",
                vec![
                    "PowerMode",
                    "BatteryMode",
                    "BatteryNightChargeMode",
                    "ChargeThreshold",
                    "AlwaysOnUsb",
                    "InstantBoot",
                    "FlipToStart",
                ],
            ),
            (
                "Graphics",
                vec!["HybridMode", "DiscreteGpu", "OverclockDiscreteGpu"],
            ),
            (
                "Display",
                vec![
                    "Resolution",
                    "RefreshRate",
                    "DpiScale",
                    "Hdr",
                    "AutoColorManagement",
                    "OverDrive",
                    "TurnOffMonitors",
                ],
            ),
            (
                "Other",
                vec![
                    "Microphone",
                    "WhiteKeyboardBacklight",
                    "PanelLogoBacklight",
                    "PortsBacklight",
                    "PowerButtonLed",
                    "TouchpadLock",
                    "FnLock",
                    "WinKeyLock",
                ],
            ),
        ]
    };
    groups
        .into_iter()
        .map(|(ty, items)| json!({"Type":ty,"CustomName":null,"Items":items}))
        .collect()
}
pub(super) fn validate(groups: &[Value], sensor: bool) -> Result<()> {
    for group in groups {
        let ty = group.get("Type").and_then(Value::as_str).unwrap_or("");
        if !group_types(sensor).iter().any(|c| c.value == ty) {
            bail!("{}", tr_static("分组类型不可用，请选择正确类别。"));
        }
        if ty == "Custom"
            && group
                .get("CustomName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .is_empty()
        {
            bail!("{}", tr_static("请填写自定义分组名称。"));
        }
        if !group.get("Items").is_some_and(Value::is_array) {
            bail!("{}", tr_static("分组项目无效。"));
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_group_order_and_local_exclusion() {
        let g = defaults(false);
        assert_eq!(g[0]["Type"], "Power");
        assert_eq!(g[1]["Type"], "Graphics");
        assert_eq!(g[2]["Type"], "Display");
        assert_eq!(g[3]["Type"], "Other");
        assert!(!items(false).iter().any(|i| i.value == "ItsMode"));
        validate(&g, false).unwrap();
        assert_eq!(
            g.iter()
                .map(|g| g["Items"].as_array().unwrap().len())
                .sum::<usize>(),
            25
        );
    }
}
