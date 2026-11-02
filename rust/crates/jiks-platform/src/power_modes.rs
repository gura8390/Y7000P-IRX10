//! Exact raw mode discovery, including the upstream firmware Extreme=224 mode.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerModeOption {
    pub raw: u32,
    pub name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerModes {
    pub current_raw: u32,
    pub modes: Vec<PowerModeOption>,
    pub reported_mask: Option<u32>,
    pub smart_fan_version: Option<u32>,
    pub legion_zone_version: Option<u32>,
    pub source: String,
}
pub fn decode(
    current_raw: u32,
    mask: Option<u32>,
    smart: Option<u32>,
    zone: Option<u32>,
    capability_source: bool,
) -> PowerModes {
    let mut modes = vec![
        PowerModeOption {
            raw: 1,
            name: "安静".into(),
        },
        PowerModeOption {
            raw: 2,
            name: "均衡".into(),
        },
        PowerModeOption {
            raw: 3,
            name: "性能".into(),
        },
    ];
    // Exact Compatibility.GetSupportsExtremeMode predicate from the original
    // toolkit. The fallback GetSupportThermalMode API has no Extreme mapping.
    if capability_source
        && mask.is_some_and(|value| value & (1 << 4) != 0)
        && (smart.is_some_and(|v| matches!(v, 6 | 7 | 8))
            || zone.is_some_and(|v| matches!(v, 3 | 4 | 5)))
    {
        modes.push(PowerModeOption {
            raw: 224,
            name: "超能".into(),
        });
    }
    if mask.is_some_and(|value| value & (1 << 16) != 0)
        && (smart.is_some_and(|v| (4..=9).contains(&v))
            || zone.is_some_and(|v| (1..=6).contains(&v)))
    {
        modes.push(PowerModeOption {
            raw: 255,
            name: "自定义".into(),
        });
    }
    PowerModes {
        current_raw,
        modes,
        reported_mask: mask,
        smart_fan_version: smart,
        legion_zone_version: zone,
        source: if capability_source {
            "Lenovo SupportedPowerModes 0x00070000 + original compatibility version gate"
        } else {
            "Lenovo GetSupportThermalMode fallback; Extreme is not inferred"
        }
        .into(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extreme_needs_both_original_firmware_flag_and_supported_version() {
        let bit = Some((1 << 4) | (1 << 16));
        assert!(
            decode(3, bit, Some(8), None, true)
                .modes
                .iter()
                .any(|v| v.raw == 224)
        );
        assert!(
            !decode(3, bit, Some(9), Some(6), true)
                .modes
                .iter()
                .any(|v| v.raw == 224)
        );
        assert!(
            !decode(3, bit, Some(8), Some(5), false)
                .modes
                .iter()
                .any(|v| v.raw == 224)
        );
        assert!(
            !decode(224, None, Some(8), Some(5), true)
                .modes
                .iter()
                .any(|v| v.raw == 224)
        );
        assert!(
            decode(3, bit, Some(9), Some(6), true)
                .modes
                .iter()
                .any(|v| v.raw == 255)
        );
    }
}
