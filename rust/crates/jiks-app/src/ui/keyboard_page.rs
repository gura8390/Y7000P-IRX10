use super::{Field, FieldKind, Section, UiChoice, UiModel, doc, execute, field, tr_static};

const WHITE_LEVEL: &str = "keyboard.white_level";
const WHITE_SWITCH: &str = "keyboard.white_single";
const PROFILE: &str = "keyboard.profile";
const BRIGHTNESS: &str = "keyboard.brightness";

pub(super) fn white_field(model: &UiModel, label: &'static str) -> Option<Field> {
    if let Some(range) = model.ranges.get(WHITE_LEVEL)
        && range.minimum == 0
        && (1..=3).contains(&range.maximum)
        && range.step == 1
        && model
            .value(WHITE_LEVEL)
            .as_i64()
            .is_some_and(|level| (0..=i64::from(range.maximum)).contains(&level))
    {
        let choices = &["熄灭", "低亮度", "高亮度", "自动"][..=range.maximum as usize];
        return Some(field(
            WHITE_LEVEL,
            label,
            FieldKind::Choice(
                choices
                    .iter()
                    .enumerate()
                    .map(|(level, label)| UiChoice::new(tr_static(label), level))
                    .collect(),
            ),
            "keyboard.white",
        ));
    }
    model.value(WHITE_SWITCH).as_bool().map(|_| {
        field(
            WHITE_SWITCH,
            label,
            FieldKind::Toggle,
            "keyboard.white_single",
        )
    })
}

pub(super) fn sections(model: &UiModel) -> Vec<Section> {
    let ready = model.value("backend.ready").as_bool() == Some(true);
    if model.value("backend.connection_failed").as_bool() == Some(true)
        || !model.value("backend.terminal_error").is_null()
    {
        return vec![Section {
            title: tr_static("键盘背光连接中断").into(),
            hint: tr_static("当前无法读取背光。请查看窗口下方的连接状态。"),
            fields: vec![],
            actions: vec![],
        }];
    }
    let interfaces = model.collections.get("keyboard.interfaces").map(Vec::len);
    if interfaces == Some(1) && model.can("keyboard.apply") {
        return spectrum_sections(model);
    }
    if let Some(mut backlight) = white_field(model, "亮度级别") {
        if backlight.key == WHITE_SWITCH {
            backlight.label = tr_static("背光开关");
        }
        let writable = model.can(backlight.capability) && model.can("keys.apply");
        return vec![Section {
            title: tr_static("白光背光").into(),
            hint: if writable { "" } else { tr_static("当前只能查看背光状态。") },
            actions: if writable {
                vec![execute("应用背光", "keys.apply", &[backlight.key])]
            } else {
                vec![]
            },
            fields: vec![backlight],
        }];
    }
    let (title, hint) = match (ready, interfaces) {
        (false, _) => ("正在读取键盘背光", "请稍候，正在读取本机的背光状态。"),
        (true, Some(0)) => (
            "未找到可用的键盘背光",
            "未读到白光亮度，也未找到彩色背光接口。可以重新读取状态。",
        ),
        (true, Some(1)) => (
            "彩色背光当前不可调节",
            "已找到彩色背光接口。当前设备状态或运行模式限制了灯效控制。",
        ),
        (true, Some(_)) => (
            "未能确定彩色背光接口",
            "检测到多个接口。需要确定唯一接口后才能配置灯效。",
        ),
        (true, None) => (
            "尚未读取到背光状态",
            "可以重新读取本机状态，或检测是否有彩色背光接口。",
        ),
    };
    let mut actions = Vec::new();
    if ready && model.can("refresh") {
        actions.push(execute("重新读取", "refresh", &[]));
    }
    if ready && model.can("keyboard.candidates") {
        actions.push(execute("检测彩色背光", "keyboard.candidates", &[]));
    }
    vec![Section {
        title: tr_static(title).into(),
        hint: tr_static(hint),
        fields: vec![],
        actions,
    }]
}

fn spectrum_sections(model: &UiModel) -> Vec<Section> {
    let mut sections = vec![Section {
        title: tr_static("彩色背光").into(),
        hint: tr_static("选择灯光预设和亮度，再应用到键盘。"),
        fields: vec![
            field(PROFILE, "预设", FieldKind::Choice(vec![]), "keyboard.apply"),
            field(
                BRIGHTNESS,
                "亮度",
                FieldKind::Choice(
                    (0..=9)
                        .map(|level| UiChoice::new(level.to_string(), level))
                        .collect(),
                ),
                "keyboard.apply",
            ),
        ],
        actions: vec![execute(
            "应用背光",
            "keyboard.apply",
            &[PROFILE, BRIGHTNESS],
        )],
    }];
    if model.can("keyboard.effects.refresh") {
        sections.push(Section {
            title: tr_static("灯效与分区").into(),
            hint: tr_static("读取一个预设后，可以调整其中的灯效、颜色与分区。"),
            fields: vec![],
            actions: vec![doc("编辑灯效…", "spectrum_keyboard")],
        });
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_model::{UiItem, UiRange};
    use serde_json::json;

    fn white_model(maximum: i32) -> UiModel {
        let mut model = UiModel::default();
        model.set_value("backend.ready", json!(true));
        model.set_value(WHITE_LEVEL, json!(maximum));
        model.ranges.insert(
            WHITE_LEVEL.into(),
            UiRange {
                minimum: 0,
                maximum,
                step: 1,
                unit: String::new(),
            },
        );
        model
    }

    #[test]
    fn white_backlight_uses_reported_levels_and_only_writes_its_own_field() {
        for maximum in 1..=3 {
            let mut model = white_model(maximum);
            let readonly = sections(&model);
            let FieldKind::Choice(choices) = &readonly[0].fields[0].kind else {
                panic!("Brightness should have a selector");
            };
            assert_eq!(
                choices
                    .iter()
                    .map(|choice| choice.value.clone())
                    .collect::<Vec<_>>(),
                (0..=maximum).map(|level| json!(level)).collect::<Vec<_>>(),
            );
            assert_eq!(readonly[0].fields[0].key, WHITE_LEVEL);
            if maximum == 3 {
                assert_eq!(choices[3].label, tr_static("自动"));
            }
            assert!(readonly[0].actions.is_empty());

            model.set_capability("keyboard.white", true, "");
            model.set_capability("keys.apply", true, "");
            let writable = sections(&model);
            assert!(matches!(
                &writable[0].actions[0].kind,
                super::super::ActionKind::Execute { command: "keys.apply", fields, .. }
                    if fields.as_slice() == [WHITE_LEVEL]
            ));
        }
    }

    #[test]
    fn loading_disconnected_and_missing_backlight_do_not_create_empty_inputs() {
        let mut model = UiModel::default();
        model.set_capability("refresh", true, "");
        model.set_capability("keyboard.candidates", true, "");
        let loading = sections(&model);
        assert!(loading[0].fields.is_empty());
        assert!(loading[0].actions.is_empty());

        model.set_value("backend.ready", json!(true));
        let missing = sections(&model);
        assert!(missing[0].fields.is_empty());
        assert_eq!(missing[0].actions.len(), 2);

        model.set_value("backend.connection_failed", json!(true));
        let disconnected = sections(&model);
        assert!(disconnected[0].fields.is_empty());
        assert!(disconnected[0].actions.is_empty());
        assert_ne!(disconnected[0].title, loading[0].title);
    }

    #[test]
    fn spectrum_controls_require_one_interface_and_available_commands() {
        let mut model = UiModel::default();
        model.set_value("backend.ready", json!(true));
        model.set_capability("keyboard.apply", true, "");
        model.set_capability("keyboard.effects.refresh", true, "");
        model.collections.insert(
            "keyboard.interfaces".into(),
            vec![UiItem::default(), UiItem::default()],
        );
        assert!(sections(&model)[0].fields.is_empty());
        model
            .collections
            .get_mut("keyboard.interfaces")
            .unwrap()
            .pop();
        let controls = sections(&model);
        assert_eq!(controls[0].fields.len(), 2);
        assert_eq!(controls[0].fields[0].key, PROFILE);
        assert_eq!(controls[0].fields[1].key, BRIGHTNESS);
        assert_eq!(controls[1].actions.len(), 1);

        model.set_capability("keyboard.apply", false, "read-only");
        assert!(sections(&model)[0].fields.is_empty());
    }
}
