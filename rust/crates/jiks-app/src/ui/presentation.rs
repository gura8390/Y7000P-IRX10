//! Short descriptions for the same settings exposed by the original toolbox.
//! These explain controls, not runtime implementation details.
pub(super) fn field_hint(key: &str) -> &'static str {
    match key {
        "power.mode" => "选择性能模式；自定义模式可进一步配置功耗、风扇与温度限制。",
        "battery.mode" => "选择电池充电策略，兼顾续航与长期电池维护。",
        "battery.start_threshold" => "电量低于此值时开始充电。",
        "battery.stop_threshold" => "充电达到此值后停止，减少长期满电停留。",
        "battery.night_charge" => "根据夜间使用习惯优化充电。",
        "battery.always_on_usb" => "允许支持的 USB 接口在关机后继续供电。",
        "graphics.mode" => "选择核显与独显的工作方式；部分模式需要重启。",
        "gpu.keep_awake" => "保持独显处于唤醒状态，会增加待机耗电。",
        "display.resolution" => "更改当前显示器的分辨率。",
        "display.refresh" => "调整刷新率，平衡画面流畅度与电池续航。",
        "display.dpi" => "调整文字、图标和界面元素的缩放。",
        "display.hdr" => "启用高动态范围，以增强对比度与色彩层次。",
        "display.overdrive" => "加快内置屏幕响应；部分画面可能出现过冲。",
        "microphone.enabled" => "控制可用麦克风的静音状态。",
        "keyboard.white_brightness" => "也可以使用 Fn + 空格调整键盘背光亮度。",
        "keyboard.fn_lock" => "切换功能键与 F1–F12 键的默认行为。",
        "keyboard.win_lock" => "锁定内置键盘的 Windows 键，减少误触。",
        "touchpad.enabled" => "控制触控板，使用外接鼠标时可减少误触。",
        "app.autorun" => "登录 Windows 后启动工具箱。",
        "app.minimize_to_tray" => "最小化时收起到系统托盘。",
        "app.close_to_tray" => "关闭窗口时保留后台；使用退出按钮结束运行。",
        "app.lock_size" => "保持当前窗口尺寸。",
        "app.remember_position" => "下次启动时恢复窗口的位置。",
        "app.always_on_top" => "让工具箱保持在其他窗口上方。",
        "app.compact" => "减小设置行间距，显示更多内容。",
        "app.logging" => "记录运行日志，便于排查问题。",
        "appearance.theme" => "选择浅色、深色或跟随 Windows 的外观。",
        "appearance.language" => "更改工具箱的界面语言。",
        "appearance.accent" => "设置选中标记、开关与主要按钮的颜色。",
        "appearance.background" => "选择窗口背景图片。留空时使用默认背景。",
        "appearance.opacity" => "调整背景图片与界面的混合程度。",
        "sensors.enabled" => "在控制台显示硬件状态；可进入传感器设置调整。",
        "sensors.new_dashboard" => "使用可自定义分组的传感器布局。",
        "osd.enabled" => "在桌面显示可独立配置的硬件状态面板。",
        "thermal.enabled" => "启用本机 CPU 温度控制。请先检查当前性能模式。",
        "thermal.shadow" => "观察控温策略的判断，不下发限制。",
        _ => "",
    }
}

pub(super) fn field_icon(key: &str) -> u32 {
    if key.contains("keyboard") || key.contains("lock") {
        1
    } else if key.contains("battery") || key.contains("charge") {
        2
    } else if key.contains("display") || key.contains("screen") {
        5
    } else if key.contains("power") || key.contains("thermal") || key.contains("gpu") {
        3
    } else {
        6
    }
}
