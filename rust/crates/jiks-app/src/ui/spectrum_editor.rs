//! Structured editing of cached 83NN Spectrum effects. No HID operations here.
use super::*;
use crate::ui::i18n::{tr, tr_static};
const TYPES: &[(&str, &str)] = &[
    ("常亮", "Always"),
    ("色彩波动", "ColorWave"),
    ("色彩脉冲", "ColorPulse"),
    ("平滑", "Smooth"),
    ("音频律动", "AudioBounce"),
];
fn color(value: &Value) -> String {
    format!(
        "#{:02X}{:02X}{:02X}",
        value.get("R").and_then(Value::as_u64).unwrap_or(0),
        value.get("G").and_then(Value::as_u64).unwrap_or(0),
        value.get("B").and_then(Value::as_u64).unwrap_or(0)
    )
}
fn parse_color(text: &str) -> Result<Value> {
    let hex = text
        .trim()
        .strip_prefix('#')
        .ok_or_else(|| anyhow!("{}", tr_static("颜色请使用 #RRGGBB。")))?;
    if hex.len() != 6 {
        bail!("{}", tr_static("颜色请使用 #RRGGBB。"));
    }
    let n = u32::from_str_radix(hex, 16)
        .map_err(|_| anyhow!("{}", tr_static("颜色请使用 #RRGGBB。")))?;
    Ok(json!({"R":(n>>16)&255,"G":(n>>8)&255,"B":n&255}))
}
pub(super) fn cached(model: &UiModel, profile: &Value) -> Option<Vec<Value>> {
    if !profile.as_u64().is_some_and(|value| (1..=6).contains(&value))
        || model.values.get("keyboard.effects_profile") != Some(profile)
    {
        return None;
    }
    model
        .values
        .get("keyboard.effects")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| {
            model
                .collections
                .get("keyboard.effects")
                .map(|items| items.iter().map(|item| item.value.clone()).collect())
        })
}
fn validate(effects: &[Value]) -> Result<()> {
    if effects.is_empty() {
        bail!("{}", tr_static("请至少添加一个灯效，或使用恢复预设。"));
    }
    for effect in effects {
        let ty = effect.get("Type").and_then(Value::as_str).unwrap_or("");
        if !TYPES.iter().any(|(_, id)| *id == ty) {
            bail!(
                "{}",
                tr_static("此灯效不适用于本机 24 分区键盘，请移除或更改后再应用。")
            );
        }
        let keys = effect
            .get("Keys")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("{}", tr_static("分区数据无效。")))?;
        if keys
            .iter()
            .any(|key| !key.as_u64().is_some_and(|n| (1..=24).contains(&n)))
        {
            bail!("{}", tr_static("分区编号应为1–24。"));
        }
        if ty != "AudioBounce" && keys.is_empty() {
            bail!("{}", tr_static("请为灯效选择至少一个分区。"));
        }
        if ty == "AudioBounce" && !keys.is_empty() {
            bail!("{}", tr_static("音频律动使用整个键盘。"));
        }
        let colors = effect
            .get("Colors")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("{}", tr_static("颜色列表无效。")))?;
        if colors.len() > 255 {
            bail!("{}", tr_static("颜色列表最多255项。"));
        }
        if ty != "AudioBounce" && colors.is_empty() {
            bail!("{}", tr_static("请至少添加一种颜色。"));
        }
        for color in colors {
            for channel in ["R", "G", "B"] {
                if !color
                    .get(channel)
                    .and_then(Value::as_u64)
                    .is_some_and(|n| n <= 255)
                {
                    bail!("{}", tr_static("颜色通道范围为0–255。"));
                }
            }
        }
        if effect.get("UseVantageColorBoost").and_then(Value::as_bool) == Some(true) {
            bail!("{}", tr_static("本机24分区键盘不支持 Aurora 色彩增强。"));
        }
    }
    Ok(())
}
impl Editor {
    pub(super) fn load_spectrum_effect(&mut self) {
        let effect=self.selected_step.and_then(|i|self.steps.get(i)).cloned().unwrap_or_else(||json!({"Type":"Always","Speed":"None","Direction":"None","ClockwiseDirection":"None","ColorMode":"None","Colors":[{"R":255,"G":255,"B":255}],"Keys":(1..=24).collect::<Vec<_>>()}));
        for (draft, key, default) in [
            ("spectrum.type", "Type", "Always"),
            ("spectrum.speed", "Speed", "None"),
            ("spectrum.direction", "Direction", "None"),
            ("spectrum.clockwise", "ClockwiseDirection", "None"),
            ("spectrum.color_mode", "ColorMode", "None"),
        ] {
            self.drafts.insert(
                draft.into(),
                effect.get(key).cloned().unwrap_or_else(|| json!(default)),
            );
        }
        for zone in 1..=24 {
            self.drafts.insert(
                format!("spectrum.zone.{zone}"),
                json!(
                    effect
                        .get("Keys")
                        .and_then(Value::as_array)
                        .is_some_and(|keys| keys.contains(&json!(zone)))
                ),
            );
        }
        self.conditions = effect
            .get("Colors")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.selected_condition = None;
        self.drafts
            .insert("spectrum.color".into(), json!("#FFFFFF"));
    }
    pub(super) fn make_spectrum_effect(&self) -> Result<Value> {
        let ty = self.value("spectrum.type");
        let all = ty == "AudioBounce";
        let keys = if all {
            vec![]
        } else {
            (1..=24)
                .filter(|zone| {
                    self.value(&format!("spectrum.zone.{zone}"))
                        .as_bool()
                        .unwrap_or(false)
                })
                .collect::<Vec<_>>()
        };
        let mut effect = self
            .selected_step
            .and_then(|i| self.steps.get(i))
            .cloned()
            .unwrap_or_else(|| json!({}));
        for (key, value) in [
            ("Type", ty),
            ("Speed", self.value("spectrum.speed")),
            ("Direction", self.value("spectrum.direction")),
            ("ClockwiseDirection", self.value("spectrum.clockwise")),
            ("ColorMode", self.value("spectrum.color_mode")),
            ("Colors", json!(self.conditions)),
            ("Keys", json!(keys)),
            ("UseVantageColorBoost", json!(false)),
        ] {
            effect[key] = value;
        }
        validate(&[effect.clone()])?;
        Ok(effect)
    }
    pub(super) unsafe fn spectrum_form(&mut self, width: i32, mut y: i32, model: &UiModel) -> i32 {
        unsafe {
            self.label("Spectrum 多灯效与分区", y, width, 32, true);
            y += 42;
            self.label(
                if self.data.get("Loaded").and_then(Value::as_bool) == Some(true)
                    && self.data.get("Profile") == Some(&self.value("spectrum.profile"))
                {
                    "灯效从后台快照读取。列表后面的灯效覆盖前面重叠的分区；应用后才会改变键盘。"
                } else {
                    "尚未读取此预设的灯效。可以编辑草稿；读取成功前不能应用。"
                },
                y,
                width,
                58,
                false,
            );
            y += 64;
            self.input(
                "spectrum.profile",
                "预设",
                Kind::Choice(
                    (1..=6)
                        .map(|n| {
                            UiChoice::new(tr_static("预设 {n}").replace("{n}", &n.to_string()), n)
                        })
                        .collect(),
                ),
                y,
                width,
            );
            y += 42;
            self.button("重新读取", 127, 20, y, 112, "keyboard.effects.refresh");
            self.button("恢复此预设…", 128, 144, y, 148, "keyboard.effects.reset");
            y += 44;
            let list = self.place(
                "LISTBOX",
                "",
                WS_BORDER | WS_VSCROLL | WS_TABSTOP | LBS_NOTIFY as u32,
                120,
                [20, y, width, 130],
                self.font,
            );
            for (index, effect) in self.steps.iter().enumerate() {
                let ty = effect
                    .get("Type")
                    .and_then(Value::as_str)
                    .unwrap_or("未知灯效");
                let name = TYPES
                    .iter()
                    .find(|(_, id)| *id == ty)
                    .map(|(label, _)| *label)
                    .unwrap_or(ty);
                let count = effect
                    .get("Keys")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                SendMessageW(
                    list,
                    LB_ADDSTRING,
                    0,
                    wide(&format!(
                        "{}. {} · {}",
                        index + 1,
                        tr(name),
                        if count == 0 {
                            tr_static("全键盘").into()
                        } else {
                            tr_static("{count} 个分区").replace("{count}", &count.to_string())
                        }
                    ))
                    .as_ptr() as isize,
                );
            }
            if let Some(i) = self.selected_step {
                SendMessageW(list, LB_SETCURSEL, i, 0);
            }
            y += 140;
            for (label, id, x, w) in [
                ("添加灯效", 121, 20, 108),
                ("更新所选", 122, 140, 108),
                ("删除", 123, 260, 80),
                ("上移", 124, 352, 80),
                ("下移", 125, 444, 80),
                ("清空", 126, 536, 80),
            ] {
                self.button(label, id, x, y, w, "");
            }
            y += 44;
            self.input(
                "spectrum.type",
                "灯效",
                Kind::Choice(
                    TYPES
                        .iter()
                        .map(|(label, id)| UiChoice::new(tr(label).into_owned(), *id))
                        .collect(),
                ),
                y,
                width,
            );
            y += 42;
            self.input(
                "spectrum.speed",
                "速度",
                Kind::Choice(
                    [
                        ("无", "None"),
                        ("慢", "Speed1"),
                        ("中", "Speed2"),
                        ("快", "Speed3"),
                    ]
                    .iter()
                    .map(|(l, v)| UiChoice::new(tr(l).into_owned(), *v))
                    .collect(),
                ),
                y,
                width,
            );
            y += 42;
            self.input(
                "spectrum.direction",
                "方向",
                Kind::Choice(
                    [
                        ("无", "None"),
                        ("下至上", "BottomToTop"),
                        ("上至下", "TopToBottom"),
                        ("左至右", "LeftToRight"),
                        ("右至左", "RightToLeft"),
                    ]
                    .iter()
                    .map(|(l, v)| UiChoice::new(tr(l).into_owned(), *v))
                    .collect(),
                ),
                y,
                width,
            );
            y += 42;
            self.input(
                "spectrum.color_mode",
                "色彩模式",
                Kind::Choice(
                    [
                        ("按效果默认", "None"),
                        ("随机颜色", "RandomColor"),
                        ("颜色列表", "ColorList"),
                    ]
                    .iter()
                    .map(|(l, v)| UiChoice::new(tr(l).into_owned(), *v))
                    .collect(),
                ),
                y,
                width,
            );
            y += 44;
            self.label("颜色列表 · #RRGGBB", y, width, 28, true);
            y += 34;
            let list = self.place(
                "LISTBOX",
                "",
                WS_BORDER | WS_VSCROLL | WS_TABSTOP | LBS_NOTIFY as u32,
                130,
                [20, y, width, 95],
                self.font,
            );
            for value in &self.conditions {
                SendMessageW(list, LB_ADDSTRING, 0, wide(&color(value)).as_ptr() as isize);
            }
            if let Some(i) = self.selected_condition {
                SendMessageW(list, LB_SETCURSEL, i, 0);
            }
            y += 105;
            self.input("spectrum.color", "添加颜色", Kind::Text, y, width);
            y += 42;
            for (label, id, x, w) in [
                ("添加颜色", 131, 20, 108),
                ("删除颜色", 132, 140, 108),
                ("上移", 133, 260, 80),
                ("下移", 134, 352, 80),
            ] {
                self.button(label, id, x, y, w, "");
            }
            y += 44;
            self.label("24 分区 · 音频律动自动使用全键盘", y, width, 30, true);
            y += 36;
            let columns = if width >= 700 { 8 } else { 6 };
            let cell = (width / columns).max(65);
            for zone in 1..=24 {
                let index = zone - 1;
                let id = 1000 + self.edits.len() as u16;
                let key = format!("spectrum.zone.{zone}");
                let h = self.place(
                    "BUTTON",
                    &format!("{zone}"),
                    BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
                    id,
                    [
                        20 + (index % columns) * cell,
                        y + (index / columns) * 34,
                        cell - 4,
                        30,
                    ],
                    self.font,
                );
                a11y::annotate(
                    h,
                    &a11y::key(&self.mode.accessibility_key(), "field", &key),
                    Some(&format!("{} {zone}", tr_static("分区"))),
                    None,
                );
                SendMessageW(
                    h,
                    BM_SETCHECK,
                    if self.value(&key).as_bool().unwrap_or(false) {
                        BST_CHECKED as usize
                    } else {
                        BST_UNCHECKED as usize
                    },
                    0,
                );
                self.edits.push(Edit {
                    key,
                    hwnd: h,
                    kind: Kind::Check,
                });
            }
            y += ((24 + columns - 1) / columns) * 34 + 8;
            self.button("全选分区", 142, 20, y, 120, "");
            self.button("清除选择", 143, 152, y, 120, "");
            y += 46;
            self.input("spectrum.path", "导入 / 导出文件路径", Kind::Text, y, width);
            y += 42;
            self.button("导入到草稿", 135, 20, y, 136, "keyboard.effects.import");
            self.button("导出灯效", 136, 168, y, 120, "keyboard.effects.export");
            self.button("选择导出文件…", 147, 300, y, 170, "");
            y += 46;
            let _ = model;
            y
        }
    }
    pub(super) unsafe fn spectrum_command(&mut self, id: u16, notification: u16) -> Result<()> {
        unsafe {
            match id {
                120 if notification == LBN_SELCHANGE as u16 => {
                    let i = SendMessageW(GetDlgItem(self.hwnd, 120), LB_GETCURSEL, 0, 0);
                    self.selected_step = (i >= 0).then_some(i.max(0) as usize);
                    self.load_spectrum_effect();
                }
                121 => {
                    let effect = self.make_spectrum_effect()?;
                    self.steps.push(effect);
                    self.selected_step = Some(self.steps.len() - 1);
                }
                122 => {
                    let index = self
                        .selected_step
                        .ok_or_else(|| anyhow!("{}", tr_static("请选择要更新的灯效。")))?;
                    self.steps[index] = self.make_spectrum_effect()?;
                }
                123 => {
                    let index = self
                        .selected_step
                        .ok_or_else(|| anyhow!("{}", tr_static("请选择要删除的灯效。")))?;
                    self.steps.remove(index);
                    self.selected_step = None;
                    self.load_spectrum_effect();
                }
                124 | 125 => {
                    let i = self
                        .selected_step
                        .ok_or_else(|| anyhow!("{}", tr_static("请选择灯效。")))?;
                    let target = if id == 124 {
                        i.saturating_sub(1)
                    } else {
                        (i + 1).min(self.steps.len() - 1)
                    };
                    self.steps.swap(i, target);
                    self.selected_step = Some(target);
                }
                126 => {
                    if confirm(self.hwnd, "清空灯效草稿？应用前不会改变键盘。") {
                        self.steps.clear();
                        self.selected_step = None;
                        self.load_spectrum_effect();
                    }
                }
                127 | 128 => {
                    if id == 128
                        && !confirm(
                            self.hwnd,
                            "恢复所选键盘预设的固件默认灯效？此操作会改变键盘灯效。",
                        )
                    {
                        return Ok(());
                    }
                    self.submit(
                        if id == 127 {
                            "keyboard.effects.refresh"
                        } else {
                            "keyboard.effects.reset"
                        },
                        json!({"profile":self.value("spectrum.profile")}),
                        false,
                        None,
                    )?;
                    return Ok(());
                }
                130 if notification == LBN_SELCHANGE as u16 => {
                    let i = SendMessageW(GetDlgItem(self.hwnd, 130), LB_GETCURSEL, 0, 0);
                    self.selected_condition = (i >= 0).then_some(i.max(0) as usize);
                }
                131 => {
                    if self.conditions.len() >= 255 {
                        bail!("{}", tr_static("颜色列表最多255项。"));
                    }
                    self.conditions.push(parse_color(
                        self.value("spectrum.color").as_str().unwrap_or(""),
                    )?);
                    self.selected_condition = Some(self.conditions.len() - 1);
                }
                132 => {
                    let i = self
                        .selected_condition
                        .ok_or_else(|| anyhow!("{}", tr_static("请选择颜色。")))?;
                    self.conditions.remove(i);
                    self.selected_condition = None;
                }
                133 | 134 => {
                    let i = self
                        .selected_condition
                        .ok_or_else(|| anyhow!("{}", tr_static("请选择颜色。")))?;
                    let target = if id == 133 {
                        i.saturating_sub(1)
                    } else {
                        (i + 1).min(self.conditions.len() - 1)
                    };
                    self.conditions.swap(i, target);
                    self.selected_condition = Some(target);
                }
                135 | 136 => {
                    let path = self
                        .value("spectrum.path")
                        .as_str()
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    if path.is_empty() {
                        bail!("{}", tr_static("请填写文件路径。"));
                    }
                    self.submit(if id==135{"keyboard.effects.import"}else{"keyboard.effects.export"},json!({"profile":self.value("spectrum.profile"),"path":path,"effects":self.steps}),false,None)?;
                    return Ok(());
                }
                142 | 143 => {
                    for zone in 1..=24 {
                        self.drafts
                            .insert(format!("spectrum.zone.{zone}"), json!(id == 142));
                    }
                }
                _ => return Ok(()),
            }
            self.build();
            Ok(())
        }
    }
    pub(super) fn spectrum_payload(&self) -> Result<Value> {
        if self.data.get("Loaded").and_then(Value::as_bool) != Some(true) {
            bail!(
                "{}",
                tr_static("请先读取此键盘预设，避免覆盖未读取的灯效。")
            );
        }
        if self.data.get("Profile") != Some(&self.value("spectrum.profile")) {
            bail!("{}", tr_static("请先重新读取所选预设。"));
        }
        validate(&self.steps)?;
        Ok(json!({"profile":self.value("spectrum.profile"),"effects":self.steps}))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_effects_belong_only_to_the_reported_preset() {
        let mut model = UiModel::default();
        model.collections.insert("keyboard.effects".into(), vec![]);
        assert!(cached(&model, &json!(2)).is_none());
        model.set_value("keyboard.effects_profile", json!(4));
        model.set_value("keyboard.profile", json!(2));
        assert!(cached(&model, &json!(2)).is_none());
        assert_eq!(cached(&model, &json!(4)), Some(vec![]));
    }
    #[test]
    fn invalid_colors_and_unsupported_machine_effects_reject() {
        assert!(parse_color("#12345Z").is_err());
        assert_eq!(
            parse_color("#102030").unwrap(),
            json!({"R":16,"G":32,"B":48})
        );
        assert!(validate(&[json!({"Type":"AuroraSync","Keys":[],"Colors":[]})]).is_err());
        assert!(
            validate(&[json!({"Type":"Always","Keys":[1,24],"Colors":[{"R":255,"G":0,"B":0}]})])
                .is_ok()
        );
        assert!(validate(&[json!({"Type":"Always","Keys":[25],"Colors":[]})]).is_err());
    }
}
