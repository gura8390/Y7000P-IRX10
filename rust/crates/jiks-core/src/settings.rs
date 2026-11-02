//! Lossless opaque JSON values and explicit legacy settings views. This module never reads a file.
use crate::thermal::{ThermalControlOptions, ThermalOptions};
use crate::{CoreError, CoreResult};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonNode {
    Null,
    Bool(bool),
    String(String),
    Number(String),
    Array(Vec<JsonNode>),
    Object(Vec<(String, JsonNode)>),
}
impl JsonNode {
    pub fn parse(text: &str) -> CoreResult<Self> {
        if text.len() > 16 * 1024 * 1024 {
            return Err(CoreError::Capacity("configuration exceeds 16 MiB".into()));
        }
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut parser = Parser {
            text,
            offset: 0,
            nodes: 0,
        };
        let value = parser.value(0)?;
        parser.whitespace()?;
        if parser.offset != text.len() {
            return Err(parser.error("additional content"));
        }
        Ok(value)
    }
    pub fn object() -> Self {
        Self::Object(Vec::new())
    }
    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(fields) => fields
                .iter()
                .rev()
                .find(|(name, _)| name == key)
                .map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn get_ci(&self, key: &str) -> Option<&Self> {
        self.get(key).or_else(|| match self {
            Self::Object(fields) => fields
                .iter()
                .rev()
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            _ => None,
        })
    }
    pub fn get_path<'a>(&'a self, path: &[&str]) -> Option<&'a Self> {
        path.iter().try_fold(self, |node, key| node.get(key))
    }
    pub fn get_path_ci<'a>(&'a self, path: &[&str]) -> Option<&'a Self> {
        path.iter().try_fold(self, |node, key| node.get_ci(key))
    }
    pub fn set(&mut self, key: impl Into<String>, value: Self) -> CoreResult<()> {
        let key = key.into();
        let Self::Object(fields) = self else {
            return Err(CoreError::invalid("JSON object required"));
        };
        if let Some((_, old)) = fields.iter_mut().find(|(name, _)| name == &key) {
            *old = value;
        } else {
            fields.push((key, value));
        }
        Ok(())
    }
    /// Use only for schema-known fields. Unknown keys must remain case-distinct.
    pub fn set_known(&mut self, key: &str, value: Self) -> CoreResult<()> {
        let Self::Object(fields) = self else {
            return Err(CoreError::invalid("JSON object required"));
        };
        fields.retain(|(name, _)| !name.eq_ignore_ascii_case(key));
        fields.push((key.into(), value));
        Ok(())
    }
    pub fn set_path(&mut self, path: &[&str], value: Self) -> CoreResult<()> {
        let Some((key, rest)) = path.split_first() else {
            *self = value;
            return Ok(());
        };
        if rest.is_empty() {
            return self.set(*key, value);
        }
        let Self::Object(fields) = self else {
            return Err(CoreError::invalid("JSON object path required"));
        };
        let index = fields
            .iter()
            .position(|(name, _)| name == key)
            .ok_or_else(|| CoreError::invalid(format!("missing object path {key}")))?;
        fields[index].1.set_path(rest, value)
    }
    pub fn as_str(&self) -> Option<&str> {
        if let Self::String(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let Self::Bool(v) = self {
            Some(*v)
        } else {
            None
        }
    }
    pub fn as_array(&self) -> Option<&[Self]> {
        if let Self::Array(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_object(&self) -> Option<&[(String, Self)]> {
        if let Self::Object(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        if let Self::Number(v) = self {
            v.parse().ok()
        } else {
            None
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(v) | Self::String(v) => v.parse().ok(),
            _ => None,
        }
    }
    pub fn number(value: impl ToString) -> CoreResult<Self> {
        let value = value.to_string();
        if !standard_number(&value) {
            return Err(CoreError::invalid("strict JSON number required"));
        }
        Ok(Self::Number(value))
    }
    pub fn to_json(&self) -> String {
        let mut output = String::new();
        self.write(&mut output);
        output
    }
    pub fn validate(&self) -> CoreResult<()> {
        fn check(node: &JsonNode, depth: usize, count: &mut usize) -> CoreResult<()> {
            *count += 1;
            if depth > 64 || *count > 1_000_000 {
                return Err(CoreError::Capacity(
                    "configuration depth/token budget".into(),
                ));
            }
            match node {
                JsonNode::Number(value) if !standard_number(value) => {
                    return Err(CoreError::invalid("non-JSON numeric token"));
                }
                JsonNode::Array(values) => {
                    for value in values {
                        check(value, depth + 1, count)?;
                    }
                }
                JsonNode::Object(fields) => {
                    for (_, value) in fields {
                        check(value, depth + 1, count)?;
                    }
                }
                _ => {}
            }
            Ok(())
        }
        check(self, 0, &mut 0)
    }
    pub fn try_to_json(&self) -> CoreResult<String> {
        self.validate()?;
        let text = self.to_json();
        if text.len() > 16 * 1024 * 1024 {
            Err(CoreError::Capacity("configuration exceeds 16 MiB".into()))
        } else {
            Ok(text)
        }
    }
    fn write(&self, output: &mut String) {
        match self {
            Self::Null => output.push_str("null"),
            Self::Bool(v) => output.push_str(if *v { "true" } else { "false" }),
            Self::String(v) => output
                .push_str(&serde_json::to_string(v).expect("a Rust string is JSON serializable")),
            Self::Number(v) => output.push_str(v),
            Self::Array(values) => {
                output.push('[');
                for (i, value) in values.iter().enumerate() {
                    if i > 0 {
                        output.push(',');
                    }
                    value.write(output);
                }
                output.push(']');
            }
            Self::Object(fields) => {
                output.push('{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        output.push(',');
                    }
                    output.push_str(&serde_json::to_string(key).expect("key serialization"));
                    output.push(':');
                    value.write(output);
                }
                output.push('}');
            }
        }
    }
    pub fn to_value(&self) -> CoreResult<serde_json::Value> {
        serde_json::from_str(&self.to_json()).map_err(|e| CoreError::invalid(e.to_string()))
    }
}
impl Serialize for JsonNode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let raw = serde_json::value::RawValue::from_string(self.to_json())
            .map_err(serde::ser::Error::custom)?;
        raw.serialize(serializer)
    }
}
impl From<bool> for JsonNode {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}
impl From<String> for JsonNode {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}
impl From<&str> for JsonNode {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}

fn standard_number(text: &str) -> bool {
    let b = text.as_bytes();
    let mut p = usize::from(b.first() == Some(&b'-'));
    if b.get(p) == Some(&b'0') {
        p += 1;
    } else {
        let start = p;
        while b.get(p).is_some_and(u8::is_ascii_digit) {
            p += 1;
        }
        if p == start {
            return false;
        }
    }
    if b.get(p) == Some(&b'.') {
        p += 1;
        let start = p;
        while b.get(p).is_some_and(u8::is_ascii_digit) {
            p += 1;
        }
        if p == start {
            return false;
        }
    }
    if matches!(b.get(p), Some(b'e' | b'E')) {
        p += 1;
        if matches!(b.get(p), Some(b'+' | b'-')) {
            p += 1;
        }
        let start = p;
        while b.get(p).is_some_and(u8::is_ascii_digit) {
            p += 1;
        }
        if p == start {
            return false;
        }
    }
    p == b.len()
}
struct Parser<'a> {
    text: &'a str,
    offset: usize,
    nodes: usize,
}
impl Parser<'_> {
    fn error(&self, message: impl Into<String>) -> CoreError {
        CoreError::Json {
            offset: self.offset,
            message: message.into(),
        }
    }
    fn byte(&self) -> Option<u8> {
        self.text.as_bytes().get(self.offset).copied()
    }
    fn whitespace(&mut self) -> CoreResult<()> {
        loop {
            while self.byte().is_some_and(|c| c.is_ascii_whitespace()) {
                self.offset += 1;
            }
            if self.text[self.offset..].starts_with("//") {
                while self.byte().is_some_and(|c| c != b'\n' && c != b'\r') {
                    self.offset += 1;
                }
            } else if self.text[self.offset..].starts_with("/*") {
                self.offset += 2;
                let end = self.text[self.offset..]
                    .find("*/")
                    .ok_or_else(|| self.error("unclosed comment"))?;
                self.offset += end + 2;
            } else {
                return Ok(());
            }
        }
    }
    fn value(&mut self, depth: usize) -> CoreResult<JsonNode> {
        self.whitespace()?;
        self.nodes += 1;
        if depth > 64 || self.nodes > 1_000_000 {
            return Err(CoreError::Capacity(
                "configuration depth/token budget".into(),
            ));
        }
        match self.byte() {
            Some(b'{') => {
                self.offset += 1;
                let mut value = JsonNode::object();
                self.whitespace()?;
                while self.byte() != Some(b'}') {
                    let key = if matches!(self.byte(), Some(b'\'' | b'"')) {
                        self.string()?
                    } else {
                        self.identifier()?
                    };
                    self.whitespace()?;
                    if self.byte() != Some(b':') {
                        return Err(self.error("expected colon"));
                    }
                    self.offset += 1;
                    value.set(key, self.value(depth + 1)?)?;
                    self.whitespace()?;
                    if self.byte() == Some(b'}') {
                        break;
                    }
                    if self.byte() != Some(b',') {
                        return Err(self.error("expected comma or object end"));
                    }
                    self.offset += 1;
                    self.whitespace()?;
                }
                if self.byte() != Some(b'}') {
                    return Err(self.error("unclosed object"));
                }
                self.offset += 1;
                Ok(value)
            }
            Some(b'[') => {
                self.offset += 1;
                let mut values = Vec::new();
                self.whitespace()?;
                while self.byte() != Some(b']') {
                    values.push(self.value(depth + 1)?);
                    self.whitespace()?;
                    if self.byte() == Some(b']') {
                        break;
                    }
                    if self.byte() != Some(b',') {
                        return Err(self.error("expected comma or array end"));
                    }
                    self.offset += 1;
                    self.whitespace()?;
                }
                if self.byte() != Some(b']') {
                    return Err(self.error("unclosed array"));
                }
                self.offset += 1;
                Ok(JsonNode::Array(values))
            }
            Some(b'\'' | b'"') => Ok(JsonNode::String(self.string()?)),
            Some(b't') if self.text[self.offset..].starts_with("true") => {
                self.offset += 4;
                Ok(JsonNode::Bool(true))
            }
            Some(b'f') if self.text[self.offset..].starts_with("false") => {
                self.offset += 5;
                Ok(JsonNode::Bool(false))
            }
            Some(b'n') if self.text[self.offset..].starts_with("null") => {
                self.offset += 4;
                Ok(JsonNode::Null)
            }
            Some(_) => self.numeric(),
            None => Err(self.error("expected value")),
        }
    }
    fn identifier(&mut self) -> CoreResult<String> {
        let start = self.offset;
        while let Some(ch) = self.text[self.offset..].chars().next() {
            if !ch.is_alphanumeric() && !matches!(ch, '_' | '$') {
                break;
            }
            self.offset += ch.len_utf8();
        }
        if self.offset == start {
            return Err(self.error("object member name required"));
        }
        Ok(self.text[start..self.offset].to_owned())
    }
    fn hex4(&mut self) -> CoreResult<u16> {
        let end = self
            .offset
            .checked_add(4)
            .filter(|&n| n <= self.text.len())
            .ok_or_else(|| self.error("incomplete Unicode escape"))?;
        let value = self
            .text
            .get(self.offset..end)
            .and_then(|s| u16::from_str_radix(s, 16).ok())
            .ok_or_else(|| self.error("invalid Unicode escape"))?;
        self.offset = end;
        Ok(value)
    }
    fn string(&mut self) -> CoreResult<String> {
        let quote = self.byte().ok_or_else(|| self.error("string quote"))?;
        self.offset += 1;
        let mut output = String::new();
        loop {
            let byte = self.byte().ok_or_else(|| self.error("unclosed string"))?;
            if byte == quote {
                self.offset += 1;
                return Ok(output);
            }
            if byte < 0x20 {
                return Err(self.error("unescaped control character"));
            }
            if byte != b'\\' {
                let ch = self.text[self.offset..].chars().next().unwrap();
                output.push(ch);
                self.offset += ch.len_utf8();
                continue;
            }
            self.offset += 1;
            let escaped = self
                .byte()
                .ok_or_else(|| self.error("incomplete string escape"))?;
            self.offset += 1;
            match escaped {
                b'\'' => output.push('\''),
                b'"' => output.push('"'),
                b'\\' => output.push('\\'),
                b'/' => output.push('/'),
                b'b' => output.push('\u{8}'),
                b'f' => output.push('\u{c}'),
                b'n' => output.push('\n'),
                b'r' => output.push('\r'),
                b't' => output.push('\t'),
                b'u' => {
                    let high = self.hex4()?;
                    let code = if (0xD800..=0xDBFF).contains(&high) {
                        if !self.text[self.offset..].starts_with("\\u") {
                            return Err(self.error("unpaired high surrogate"));
                        }
                        self.offset += 2;
                        let low = self.hex4()?;
                        if !(0xDC00..=0xDFFF).contains(&low) {
                            return Err(self.error("unpaired high surrogate"));
                        }
                        0x10000 + ((u32::from(high) - 0xD800) << 10) + u32::from(low) - 0xDC00
                    } else {
                        u32::from(high)
                    };
                    output.push(
                        char::from_u32(code)
                            .ok_or_else(|| self.error("unpaired Unicode surrogate"))?,
                    );
                }
                _ => return Err(self.error("unsupported string escape")),
            }
        }
    }
    fn numeric(&mut self) -> CoreResult<JsonNode> {
        let start = self.offset;
        while self
            .byte()
            .is_some_and(|c| !c.is_ascii_whitespace() && !matches!(c, b',' | b']' | b'}' | b'/'))
        {
            self.offset += 1;
        }
        let token = &self.text[start..self.offset];
        if standard_number(token) {
            return Ok(JsonNode::Number(token.into()));
        }
        if matches!(token, "NaN" | "Infinity" | "-Infinity") {
            return Ok(JsonNode::String(token.into()));
        }
        if let Some(hex) = token
            .strip_prefix("0x")
            .or_else(|| token.strip_prefix("0X"))
        {
            let value = u64::from_str_radix(hex, 16)
                .map_err(|_| self.error("invalid hexadecimal integer"))?;
            return Ok(JsonNode::Number(value.to_string()));
        }
        if token.len() > 1 && token.starts_with('0') && token.bytes().all(|c| c.is_ascii_digit()) {
            let value = u64::from_str_radix(&token[1..], 8)
                .map_err(|_| self.error("invalid octal integer"))?;
            return Ok(JsonNode::Number(value.to_string()));
        }
        let normalized = if token.starts_with('.') {
            Some(format!("0{token}"))
        } else if token.starts_with("-.") {
            Some(token.replacen("-.", "-0.", 1))
        } else {
            None
        };
        if let Some(value) = normalized.filter(|v| standard_number(v)) {
            return Ok(JsonNode::Number(value));
        }
        Err(self.error("invalid JSON value or unsupported numeric spelling"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schema {
    Replace,
    Object(BTreeMap<String, Schema>),
}
impl Schema {
    pub fn object(names: &[&str]) -> Self {
        Self::Object(
            names
                .iter()
                .map(|&name| (name.to_owned(), Self::Replace))
                .collect(),
        )
    }
    pub fn with(mut self, key: &str, schema: Self) -> Self {
        if let Self::Object(fields) = &mut self {
            fields.insert(key.into(), schema);
        }
        self
    }
}
/// Known fields come from `current`; only schema-unknown fields are taken from the old snapshot.
pub fn preserve_unknown(original: &JsonNode, current: &JsonNode, schema: &Schema) -> JsonNode {
    let (JsonNode::Object(old), JsonNode::Object(_), Schema::Object(fields)) =
        (original, current, schema)
    else {
        return current.clone();
    };
    let mut result = current.clone();
    for (name, old_value) in old {
        let member = fields.get_key_value(name).or_else(|| {
            fields
                .iter()
                .find(|(known, _)| known.eq_ignore_ascii_case(name))
        });
        if let Some((canonical, nested_schema)) = member {
            if let Some(new_value) = result.get(canonical).cloned() {
                let merged = preserve_unknown(old_value, &new_value, nested_schema);
                let _ = result.set(canonical.clone(), merged);
            }
        } else if result.get(name).is_none() {
            let _ = result.set(name.clone(), old_value.clone());
        }
    }
    result
}
pub fn application_schema() -> Schema {
    Schema::object(&[
        "Theme",
        "AccentColor",
        "AccentColorSource",
        "PowerModeMappingMode",
        "PowerPlans",
        "PowerModes",
        "Overrides",
        "ITSPowerPlans",
        "ITSPowerModes",
        "ITSOverrides",
        "MinimizeToTray",
        "MinimizeOnClose",
        "WindowSize",
        "TemperatureUnit",
        "ExcludedRefreshRates",
        "WarrantyInfo",
        "SynchronizeBrightnessToAllPowerPlans",
        "SmartFnLockFlags",
        "ResetBatteryOnSinceTimerOnReboot",
        "UseNewSensorDashboard",
        "EnableHardwareSensors",
        "EnableThermalShadow",
        "EnableCpuThermalControl",
        "CpuThermalControl",
        "CpuThermalSamplingMilliseconds",
        "CpuThermalFirmwareLimitC",
        "LockWindowSize",
        "AlwaysOnTop",
        "CompactMode",
        "WindowPosition",
        "RememberWindowPosition",
        "EnableLogging",
        "BackGroundImageFilePath",
        "Opacity",
        "BackgroundImageBlur",
        "BackgroundImageStretch",
        "ExcludedProcesses",
        "IncludedProcesses",
        "GameDetection",
        "DynamicLightingWarningDontShowAgain",
        "PowerButtonLedBrightness",
        "PowerButtonLedLevelOffset",
        "CustomModeWarningDontShowAgain",
        "EnableHardwareAcceleration",
        "GPUMonitoringInterval",
        "GPUMonitoringStartupDelay",
        "GPUKillProcessDelay",
        "BackdropType",
        "KeepDgpuAwake",
        "LockedPStateId",
    ])
    .with(
        "GameDetection",
        Schema::object(&[
            "UseDiscreteGPU",
            "UseGameConfigStore",
            "UseEffectiveGameMode",
        ]),
    )
    .with("WindowSize", Schema::object(&["Width", "Height"]))
    .with("WindowPosition", Schema::object(&["Left", "Top"]))
    .with("AccentColor", Schema::object(&["R", "G", "B"]))
    .with("WarrantyInfo", Schema::object(&["Start", "End", "Link"]))
    .with(
        "CpuThermalControl",
        Schema::object(&[
            "Policy",
            "CommandTimeout",
            "CommandLifetimeMilliseconds",
            "ReadbackIntervalMilliseconds",
            "PredictionMilliseconds",
            "MaximumPredictionC",
        ])
        .with(
            "Policy",
            Schema::object(&[
                "TargetC",
                "WarmC",
                "RecoverC",
                "FreshnessMilliseconds",
                "RecoveryMilliseconds",
                "AdjustmentMilliseconds",
                "MinimumFraction",
                "Reduction",
                "WarmReduction",
                "RecoveryStep",
            ]),
        ),
    )
}

#[derive(Debug, Clone)]
pub struct SettingsDocument {
    root: JsonNode,
}
impl SettingsDocument {
    pub fn parse(text: &str) -> CoreResult<Self> {
        let root = JsonNode::parse(text)?;
        if root.as_object().is_none() {
            return Err(CoreError::invalid("settings must be an object"));
        }
        Ok(Self { root })
    }
    pub fn from_root(root: JsonNode) -> CoreResult<Self> {
        if root.as_object().is_none() {
            return Err(CoreError::invalid("settings must be an object"));
        }
        root.validate()?;
        Ok(Self { root })
    }
    pub fn root(&self) -> &JsonNode {
        &self.root
    }
    pub fn set_path(&mut self, path: &[&str], value: JsonNode) -> CoreResult<()> {
        value.validate()?;
        self.root.set_path(path, value)
    }
    pub fn merged_application(&self, typed_current: &JsonNode) -> CoreResult<Self> {
        Self::from_root(preserve_unknown(
            &self.root,
            typed_current,
            &application_schema(),
        ))
    }
    pub fn to_json(&self) -> String {
        self.root.to_json()
    }
    /// Persistence callers should use this fallible API before touching a pending file.
    pub fn serialize(&self) -> CoreResult<String> {
        self.root.try_to_json()
    }
    pub fn normalized_application(&self) -> CoreResult<Self> {
        self.validate_application()?;
        let mut root = self.root.clone();
        for (name, empty) in [
            ("IncludedProcesses", JsonNode::Array(Vec::new())),
            ("ExcludedProcesses", JsonNode::Array(Vec::new())),
            ("ExcludedRefreshRates", JsonNode::Array(Vec::new())),
            ("PowerPlans", JsonNode::object()),
        ] {
            if root.get_ci(name).is_none_or(|v| *v == JsonNode::Null) {
                root.set_known(name, empty)?;
            }
        }
        let mut detection = match root.get_ci("GameDetection") {
            Some(value) if *value != JsonNode::Null => value.clone(),
            _ => JsonNode::object(),
        };
        for name in [
            "UseDiscreteGPU",
            "UseGameConfigStore",
            "UseEffectiveGameMode",
        ] {
            if detection.get_ci(name).is_none() {
                detection.set_known(name, true.into())?;
            }
        }
        root.set_known("GameDetection", detection)?;
        Self::from_root(root)
    }
    pub fn validate_application(&self) -> CoreResult<()> {
        self.root.validate()?;
        for name in [
            "MinimizeToTray",
            "MinimizeOnClose",
            "SynchronizeBrightnessToAllPowerPlans",
            "ResetBatteryOnSinceTimerOnReboot",
            "UseNewSensorDashboard",
            "EnableHardwareSensors",
            "EnableThermalShadow",
            "EnableCpuThermalControl",
            "LockWindowSize",
            "AlwaysOnTop",
            "CompactMode",
            "RememberWindowPosition",
            "EnableLogging",
            "DynamicLightingWarningDontShowAgain",
            "CustomModeWarningDontShowAgain",
            "EnableHardwareAcceleration",
            "KeepDgpuAwake",
        ] {
            if let Some(value) = self.root.get_ci(name) {
                if value.as_bool().is_none() {
                    return Err(CoreError::invalid(format!("{name} non-null boolean")));
                }
            }
        }
        for name in [
            "CpuThermalSamplingMilliseconds",
            "CpuThermalFirmwareLimitC",
            "BackgroundImageBlur",
            "PowerButtonLedBrightness",
            "GPUMonitoringInterval",
            "GPUMonitoringStartupDelay",
            "GPUKillProcessDelay",
            "LockedPStateId",
        ] {
            if let Some(value) = self.root.get_ci(name) {
                if value.as_i64().and_then(|v| i32::try_from(v).ok()).is_none() {
                    return Err(CoreError::invalid(format!("{name} Int32")));
                }
            }
        }
        for (name, allowed) in [
            ("TemperatureUnit", &["C", "F"][..]),
            ("Theme", &["System", "Light", "Dark"][..]),
            ("AccentColorSource", &["System", "Custom"][..]),
            (
                "PowerModeMappingMode",
                &["Disabled", "WindowsPowerMode", "WindowsPowerPlan"][..],
            ),
            ("BackgroundImageStretch", &["Fill", "Fit", "Crop"][..]),
            ("BackdropType", &["None", "Mica", "Acrylic", "Tabbed"][..]),
        ] {
            if let Some(value) = self.root.get_ci(name) {
                validate_enum(value, allowed)?;
            }
        }
        for name in ["IncludedProcesses", "ExcludedProcesses"] {
            if let Some(value) = self.root.get_ci(name).filter(|v| **v != JsonNode::Null) {
                let items = value
                    .as_array()
                    .ok_or_else(|| CoreError::invalid(format!("{name} array")))?;
                if items
                    .iter()
                    .any(|v| !matches!(v, JsonNode::String(_) | JsonNode::Null))
                {
                    return Err(CoreError::invalid(format!("{name} string array")));
                }
            }
        }
        if let Some(value) = self
            .root
            .get_ci("ExcludedRefreshRates")
            .filter(|v| **v != JsonNode::Null)
        {
            if value.as_array().is_none() {
                return Err(CoreError::invalid("ExcludedRefreshRates array"));
            }
        }
        if let Some(value) = self
            .root
            .get_ci("PowerPlans")
            .filter(|v| **v != JsonNode::Null)
        {
            for (key, guid) in value
                .as_object()
                .ok_or_else(|| CoreError::invalid("PowerPlans object"))?
            {
                validate_enum(
                    &JsonNode::String(key.clone()),
                    &["Quiet", "Balance", "Performance", "Extreme", "GodMode"],
                )?;
                crate::automation::normalize_guid(
                    guid.as_str()
                        .ok_or_else(|| CoreError::invalid("PowerPlans Guid string"))?,
                )?;
            }
        }
        if let Some(value) = self
            .root
            .get_ci("GameDetection")
            .filter(|v| **v != JsonNode::Null)
        {
            if value.as_object().is_none() {
                return Err(CoreError::invalid("GameDetection object"));
            }
            for name in [
                "UseDiscreteGPU",
                "UseGameConfigStore",
                "UseEffectiveGameMode",
            ] {
                if let Some(value) = value.get_ci(name) {
                    if value.as_bool().is_none() {
                        return Err(CoreError::invalid(format!("GameDetection.{name} boolean")));
                    }
                }
            }
        }
        ApplicationThermalSettings::from_document(self)?;
        Ok(())
    }
}
fn validate_enum(node: &JsonNode, allowed: &[&str]) -> CoreResult<()> {
    match node {
        JsonNode::Number(_) if node.as_i64().and_then(|n| i32::try_from(n).ok()).is_some() => {
            Ok(())
        }
        JsonNode::String(name)
            if allowed.iter().any(|n| n.eq_ignore_ascii_case(name))
                || name.parse::<i32>().is_ok() =>
        {
            Ok(())
        }
        _ => Err(CoreError::invalid(
            "unknown enum string or invalid enum representation",
        )),
    }
}

/// Every legacy store filename is explicit. Unknown files are left to the caller, never deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsKind {
    Application,
    GodMode,
    BalanceMode,
    GpuOverclock,
    AmdOverclock,
    ItsMode,
    Integrations,
    Notification,
    Osd,
    SpecialKey,
    RgbKeyboard,
    SpectrumKeyboard,
    LampArray,
    PackageDownloader,
    SunriseSunset,
    Update,
    Automation,
    Macro,
    HardwareSensors,
    SensorDashboard,
    Dashboard,
}
impl SettingsKind {
    pub const ALL: &'static [Self] = &[
        Self::Application,
        Self::GodMode,
        Self::BalanceMode,
        Self::GpuOverclock,
        Self::AmdOverclock,
        Self::ItsMode,
        Self::Integrations,
        Self::Notification,
        Self::Osd,
        Self::SpecialKey,
        Self::RgbKeyboard,
        Self::SpectrumKeyboard,
        Self::LampArray,
        Self::PackageDownloader,
        Self::SunriseSunset,
        Self::Update,
        Self::Automation,
        Self::Macro,
        Self::HardwareSensors,
        Self::SensorDashboard,
        Self::Dashboard,
    ];
    pub const fn filename(self) -> &'static str {
        match self {
            Self::Application => "settings.json",
            Self::GodMode => "godmode.json",
            Self::BalanceMode => "balancemode.json",
            Self::GpuOverclock => "gpu_oc.json",
            Self::AmdOverclock => "amd_oc.json",
            Self::ItsMode => "itsmode_settings.json",
            Self::Integrations => "integrations.json",
            Self::Notification => "notification_settings.json",
            Self::Osd => "osd.json",
            Self::SpecialKey => "special_key.json",
            Self::RgbKeyboard => "rgb_keyboard.json",
            Self::SpectrumKeyboard => "spectrum_keyboard.json",
            Self::LampArray => "lamp_array.json",
            Self::PackageDownloader => "package_downloader.json",
            Self::SunriseSunset => "sunrise_sunset.json",
            Self::Update => "update_settings.json",
            Self::Automation => "automation.json",
            Self::Macro => "macro.json",
            Self::HardwareSensors => "hardware_sensors.json",
            Self::SensorDashboard => "sensor_dashboard.json",
            Self::Dashboard => "dashboard.json",
        }
    }
    pub fn from_filename(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.filename().eq_ignore_ascii_case(name))
    }
    pub fn parse(self, text: &str) -> CoreResult<SettingsDocument> {
        let document = SettingsDocument::parse(text)?;
        if self == Self::Application {
            document.normalized_application()
        } else {
            Ok(document)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroIdentifier {
    pub source: String,
    pub key: u64,
}
impl MacroIdentifier {
    pub fn parse(value: &str) -> CoreResult<Self> {
        let (source, key) = value
            .split_once(',')
            .ok_or_else(|| CoreError::invalid("macro identifier Source,ulong"))?;
        if !["Unknown", "Keyboard", "Mouse"].contains(&source) && source.parse::<i32>().is_err() {
            return Err(CoreError::invalid("MacroSource enum"));
        }
        Ok(Self {
            source: source.into(),
            key: key
                .trim()
                .parse()
                .map_err(|_| CoreError::invalid("macro ulong key"))?,
        })
    }
    pub fn legacy_key(&self) -> String {
        format!("{},{}", self.source, self.key)
    }
}

pub fn parse_timespan_ticks(text: &str) -> CoreResult<i64> {
    let negative = text.starts_with('-');
    let text = text.strip_prefix('-').unwrap_or(text);
    let parts: Vec<_> = text.split(':').collect();
    if parts.len() != 3 {
        return Err(CoreError::invalid(
            "TimeSpan needs [days.]hours:minutes:seconds",
        ));
    }
    let (days, hours) = if let Some((d, h)) = parts[0].split_once('.') {
        (d.parse::<i128>().ok(), h.parse::<i128>().ok())
    } else {
        (Some(0), parts[0].parse::<i128>().ok())
    };
    let minutes = parts[1].parse::<i128>().ok();
    let (seconds, fraction) = parts[2].split_once('.').unwrap_or((parts[2], ""));
    let seconds = seconds.parse::<i128>().ok();
    let (Some(days), Some(hours), Some(minutes), Some(seconds)) = (days, hours, minutes, seconds)
    else {
        return Err(CoreError::invalid("TimeSpan components"));
    };
    if days < 0
        || !(0..24).contains(&hours)
        || !(0..60).contains(&minutes)
        || !(0..60).contains(&seconds)
        || fraction.len() > 7
        || !fraction.bytes().all(|c| c.is_ascii_digit())
    {
        return Err(CoreError::invalid("TimeSpan range"));
    }
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<i128>()
            .map_err(|_| CoreError::invalid("TimeSpan fraction"))?
            * 10_i128.pow(7 - fraction.len() as u32)
    };
    let total = days
        .checked_mul(24)
        .and_then(|v| v.checked_add(hours))
        .and_then(|v| v.checked_mul(60))
        .and_then(|v| v.checked_add(minutes))
        .and_then(|v| v.checked_mul(60))
        .and_then(|v| v.checked_add(seconds))
        .and_then(|v| v.checked_mul(10_000_000))
        .and_then(|v| v.checked_add(fraction))
        .ok_or_else(|| CoreError::invalid("TimeSpan overflow"))?;
    i64::try_from(if negative { -total } else { total })
        .map_err(|_| CoreError::invalid("TimeSpan overflow"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationThermalSettings {
    pub enabled: bool,
    pub shadow_enabled: bool,
    pub hardware_sensors_enabled: bool,
    pub sampling_ms: u64,
    pub firmware_limit_c: i32,
    pub options: ThermalControlOptions,
}
fn boolean(root: &JsonNode, name: &str, default: bool) -> CoreResult<bool> {
    root.get_ci(name).map_or(Ok(default), |n| {
        n.as_bool()
            .ok_or_else(|| CoreError::invalid(format!("{name} must be boolean")))
    })
}
fn integer(root: &JsonNode, name: &str, default: i64) -> CoreResult<i64> {
    root.get_ci(name).map_or(Ok(default), |n| {
        n.as_i64()
            .ok_or_else(|| CoreError::invalid(format!("{name} must be an integer")))
    })
}
fn real(root: &JsonNode, name: &str, default: f64) -> CoreResult<f64> {
    root.get_ci(name).map_or(Ok(default), |n| {
        n.as_f64()
            .ok_or_else(|| CoreError::invalid(format!("{name} must be numeric")))
    })
}
impl ApplicationThermalSettings {
    pub fn from_document(document: &SettingsDocument) -> CoreResult<Self> {
        let root = document.root();
        let mut options = ThermalControlOptions::default();
        if let Some(control) = root
            .get_ci("CpuThermalControl")
            .filter(|n| **n != JsonNode::Null)
        {
            if control.as_object().is_none() {
                return Err(CoreError::invalid("CpuThermalControl object"));
            }
            if let Some(policy) = control.get_ci("Policy") {
                if policy.as_object().is_none() {
                    return Err(CoreError::invalid("thermal Policy object"));
                }
                let p = &options.policy;
                options.policy = ThermalOptions {
                    target_c: real(policy, "TargetC", p.target_c)?,
                    warm_c: real(policy, "WarmC", p.warm_c)?,
                    recover_c: real(policy, "RecoverC", p.recover_c)?,
                    freshness_ms: positive_or_zero(integer(
                        policy,
                        "FreshnessMilliseconds",
                        p.freshness_ms as i64,
                    )?)?,
                    recovery_ms: positive_or_zero(integer(
                        policy,
                        "RecoveryMilliseconds",
                        p.recovery_ms as i64,
                    )?)?,
                    adjustment_ms: positive_or_zero(integer(
                        policy,
                        "AdjustmentMilliseconds",
                        p.adjustment_ms as i64,
                    )?)?,
                    minimum_fraction: real(policy, "MinimumFraction", p.minimum_fraction)?,
                    reduction: real(policy, "Reduction", p.reduction)?,
                    warm_reduction: real(policy, "WarmReduction", p.warm_reduction)?,
                    recovery_step: real(policy, "RecoveryStep", p.recovery_step)?,
                };
            }
            if let Some(timeout) = control.get_ci("CommandTimeout") {
                let ticks = parse_timespan_ticks(
                    timeout
                        .as_str()
                        .ok_or_else(|| CoreError::invalid("CommandTimeout TimeSpan string"))?,
                )?;
                if ticks <= 0 {
                    return Err(CoreError::invalid("CommandTimeout positive"));
                }
                options.command_timeout_ms = u64::try_from((i128::from(ticks) + 9999) / 10000)
                    .map_err(|_| CoreError::invalid("timeout overflow"))?;
            }
            options.command_lifetime_ms = positive_or_zero(integer(
                control,
                "CommandLifetimeMilliseconds",
                options.command_lifetime_ms as i64,
            )?)?;
            options.readback_interval_ms = positive_or_zero(integer(
                control,
                "ReadbackIntervalMilliseconds",
                options.readback_interval_ms as i64,
            )?)?;
            options.prediction_ms = positive_or_zero(integer(
                control,
                "PredictionMilliseconds",
                options.prediction_ms as i64,
            )?)?;
            options.maximum_prediction_c =
                real(control, "MaximumPredictionC", options.maximum_prediction_c)?;
        }
        let sampling = integer(root, "CpuThermalSamplingMilliseconds", 250)?;
        let firmware = integer(root, "CpuThermalFirmwareLimitC", 88)?;
        if i32::try_from(sampling).is_err() {
            return Err(CoreError::invalid("sampling Int32 overflow"));
        }
        Ok(Self {
            enabled: boolean(root, "EnableCpuThermalControl", false)?,
            shadow_enabled: boolean(root, "EnableThermalShadow", false)?,
            hardware_sensors_enabled: boolean(root, "EnableHardwareSensors", false)?,
            sampling_ms: positive_or_zero(sampling)?,
            firmware_limit_c: i32::try_from(firmware)
                .map_err(|_| CoreError::invalid("firmware limit Int32 overflow"))?,
            options,
        })
    }
    pub fn validate_control(&self) -> CoreResult<()> {
        self.options
            .validate_machine(self.sampling_ms, self.firmware_limit_c)
    }
}
fn positive_or_zero(value: i64) -> CoreResult<u64> {
    u64::try_from(value).map_err(|_| CoreError::invalid("negative duration/interval"))
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct ArgumentFlags {
    entries: Vec<String>,
}
impl std::fmt::Debug for ArgumentFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArgumentFlags")
            .field("entry_count", &self.entries.len())
            .field("values", &"[redacted]")
            .finish()
    }
}
impl ArgumentFlags {
    pub fn parse(startup: &[String], external_lines: &[String]) -> Self {
        Self {
            entries: startup.iter().chain(external_lines).cloned().collect(),
        }
    }
    pub fn boolean(&self, key: &str) -> bool {
        self.entries.iter().any(|v| v == key)
    }
    pub fn string(&self, key: &str) -> Option<&str> {
        let prefix = format!("{key}=");
        self.entries.iter().find_map(|v| v.strip_prefix(&prefix))
    }
    pub fn entries(&self) -> &[String] {
        &self.entries
    }
    /// Unknown entries are retained. Secrets must never be printed via this structure.
    pub fn set_string(&mut self, key: &str, value: Option<&str>) {
        let prefix = format!("{key}=");
        self.entries.retain(|v| !v.starts_with(&prefix));
        if let Some(value) = value {
            self.entries.push(format!("{prefix}{value}"));
        }
    }
}
