//! All legacy step families become inert requests. Only platform dispatch executes them.
use super::dto::{integer, optional_bool, optional_string};
use super::{RunOptions, StepDto, StepKind, normalize_guid};
use crate::settings::JsonNode;
use crate::{CoreError, CoreResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureState {
    Enum(i32),
    DpiScale(i32),
    Resolution {
        width: i32,
        height: i32,
    },
    RefreshRate {
        frequency: i32,
        is_dynamic: bool,
        base_frequency: i32,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionRequest {
    Feature {
        kind: StepKind,
        state: FeatureState,
    },
    Delay {
        seconds: u32,
    },
    DisplayBrightness(u8),
    SpeakerVolume(u8),
    SpectrumBrightness(u8),
    SpectrumProfile(u8),
    GodModePreset {
        id: String,
    },
    CycleGodModePreset,
    QuickAction {
        pipeline_id: Option<String>,
    },
    Run(RunOptions),
    Notification {
        text: Option<String>,
        overrides: JsonNode,
    },
    PlaySound {
        path: Option<String>,
    },
    ImportSpectrumProfile {
        path: Option<String>,
    },
    CloseApp,
    ShowApp,
    TurnOffMonitors,
}
fn i32_field(node: &JsonNode, key: &str) -> CoreResult<i32> {
    i32::try_from(integer(node, key)?).map_err(|_| CoreError::invalid(format!("{key} Int32")))
}
fn enum_values(kind: StepKind) -> CoreResult<&'static [(&'static str, i32)]> {
    Ok(match kind {
        StepKind::AlwaysOnUsb => &[("Off", 0), ("OnWhenSleeping", 1), ("OnAlways", 2)],
        StepKind::BatteryNightCharge => &[("On", 0), ("Off", 1)],
        StepKind::Battery => &[("Conservation", 0), ("Normal", 1), ("RapidCharge", 2)],
        StepKind::InstantBoot => &[
            ("Off", 0),
            ("AcAdapter", 1),
            ("UsbPowerDelivery", 2),
            ("AcAdapterAndUsbPowerDelivery", 3),
        ],
        StepKind::HybridMode => &[
            ("On", 0),
            ("OnIGPUOnly", 1),
            ("OnAuto", 2),
            ("UMA", 3),
            ("Off", 4),
        ],
        StepKind::ItsMode => &[
            ("None", 0),
            ("ItsAuto", 1),
            ("MmcCool", 2),
            ("MmcPerformance", 3),
            ("MmcGeek", 4),
        ],
        StepKind::PowerMode => &[
            ("Quiet", 0),
            ("Balance", 1),
            ("Performance", 2),
            ("Extreme", 223),
            ("GodMode", 254),
        ],
        StepKind::Speaker => &[("Mute", 0), ("Unmute", 1)],
        StepKind::WhiteKeyboardBacklight => &[("Off", 0), ("Low", 1), ("High", 2), ("Auto", 3)],
        StepKind::RgbKeyboardBacklight => &[
            ("Off", -1),
            ("One", 0),
            ("Two", 1),
            ("Three", 2),
            ("Four", 3),
        ],
        StepKind::DeactivateGpu => &[("KillApps", 0), ("RestartGPU", 1)],
        StepKind::AiEngine
        | StepKind::AirplaneMode
        | StepKind::FanMaxSpeed
        | StepKind::Wifi
        | StepKind::OverclockDiscreteGpu
        | StepKind::OsdLockPosition
        | StepKind::Osd
        | StepKind::Macro => &[("Off", 0), ("On", 1), ("Toggle", 2)],
        StepKind::AutoColorManagement
        | StepKind::Hdr
        | StepKind::HardwareSensors
        | StepKind::FnLock
        | StepKind::FlipToStart
        | StepKind::Microphone
        | StepKind::OneLevelWhiteKeyboardBacklight
        | StepKind::WinKey
        | StepKind::TouchpadLock
        | StepKind::PortsBacklight
        | StepKind::PanelLogoBacklight
        | StepKind::OverDrive => &[("Off", 0), ("On", 1)],
        _ => return Err(CoreError::invalid("not an enum feature step")),
    })
}
impl StepDto {
    pub fn feature_state(&self) -> CoreResult<FeatureState> {
        let default = JsonNode::Number("0".into());
        let raw = self.field("State").unwrap_or(&default);
        match self.kind {
            StepKind::DpiScale => {
                let scale = i32_field(raw, "Scale")?;
                if scale <= 0 {
                    return Err(CoreError::invalid("DPI scale must be positive"));
                }
                Ok(FeatureState::DpiScale(scale))
            }
            StepKind::Resolution => {
                let width = i32_field(raw, "Width")?;
                let height = i32_field(raw, "Height")?;
                if width <= 0 || height <= 0 {
                    return Err(CoreError::invalid("resolution dimensions"));
                }
                Ok(FeatureState::Resolution { width, height })
            }
            StepKind::RefreshRate => {
                let frequency = i32_field(raw, "Frequency")?;
                let is_dynamic = optional_bool(raw, "IsDynamic", false)?;
                let base_frequency = raw
                    .get_ci("BaseFrequency")
                    .map_or(Ok(0), |_| i32_field(raw, "BaseFrequency"))?;
                if frequency <= 0 || base_frequency < 0 || is_dynamic && base_frequency <= 0 {
                    return Err(CoreError::invalid("refresh rate parameters"));
                }
                Ok(FeatureState::RefreshRate {
                    frequency,
                    is_dynamic,
                    base_frequency,
                })
            }
            _ => {
                let allowed = enum_values(self.kind)?;
                let value = match raw {
                    JsonNode::String(value) => allowed
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case(value))
                        .map(|(_, v)| *v)
                        .or_else(|| value.parse::<i32>().ok()),
                    JsonNode::Number(value) => value.parse().ok(),
                    _ => None,
                }
                .ok_or_else(|| {
                    CoreError::Unsupported(format!("unknown feature State for {:?}", self.kind))
                })?;
                if !allowed.iter().any(|(_, v)| *v == value) {
                    return Err(CoreError::Unsupported(format!(
                        "future numeric feature State {value} retained but cannot execute"
                    )));
                }
                Ok(FeatureState::Enum(value))
            }
        }
    }
    pub fn action_request(&self) -> CoreResult<ActionRequest> {
        self.validate()?;
        Ok(match self.kind {
            StepKind::Delay => ActionRequest::Delay {
                seconds: integer(self.field("State").unwrap(), "DelaySeconds")? as u32,
            },
            StepKind::DisplayBrightness => {
                ActionRequest::DisplayBrightness(integer(self.raw(), "Brightness")? as u8)
            }
            StepKind::SpeakerVolume => {
                ActionRequest::SpeakerVolume(integer(self.raw(), "Volume")? as u8)
            }
            StepKind::SpectrumKeyboardBrightness => {
                ActionRequest::SpectrumBrightness(integer(self.raw(), "State")? as u8)
            }
            StepKind::SpectrumKeyboardProfile => {
                ActionRequest::SpectrumProfile(integer(self.raw(), "State")? as u8)
            }
            StepKind::GodModePreset => ActionRequest::GodModePreset {
                id: normalize_guid(&optional_string(self.raw(), "PresetId")?.unwrap())?,
            },
            StepKind::CycleGodModePreset => ActionRequest::CycleGodModePreset,
            StepKind::QuickAction => ActionRequest::QuickAction {
                pipeline_id: self.referenced_pipeline()?,
            },
            StepKind::Run => ActionRequest::Run(self.run_options()?),
            StepKind::Notification => {
                let mut overrides = JsonNode::object();
                for key in [
                    "IconOverride",
                    "ColorOverride",
                    "TextColorOverride",
                    "PositionOverride",
                    "DurationOverride",
                ] {
                    if let Some(value) = self.field(key) {
                        overrides.set(key, value.clone())?;
                    }
                }
                ActionRequest::Notification {
                    text: optional_string(self.raw(), "Text")?,
                    overrides,
                }
            }
            StepKind::PlaySound => ActionRequest::PlaySound {
                path: optional_string(self.raw(), "Path")?,
            },
            StepKind::SpectrumKeyboardImportProfile => ActionRequest::ImportSpectrumProfile {
                path: optional_string(self.raw(), "Path")?,
            },
            StepKind::CloseApp => ActionRequest::CloseApp,
            StepKind::ShowApp => ActionRequest::ShowApp,
            StepKind::TurnOffMonitors => ActionRequest::TurnOffMonitors,
            StepKind::Unknown => {
                return Err(CoreError::Unsupported("opaque step cannot execute".into()));
            }
            _ => ActionRequest::Feature {
                kind: self.kind,
                state: self.feature_state()?,
            },
        })
    }
}
