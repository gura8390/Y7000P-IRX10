use super::i18n::{tr, tr_static};
use crate::ui_model::{UiChoice, UiModel};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Dashboard,
    Keyboard,
    Battery,
    Automation,
    Macros,
    Packages,
    Settings,
    Donate,
    About,
    Performance,
    Display,
}
impl Page {
    pub const ALL: [Self; 9] = [
        Self::Dashboard,
        Self::Keyboard,
        Self::Battery,
        Self::Automation,
        Self::Macros,
        Self::Packages,
        Self::Settings,
        Self::Donate,
        Self::About,
    ];
    pub fn title(self) -> &'static str {
        tr_static(match self {
            Self::Dashboard => "控制台",
            Self::Keyboard => "键盘背光",
            Self::Battery => "电池",
            Self::Automation => "自动化",
            Self::Macros => "键盘宏",
            Self::Packages => "驱动下载",
            Self::Settings => "设置",
            Self::Donate => "赞助",
            Self::About => "关于",
            Self::Performance => "性能",
            Self::Display => tr_static("显示"),
        })
    }
    pub fn key(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Keyboard => "keyboard",
            Self::Battery => "battery",
            Self::Automation => "automation",
            Self::Macros => "macros",
            Self::Packages => "packages",
            Self::Settings => "settings",
            Self::Donate => "donate",
            Self::About => "about",
            Self::Performance => "performance",
            Self::Display => "display",
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum EditorKind {
    Preset,
    Automation,
    Macro,
}
#[derive(Clone, Debug)]
pub enum FieldKind {
    Toggle,
    Text,
    Number,
    ReadOnly,
    Choice(Vec<UiChoice>),
    Collection(&'static str),
}
#[derive(Clone, Debug)]
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    pub capability: &'static str,
}
#[derive(Clone, Debug)]
pub enum ActionKind {
    Execute {
        command: &'static str,
        fields: Vec<&'static str>,
        extra: Value,
        confirm: Option<&'static str>,
    },
    Structured {
        kind: EditorKind,
        collection: &'static str,
        fresh: bool,
    },
    Document(&'static str),
    SensorSettings,
    Cancel(Vec<&'static str>),
}
#[derive(Clone, Debug)]
pub struct Action {
    pub accessibility_role: &'static str,
    pub label: &'static str,
    pub capability: &'static str,
    pub kind: ActionKind,
}
#[derive(Clone, Debug)]
pub struct Section {
    pub title: String,
    pub hint: &'static str,
    pub fields: Vec<Field>,
    pub actions: Vec<Action>,
}

fn choices(pairs: &[(&str, &str)]) -> FieldKind {
    FieldKind::Choice(
        pairs
            .iter()
            .map(|(label, raw)| {
                UiChoice::new(
                    tr(label).into_owned(),
                    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| json!(raw)),
                )
            })
            .collect(),
    )
}
fn field(
    key: &'static str,
    label: &'static str,
    kind: FieldKind,
    capability: &'static str,
) -> Field {
    Field {
        key,
        label: tr_static(label),
        kind,
        capability,
    }
}
fn execute(label: &'static str, command: &'static str, fields: &[&'static str]) -> Action {
    Action {
        accessibility_role: "execute",
        label: tr_static(label),
        capability: command,
        kind: ActionKind::Execute {
            command,
            fields: fields.to_vec(),
            extra: json!({}),
            confirm: None,
        },
    }
}
fn confirmed(
    label: &'static str,
    command: &'static str,
    fields: &[&'static str],
    message: &'static str,
) -> Action {
    Action {
        accessibility_role: "confirmed",
        label: tr_static(label),
        capability: command,
        kind: ActionKind::Execute {
            command,
            fields: fields.to_vec(),
            extra: json!({}),
            confirm: Some(tr_static(message)),
        },
    }
}
fn edit(
    label: &'static str,
    kind: EditorKind,
    collection: &'static str,
    fresh: bool,
    capability: &'static str,
) -> Action {
    Action {
        accessibility_role: "edit",
        label: tr_static(label),
        capability,
        kind: ActionKind::Structured {
            kind,
            collection,
            fresh,
        },
    }
}
fn doc(label: &'static str, document: &'static str) -> Action {
    Action {
        accessibility_role: "document",
        label: tr_static(label),
        capability: "",
        kind: ActionKind::Document(document),
    }
}
fn cancel(fields: &[&'static str]) -> Action {
    Action {
        accessibility_role: "cancel",
        label: tr_static("取消修改"),
        capability: "",
        kind: ActionKind::Cancel(fields.to_vec()),
    }
}

pub fn sections(page: Page, tools_tab: usize, model: &UiModel) -> Vec<Section> {
    use FieldKind::*;
    match page {
        Page::Dashboard => dashboard(model),
        Page::Performance => vec![
            Section {
                title: tr_static("性能模式").into(),
                hint: tr_static(
                    "只列出本机已验证支持的模式。切换自定义模式可能应用完整预设，请先检查预算。",
                ),
                fields: vec![
                    field(
                        "power.mode",
                        "模式",
                        choices(&[
                            ("安静", "1"),
                            ("均衡", "2"),
                            ("性能", "3"),
                            ("自定义", "255"),
                        ]),
                        "power.apply",
                    ),
                    field(
                        "power.fnq_custom",
                        "Fn + Q 包含自定义模式",
                        Toggle,
                        "power.apply",
                    ),
                ],
                actions: vec![
                    execute(
                        "应用模式",
                        "power.apply",
                        &["power.mode", "power.fnq_custom"],
                    ),
                    cancel(&["power.mode", "power.fnq_custom"]),
                    doc("均衡 AI 设置", "balance"),
                ],
            },
            Section {
                title: tr_static("自定义预设").into(),
                hint: tr_static("预设包含 CPU/GPU 合法范围与风扇设置；保存不等于已应用到硬件。"),
                fields: vec![field(
                    "selection.presets",
                    "预设",
                    Collection("presets"),
                    "",
                )],
                actions: vec![
                    edit("新建", EditorKind::Preset, "presets", true, "preset.save"),
                    edit("编辑", EditorKind::Preset, "presets", false, "preset.save"),
                    execute("应用所选预设", "preset.apply", &["selection.presets"]),
                    confirmed(
                        "删除",
                        "preset.delete",
                        &["selection.presets"],
                        "删除所选预设？当前硬件值不会因此自动恢复。",
                    ),
                    doc("风扇与高级配置", "godmode"),
                ],
            },
            Section {
                title: tr_static("CPU 温控").into(),
                hint: tr_static(
                    "默认不启用。固件聚合温度与核心热点分开标注；温度与压力验收由用户进行。",
                ),
                fields: vec![
                    field(
                        "thermal.enabled",
                        "启用本机 CPU 温控",
                        Toggle,
                        "thermal.apply",
                    ),
                    field("thermal.shadow", "仅观察决策", Toggle, "thermal.apply"),
                    field(
                        "thermal.sampling_ms",
                        "采样间隔 · ms",
                        Number,
                        "thermal.apply",
                    ),
                    field(
                        "thermal.firmware_limit_c",
                        "固件温限 · °C",
                        Number,
                        "thermal.apply",
                    ),
                ],
                actions: vec![
                    execute(
                        "应用温控设置",
                        "thermal.apply",
                        &[
                            "thermal.enabled",
                            "thermal.shadow",
                            "thermal.sampling_ms",
                            "thermal.firmware_limit_c",
                        ],
                    ),
                    execute("停止并确认恢复", "thermal.stop", &[]),
                    doc("传感器显示设置", "hardware_sensors"),
                    cancel(&[
                        "thermal.enabled",
                        "thermal.shadow",
                        "thermal.sampling_ms",
                        "thermal.firmware_limit_c",
                    ]),
                ],
            },
        ],
        Page::Display => vec![
            Section {
                title: tr_static("显示设置").into(),
                hint: tr_static("选项以所选显示器真实能力为准。失败时保留正在编辑的值并说明原因。"),
                fields: vec![
                    field("display.monitor", "显示器", Choice(vec![]), "display.apply"),
                    field(
                        "display.resolution",
                        "分辨率",
                        Choice(vec![]),
                        "display.apply",
                    ),
                    field(
                        "display.refresh_rate",
                        "刷新率",
                        Choice(vec![]),
                        "display.apply",
                    ),
                    field("display.dpi", "缩放比例 · %", Number, "display.apply"),
                    field("display.brightness", "亮度 · %", Number, "display.apply"),
                    field("display.hdr", "HDR", Toggle, "display.hdr"),
                    field("display.acm", "自动色彩管理", Toggle, "display.acm"),
                    field(
                        "display.overdrive",
                        "OverDrive",
                        Toggle,
                        "display.overdrive",
                    ),
                ],
                actions: vec![
                    execute(
                        "应用显示设置",
                        "display.apply",
                        &[
                            "display.monitor",
                            "display.resolution",
                            "display.refresh_rate",
                            "display.dpi",
                            "display.brightness",
                            "display.hdr",
                            "display.acm",
                            "display.overdrive",
                        ],
                    ),
                    execute("关闭显示器", "display.turn_off", &[]),
                    doc("排除刷新率", "excluded_refresh_rates"),
                ],
            },
            Section {
                title: tr_static("显卡工作模式").into(),
                hint: tr_static("独显直连等切换可能需要重启；后台确认支持与重启要求后执行。"),
                fields: vec![field(
                    "gpu.hybrid_mode",
                    "工作模式",
                    Choice(vec![]),
                    "gpu.hybrid.apply",
                )],
                actions: vec![confirmed(
                    "应用显卡模式",
                    "gpu.hybrid.apply",
                    &["gpu.hybrid_mode"],
                    "应用所选显卡工作模式？若需要重启，后台将先返回说明。",
                )],
            },
            Section {
                title: tr_static("独显管理").into(),
                hint: tr_static(
                    "关闭占用应用会丢失该应用未保存的内容；设备重启会影响正在使用独显的工作。",
                ),
                fields: vec![
                    field(
                        "selection.gpu.processes",
                        "占用应用",
                        Collection("gpu.processes"),
                        "",
                    ),
                    field(
                        "gpu.process.preference",
                        "图形偏好",
                        choices(&[
                            ("Windows 决定", "auto"),
                            ("核显", "integrated"),
                            ("独显", "discrete"),
                        ]),
                        "gpu.process.preference",
                    ),
                    field(
                        "gpu.keep_awake",
                        "保持独显唤醒",
                        Toggle,
                        "gpu.settings.save",
                    ),
                    field(
                        "gpu.locked_pstate",
                        "锁定性能状态",
                        Choice(vec![UiChoice::new(tr_static("自动"), -1)]),
                        "gpu.settings.save",
                    ),
                    field("gpu.poll_ms", "轮询间隔 · ms", Number, "gpu.settings.save"),
                ],
                actions: vec![
                    confirmed(
                        "读取独显库存",
                        "gpu.refresh",
                        &[],
                        "显式读取独显库存可能唤醒独显；继续读取？",
                    ),
                    confirmed(
                        "读取独显占用应用",
                        "gpu.processes.refresh",
                        &[],
                        "读取占用应用可能唤醒独显；继续读取？",
                    ),
                    confirmed(
                        "读取性能状态选项",
                        "gpu.pstate.refresh",
                        &[],
                        "读取合法性能状态可能唤醒独显；继续读取？",
                    ),
                    execute(
                        "保存显卡设置",
                        "gpu.settings.save",
                        &["gpu.keep_awake", "gpu.locked_pstate", "gpu.poll_ms"],
                    ),
                    execute(
                        "设置应用偏好",
                        "gpu.process.preference",
                        &["selection.gpu.processes", "gpu.process.preference"],
                    ),
                    confirmed(
                        "结束所选应用",
                        "gpu.process.kill",
                        &["selection.gpu.processes"],
                        "强制结束所选进程？未保存的内容可能丢失。",
                    ),
                    execute("请求独显休眠", "gpu.sleep", &[]),
                    confirmed(
                        "重启独显设备",
                        "gpu.restart",
                        &[],
                        "重启独显设备？屏幕可能短暂闪烁，正在使用独显的任务可能中断。",
                    ),
                ],
            },
            Section {
                title: tr_static("独显频率与电压").into(),
                hint: tr_static("只有 NVAPI 返回支持且数值范围有效时才能应用。"),
                fields: vec![
                    field("gpu.oc.enabled", "启用独显配置", Toggle, "gpu.oc.apply"),
                    field(
                        "gpu.oc.core_offset_mhz",
                        "核心偏移 · MHz",
                        Number,
                        "gpu.oc.apply",
                    ),
                    field(
                        "gpu.oc.memory_offset_mhz",
                        "显存偏移 · MHz",
                        Number,
                        "gpu.oc.apply",
                    ),
                    field("gpu.oc.voltage_mv", "电压上限 · mV", Number, "gpu.oc.apply"),
                    field("gpu.oc.lock_voltage", "锁定电压", Toggle, "gpu.oc.apply"),
                ],
                actions: vec![
                    confirmed(
                        "读取频率与电压范围",
                        "gpu.oc.refresh",
                        &[],
                        "读取独显频率、电压与合法范围可能唤醒独显；继续读取？",
                    ),
                    execute(
                        "应用并保存独显配置",
                        "gpu.oc.apply",
                        &[
                            "gpu.oc.enabled",
                            "gpu.oc.core_offset_mhz",
                            "gpu.oc.memory_offset_mhz",
                            "gpu.oc.voltage_mv",
                            "gpu.oc.lock_voltage",
                        ],
                    ),
                    doc("高级独显配置", "gpu_oc"),
                ],
            },
        ],
        Page::Battery => vec![
            Section {
                title: tr_static("电池与充电").into(),
                hint: tr_static("电量、健康度、循环与功率显示在上方。没有传感器的数据保留未知。"),
                fields: vec![
                    field("battery.mode", "充电模式", Choice(vec![]), "battery.apply"),
                    field(
                        "battery.start_threshold",
                        "开始充电 · %",
                        Number,
                        "battery.threshold",
                    ),
                    field(
                        "battery.stop_threshold",
                        "停止充电 · %",
                        Number,
                        "battery.threshold",
                    ),
                    field(
                        "battery.night_charge",
                        "夜间充电优化",
                        Toggle,
                        "battery.night_charge",
                    ),
                    field("battery.health_percent", "电池健康度 · %", ReadOnly, ""),
                    field("battery.cycle_count", "循环次数", ReadOnly, ""),
                    field("battery.full_charged_capacity", "满充容量", ReadOnly, ""),
                    field("battery.designed_capacity", "设计容量", ReadOnly, ""),
                    field("battery.rate", "当前充放电速率 · 单位见状态", ReadOnly, ""),
                    field("battery.rate_unit", "速率单位", ReadOnly, ""),
                    field(
                        "battery.minimum_rate_w",
                        "当前方向最小充放电功率 · W",
                        ReadOnly,
                        "",
                    ),
                    field(
                        "battery.maximum_rate_w",
                        "当前方向最大充放电功率 · W",
                        ReadOnly,
                        "",
                    ),
                    field(
                        "battery.on_battery_since",
                        "本次电池供电开始 · UTC",
                        ReadOnly,
                        "",
                    ),
                ],
                actions: vec![
                    execute(
                        "应用充电设置",
                        "battery.apply",
                        &[
                            "battery.mode",
                            "battery.start_threshold",
                            "battery.stop_threshold",
                            "battery.night_charge",
                        ],
                    ),
                    execute("读取电池详细状态", "battery.refresh", &[]),
                    execute("读取供电事件计时", "battery.history.refresh", &[]),
                    execute("重置电池使用计时", "battery.timer.reset", &[]),
                ],
            },
            Section {
                title: tr_static("供电与开机").into(),
                hint: tr_static("每个开关独立验证固件支持，不把不同开机功能合并为一个命令。"),
                fields: vec![
                    field(
                        "power.always_on_usb",
                        "关机 USB 供电",
                        Toggle,
                        "power.accessories.apply",
                    ),
                    field(
                        "power.instant_boot",
                        "接电开机",
                        Toggle,
                        "power.accessories.apply",
                    ),
                    field(
                        "power.flip_to_start",
                        "翻盖开机",
                        Toggle,
                        "power.accessories.apply",
                    ),
                ],
                actions: vec![execute(
                    "应用供电设置",
                    "power.accessories.apply",
                    &[
                        "power.always_on_usb",
                        "power.instant_boot",
                        "power.flip_to_start",
                    ],
                )],
            },
        ],
        Page::Keyboard => vec![
            Section {
                title: tr_static("键盘背光").into(),
                hint: tr_static(
                    "83NN 保留 Spectrum 固定分区候选；真实协议和键盘类型决定可用效果。未连接接口不会显示为已应用。",
                ),
                fields: vec![
                    field("keyboard.profile", "预设", Choice(vec![]), "keyboard.apply"),
                    field("keyboard.brightness", "亮度", Number, "keyboard.apply"),
                    field("keyboard.effect", "灯效", Choice(vec![]), "keyboard.apply"),
                    field(
                        "keyboard.zones",
                        "目标分区（以逗号分隔）",
                        Text,
                        "keyboard.apply",
                    ),
                    field("keyboard.color", "颜色 · #RRGGBB", Text, "keyboard.apply"),
                    field("keyboard.speed", "速度", Number, "keyboard.apply"),
                    field(
                        "keyboard.direction",
                        "方向",
                        Choice(vec![]),
                        "keyboard.apply",
                    ),
                    field("keyboard.preview", "实时预览", Toggle, "keyboard.preview"),
                ],
                actions: vec![
                    execute("检测键盘接口", "keyboard.candidates", &[]),
                    execute(
                        "应用背光",
                        "keyboard.apply",
                        &[
                            "keyboard.profile",
                            "keyboard.brightness",
                            "keyboard.effect",
                            "keyboard.zones",
                            "keyboard.color",
                            "keyboard.speed",
                            "keyboard.direction",
                            "keyboard.preview",
                        ],
                    ),
                    doc("多灯效与分区配置", "spectrum_keyboard"),
                    doc("旧式 RGB 配置", "rgb_keyboard"),
                ],
            },
            Section {
                title: tr_static("锁定与指示灯").into(),
                hint: tr_static("硬件不支持的控件会明确禁用。"),
                fields: vec![
                    field("keys.fn_lock", "Fn Lock", Toggle, "keys.apply"),
                    field("keys.win_lock", "Win 键锁定", Toggle, "keys.apply"),
                    field("keys.touchpad_lock", "触摸板锁定", Toggle, "keys.apply"),
                    field("keys.microphone_muted", "麦克风静音", Toggle, "keys.apply"),
                    field(
                        "keyboard.white_level",
                        "白光亮度",
                        Choice(vec![]),
                        "keyboard.white",
                    ),
                    field("keyboard.logo", "Logo 背光", Toggle, "keyboard.logo"),
                    field("keyboard.ports", "接口背光", Toggle, "keyboard.ports"),
                    field(
                        "keyboard.power_led",
                        "电源指示灯亮度",
                        Number,
                        "keyboard.power_led",
                    ),
                ],
                actions: vec![
                    execute(
                        "应用按键与指示灯",
                        "keys.apply",
                        &[
                            "keys.fn_lock",
                            "keys.win_lock",
                            "keys.touchpad_lock",
                            "keys.microphone_muted",
                            "keyboard.white_level",
                            "keyboard.logo",
                            "keyboard.ports",
                            "keyboard.power_led",
                        ],
                    ),
                    execute("发现按键：开始", "keys.discover.start", &[]),
                    execute("发现按键：停止", "keys.discover.stop", &[]),
                    doc("自定义 Fn 快捷键", "special_keys"),
                ],
            },
        ],
        Page::Automation => vec![Section {
            title: tr_static("自动化与快捷操作").into(),
            hint: tr_static("按顺序执行；无效参数、递归引用或不支持的硬件动作必须在执行前拒绝。"),
            fields: vec![
                field(
                    "automation.enabled",
                    "启用自动化",
                    Toggle,
                    "automation.settings.save",
                ),
                field(
                    "selection.automation",
                    "规则 / 快捷操作",
                    Collection("automation"),
                    "",
                ),
            ],
            actions: vec![
                execute(
                    "保存总开关",
                    "automation.settings.save",
                    &["automation.enabled"],
                ),
                edit(
                    "新建规则",
                    EditorKind::Automation,
                    "automation",
                    true,
                    "automation.save",
                ),
                edit(
                    "编辑所选规则",
                    EditorKind::Automation,
                    "automation",
                    false,
                    "automation.save",
                ),
                execute("立即运行", "automation.run", &["selection.automation"]),
                confirmed(
                    "删除规则",
                    "automation.delete",
                    &["selection.automation"],
                    "删除所选自动化规则？",
                ),
                execute("上移", "automation.move_up", &["selection.automation"]),
                execute("下移", "automation.move_down", &["selection.automation"]),
                doc("高级导入与导出", "automation"),
            ],
        }],
        Page::Macros => vec![Section {
            title: tr_static("键鼠宏").into(),
            hint: tr_static(
                "每个宏可以设置触发键、重复、延时和中断条件。录制必须显式开始，并提供停止入口。",
            ),
            fields: vec![
                field("macro.enabled", "启用宏", Toggle, "macro.settings.save"),
                field("selection.macros", "宏序列", Collection("macros"), ""),
            ],
            actions: vec![
                execute("保存总开关", "macro.settings.save", &["macro.enabled"]),
                edit("新建宏", EditorKind::Macro, "macros", true, "macro.save"),
                edit(
                    "编辑 / 录制",
                    EditorKind::Macro,
                    "macros",
                    false,
                    "macro.save",
                ),
                execute("运行所选宏", "macro.run", &["selection.macros"]),
                execute("停止回放与录制", "macro.stop", &[]),
                confirmed(
                    "删除宏",
                    "macro.delete",
                    &["selection.macros"],
                    "删除所选宏序列？",
                ),
                doc("高级导入与导出", "macro"),
            ],
        }],
        Page::Settings => tools(tools_tab, model),
        Page::Packages => packages(),
        Page::About => vec![Section {
            title: tr_static("项目与许可").into(),
            hint: tr_static(
                "仅适配本机 Y7000P IRX10 · 83NN。原生 Rust / Win32 界面。版本、设备能力和运行状态以后台确认结果为准。",
            ),
            fields: vec![],
            actions: vec![
                execute("版本与许可证", "tools.about", &[]),
                execute("打开数据目录", "tools.open_data", &[]),
                execute("打开日志目录", "tools.open_logs", &[]),
            ],
        }],
        Page::Donate => vec![Section {
            title: tr_static("项目与致谢").into(),
            hint: tr_static(
                "保留 Lenovo Legion Toolkit 原作者与维护者归属；机控是独立的本机 Rust 重构。赞助链接由后台明确展示并按用户点击打开。",
            ),
            fields: vec![],
            actions: vec![
                execute("原项目与作者", "tools.credits", &[]),
                execute("查看赞助方式", "tools.donate", &[]),
            ],
        }],
    }
}

pub const TOOL_TABS: [&str; 8] = [
    "应用行为",
    "外观",
    "电源",
    "显示",
    "智能按键",
    "软件控制",
    "更新",
    "集成",
];
pub const UPDATE_TAB: usize = 6;
fn tools(tab: usize, model: &UiModel) -> Vec<Section> {
    use FieldKind::*;
    let fields = match tab {
        0 => vec![
            field(
                "app.autorun",
                "开机启动",
                choices(&[("关闭", "off"), ("启动", "normal"), ("延迟启动", "delayed")]),
                "app.preferences.save",
            ),
            field(
                "app.minimize_to_tray",
                "最小化至托盘",
                Toggle,
                "app.preferences.save",
            ),
            field(
                "app.close_to_tray",
                "关闭时最小化",
                Toggle,
                "app.preferences.save",
            ),
            field(
                "app.always_on_top",
                "置顶窗口",
                Toggle,
                "app.preferences.save",
            ),
            field(
                "app.remember_position",
                "记住窗口位置",
                Toggle,
                "app.preferences.save",
            ),
            field(
                "app.lock_size",
                "锁定窗口大小",
                Toggle,
                "app.preferences.save",
            ),
            field("app.compact", "紧凑模式", Toggle, "app.preferences.save"),
            field("app.logging", "启用日志", Toggle, "app.preferences.save"),
            field(
                "sensors.enabled",
                "硬件传感器",
                Toggle,
                "app.preferences.save",
            ),
            field(
                "sensors.new_dashboard",
                "新的传感器仪表板",
                Toggle,
                "app.preferences.save",
            ),
            field("osd.enabled", "启用 OSD", Toggle, "osd.apply"),
            field("fps.status", "提交FPS采集状态", ReadOnly, ""),
            field("fps.source", "FPS 数据来源 · 非实际显示帧率", ReadOnly, ""),
            field("sensors.aux.status", "辅助传感读取状态", ReadOnly, ""),
            field(
                "game.detection",
                "游戏检测方式",
                Choice(vec![]),
                "app.preferences.save",
            ),
        ],
        1 => vec![
            field(
                "appearance.language",
                "语言",
                Choice(vec![]),
                "app.preferences.save",
            ),
            field("appearance.language_status", "语言状态", ReadOnly, ""),
            field(
                "appearance.theme",
                "主题",
                choices(&[("浅色", "light"), ("深色", "dark"), ("跟随系统", "system")]),
                "app.preferences.save",
            ),
            field(
                "appearance.accent",
                "主题色 · #RRGGBB",
                Text,
                "app.preferences.save",
            ),
            field(
                "appearance.background",
                "背景图片路径",
                Text,
                "app.preferences.save",
            ),
            field(
                "appearance.opacity",
                "背景透明度 · %",
                Number,
                "app.preferences.save",
            ),
            field("appearance.blur", "虚化", Number, "app.preferences.save"),
        ],
        2 => vec![
            field(
                "power.fnq_custom",
                "使用 Fn + Q 切换自定义模式",
                Toggle,
                "app.preferences.save",
            ),
            field(
                "power.mapping",
                "电源模式同步",
                choices(&[
                    ("不改变 Windows 设置", "disabled"),
                    ("Windows 电源模式", "mode"),
                    ("Windows 电源计划", "plan"),
                ]),
                "app.preferences.save",
            ),
            field(
                "battery.reset_timer_on_boot",
                "启动时重置电池使用时间",
                Toggle,
                "app.preferences.save",
            ),
        ],
        3 => vec![
            field(
                "display.sync_brightness",
                "固定显示亮度",
                Toggle,
                "app.preferences.save",
            ),
            field(
                "display.boot_logo.path",
                "自定义开机图案路径",
                Text,
                "display.boot_logo.apply",
            ),
            field(
                "display.boot_logo.enabled",
                "自定义开机图案",
                Toggle,
                "display.boot_logo.apply",
            ),
            field(
                "display.boot_animation",
                "开机动画",
                Toggle,
                "display.boot_logo.apply",
            ),
            field(
                "display.boot_logo.recovery_required",
                "是否有待恢复事务",
                ReadOnly,
                "",
            ),
            field(
                "selection.boot_logo_backups",
                "原值备份",
                Collection("boot_logo_backups"),
                "",
            ),
        ],
        4 => vec![
            field(
                "keys.smart_fn_lock",
                "智能锁定 Fn 键",
                Choice(vec![
                    UiChoice::new(tr_static("关闭"), 0),
                    UiChoice::new(tr_static("Shift"), 1),
                    UiChoice::new(tr_static("Ctrl"), 2),
                    UiChoice::new(tr_static("Shift / Ctrl"), 3),
                    UiChoice::new(tr_static("Alt"), 4),
                    UiChoice::new(tr_static("Shift / Alt"), 5),
                    UiChoice::new(tr_static("Ctrl / Alt"), 6),
                    UiChoice::new(tr_static("Shift / Ctrl / Alt"), 7),
                ]),
                "app.preferences.save",
            ),
            field("keys.status", "按键监听实际状态", ReadOnly, ""),
            field(
                "selection.special_keys",
                "自定义 Fn 按键",
                Collection("special_keys"),
                "",
            ),
        ],
        5 => vec![
            field(
                "software.vantage_disabled",
                "禁用 Vantage 与 ImController",
                Toggle,
                "software.apply",
            ),
            field(
                "software.zone_disabled",
                "禁用 Legion Zone",
                Toggle,
                "software.apply",
            ),
            field(
                "software.space_disabled",
                "禁用 Legion Space",
                Toggle,
                "software.apply",
            ),
            field(
                "software.hotkeys_disabled",
                "禁用 Lenovo Hotkeys",
                Toggle,
                "software.apply",
            ),
        ],
        6 => vec![
            field(
                "update.channel",
                "更新通道",
                choices(&[("稳定", "stable"), ("预览", "preview")]),
                "app.preferences.save",
            ),
            field(
                "update.method",
                "更新途径",
                Choice(vec![]),
                "app.preferences.save",
            ),
            field(
                "update.interval_hours",
                "更新频率 · 小时",
                Number,
                "app.preferences.save",
            ),
        ],
        _ => vec![
            field(
                "integration.hwinfo",
                "HWiNFO64 数据共享",
                Toggle,
                "integration.apply",
            ),
            field("integration.cli", "命令行界面", Toggle, "integration.apply"),
            field(
                "integration.path",
                "将 CLI 加入用户 PATH",
                Toggle,
                "integration.apply",
            ),
            field("script.command", "本机命令 / 脚本", Text, "script.run"),
            field("integration.cli.status", "CLI实际状态", ReadOnly, ""),
            field("integration.hwinfo.status", "HWiNFO实际状态", ReadOnly, ""),
            field("integration.error", "集成错误", ReadOnly, ""),
        ],
    };
    let keys: Vec<_> = fields
        .iter()
        .filter(|field| !matches!(field.kind, FieldKind::ReadOnly))
        .map(|f| f.key)
        .collect();
    let mut save = execute(
        "保存设置",
        if tab == 5 {
            "software.apply"
        } else if tab == 7 {
            "integration.apply"
        } else {
            "app.preferences.save"
        },
        &keys,
    );
    save.accessibility_role = "save-settings";
    let mut actions = vec![save, cancel(&keys)];
    match tab {
        0 => actions.extend([
            Action {
                accessibility_role: "sensor-settings",
                label: tr_static("传感器设置"),
                capability: "",
                kind: ActionKind::SensorSettings,
            },
            doc("自定义控制台", "dashboard"),
            doc("传感器分组", "sensor_dashboard"),
            doc("提示弹窗", "notifications"),
            doc("OSD 设置", "osd"),
            doc("游戏进程包含与排除", "game_detection"),
            doc("备份与还原设置", "settings_backup"),
            doc("启动参数", "arguments"),
            execute("重试辅助传感清理", "sensors.aux.retry_cleanup", &[]),
            execute("重试 FPS 清理", "fps.retry_cleanup", &[]),
            execute("脚本控制台", "script.console", &[]),
            confirmed(
                "重置设置",
                "settings.reset",
                &[],
                "重置机控设置？后台必须先保留可恢复备份。",
            ),
        ]),
        1 => {}
        2 => actions.extend([
            doc("Windows 电源模式", "power_modes"),
            doc("Windows 电源计划", "power_mapping"),
            execute("打开 Windows 电源计划", "tools.power_settings", &[]),
        ]),
        3 => actions.extend([
            execute("读取开机图案状态", "read.boot_logo", &[]),
            execute(
                "应用开机图案",
                "display.boot_logo.apply",
                &[
                    "display.boot_logo.path",
                    "display.boot_logo.enabled",
                    "display.boot_animation",
                ],
            ),
            confirmed(
                "禁用自定义开机图案",
                "display.boot_logo.disable",
                &[],
                "禁用当前自定义开机图案？修改前会保留本机原始备份。",
            ),
            confirmed(
                "还原所选原值备份",
                "display.boot_logo.restore",
                &["selection.boot_logo_backups"],
                "按所选本机备份还原图案、EFI文件与动画状态？",
            ),
            execute("重试未完成恢复", "display.boot_logo.retry_recovery", &[]),
        ]),
        4 => actions.extend([
            doc("自定义热键", "special_keys"),
            doc("排除刷新率", "excluded_refresh_rates"),
            execute("发现按键：开始", "keys.discover.start", &[]),
            execute("发现按键：停止", "keys.discover.stop", &[]),
            execute("重试按键清理", "keys.retry_cleanup", &[]),
        ]),
        5 => {
            actions.push(execute("刷新软件状态", "software.refresh", &[]));
            actions.push(confirmed(
                "应用软件控制",
                "software.apply",
                &keys,
                "更改联想软件服务状态？后台逐项核对并报告部分失败；已完成的更改不会自动撤销。",
            ));
        }
        6 => actions.extend([
            execute("检查更新", "update.check", &[]),
            execute("打开发行说明", "update.open_release", &[]),
        ]),
        _ => actions.extend([
            execute("应用集成设置", "integration.apply", &keys),
            execute("运行命令", "script.run", &["script.command"]),
            execute("停止命令", "script.stop", &[]),
            doc("旧脚本与扩展兼容记录", "legacy_extensions"),
        ]),
    }
    let _ = model;
    vec![Section {
        title: tr_static(TOOL_TABS[tab.min(7)]).into(),
        hint: tr_static([
            "管理启动、窗口行为、传感器与桌面显示。修改后保存设置。",
            "调整语言、主题色与窗口背景。修改后保存设置。",
            "设置性能模式与 Windows 电源选项之间的同步方式。",
            "管理屏幕亮度与开机画面。",
            "配置智能按键与自定义 Fn 功能。",
            "查看和管理本机联想软件的控制状态。",
            "检查工具箱更新与可用版本。",
            "管理集成选项与外部命令。",
        ][tab.min(7)]),
        fields,
        actions,
    }]
}

fn dashboard(model: &UiModel) -> Vec<Section> {
    use FieldKind::*;
    let mut power = sections(Page::Performance, 0, model).remove(0);
    power.title = "电源".into();
    power.hint = "性能模式、充电与供电。自定义模式设置仍在模式旁的配置入口。";
    power.fields.retain(|f| f.key != "power.fnq_custom");
    let mut battery = sections(Page::Battery, 0, model);
    power.fields.extend(
        battery[0]
            .fields
            .iter()
            .filter(|field| {
                [
                    "battery.mode",
                    "battery.start_threshold",
                    "battery.stop_threshold",
                    "battery.night_charge",
                ]
                .contains(&field.key)
            })
            .cloned(),
    );
    power.fields.append(&mut battery[1].fields);
    power.fields.push(field(
        "selection.presets",
        "自定义预设",
        Collection("presets"),
        "",
    ));
    power.actions.extend([
        edit(
            "自定义模式设置",
            EditorKind::Preset,
            "presets",
            false,
            "preset.save",
        ),
        edit(
            "新增预设",
            EditorKind::Preset,
            "presets",
            true,
            "preset.save",
        ),
        execute(
            "应用充电设置",
            "battery.apply",
            &[
                "battery.mode",
                "battery.start_threshold",
                "battery.stop_threshold",
                "battery.night_charge",
            ],
        ),
        execute(
            "应用供电设置",
            "power.accessories.apply",
            &[
                "power.always_on_usb",
                "power.instant_boot",
                "power.flip_to_start",
            ],
        ),
    ]);
    let mut display = sections(Page::Display, 0, model);
    let screen = display.remove(0);
    let mut graphics = display.remove(0);
    graphics.title = "显卡".into();
    graphics.hint = "显卡工作模式、独显状态与占用应用、频率和电压配置。";
    for mut section in display {
        graphics.fields.append(&mut section.fields);
        graphics.actions.append(&mut section.actions);
    }
    let other = Section {
        title: tr_static("其他").into(),
        hint: tr_static("本机支持的按键、指示灯与设备开关。无此硬件时显示明确原因。"),
        fields: vec![
            field("keys.microphone_muted", "麦克风静音", Toggle, "keys.apply"),
            field(
                "keyboard.white_level",
                "键盘白光",
                Choice(vec![]),
                "keyboard.white",
            ),
            field("keyboard.logo", "Logo 背光", Toggle, "keyboard.logo"),
            field("keyboard.ports", "接口背光", Toggle, "keyboard.ports"),
            field(
                "keyboard.power_led",
                "电源指示灯",
                FieldKind::Number,
                "keyboard.power_led",
            ),
            field("keys.touchpad_lock", "触摸板锁定", Toggle, "keys.apply"),
            field("keys.fn_lock", "Fn Lock", Toggle, "keys.apply"),
            field("keys.win_lock", "Win 键锁定", Toggle, "keys.apply"),
        ],
        actions: vec![
            execute(
                "应用设备开关",
                "keys.apply",
                &[
                    "keys.microphone_muted",
                    "keyboard.white_level",
                    "keyboard.logo",
                    "keyboard.ports",
                    "keyboard.power_led",
                    "keys.touchpad_lock",
                    "keys.fn_lock",
                    "keys.win_lock",
                ],
            ),
            doc("自定义控制台", "dashboard"),
        ],
    };
    configured_dashboard(vec![power, graphics, screen, other], model)
}
fn dashboard_item_fields(item: &str) -> &'static [&'static str] {
    match item {
        "PowerMode" => &["power.mode", "selection.presets"],
        "BatteryMode" => &["battery.mode"],
        "BatteryNightChargeMode" => &["battery.night_charge"],
        "ChargeThreshold" => &["battery.start_threshold", "battery.stop_threshold"],
        "AlwaysOnUsb" => &["power.always_on_usb"],
        "InstantBoot" => &["power.instant_boot"],
        "FlipToStart" => &["power.flip_to_start"],
        "HybridMode" => &["gpu.hybrid_mode"],
        "DiscreteGpu" => &[
            "selection.gpu.processes",
            "gpu.process.preference",
            "gpu.keep_awake",
            "gpu.locked_pstate",
            "gpu.poll_ms",
        ],
        "OverclockDiscreteGpu" => &[
            "gpu.oc.enabled",
            "gpu.oc.core_offset_mhz",
            "gpu.oc.memory_offset_mhz",
            "gpu.oc.voltage_mv",
            "gpu.oc.lock_voltage",
        ],
        "Resolution" => &[
            "display.monitor",
            "display.resolution",
            "display.brightness",
        ],
        "RefreshRate" => &["display.refresh_rate"],
        "DpiScale" => &["display.dpi"],
        "Hdr" => &["display.hdr"],
        "AutoColorManagement" => &["display.acm"],
        "OverDrive" => &["display.overdrive"],
        "Microphone" => &["keys.microphone_muted"],
        "WhiteKeyboardBacklight" => &["keyboard.white_level"],
        "PanelLogoBacklight" => &["keyboard.logo"],
        "PortsBacklight" => &["keyboard.ports"],
        "PowerButtonLed" => &["keyboard.power_led"],
        "TouchpadLock" => &["keys.touchpad_lock"],
        "FnLock" => &["keys.fn_lock"],
        "WinKeyLock" => &["keys.win_lock"],
        _ => &[],
    }
}
fn configured_dashboard(original: Vec<Section>, model: &UiModel) -> Vec<Section> {
    let Some(groups) = model
        .documents
        .get("dashboard")
        .and_then(|d| d.value.get("Groups"))
        .and_then(Value::as_array)
    else {
        return original;
    };
    let all_fields = original
        .iter()
        .flat_map(|s| &s.fields)
        .cloned()
        .collect::<Vec<_>>();
    let all_actions = original
        .iter()
        .flat_map(|s| &s.actions)
        .cloned()
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    for group in groups {
        let Some(items) = group.get("Items").and_then(Value::as_array) else {
            continue;
        };
        let items = items.iter().filter_map(Value::as_str).collect::<Vec<_>>();
        let mut fields = Vec::new();
        for item in &items {
            for key in dashboard_item_fields(item) {
                if let Some(field) = all_fields.iter().find(|f| f.key == *key) {
                    if !fields.iter().any(|f: &Field| f.key == field.key) {
                        fields.push(field.clone());
                    }
                }
            }
        }
        let actions = all_actions
            .iter()
            .filter(|a| match &a.kind {
                ActionKind::Execute {
                    command,
                    fields: required,
                    ..
                } => {
                    if *command == "display.turn_off" {
                        return items.contains(&"TurnOffMonitors");
                    }
                    if *command == "power.ai.settings" {
                        return items.contains(&"PowerMode");
                    }
                    if [
                        "gpu.sleep",
                        "gpu.restart",
                        "gpu.refresh",
                        "gpu.processes.refresh",
                        "gpu.pstate.refresh",
                    ]
                    .contains(command)
                    {
                        return items.contains(&"DiscreteGpu");
                    }
                    if *command == "gpu.oc.refresh" {
                        return items.contains(&"OverclockDiscreteGpu");
                    }
                    required
                        .iter()
                        .any(|key| fields.iter().any(|f| f.key == *key))
                }
                ActionKind::Structured { collection, .. } => {
                    *collection == "presets" && items.contains(&"PowerMode")
                }
                ActionKind::Document(id) => match *id {
                    "dashboard" => false,
                    "gpu_oc" => items.contains(&"OverclockDiscreteGpu"),
                    "excluded_refresh_rates" => items.contains(&"RefreshRate"),
                    _ => false,
                },
                ActionKind::Cancel(keys) => {
                    keys.iter().any(|key| fields.iter().any(|f| f.key == *key))
                }
                _ => false,
            })
            .cloned()
            .collect::<Vec<_>>();
        let ty = group
            .get("Type")
            .and_then(Value::as_str)
            .unwrap_or("Custom");
        let title = match ty {
            "Power" => tr_static("电源"),
            "Graphics" => tr_static("显卡"),
            "Display" => tr_static("显示"),
            "Other" => tr_static("其他"),
            _ => group
                .get("CustomName")
                .and_then(Value::as_str)
                .unwrap_or_else(|| tr_static("自定义分组")),
        }
        .to_owned();
        output.push(Section {
            title,
            hint: tr_static("按已保存的控制台分组显示。"),
            fields,
            actions,
        });
    }
    if let Some(last) = output.last_mut() {
        last.actions.push(doc("自定义控制台", "dashboard"));
    } else {
        output.push(Section {
            title: tr_static("控制台").into(),
            hint: tr_static("当前没有显示的分组。"),
            fields: vec![],
            actions: vec![doc("自定义控制台", "dashboard")],
        });
    }
    output
}
fn packages() -> Vec<Section> {
    use FieldKind::*;
    vec![Section {
        title: tr_static("驱动下载").into(),
        hint: tr_static(
            "按83NN与当前系统获取驱动。这里只下载，不会擅自安装软件；可打开说明和下载目录。",
        ),
        fields: vec![
            field(
                "packages.machine_type",
                "设备代号",
                Text,
                "packages.refresh",
            ),
            field("packages.os", "系统", Choice(vec![]), "packages.refresh"),
            field("packages.directory", "下载文件夹", Text, "packages.refresh"),
            field(
                "packages.source",
                "来源",
                Choice(vec![]),
                "packages.refresh",
            ),
            field(
                "packages.filter",
                "名称、类别或说明过滤",
                Text,
                "packages.filter",
            ),
            field(
                "packages.sort",
                "排序",
                choices(&[("名称", "name"), ("类别", "category"), ("日期", "date")]),
                "packages.filter",
            ),
            field(
                "packages.only_updates",
                "仅显示有更新的项目",
                Toggle,
                "packages.refresh",
            ),
            field(
                "selection.packages",
                "驱动与软件",
                Collection("packages"),
                "",
            ),
        ],
        actions: vec![
            execute(
                "刷新",
                "packages.refresh",
                &[
                    "packages.machine_type",
                    "packages.os",
                    "packages.directory",
                    "packages.source",
                    "packages.filter",
                    "packages.sort",
                    "packages.only_updates",
                ],
            ),
            execute(
                "筛选与排序",
                "packages.filter",
                &["packages.filter", "packages.sort"],
            ),
            execute("取消", "packages.cancel", &[]),
            execute(
                "下载所选项目",
                "packages.download",
                &["selection.packages", "packages.directory"],
            ),
            execute("查看说明", "packages.readme", &["selection.packages"]),
            execute("隐藏所选项目", "packages.hide", &["selection.packages"]),
            confirmed(
                "隐藏当前列表全部",
                "packages.hide_all",
                &[],
                "隐藏当前过滤列表中的全部项目？可以恢复隐藏项目。",
            ),
            execute(
                "复制所选信息",
                "packages.copy.info",
                &["selection.packages"],
            ),
            execute("复制下载链接", "packages.copy.url", &["selection.packages"]),
            execute("恢复隐藏项目", "packages.unhide_all", &[]),
            execute(
                "打开下载文件夹",
                "packages.open_directory",
                &["packages.directory"],
            ),
        ],
    }]
}

/// Full original action/trigger families remain represented. Backend capability
/// gates determine which can actually be saved/executed on this machine.
pub const AUTOMATION_ACTIONS: &[(&str, &str)] = &[
    ("power_mode", "性能模式"),
    ("godmode_preset", "自定义预设"),
    ("cycle_preset", "循环预设"),
    ("ai_engine", "AI 引擎"),
    ("battery", "充电模式"),
    ("battery_night", "夜间充电"),
    ("always_on_usb", "USB 供电"),
    ("deactivate_gpu", "独显休眠"),
    ("delay", "等待"),
    ("display_brightness", "屏幕亮度"),
    ("dpi", "显示缩放"),
    ("flip_to_start", "翻盖开机"),
    ("fn_lock", "Fn Lock"),
    ("sensors", "硬件传感器"),
    ("hdr", "HDR"),
    ("acm", "自动色彩管理"),
    ("its_mode", "ITS 模式（本机不适用）"),
    ("hybrid", "显卡工作模式"),
    ("instant_boot", "接电开机"),
    ("macro", "宏"),
    ("microphone", "麦克风"),
    ("speaker", "扬声器"),
    ("volume", "音量"),
    ("notification", "通知"),
    ("white_single", "单级白光"),
    ("gpu_oc", "独显频率配置"),
    ("overdrive", "OverDrive"),
    ("panel_logo", "Logo 背光"),
    ("play_sound", "播放声音"),
    ("ports_light", "接口背光"),
    ("quick_action", "快捷操作"),
    ("refresh_rate", "刷新率"),
    ("resolution", "分辨率"),
    ("rgb_keyboard", "RGB 背光"),
    ("run", "运行程序"),
    ("spectrum_brightness", "Spectrum 亮度"),
    ("spectrum_profile", "Spectrum 预设"),
    ("spectrum_import", "导入 Spectrum 预设"),
    ("touchpad", "触摸板锁定"),
    ("monitors_off", "关闭显示器"),
    ("wifi", "Wi-Fi"),
    ("airplane", "飞行模式"),
    ("white_keyboard", "白光背光"),
    ("win_key", "Win 键锁定"),
    ("close_app", "关闭机控"),
    ("show_app", "显示机控"),
    ("osd", "OSD"),
    ("osd_lock", "锁定 OSD"),
    ("fan_max", "风扇全速"),
];
pub const AUTOMATION_TRIGGERS: &[(&str, &str)] = &[
    ("manual", "手动快捷操作"),
    ("ac_connected", "接入电源"),
    ("low_watt_ac", "低功率电源"),
    ("ac_disconnected", "断开电源"),
    ("battery_percentage", "电量阈值"),
    ("power_mode", "性能模式改变"),
    ("godmode_preset", "预设改变"),
    ("games_running", "游戏启动"),
    ("games_stopped", "游戏停止"),
    ("process_running", "进程启动"),
    ("process_stopped", "进程结束"),
    ("user_idle", "用户空闲"),
    ("session_lock", "锁屏"),
    ("session_unlock", "解锁"),
    ("lid_open", "开盖"),
    ("lid_closed", "合盖"),
    ("display_on", "屏幕开启"),
    ("display_off", "屏幕关闭"),
    ("hdr_on", "HDR 开启"),
    ("hdr_off", "HDR 关闭"),
    ("hybrid", "显卡模式改变"),
    ("device_connected", "设备接入"),
    ("device_disconnected", "设备拔出"),
    ("external_display_connected", "外接屏接入"),
    ("external_display_disconnected", "外接屏断开"),
    ("wifi_connected", "Wi-Fi 连接"),
    ("wifi_disconnected", "Wi-Fi 断开"),
    ("time", "时间 / 日出日落"),
    ("time_range", "时间范围"),
    ("periodic", "周期"),
    ("startup", "启动后"),
    ("resume", "恢复唤醒"),
    ("its_mode", "ITS（本机不适用）"),
];

#[cfg(test)]
mod translation_tests {
    use super::*;
    use crate::ui::i18n::{TestLanguage, set_language};
    use crate::ui_model::UiDocument;
    #[test]
    fn translated_metadata_preserves_commands_and_custom_group_names_even_if_they_equal_a_product_label()
     {
        let _restore = TestLanguage::new("zh-CN");
        let mut model = UiModel::default();
        model.documents.insert("dashboard".into(),UiDocument{value:json!({"Groups":[{"Type":"Custom","CustomName":"设置","Items":["PowerMode"]}]}),..Default::default()});
        let before = sections(Page::Dashboard, 0, &model);
        assert_eq!(before[0].title, "设置");
        set_language("en");
        let after = sections(Page::Dashboard, 0, &model);
        assert_eq!(Page::Settings.title(), "Settings");
        assert_eq!(after[0].title, "设置");
        let commands = |groups: Vec<Section>| {
            groups
                .into_iter()
                .flat_map(|s| s.actions)
                .filter_map(|a| {
                    if let ActionKind::Execute {
                        command,
                        fields,
                        extra,
                        ..
                    } = a.kind
                    {
                        Some((command, fields, extra))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(commands(before), commands(after));
    }
}

#[cfg(test)]
mod readonly_field_tests {
    use super::*;
    #[test]
    fn readonly_status_keys_never_enter_preferences_save_payload() {
        let model = UiModel::default();
        for tab in 0..8 {
            for section in sections(Page::Settings, tab, &model) {
                let readonly = section
                    .fields
                    .iter()
                    .filter(|f| matches!(f.kind, FieldKind::ReadOnly))
                    .map(|f| f.key)
                    .collect::<Vec<_>>();
                for action in section.actions {
                    if let ActionKind::Execute { fields, .. } = action.kind {
                        assert!(
                            !fields.iter().any(|key| readonly.contains(key)),
                            "readonly field in save payload: {tab}"
                        );
                    }
                }
            }
        }
    }
}
