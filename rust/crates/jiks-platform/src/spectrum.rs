//! Spectrum protocol for the verified 83NN 24-zone keyboard.
//! Sources: old Native.cs, Devices.GetKeyboardConfig, Spectrum controller and
//! SpectrumKeyboard24ZoneControl.xaml. This module never opens a HID device.
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const REPORT_BYTES: usize = 960;
pub const REPORT_ID: u8 = 7;
pub const ZONES: usize = 24;
pub type FeatureReport = [u8; REPORT_BYTES];

/// Descriptor match is only a candidate; it does not perform the old D1 handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HidCandidate {
    pub vendor_id: u16,
    pub product_id: u16,
    pub feature_report_bytes: u16,
}
pub fn is_83nn_candidate(machine: &jiks_core::MachineIdentity, hid: HidCandidate) -> bool {
    machine.is_supported()
        && hid.vendor_id == 0x048d
        && hid.product_id & 0xff00 == 0xc100
        && hid.feature_report_bytes == 0x03c0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Operation {
    Compatibility = 0xd1,
    KeyCount = 0xc4,
    KeyPage = 0xc5,
    ProfileChange = 0xc8,
    ProfileDefault = 0xc9,
    Profile = 0xca,
    EffectChange = 0xcb,
    Effect = 0xcc,
    GetBrightness = 0xcd,
    Brightness = 0xce,
}
fn request(op: Operation) -> FeatureReport {
    let mut frame = [0; REPORT_BYTES];
    frame[..4].copy_from_slice(&[REPORT_ID, op as u8, 0xc0, 3]);
    frame
}
pub fn compatibility_request() -> FeatureReport {
    request(Operation::Compatibility)
}
pub fn key_count_request() -> FeatureReport {
    let mut frame = request(Operation::KeyCount);
    frame[4] = 7;
    frame
}
pub fn key_page_request(index: u8, secondary: bool) -> FeatureReport {
    let mut frame = request(Operation::KeyPage);
    frame[4] = if secondary { 8 } else { 7 };
    frame[5] = index;
    frame
}
pub fn brightness_request() -> FeatureReport {
    request(Operation::GetBrightness)
}
pub fn set_brightness(brightness: u8) -> Result<FeatureReport> {
    ensure!(brightness <= 9, "Spectrum brightness must be 0..9");
    let mut frame = request(Operation::Brightness);
    frame[4] = brightness;
    Ok(frame)
}
pub fn profile_request() -> FeatureReport {
    request(Operation::Profile)
}
fn profile_frame(op: Operation, profile: u8, allow_off: bool) -> Result<FeatureReport> {
    ensure!(
        profile <= 6 && (allow_off || profile > 0),
        "Invalid Spectrum profile"
    );
    let mut frame = request(op);
    frame[4] = profile;
    Ok(frame)
}
pub fn set_profile(profile: u8) -> Result<FeatureReport> {
    profile_frame(Operation::ProfileChange, profile, true)
}
pub fn reset_profile(profile: u8) -> Result<FeatureReport> {
    profile_frame(Operation::ProfileDefault, profile, false)
}
pub fn effects_request(profile: u8) -> Result<FeatureReport> {
    profile_frame(Operation::Effect, profile, false)
}
fn response(frame: &[u8], op: Operation) -> Result<()> {
    ensure!(
        frame.len() == REPORT_BYTES,
        "Spectrum response must contain 960 bytes"
    );
    ensure!(
        frame[0] == REPORT_ID && frame[1] == op as u8,
        "Unexpected Spectrum response header"
    );
    Ok(())
}
pub fn decode_compatibility(frame: &[u8]) -> Result<bool> {
    response(frame, Operation::Compatibility)?;
    Ok(frame[4] == 0)
}
pub fn decode_brightness(frame: &[u8]) -> Result<u8> {
    response(frame, Operation::GetBrightness)?;
    ensure!(frame[4] <= 9, "Invalid Spectrum brightness response");
    Ok(frame[4])
}
pub fn decode_profile(frame: &[u8]) -> Result<u8> {
    response(frame, Operation::Profile)?;
    ensure!(frame[4] <= 6, "Invalid Spectrum profile response");
    Ok(frame[4])
}
pub fn decode_key_count(frame: &[u8]) -> Result<(u8, u8)> {
    response(frame, Operation::KeyCount)?;
    ensure!(
        frame[4] == 7 && frame[5] > 0 && frame[6] > 0 && frame[6] <= 32,
        "Invalid Spectrum key map dimensions"
    );
    ensure!(
        usize::from(frame[5]) * usize::from(frame[6]) <= 192,
        "Spectrum key map exceeds protocol capacity"
    );
    Ok((frame[5], frame[6]))
}
pub fn decode_key_page(frame: &[u8], index: u8, secondary: bool, width: u8) -> Result<Vec<u16>> {
    response(frame, Operation::KeyPage)?;
    ensure!(
        width > 0 && width <= 32 && frame[4] == if secondary { 8 } else { 7 } && frame[5] == index,
        "Spectrum key page mismatch"
    );
    Ok((0..usize::from(width))
        .map(|i| u16::from_le_bytes([frame[7 + i * 3], frame[8 + i * 3]]))
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum EffectType {
    Always,
    RainbowScrew,
    RainbowWave,
    ColorChange,
    ColorWave,
    ColorPulse,
    Smooth,
    Rain,
    Ripple,
    Type,
    AudioBounce,
    AudioRipple,
    AuroraSync,
}
impl EffectType {
    pub fn supported_24_zone(self) -> bool {
        matches!(
            self,
            Self::Always | Self::ColorWave | Self::ColorPulse | Self::Smooth | Self::AudioBounce
        )
    }
    fn wire(self) -> u8 {
        match self {
            Self::RainbowScrew => 1,
            Self::RainbowWave => 2,
            Self::ColorChange => 3,
            Self::ColorPulse => 4,
            Self::ColorWave => 5,
            Self::Smooth => 6,
            Self::Rain => 7,
            Self::Ripple => 8,
            Self::AudioBounce => 9,
            Self::AudioRipple => 10,
            Self::Always => 11,
            Self::Type => 12,
            Self::AuroraSync => 13,
        }
    }
    fn from_wire(value: u8) -> Result<Self> {
        Ok(match value {
            1 => Self::RainbowScrew,
            2 => Self::RainbowWave,
            3 => Self::ColorChange,
            4 => Self::ColorPulse,
            5 => Self::ColorWave,
            6 => Self::Smooth,
            7 => Self::Rain,
            8 => Self::Ripple,
            9 => Self::AudioBounce,
            10 => Self::AudioRipple,
            11 => Self::Always,
            12 => Self::Type,
            13 => Self::AuroraSync,
            _ => bail!("Unknown Spectrum effect {value}"),
        })
    }
    fn all_lights(self) -> bool {
        matches!(
            self,
            Self::AudioBounce | Self::AudioRipple | Self::AuroraSync
        )
    }
    fn whole_keyboard(self) -> bool {
        matches!(self, Self::Type | Self::Ripple)
    }
    fn supports_color(self) -> bool {
        matches!(
            self,
            Self::Always
                | Self::ColorChange
                | Self::ColorPulse
                | Self::ColorWave
                | Self::Smooth
                | Self::Rain
                | Self::Ripple
                | Self::Type
        )
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Speed {
    #[default]
    None,
    Speed1,
    Speed2,
    Speed3,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    #[default]
    None,
    BottomToTop,
    TopToBottom,
    LeftToRight,
    RightToLeft,
}
impl Direction {
    fn wire(self) -> u8 {
        match self {
            Self::None => 0,
            Self::BottomToTop => 1,
            Self::TopToBottom => 2,
            Self::RightToLeft => 3,
            Self::LeftToRight => 4,
        }
    }
    fn from_wire(n: u8) -> Result<Self> {
        Ok(match n {
            0 => Self::None,
            1 => Self::BottomToTop,
            2 => Self::TopToBottom,
            3 => Self::RightToLeft,
            4 => Self::LeftToRight,
            _ => bail!("Invalid Spectrum direction"),
        })
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClockwiseDirection {
    #[default]
    None,
    Clockwise,
    CounterClockwise,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorMode {
    #[default]
    None,
    RandomColor,
    ColorList,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Effect {
    #[serde(rename = "Type")]
    pub kind: EffectType,
    #[serde(default)]
    pub speed: Speed,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default)]
    pub clockwise_direction: ClockwiseDirection,
    #[serde(default)]
    pub colors: Vec<Rgb>,
    #[serde(default)]
    pub keys: Vec<u16>,
    #[serde(default)]
    pub use_vantage_color_boost: bool,
    #[serde(default)]
    pub color_mode: ColorMode,
}
impl Effect {
    fn validate_24_zone(&self) -> Result<()> {
        ensure!(
            self.kind.supported_24_zone(),
            "Effect {:?} is unsupported on 83NN 24-zone keyboard",
            self.kind
        );
        ensure!(
            self.colors.len() <= 255,
            "Spectrum color list exceeds one byte count"
        );
        ensure!(
            self.keys.iter().all(|k| (1..=24).contains(k)),
            "24-zone key must be 1..24"
        );
        ensure!(
            !self.kind.all_lights() || self.keys.is_empty(),
            "All-lights effect cannot select individual zones"
        );
        ensure!(
            !self.use_vantage_color_boost,
            "Aurora color boost is unavailable on 24-zone hardware"
        );
        Ok(())
    }
}
/// Same last-effect-wins compression as the C# controller, before serialization.
pub fn compress(effects: &[Effect]) -> Vec<Effect> {
    if let Some(last) = effects.iter().rev().find(|e| e.kind.all_lights()) {
        return vec![last.clone()];
    }
    let mut used = BTreeSet::new();
    let mut result = Vec::new();
    for effect in effects.iter().rev() {
        if effect.kind.whole_keyboard() && effect.keys.iter().any(|key| used.contains(key)) {
            continue;
        }
        let mut effect = effect.clone();
        let mut unique = BTreeSet::new();
        effect
            .keys
            .retain(|key| !used.contains(key) && unique.insert(*key));
        used.extend(effect.keys.iter().copied());
        if !effect.keys.is_empty() {
            result.push(effect);
        }
    }
    result.reverse();
    result
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub profile: u8,
    pub effects: Vec<Effect>,
}
impl Profile {
    pub fn encode_24_zone(&self) -> Result<FeatureReport> {
        ensure!(
            (1..=6).contains(&self.profile),
            "Effect profile must be 1..6"
        );
        // Validate the entire original input before compression; never hide a bad entry.
        for effect in &self.effects {
            effect.validate_24_zone()?;
        }
        let effects = compress(&self.effects);
        ensure!(effects.len() <= 255, "Too many Spectrum effects");
        let mut bytes = vec![
            REPORT_ID,
            Operation::EffectChange as u8,
            0,
            0,
            self.profile,
            1,
            1,
        ];
        for (index, effect) in effects.iter().enumerate() {
            let mode = if effect.color_mode != ColorMode::None {
                effect.color_mode
            } else if effect.kind.supports_color() {
                if effect.colors.is_empty() {
                    ColorMode::RandomColor
                } else {
                    ColorMode::ColorList
                }
            } else {
                ColorMode::None
            };
            let colors = if mode == ColorMode::ColorList {
                effect.colors.as_slice()
            } else {
                &[]
            };
            let keys: &[u16] = if effect.kind.all_lights() {
                &[0x65]
            } else {
                &effect.keys
            };
            bytes.extend_from_slice(&[
                (index + 1) as u8,
                6,
                1,
                effect.kind.wire(),
                2,
                effect.speed as u8,
                3,
                effect.clockwise_direction as u8,
                4,
                effect.direction.wire(),
                5,
                mode as u8,
                6,
                0,
                colors.len() as u8,
            ]);
            for color in colors {
                bytes.extend_from_slice(&[color.r, color.g, color.b]);
            }
            bytes.push(keys.len() as u8);
            for key in keys {
                bytes.extend_from_slice(&key.to_le_bytes());
            }
        }
        // Keep a zero terminator. The payload length occupies both bytes 2 and 3,
        // unlike the fixed command header whose byte 3 is always 3.
        ensure!(
            bytes.len() < REPORT_BYTES,
            "Spectrum effects exceed feature report capacity"
        );
        let len = (bytes.len() - 4) as u16;
        bytes[2..4].copy_from_slice(&len.to_le_bytes());
        let mut frame = [0; REPORT_BYTES];
        frame[..bytes.len()].copy_from_slice(&bytes);
        Ok(frame)
    }
    /// Decode all known firmware effects, including unsupported-on-this-machine
    /// effects for display/export. Re-encoding for 83NN rejects unsupported ones.
    pub fn decode(frame: &[u8]) -> Result<Self> {
        ensure!(
            frame.len() == REPORT_BYTES && frame[0] == REPORT_ID && matches!(frame[1], 0xcb | 0xcc),
            "Invalid Spectrum effect frame"
        );
        ensure!(
            (1..=6).contains(&frame[4]) && frame[5..7] == [1, 1],
            "Invalid Spectrum effect profile header"
        );
        // Response firmware has historically reused C0/03; bound by the actual
        // 960 bytes and monotone effect IDs rather than trusting that header.
        let mut offset = 7;
        let mut previous = 0;
        let mut effects = Vec::new();
        while offset < REPORT_BYTES && frame[offset] != 0 {
            ensure!(
                frame[offset] > previous,
                "Duplicate/out-of-order Spectrum effect number"
            );
            previous = frame[offset];
            offset += 1;
            ensure!(
                offset + 14 <= REPORT_BYTES,
                "Truncated Spectrum effect header"
            );
            let h = &frame[offset..offset + 13];
            ensure!(
                h[0..2] == [6, 1]
                    && h[3] == 2
                    && h[5] == 3
                    && h[7] == 4
                    && h[9] == 5
                    && h[11..13] == [6, 0],
                "Malformed Spectrum effect tags"
            );
            let kind = EffectType::from_wire(h[2])?;
            let speed = match h[4] {
                0 => Speed::None,
                1 => Speed::Speed1,
                2 => Speed::Speed2,
                3 => Speed::Speed3,
                _ => bail!("Invalid Spectrum speed"),
            };
            let clockwise_direction = match h[6] {
                0 => ClockwiseDirection::None,
                1 => ClockwiseDirection::Clockwise,
                2 => ClockwiseDirection::CounterClockwise,
                _ => bail!("Invalid Spectrum rotation"),
            };
            let direction = Direction::from_wire(h[8])?;
            let color_mode = match h[10] {
                0 => ColorMode::None,
                1 => ColorMode::RandomColor,
                2 => ColorMode::ColorList,
                _ => bail!("Invalid Spectrum color mode"),
            };
            offset += 13;
            let count = usize::from(frame[offset]);
            offset += 1;
            ensure!(
                offset + count * 3 < REPORT_BYTES,
                "Truncated Spectrum colors"
            );
            let colors = (0..count)
                .map(|i| Rgb {
                    r: frame[offset + i * 3],
                    g: frame[offset + i * 3 + 1],
                    b: frame[offset + i * 3 + 2],
                })
                .collect();
            offset += count * 3;
            let count = usize::from(frame[offset]);
            offset += 1;
            ensure!(offset + count * 2 < REPORT_BYTES, "Truncated Spectrum keys");
            let mut keys = (0..count)
                .map(|i| u16::from_le_bytes([frame[offset + i * 2], frame[offset + i * 2 + 1]]))
                .collect::<Vec<_>>();
            offset += count * 2;
            if keys == [0x65] {
                keys.clear();
            }
            effects.push(Effect {
                kind,
                speed,
                direction,
                clockwise_direction,
                colors,
                keys,
                use_vantage_color_boost: kind == EffectType::AuroraSync && speed == Speed::Speed3,
                color_mode,
            });
        }
        ensure!(offset < REPORT_BYTES, "Spectrum effects lack terminator");
        Ok(Self {
            profile: frame[4],
            effects,
        })
    }
}

/// The platform actor supplies the owned HID handle. Merely constructing codecs
/// or a transport never requests compatibility, responsiveness, or enumeration.
pub trait SpectrumTransport {
    fn set_feature(&mut self, frame: &FeatureReport) -> Result<()>;
    fn get_feature(&mut self, frame: &mut FeatureReport) -> Result<()>;
}
fn transact(transport: &mut impl SpectrumTransport, frame: FeatureReport) -> Result<FeatureReport> {
    transport.set_feature(&frame)?;
    let mut response = [0; REPORT_BYTES];
    response[0] = REPORT_ID;
    transport.get_feature(&mut response)?;
    Ok(response)
}
pub fn read_brightness(transport: &mut impl SpectrumTransport) -> Result<u8> {
    decode_brightness(&transact(transport, brightness_request())?)
}
pub fn read_profile(transport: &mut impl SpectrumTransport) -> Result<u8> {
    decode_profile(&transact(transport, profile_request())?)
}
pub fn read_effects(transport: &mut impl SpectrumTransport, profile: u8) -> Result<Profile> {
    let decoded = Profile::decode(&transact(transport, effects_request(profile)?)?)?;
    ensure!(
        decoded.profile == profile,
        "Spectrum returned a different profile"
    );
    Ok(decoded)
}
/// Guard is re-evaluated before each SetFeature (including read requests).
/// Caller retains machine-scoped ownership and the HID handle until return.
pub fn apply_profile_verified(
    transport: &mut impl SpectrumTransport,
    profile: &Profile,
    mut guard: impl FnMut() -> Result<()>,
) -> Result<()> {
    let frame = profile.encode_24_zone()?;
    let expected = Profile::decode(&frame)?;
    guard()?;
    transport.set_feature(&frame)?;
    guard()?;
    let actual = read_effects(transport, profile.profile)?;
    ensure!(
        actual == expected,
        "Spectrum profile readback differs from requested effects"
    );
    Ok(())
}
pub fn apply_brightness_verified(
    transport: &mut impl SpectrumTransport,
    brightness: u8,
    mut guard: impl FnMut() -> Result<()>,
) -> Result<()> {
    let frame = set_brightness(brightness)?;
    guard()?;
    transport.set_feature(&frame)?;
    guard()?;
    ensure!(
        read_brightness(transport)? == brightness,
        "Spectrum brightness readback mismatch"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn effect(kind: EffectType, keys: &[u16]) -> Effect {
        Effect {
            kind,
            speed: Speed::Speed2,
            direction: Direction::LeftToRight,
            clockwise_direction: ClockwiseDirection::None,
            colors: vec![Rgb { r: 1, g: 2, b: 3 }],
            keys: keys.to_vec(),
            use_vantage_color_boost: false,
            color_mode: ColorMode::None,
        }
    }
    #[test]
    fn fixed_requests_match_csharp_layout() {
        assert_eq!(&compatibility_request()[..5], &[7, 0xd1, 0xc0, 3, 0]);
        assert_eq!(&set_brightness(9).unwrap()[..6], &[7, 0xce, 0xc0, 3, 9, 0]);
        assert_eq!(
            &key_page_request(2, true)[..7],
            &[7, 0xc5, 0xc0, 3, 8, 2, 0]
        );
        assert!(set_brightness(10).is_err());
        assert!(set_profile(7).is_err());
        assert!(reset_profile(0).is_err());
        assert!(set_profile(0).is_ok());
    }
    #[test]
    fn effect_bytes_match_legacy_binary_writer() {
        let frame = Profile {
            profile: 2,
            effects: vec![effect(EffectType::Always, &[1, 24])],
        }
        .encode_24_zone()
        .unwrap();
        assert_eq!(
            &frame[..30],
            &[
                7, 0xcb, 26, 0, 2, 1, 1, 1, 6, 1, 11, 2, 2, 3, 0, 4, 4, 5, 2, 6, 0, 1, 1, 2, 3, 2,
                1, 0, 24, 0
            ]
        );
        assert!(frame[30..].iter().all(|b| *b == 0));
        let decoded = Profile::decode(&frame).unwrap();
        assert_eq!(decoded.effects[0].direction, Direction::LeftToRight);
        assert_eq!(decoded.effects[0].color_mode, ColorMode::ColorList);
        assert_eq!(decoded.encode_24_zone().unwrap(), frame);
    }
    #[test]
    fn last_zone_effect_wins_and_audio_replaces_all() {
        let input = [
            effect(EffectType::Always, &[1, 2, 3]),
            effect(EffectType::ColorPulse, &[2, 2]),
        ];
        let result = compress(&input);
        assert_eq!(result[0].keys, vec![1, 3]);
        assert_eq!(result[1].keys, vec![2]);
        let input = [
            input[0].clone(),
            effect(EffectType::AudioBounce, &[]),
            input[1].clone(),
        ];
        assert_eq!(compress(&input), vec![input[1].clone()]);
    }
    #[test]
    fn unsupported_and_invalid_profiles_fail_before_compression() {
        for kind in [
            EffectType::AuroraSync,
            EffectType::AudioRipple,
            EffectType::ColorChange,
            EffectType::Rain,
            EffectType::RainbowScrew,
            EffectType::RainbowWave,
            EffectType::Ripple,
            EffectType::Type,
        ] {
            assert!(!kind.supported_24_zone());
            assert!(
                Profile {
                    profile: 1,
                    effects: vec![effect(kind, &[1])]
                }
                .encode_24_zone()
                .is_err()
            );
        }
        assert!(
            Profile {
                profile: 1,
                effects: vec![
                    effect(EffectType::Always, &[25]),
                    effect(EffectType::AudioBounce, &[])
                ]
            }
            .encode_24_zone()
            .is_err()
        );
        let mut huge = effect(EffectType::Always, &[1]);
        huge.colors = vec![Rgb::default(); 255];
        assert!(
            Profile {
                profile: 1,
                effects: vec![huge.clone(), effect(EffectType::ColorPulse, &[2])]
            }
            .encode_24_zone()
            .is_ok()
        );
        huge.colors.push(Rgb::default());
        assert!(
            Profile {
                profile: 1,
                effects: vec![huge]
            }
            .encode_24_zone()
            .is_err()
        );
    }
    #[test]
    fn decoder_rejects_corruption_and_uses_unsigned_length() {
        let mut huge = effect(EffectType::Always, &[1]);
        huge.colors = vec![Rgb::default(); 100];
        let frame = Profile {
            profile: 6,
            effects: vec![huge],
        }
        .encode_24_zone()
        .unwrap();
        assert!(u16::from_le_bytes([frame[2], frame[3]]) > 255);
        assert_eq!(
            Profile::decode(&frame).unwrap().effects[0].colors.len(),
            100
        );
        for index in [0, 5, 8, 9, 11, 13, 15, 17, 19, 20] {
            let mut bad = frame;
            bad[index] = 0xff;
            assert!(Profile::decode(&bad).is_err(), "index {index}");
        }
        assert!(Profile::decode(&frame[..959]).is_err());
    }
    #[test]
    fn json_legacy_names_and_directions_are_not_wire_ordinals() {
        let value = serde_json::to_value(effect(EffectType::ColorWave, &[24])).unwrap();
        assert_eq!(value["Type"], "ColorWave");
        assert_eq!(value["Colors"][0]["R"], 1);
        assert_eq!(value["Direction"], "LeftToRight");
        assert_eq!(Direction::LeftToRight.wire(), 4);
        let decoded: Effect = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.keys, vec![24]);
    }
    #[test]
    fn candidate_filter_does_not_claim_protocol_support() {
        let machine = jiks_core::MachineIdentity {
            manufacturer: "LENOVO".into(),
            model: "83NN".into(),
            bios: "S9CN15WW".into(),
            cpu: "i9-14900HX".into(),
            logical_processors: 32,
        };
        let hid = HidCandidate {
            vendor_id: 0x048d,
            product_id: 0xc1ab,
            feature_report_bytes: 960,
        };
        assert!(is_83nn_candidate(&machine, hid));
        for wrong in [
            HidCandidate {
                vendor_id: 0x17ef,
                ..hid
            },
            HidCandidate {
                product_id: 0xc900,
                ..hid
            },
            HidCandidate {
                feature_report_bytes: 33,
                ..hid
            },
        ] {
            assert!(!is_83nn_candidate(&machine, wrong));
        }
    }
    #[test]
    fn fake_transport_readback_and_guard_fail_closed() {
        struct Fake {
            writes: Vec<FeatureReport>,
            value: u8,
        }
        impl SpectrumTransport for Fake {
            fn set_feature(&mut self, f: &FeatureReport) -> Result<()> {
                self.writes.push(*f);
                Ok(())
            }
            fn get_feature(&mut self, f: &mut FeatureReport) -> Result<()> {
                *f = request(Operation::GetBrightness);
                f[4] = self.value;
                Ok(())
            }
        }
        let mut fake = Fake {
            writes: vec![],
            value: 4,
        };
        assert!(apply_brightness_verified(&mut fake, 3, || Ok(())).is_err());
        assert_eq!(fake.writes.len(), 2);
        fake.writes.clear();
        assert!(apply_brightness_verified(&mut fake, 4, || bail!("owner absent")).is_err());
        assert!(fake.writes.is_empty());
        assert!(apply_brightness_verified(&mut fake, 4, || Ok(())).is_ok());
    }
}
