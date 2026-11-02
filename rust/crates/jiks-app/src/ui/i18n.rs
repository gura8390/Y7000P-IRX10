//! Product-label localization only. Callers must keep user names, device names,
//! paths, IDs and editable contents outside this API. Lookup is exact; there is
//! no substring replacement, network, registry or operating-system locale query.
use std::{borrow::Cow, cell::Cell, collections::BTreeMap, sync::OnceLock};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Language {
    Chinese,
    English,
}
thread_local! {static LANGUAGE:Cell<Language>=const{Cell::new(Language::Chinese)};}
static ENGLISH: OnceLock<BTreeMap<String, String>> = OnceLock::new();
fn dictionary() -> &'static BTreeMap<String, String> {
    ENGLISH.get_or_init(|| {
        serde_json::from_str(include_str!("resources/en.json"))
            .expect("bundled English labels are validated by the resource tests")
    })
}
fn english() -> bool {
    LANGUAGE.with(|v| v.get() == Language::English)
}
/// No IO. Unsupported identifiers intentionally keep the Chinese fallback;
/// the settings UI advertises only zh-Hans and en, not unimplemented languages.
pub(super) fn set_language(identifier: &str) -> bool {
    let next = if ["en", "en-US", "en-GB"]
        .iter()
        .any(|known| known.eq_ignore_ascii_case(identifier))
    {
        Language::English
    } else {
        Language::Chinese
    };
    LANGUAGE.with(|value| {
        let changed = value.get() != next;
        value.set(next);
        changed
    })
}
pub(super) fn language() -> &'static str {
    if english() { "en" } else { "zh-CN" }
}
/// Chinese and unknown labels borrow the input. Known English translations
/// borrow immutable process-lifetime resources. Never changes the input string.
pub(super) fn tr(text: &str) -> Cow<'_, str> {
    if !english() {
        return Cow::Borrowed(text);
    }
    Cow::Borrowed(dictionary().get(text).map(String::as_str).unwrap_or(text))
}
/// Static-returning variant for Page::title and fixed native menu definitions.
pub(super) fn tr_static(text: &'static str) -> &'static str {
    if !english() {
        return text;
    }
    dictionary().get(text).map(String::as_str).unwrap_or(text)
}
/// Only for internally generated product placeholders, never user strings.
/// Allows a still-open editor to retain an opaque value after another action
/// changes the UI language without rebuilding that modal.
pub(super) fn matches_product_label(candidate: &str, chinese: &str) -> bool {
    candidate == chinese
        || dictionary()
            .get(chinese)
            .is_some_and(|english| candidate == english)
}

/// Formats only the product template. User-supplied arguments are copied once,
/// verbatim, and can contain braces without being treated as another template.
pub(super) fn format_product(template: &str, arguments: &[&str]) -> String {
    let template = tr(template);
    let mut parts = template.split("{}");
    let mut output = parts.next().unwrap_or_default().to_owned();
    for (index, part) in parts.enumerate() {
        output.push_str(arguments.get(index).copied().unwrap_or("{}"));
        output.push_str(part);
    }
    output
}

#[cfg(test)]
pub(super) struct TestLanguage(Language);
#[cfg(test)]
impl TestLanguage {
    pub(super) fn new(language: &str) -> Self {
        let old = LANGUAGE.with(Cell::get);
        set_language(language);
        Self(old)
    }
}
#[cfg(test)]
impl Drop for TestLanguage {
    fn drop(&mut self) {
        LANGUAGE.with(|value| value.set(self.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn switching_is_thread_local_idempotent_and_has_a_defined_fallback() {
        let _restore = TestLanguage::new("zh-CN");
        set_language("zh-CN");
        assert_eq!(language(), "zh-CN");
        assert_eq!(tr("设置"), "设置");
        assert!(set_language("en-US"));
        assert_eq!(language(), "en");
        assert!(!set_language("EN"));
        assert_eq!(tr("设置"), "Settings");
        std::thread::spawn(|| {
            assert_eq!(language(), "zh-CN");
            set_language("en-GB");
            assert_eq!(tr_static("电池"), "Battery");
        })
        .join()
        .unwrap();
        assert_eq!(language(), "en");
        assert!(set_language("unsupported"));
        assert_eq!(tr("设置"), "设置");
        for identifier in ["en-AU", "en_US", " en", "en-XX"] {
            set_language(identifier);
            assert_eq!(language(), "zh-CN");
        }
    }
    #[test]
    fn exact_lookup_preserves_data_and_never_performs_substring_replacement() {
        let _restore = TestLanguage::new("zh-CN");
        set_language("en");
        for input in [
            r"C:\用户\设备信息\设置.json",
            "NVIDIA GeForce RTX 5060 Laptop GPU",
            "用户命名：设置",
            "preset-id-123",
            "unknown product label",
            "方正公文楷体",
        ] {
            assert_eq!(tr(input), input);
        }
        assert!(matches!(tr("Settings"), Cow::Borrowed(_)));
        assert_eq!(tr("应用模式"), "Apply mode");
    }
    #[test]
    fn all_nine_navigation_pages_and_eight_original_settings_categories_are_covered() {
        let _restore = TestLanguage::new("zh-CN");
        set_language("en");
        for text in [
            "控制台",
            "键盘背光",
            "电池",
            "自动化",
            "键盘宏",
            "驱动下载",
            "设置",
            "赞助",
            "关于",
            "应用行为",
            "外观",
            "电源",
            "显示",
            "智能按键",
            "软件控制",
            "更新",
            "集成",
        ] {
            assert_ne!(
                tr_static(text),
                text,
                "untranslated navigation label: {text}"
            );
        }
    }
    #[test]
    fn formatting_translates_template_without_interpreting_user_arguments() {
        let _restore = TestLanguage::new("en");
        assert_eq!(
            format_product("{}需要整数。", &["用户 {} 预设"]),
            "用户 {} 预设 needs an integer."
        );
    }
    fn placeholders(text: &str) -> Vec<String> {
        let mut out = vec![];
        let mut chars = text.char_indices().peekable();
        while let Some((start, c)) = chars.next() {
            if c != '{' {
                continue;
            }
            if chars.peek().is_some_and(|(_, c)| *c == '{') {
                chars.next();
                continue;
            }
            for (end, c) in chars.by_ref() {
                if c == '}' {
                    out.push(text[start..=end].to_string());
                    break;
                }
            }
        }
        out.sort();
        out
    }
    #[test]
    fn bundled_resources_preserve_placeholders_and_contain_no_empty_translations() {
        assert!(dictionary().len() >= 800);
        for (source, translation) in dictionary() {
            assert!(!source.is_empty() && !translation.is_empty());
            assert!(!translation.contains('\0'));
            assert_eq!(
                placeholders(source),
                placeholders(translation),
                "format placeholders changed: {source}"
            );
        }
    }
    // Independent source-coverage gate, not a production string replacer.
    // It reads only compiled-in product source; no filesystem or configuration.
    fn literals(source: &str) -> Vec<String> {
        let mut output = vec![];
        let mut chars = source.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '"' {
                continue;
            }
            let mut text = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '"' => break,
                    '\\' => {
                        if let Some(c) = chars.next() {
                            text.push(match c {
                                'n' => '\n',
                                'r' => '\r',
                                't' => '\t',
                                '0' => '\0',
                                other => other,
                            });
                        }
                    }
                    _ => text.push(c),
                }
            }
            if text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
                && !text.starts_with(['{', '['])
                && !text.contains('\0')
            {
                output.push(text);
            }
        }
        output
    }
    #[test]
    fn current_catalog_and_forms_static_labels_are_translated() {
        let mut missing = vec![];
        for (source, name) in [
            (include_str!("catalog.rs"), "catalog"),
            (include_str!("forms.rs"), "forms"),
        ] {
            for label in literals(source.split("#[cfg(test)]").next().unwrap_or(source)) {
                if !dictionary().contains_key(&label) {
                    missing.push(format!("{name}: {label}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "Missing product translations:\n{}",
            missing.join("\n")
        );
    }
}
