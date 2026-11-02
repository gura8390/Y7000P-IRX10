use crate::settings::JsonNode;
use crate::{CoreError, CoreResult};

macro_rules! legacy_kinds {
    ($name:ident, $prefix:literal, [$($variant:ident => $class:literal),+ $(,)?]) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($variant,)+ Unknown }
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];
            pub fn class_name(self) -> Option<&'static str> { match self { $(Self::$variant => Some($class),)+ Self::Unknown => None } }
            pub fn legacy_type_name(self) -> Option<String> { self.class_name().map(|name| format!("{}{}, LenovoLegionToolkit.Lib.Automation", $prefix, name)) }
            pub fn from_legacy_name(value: &str) -> Self {
                let mut parts = value.split(',');
                let full = parts.next().unwrap_or("").trim();
                if let Some(assembly) = parts.next() {
                    if !assembly.trim().eq_ignore_ascii_case("LenovoLegionToolkit.Lib.Automation") { return Self::Unknown; }
                }
                let Some(name) = full.strip_prefix($prefix) else { return Self::Unknown; };
                match name { $($class => Self::$variant,)+ _ => Self::Unknown }
            }
        }
    };
}
legacy_kinds!(StepKind, "LenovoLegionToolkit.Lib.Automation.Steps.", [
    AlwaysOnUsb => "AlwaysOnUsbAutomationStep", Delay => "DelayAutomationStep", BatteryNightCharge => "BatteryNightChargeAutomationStep",
    Battery => "BatteryAutomationStep", AutoColorManagement => "AutoColorManagementAutomationStep", CloseApp => "CloseAppAutomationStep",
    Hdr => "HDRAutomationStep", HardwareSensors => "HardwareSensorsAutomationStep", FnLock => "FnLockAutomationStep",
    FlipToStart => "FlipToStartAutomationStep", DpiScale => "DpiScaleAutomationStep", DisplayBrightness => "DisplayBrightnessAutomationStep",
    ItsMode => "ITSModeAutomationStep", InstantBoot => "InstantBootAutomationStep", HybridMode => "HybridModeAutomationStep",
    Microphone => "MicrophoneAutomationStep", OneLevelWhiteKeyboardBacklight => "OneLevelWhiteKeyboardBacklightAutomationStep",
    WinKey => "WinKeyAutomationStep", WhiteKeyboardBacklight => "WhiteKeyboardBacklightAutomationStep", TurnOffMonitors => "TurnOffMonitorsAutomationStep",
    TouchpadLock => "TouchpadLockAutomationStep", SpeakerVolume => "SpeakerVolumeAutomationStep", Speaker => "SpeakerAutomationStep",
    ShowApp => "ShowAppAutomationStep", Resolution => "ResolutionAutomationStep", RefreshRate => "RefreshRateAutomationStep",
    QuickAction => "QuickActionAutomationStep", PowerMode => "PowerModeAutomationStep", PortsBacklight => "PortsBacklightAutomationStep",
    PlaySound => "PlaySoundAutomationStep", PanelLogoBacklight => "PanelLogoBacklightAutomationStep", OverDrive => "OverDriveAutomationStep",
    AiEngine => "AIEngineAutomationStep", AirplaneMode => "AirplaneModeAutomationStep", DeactivateGpu => "DeactivateGPUAutomationStep",
    CycleGodModePreset => "CycleGodModePresetAutomationStep", FanMaxSpeed => "FanMaxSpeedAutomationStep", GodModePreset => "GodModePresetAutomationStep",
    Wifi => "WiFiAutomationStep", SpectrumKeyboardProfile => "SpectrumKeyboardBacklightProfileAutomationStep",
    SpectrumKeyboardImportProfile => "SpectrumKeyboardBacklightImportProfileAutomationStep", SpectrumKeyboardBrightness => "SpectrumKeyboardBacklightBrightnessAutomationStep",
    Run => "RunAutomationStep", RgbKeyboardBacklight => "RGBKeyboardBacklightAutomationStep", OverclockDiscreteGpu => "OverclockDiscreteGPUAutomationStep",
    OsdLockPosition => "OsdLockPositionAutomationStep", Osd => "OsdAutomationStep", Notification => "NotificationAutomationStep", Macro => "MacroAutomationStep"
]);
legacy_kinds!(TriggerKind, "LenovoLegionToolkit.Lib.Automation.Pipeline.Triggers.", [
    WifiDisconnected => "WiFiDisconnectedAutomationPipelineTrigger", WifiConnected => "WiFiConnectedAutomationPipelineTrigger",
    UserInactivity => "UserInactivityAutomationPipelineTrigger", TimeRange => "TimeRangeAutomationPipelineTrigger", Time => "TimeAutomationPipelineTrigger",
    SessionUnlock => "SessionUnlockAutomationPipelineTrigger", SessionLock => "SessionLockAutomationPipelineTrigger",
    ProcessesStopRunning => "ProcessesStopRunningAutomationPipelineTrigger", ProcessesAreRunning => "ProcessesAreRunningAutomationPipelineTrigger",
    PowerMode => "PowerModeAutomationPipelineTrigger", Periodic => "PeriodicAutomationPipelineTrigger", Or => "OrAutomationPipelineTrigger",
    OnStartup => "OnStartupAutomationPipelineTrigger", OnResume => "OnResumeAutomationPipelineTrigger",
    LowWattageAcConnected => "LowWattageACAdapterConnectedAutomationPipelineTrigger", LidOpened => "LidOpenedAutomationPipelineTrigger", LidClosed => "LidClosedAutomationPipelineTrigger",
    ItsMode => "ITSModeAutomationPipelineTrigger", HybridMode => "HybridModeAutomationPipelineTrigger", HdrOn => "HDROnAutomationPipelineTrigger", HdrOff => "HDROffAutomationPipelineTrigger",
    GodModePresetChanged => "GodModePresetChangedAutomationPipelineTrigger", GamesStop => "GamesStopAutomationPipelineTrigger", GamesAreRunning => "GamesAreRunningAutomationPipelineTrigger",
    ExternalDisplayDisconnected => "ExternalDisplayDisconnectedAutomationPipelineTrigger", ExternalDisplayConnected => "ExternalDisplayConnectedAutomationPipelineTrigger",
    DisplayOn => "DisplayOnAutomationPipelineTrigger", DisplayOff => "DisplayOffAutomationPipelineTrigger",
    DeviceDisconnected => "DeviceDisconnectedAutomationPipelineTrigger", DeviceConnected => "DeviceConnectedAutomationPipelineTrigger",
    BatteryPercentage => "BatteryPercentageAutomationPipelineTrigger", And => "AndAutomationPipelineTrigger",
    AcDisconnected => "ACAdapterDisconnectedAutomationPipelineTrigger", AcConnected => "ACAdapterConnectedAutomationPipelineTrigger"
]);

pub fn normalize_guid(value: &str) -> CoreResult<String> {
    let value = value.trim().trim_start_matches('{').trim_end_matches('}');
    let digits: String = value.chars().filter(|&c| c != '-').collect();
    if digits.len() != 32 || !digits.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(CoreError::invalid("Guid needs 32 hexadecimal digits"));
    }
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &digits[..8],
        &digits[8..12],
        &digits[12..16],
        &digits[16..20],
        &digits[20..]
    )
    .to_ascii_lowercase())
}
fn object(node: &JsonNode) -> CoreResult<()> {
    if node.as_object().is_some() {
        Ok(())
    } else {
        Err(CoreError::invalid("automation DTO object required"))
    }
}
fn kind_name(node: &JsonNode) -> CoreResult<&str> {
    node.get_ci("$type")
        .and_then(JsonNode::as_str)
        .ok_or_else(|| CoreError::invalid("legacy polymorphic $type string required"))
}
pub(crate) fn optional_bool(node: &JsonNode, key: &str, default: bool) -> CoreResult<bool> {
    match node.get_ci(key) {
        None | Some(JsonNode::Null) => Ok(default),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| CoreError::invalid(format!("{key} boolean"))),
    }
}
fn strict_bool(node: &JsonNode, key: &str, default: bool) -> CoreResult<bool> {
    match node.get_ci(key) {
        None => Ok(default),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| CoreError::invalid(format!("{key} non-null boolean"))),
    }
}
pub(crate) fn optional_string(node: &JsonNode, key: &str) -> CoreResult<Option<String>> {
    match node.get_ci(key) {
        None | Some(JsonNode::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(|v| Some(v.into()))
            .ok_or_else(|| CoreError::invalid(format!("{key} string"))),
    }
}
pub(crate) fn integer(node: &JsonNode, key: &str) -> CoreResult<i64> {
    node.get_ci(key)
        .and_then(JsonNode::as_i64)
        .ok_or_else(|| CoreError::invalid(format!("{key} integer")))
}
pub(crate) fn string_list(node: &JsonNode, key: &str) -> CoreResult<Vec<String>> {
    match node.get_ci(key) {
        None | Some(JsonNode::Null) => Ok(Vec::new()),
        Some(JsonNode::Array(values)) => values
            .iter()
            .map(|n| {
                n.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| CoreError::invalid(format!("{key} string array")))
            })
            .collect(),
        _ => Err(CoreError::invalid(format!("{key} array"))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepDto {
    pub kind: StepKind,
    raw: JsonNode,
}
impl StepDto {
    pub fn parse(raw: JsonNode) -> CoreResult<Self> {
        object(&raw)?;
        let kind = StepKind::from_legacy_name(kind_name(&raw)?);
        Ok(Self { kind, raw })
    }
    pub fn new(kind: StepKind, fields: JsonNode) -> CoreResult<Self> {
        object(&fields)?;
        let mut raw = fields;
        let name = kind
            .legacy_type_name()
            .ok_or_else(|| CoreError::invalid("use parse for unknown type"))?;
        raw.set("$type", name.into())?;
        Ok(Self { kind, raw })
    }
    pub fn raw(&self) -> &JsonNode {
        &self.raw
    }
    pub fn field(&self, key: &str) -> Option<&JsonNode> {
        self.raw.get_ci(key)
    }
    pub fn is_dangerous_on_startup(&self) -> bool {
        matches!(
            self.kind,
            StepKind::CloseApp
                | StepKind::HybridMode
                | StepKind::TurnOffMonitors
                | StepKind::AirplaneMode
                | StepKind::Wifi
                | StepKind::DeactivateGpu
        )
    }
    pub fn referenced_pipeline(&self) -> CoreResult<Option<String>> {
        if self.kind != StepKind::QuickAction {
            return Ok(None);
        }
        optional_string(&self.raw, "PipelineId")?
            .map(|v| normalize_guid(&v))
            .transpose()
    }
    pub fn validate(&self) -> CoreResult<()> {
        match self.kind {
            StepKind::Unknown => {
                return Err(CoreError::Unsupported(format!(
                    "opaque step {}",
                    kind_name(&self.raw).unwrap_or("unknown")
                )));
            }
            StepKind::QuickAction => {
                self.referenced_pipeline()?;
            }
            StepKind::CloseApp
            | StepKind::ShowApp
            | StepKind::TurnOffMonitors
            | StepKind::CycleGodModePreset => {}
            StepKind::Run => {
                optional_string(&self.raw, "ScriptPath")?;
                optional_string(&self.raw, "ScriptArguments")?;
                optional_bool(&self.raw, "RunSilently", true)?;
                optional_bool(&self.raw, "WaitUntilFinished", false)?;
                optional_bool(&self.raw, "CheckInstance", false)?;
            }
            StepKind::Notification => {
                optional_string(&self.raw, "Text")?;
            }
            StepKind::PlaySound | StepKind::SpectrumKeyboardImportProfile => {
                optional_string(&self.raw, "Path")?;
            }
            StepKind::GodModePreset => {
                normalize_guid(
                    &optional_string(&self.raw, "PresetId")?
                        .ok_or_else(|| CoreError::invalid("PresetId required"))?,
                )?;
            }
            StepKind::DisplayBrightness | StepKind::SpeakerVolume => {
                let key = if self.kind == StepKind::DisplayBrightness {
                    "Brightness"
                } else {
                    "Volume"
                };
                if !(0..=100).contains(&integer(&self.raw, key)?) {
                    return Err(CoreError::invalid(format!("{key} 0..100")));
                }
            }
            StepKind::SpectrumKeyboardBrightness => {
                if !(0..=9).contains(&integer(&self.raw, "State")?) {
                    return Err(CoreError::invalid("Spectrum brightness 0..9"));
                }
            }
            StepKind::SpectrumKeyboardProfile => {
                if !(1..=6).contains(&integer(&self.raw, "State")?) {
                    return Err(CoreError::invalid(
                        "Spectrum profile 1..6; device may narrow further",
                    ));
                }
            }
            StepKind::Delay => {
                let state = self
                    .field("State")
                    .ok_or_else(|| CoreError::invalid("delay state"))?;
                if !(0..=i32::MAX as i64).contains(&integer(state, "DelaySeconds")?) {
                    return Err(CoreError::invalid("nonnegative bounded delay"));
                }
            }
            _ => {
                self.feature_state()?;
            }
        }
        Ok(())
    }
    pub fn run_options(&self) -> CoreResult<RunOptions> {
        if self.kind != StepKind::Run {
            return Err(CoreError::invalid("Run step required"));
        }
        Ok(RunOptions {
            script_path: optional_string(&self.raw, "ScriptPath")?,
            script_arguments: optional_string(&self.raw, "ScriptArguments")?.unwrap_or_default(),
            run_silently: optional_bool(&self.raw, "RunSilently", true)?,
            wait_until_finished: optional_bool(&self.raw, "WaitUntilFinished", false)?,
            check_instance: optional_bool(&self.raw, "CheckInstance", false)?,
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    pub script_path: Option<String>,
    pub script_arguments: String,
    pub run_silently: bool,
    pub wait_until_finished: bool,
    pub check_instance: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerDto {
    pub kind: TriggerKind,
    pub children: Vec<TriggerDto>,
    raw: JsonNode,
}
impl TriggerDto {
    pub fn parse(raw: JsonNode) -> CoreResult<Self> {
        object(&raw)?;
        let kind = TriggerKind::from_legacy_name(kind_name(&raw)?);
        let children = if matches!(kind, TriggerKind::And | TriggerKind::Or) {
            match raw.get_ci("Triggers") {
                Some(JsonNode::Array(values)) => values
                    .iter()
                    .cloned()
                    .map(Self::parse)
                    .collect::<CoreResult<Vec<_>>>()?,
                _ => return Err(CoreError::invalid("composite Triggers array required")),
            }
        } else {
            Vec::new()
        };
        Ok(Self {
            kind,
            children,
            raw,
        })
    }
    pub fn new(kind: TriggerKind, fields: JsonNode) -> CoreResult<Self> {
        let mut raw = fields;
        object(&raw)?;
        raw.set(
            "$type",
            kind.legacy_type_name()
                .ok_or_else(|| CoreError::invalid("unknown trigger kind"))?
                .into(),
        )?;
        Self::parse(raw)
    }
    pub fn composite(kind: TriggerKind, children: Vec<Self>) -> CoreResult<Self> {
        if !matches!(kind, TriggerKind::And | TriggerKind::Or) {
            return Err(CoreError::invalid("composite kind"));
        }
        let mut fields = JsonNode::object();
        fields.set(
            "Triggers",
            JsonNode::Array(children.iter().map(|c| c.raw().clone()).collect()),
        )?;
        Self::new(kind, fields)
    }
    pub fn raw(&self) -> &JsonNode {
        &self.raw
    }
    pub fn field(&self, key: &str) -> Option<&JsonNode> {
        self.raw.get_ci(key)
    }
    pub fn has_all_triggers(&self) -> bool {
        !matches!(self.kind, TriggerKind::And | TriggerKind::Or) || !self.children.is_empty()
    }
    pub fn validate_known(&self) -> CoreResult<()> {
        if self.kind == TriggerKind::Unknown {
            return Err(CoreError::Unsupported(format!(
                "opaque trigger {}",
                kind_name(&self.raw).unwrap_or("unknown")
            )));
        }
        for child in &self.children {
            child.validate_known()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineDto {
    pub id: String,
    pub name: Option<String>,
    pub icon_name: Option<String>,
    pub trigger: Option<TriggerDto>,
    pub steps: Vec<StepDto>,
    pub is_exclusive: bool,
    pub run_on_startup: bool,
    raw: JsonNode,
}
impl PipelineDto {
    pub fn parse(raw: JsonNode, generated_id: impl FnOnce() -> String) -> CoreResult<Self> {
        object(&raw)?;
        let id = normalize_guid(&optional_string(&raw, "Id")?.unwrap_or_else(generated_id))?;
        let trigger = match raw.get_ci("Trigger") {
            None | Some(JsonNode::Null) => None,
            Some(value) => Some(TriggerDto::parse(value.clone())?),
        };
        let steps = match raw.get_ci("Steps") {
            None => Vec::new(),
            Some(JsonNode::Array(values)) => values
                .iter()
                .cloned()
                .map(StepDto::parse)
                .collect::<CoreResult<Vec<_>>>()?,
            _ => return Err(CoreError::invalid("pipeline Steps array")),
        };
        Ok(Self {
            id,
            name: optional_string(&raw, "Name")?,
            icon_name: optional_string(&raw, "IconName")?,
            is_exclusive: strict_bool(&raw, "IsExclusive", true)?,
            run_on_startup: strict_bool(&raw, "RunOnStartup", false)?,
            trigger,
            steps,
            raw,
        })
    }
    pub fn new(id: &str, trigger: Option<TriggerDto>, steps: Vec<StepDto>) -> CoreResult<Self> {
        Ok(Self {
            id: normalize_guid(id)?,
            name: None,
            icon_name: None,
            trigger,
            steps,
            is_exclusive: true,
            run_on_startup: false,
            raw: JsonNode::object(),
        })
    }
    pub fn to_json_node(&self) -> JsonNode {
        let mut raw = self.raw.clone();
        for (key, value) in [
            ("Id", JsonNode::String(self.id.clone())),
            (
                "Name",
                self.name.clone().map_or(JsonNode::Null, JsonNode::String),
            ),
            (
                "IconName",
                self.icon_name
                    .clone()
                    .map_or(JsonNode::Null, JsonNode::String),
            ),
            ("IsExclusive", JsonNode::Bool(self.is_exclusive)),
            ("RunOnStartup", JsonNode::Bool(self.run_on_startup)),
            (
                "Trigger",
                self.trigger
                    .as_ref()
                    .map_or(JsonNode::Null, |t| t.raw().clone()),
            ),
            (
                "Steps",
                JsonNode::Array(self.steps.iter().map(|s| s.raw().clone()).collect()),
            ),
        ] {
            let _ = raw.set_known(key, value);
        }
        raw
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationDocument {
    pub enabled: bool,
    pub pipelines: Vec<PipelineDto>,
    raw: JsonNode,
}
impl AutomationDocument {
    pub fn parse(text: &str, mut generated_id: impl FnMut() -> String) -> CoreResult<Self> {
        let raw = JsonNode::parse(text)?;
        object(&raw)?;
        let enabled = strict_bool(&raw, "IsEnabled", false)?;
        let pipelines = match raw.get_ci("Pipelines") {
            None => Vec::new(),
            Some(JsonNode::Array(values)) => values
                .iter()
                .cloned()
                .map(|v| PipelineDto::parse(v, &mut generated_id))
                .collect::<CoreResult<Vec<_>>>()?,
            _ => return Err(CoreError::invalid("Pipelines array")),
        };
        Ok(Self {
            enabled,
            pipelines,
            raw,
        })
    }
    pub fn to_json(&self) -> String {
        let mut root = self.raw.clone();
        let _ = root.set_known("IsEnabled", self.enabled.into());
        let _ = root.set_known(
            "Pipelines",
            JsonNode::Array(
                self.pipelines
                    .iter()
                    .map(PipelineDto::to_json_node)
                    .collect(),
            ),
        );
        root.to_json()
    }
    /// Run at initialization before enabling startup. Opaque/unsupported entries remain in the DTO.
    pub fn validate_execution(&self) -> CoreResult<()> {
        for pipeline in &self.pipelines {
            if let Some(trigger) = &pipeline.trigger {
                trigger.validate()?;
            }
            super::expand_pipeline(pipeline, &self.pipelines)?;
        }
        Ok(())
    }
}
