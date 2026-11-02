// SPDX-License-Identifier: MPL-2.0
//! Read-only NVIDIA sensor codecs. No library loads or device calls on import.
//! ABI source: the fixed LLT LibreHardwareMonitor commit ad760baba99c7c4ad6c86c5c57fb3ba766a7ee71,
//! Hardware/Gpu/NvApi.cs and NvidiaGpu.cs; public NVAPI structures match NVIDIA/nvapi.
//! This file contains no fan, clock, voltage, power or driver setter.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, ffi::c_void};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorFunction {
    ThermalSettings,
    ThermalSensors,
    Clocks,
    Utilization,
    Memory,
    Fans,
    Power,
    Voltage,
}
impl SensorFunction {
    pub const fn id(self) -> u32 {
        match self {
            Self::ThermalSettings => 0xe3640a56,
            Self::ThermalSensors => 0x65fe3aad,
            Self::Clocks => 0xdcb616c3,
            Self::Utilization => 0x60ded2ed,
            Self::Memory => 0xc0599498,
            Self::Fans => 0x35aed5e8,
            Self::Power => 0xedcf624e,
            Self::Voltage => 0x465f9bcf,
        }
    }
    pub const fn layout(self) -> (usize, u32) {
        match self {
            Self::ThermalSettings => (68, 2),
            Self::ThermalSensors => (168, 2),
            Self::Clocks => (264, 2),
            Self::Utilization => (72, 1),
            Self::Memory => (80, 1),
            Self::Fans => (1704, 1),
            Self::Power => (72, 1),
            Self::Voltage => (76, 1),
        }
    }
}
/// DWORD and 64-bit fields are aligned, including the padding before MemoryEx.total.
#[derive(Debug)]
pub struct SensorPacket {
    words: Vec<u64>,
    bytes: usize,
}
impl SensorPacket {
    pub fn new(function: SensorFunction) -> Self {
        let (bytes, version) = function.layout();
        let mut p = Self {
            words: vec![0; bytes.div_ceil(8)],
            bytes,
        };
        p.set(0, (version << 16) | bytes as u32);
        p
    }
    pub fn as_mut_ptr(&mut self) -> *mut c_void {
        self.words.as_mut_ptr().cast()
    }
    pub fn len_bytes(&self) -> usize {
        self.bytes
    }
    pub fn get(&self, offset: usize) -> u32 {
        assert!(offset.is_multiple_of(4) && offset + 4 <= self.bytes);
        let v = self.words[offset / 8];
        (v >> ((offset % 8) * 8)) as u32
    }
    pub fn set(&mut self, offset: usize, value: u32) {
        assert!(offset.is_multiple_of(4) && offset + 4 <= self.bytes);
        let shift = (offset % 8) * 8;
        self.words[offset / 8] =
            (self.words[offset / 8] & !(0xffff_ffffu64 << shift)) | ((value as u64) << shift);
    }
    pub fn get_u64(&self, offset: usize) -> u64 {
        (self.get(offset) as u64) | ((self.get(offset + 4) as u64) << 32)
    }
    pub fn validate(&self, function: SensorFunction) -> Result<()> {
        let (bytes, version) = function.layout();
        ensure!(
            self.bytes == bytes && self.get(0) == (version << 16) | bytes as u32,
            "NVIDIA sensor ABI version/size changed"
        );
        Ok(())
    }
}
pub trait SensorTransport {
    fn call(&mut self, function: SensorFunction, packet: &mut SensorPacket) -> Result<()>;
    fn tachometer(&mut self) -> Result<u32>;
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorValue {
    pub value: Option<f64>,
    pub unit: String,
    pub source: String,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanSample {
    pub id: u32,
    pub rpm: u32,
    pub duty_percent: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuSample {
    #[serde(default)]
    pub is_igpu: bool,
    pub index: u32,
    pub name: String,
    #[serde(default)]
    pub performance_state: Option<u32>,
    #[serde(default)]
    pub status: Option<crate::gpu_status::GpuStatusSnapshot>,
    #[serde(default)]
    pub hybrid_off: Option<bool>,
    pub requested_at_tick_ms: u64,
    pub completed_at_tick_ms: u64,
    pub values: BTreeMap<String, SensorValue>,
    pub fans: Vec<FanSample>,
    pub errors: BTreeMap<String, String>,
}
impl GpuSample {
    pub fn new(index: u32, name: &str, tick: u64) -> Self {
        Self {
            is_igpu: false,
            index,
            name: name.into(),
            performance_state: None,
            status: None,
            hybrid_off: None,
            requested_at_tick_ms: tick,
            completed_at_tick_ms: tick,
            values: BTreeMap::new(),
            fans: Vec::new(),
            errors: BTreeMap::new(),
        }
    }
    pub fn insert(&mut self, key: &str, value: f64, unit: &str, source: &str) -> Result<()> {
        ensure!(
            value.is_finite() && value >= 0.,
            "Invalid {key} sensor value"
        );
        self.values.insert(
            key.into(),
            SensorValue {
                value: Some(value),
                unit: unit.into(),
                source: source.into(),
                error: None,
            },
        );
        Ok(())
    }
    pub fn error(&mut self, key: &str, unit: &str, error: &anyhow::Error) {
        if !self.values.contains_key(key) {
            self.values.insert(
                key.into(),
                SensorValue {
                    value: None,
                    unit: unit.into(),
                    source: "NVIDIA native getter".into(),
                    error: Some(format!("{error:#}")),
                },
            );
        }
    }
}
fn packet(t: &mut impl SensorTransport, function: SensorFunction) -> Result<SensorPacket> {
    let mut p = SensorPacket::new(function);
    t.call(function, &mut p)?;
    p.validate(function)?;
    Ok(p)
}
fn temperature(raw: u32) -> Result<f64> {
    let value = raw as i32 as f64;
    ensure!(
        value > 0. && value < 150.,
        "Invalid/absent NVIDIA temperature"
    );
    Ok(value)
}
pub fn read(
    t: &mut impl SensorTransport,
    index: u32,
    name: &str,
    tick: u64,
    thermal_mask: &mut Option<u32>,
) -> GpuSample {
    let mut sample = GpuSample::new(index, name, tick);
    macro_rules! capture {
        ($name:expr,$body:expr) => {
            if let Err(error) = (|| -> Result<()> {
                $body;
                Ok(())
            })() {
                sample.errors.insert($name.into(), format!("{error:#}"));
            }
        };
    }
    capture!("thermal", {
        let p = packet(t, SensorFunction::ThermalSettings)?;
        let count = p.get(4) as usize;
        ensure!(count <= 3, "NVIDIA thermal count exceeds ABI");
        for i in 0..count {
            let offset = 8 + i * 20;
            let key = match p.get(offset + 16) {
                1 => "GpuCoreTemperature",
                2 => "GpuVramTemperature",
                _ => continue,
            };
            sample.insert(
                key,
                temperature(p.get(offset + 12))?,
                "°C",
                "NVAPI thermal settings",
            )?;
        }
    });
    // Match the original bounded mask discovery. It runs once per opened device,
    // never from ordinary inventory or before the explicit sensor action.
    if thermal_mask.is_none() {
        let mut accepted = 0;
        for bit in 0..32 {
            let mut p = SensorPacket::new(SensorFunction::ThermalSensors);
            p.set(4, 1u32 << bit);
            match t
                .call(SensorFunction::ThermalSensors, &mut p)
                .and_then(|_| p.validate(SensorFunction::ThermalSensors))
            {
                Ok(()) => accepted = (1u32 << bit).wrapping_mul(2).wrapping_sub(1),
                Err(error) => {
                    if bit == 0 {
                        sample
                            .errors
                            .insert("thermal_mask".into(), format!("{error:#}"));
                    }
                    break;
                }
            }
        }
        *thermal_mask = Some(accepted);
    }
    if let Some(mask) = *thermal_mask {
        if mask > 0 {
            capture!("thermal_junction", {
                let mut p = SensorPacket::new(SensorFunction::ThermalSensors);
                p.set(4, mask);
                t.call(SensorFunction::ThermalSensors, &mut p)?;
                p.validate(SensorFunction::ThermalSensors)?;
                let canonical = name.strip_prefix("NVIDIA ").unwrap_or(name);
                let memory_index = if canonical.starts_with("GeForce RTX 50") {
                    2
                } else if canonical.starts_with("GeForce RTX 40") {
                    7
                } else {
                    9
                };
                for (key, at) in [
                    ("GpuVramTemperature", memory_index),
                    (
                        if canonical.starts_with("GeForce RTX 50") {
                            "GpuCoreTemperature"
                        } else {
                            "GpuHotSpotTemperature"
                        },
                        1,
                    ),
                ] {
                    let raw = p.get(40 + at * 4) as i32;
                    let value = raw as f64 / 256.;
                    ensure!(
                        value > 0. && value < 150.,
                        "Invalid/absent NVIDIA junction temperature"
                    );
                    sample.insert(key, value, "°C", "NVAPI thermal sensors Q8")?;
                }
            });
        }
    }
    capture!("clocks", {
        let p = packet(t, SensorFunction::Clocks)?;
        for (key, domain) in [
            ("GpuFrequency", 0),
            ("GpuMemoryFrequency", 4),
            ("GpuShaderFrequency", 7),
            ("GpuVideoFrequency", 8),
        ] {
            let o = 8 + domain * 8;
            if p.get(o) & 1 != 0 {
                sample.insert(
                    key,
                    p.get(o + 4) as f64 / 1000.,
                    "MHz",
                    "NVAPI current clocks",
                )?;
            }
        }
    });
    capture!("utilization", {
        let p = packet(t, SensorFunction::Utilization)?;
        for (key, domain) in [
            ("GpuUtilization", 0),
            ("GpuMemoryControllerUtilization", 1),
            ("GpuVideoUtilization", 2),
            ("GpuBusUtilization", 3),
        ] {
            let o = 8 + domain * 8;
            if p.get(o) & 1 != 0 {
                let v = p.get(o + 4);
                ensure!(v <= 100, "NVIDIA utilization exceeds 100%");
                sample.insert(key, v as f64, "%", "NVAPI utilization")?;
            }
        }
    });
    capture!("memory", {
        let p = packet(t, SensorFunction::Memory)?;
        let total = p.get_u64(8);
        let free = p.get_u64(40);
        ensure!(
            total > 0 && free <= total,
            "Invalid dedicated NVIDIA memory counters"
        );
        insert_memory(
            &mut sample,
            total,
            total - free,
            "NVAPI dedicated memory bytes",
        )?;
    });
    capture!("fans", {
        let p = packet(t, SensorFunction::Fans)?;
        let count = p.get(4) as usize;
        ensure!(count <= 32, "NVIDIA fan count exceeds ABI");
        for i in 0..count {
            let o = 40 + i * 52;
            let duty = p.get(o + 16);
            ensure!(duty <= 100, "NVIDIA fan duty exceeds 100%");
            sample.fans.push(FanSample {
                id: p.get(o),
                rpm: p.get(o + 4),
                duty_percent: duty,
            });
        }
        if let Some(fan) = sample.fans.first() {
            sample.insert("GpuFanSpeed", fan.rpm as f64, "RPM", "NVAPI fan status")?;
        }
    });
    if !sample.values.contains_key("GpuFanSpeed") {
        capture!("tachometer", {
            let rpm = t.tachometer()?;
            sample.insert("GpuFanSpeed", rpm as f64, "RPM", "NVAPI tachometer")?;
        });
    }
    capture!("power", {
        let p = packet(t, SensorFunction::Power)?;
        let count = p.get(4) as usize;
        ensure!(count <= 4, "NVIDIA power topology count exceeds ABI");
        let mut board = None;
        let mut core = None;
        for i in 0..count {
            let o = 8 + i * 16;
            match p.get(o) {
                0 => core = Some(p.get(o + 8)),
                1 => board = Some(p.get(o + 8)),
                _ => {}
            }
        }
        if let Some(v) = core {
            sample.insert(
                "GpuChipPower",
                v as f64 / 1000.,
                "W",
                "NVAPI GPU power topology",
            )?;
        }
        if let Some(v) = board {
            sample.insert(
                "GpuPower",
                v as f64 / 1000.,
                "W",
                "NVAPI board power topology",
            )?;
        }
    });
    capture!("voltage", {
        let p = packet(t, SensorFunction::Voltage)?;
        let uv = p.get_u64(40);
        ensure!(
            uv > 0 && uv <= 5_000_000,
            "Invalid/absent NVIDIA core voltage"
        );
        sample.insert(
            "GpuVoltage",
            uv as f64 / 1_000_000.,
            "V",
            "NVAPI voltage rails",
        )?;
    });
    for (key, unit, source) in [
        ("GpuCoreTemperature", "°C", "thermal"),
        ("GpuVramTemperature", "°C", "thermal_junction"),
        ("GpuFrequency", "MHz", "clocks"),
        ("GpuUtilization", "%", "utilization"),
        ("GpuVramTotal", "GB", "memory"),
        ("GpuVramUsed", "GB", "memory"),
        ("GpuVramUtilization", "%", "memory"),
        ("GpuPower", "W", "power"),
        ("GpuFanSpeed", "RPM", "fans"),
        ("GpuVoltage", "V", "voltage"),
    ] {
        if !sample.values.contains_key(key) {
            let error = sample
                .errors
                .get(source)
                .cloned()
                .unwrap_or_else(|| "Getter succeeded but did not advertise this sensor".into());
            sample.error(key, unit, &anyhow::anyhow!(error));
        }
    }
    sample
}
pub fn insert_memory(sample: &mut GpuSample, total: u64, used: u64, source: &str) -> Result<()> {
    ensure!(
        total > 0 && used <= total,
        "Invalid NVIDIA memory capacity/usage"
    );
    let gib = 1_073_741_824.;
    sample.insert("GpuVramTotal", total as f64 / gib, "GB", source)?;
    sample.insert("GpuVramUsed", used as f64 / gib, "GB", source)?;
    sample.insert(
        "GpuVramUtilization",
        used as f64 / total as f64 * 100.,
        "%",
        source,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        calls: Vec<SensorFunction>,
        bad_count: bool,
        invalid: bool,
    }
    impl SensorTransport for Fake {
        fn call(&mut self, f: SensorFunction, p: &mut SensorPacket) -> Result<()> {
            self.calls.push(f);
            p.validate(f)?;
            match f {
                SensorFunction::ThermalSettings => {
                    p.set(4, if self.bad_count { 4 } else { 1 });
                    p.set(20, 55);
                    p.set(24, 1);
                }
                SensorFunction::ThermalSensors => {
                    ensure!(p.get(4) <= 7, "unsupported mask");
                    p.set(44, if self.invalid { u32::MAX } else { 62 * 256 });
                    p.set(48, 80 * 256);
                }
                SensorFunction::Clocks => {
                    p.set(8, 1);
                    p.set(12, 2_250_000);
                    p.set(40, 1);
                    p.set(44, 1_750_000);
                }
                SensorFunction::Utilization => {
                    p.set(8, 1);
                    p.set(12, 27);
                }
                SensorFunction::Memory => {
                    p.set(8, 0);
                    p.set(12, 2);
                    p.set(40, 0);
                    p.set(44, 1);
                }
                SensorFunction::Fans => {
                    p.set(4, 1);
                    p.set(40, 9);
                    p.set(44, 2400);
                    p.set(56, 40);
                }
                SensorFunction::Power => {
                    p.set(4, 1);
                    p.set(8, 1);
                    p.set(16, 73_500);
                }
                SensorFunction::Voltage => {
                    p.set(40, 975_000);
                }
            }
            Ok(())
        }
        fn tachometer(&mut self) -> Result<u32> {
            Ok(2000)
        }
    }
    #[test]
    fn exact_abi_units_and_rtx50_junction_are_preserved() {
        let mut f = Fake {
            calls: vec![],
            bad_count: false,
            invalid: false,
        };
        let mut mask = None;
        let s = read(&mut f, 0, "GeForce RTX 5060 Laptop GPU", 42, &mut mask);
        assert_eq!(mask, Some(7));
        for (k, v) in [
            ("GpuCoreTemperature", 62.),
            ("GpuVramTemperature", 80.),
            ("GpuFrequency", 2250.),
            ("GpuMemoryFrequency", 1750.),
            ("GpuVramTotal", 8.),
            ("GpuVramUsed", 4.),
            ("GpuVramUtilization", 50.),
            ("GpuPower", 73.5),
            ("GpuVoltage", 0.975),
            ("GpuFanSpeed", 2400.),
        ] {
            assert_eq!(s.values[k].value, Some(v), "{k}");
        }
        assert_eq!(s.requested_at_tick_ms, 42);
        assert_eq!(s.fans[0].id, 9);
        let n = f.calls.len();
        read(&mut f, 0, "GeForce RTX 5060 Laptop GPU", 43, &mut mask);
        assert_eq!(f.calls.len() - n, 8);
    }
    #[test]
    fn malformed_thermal_does_not_erase_independent_good_metrics() {
        let mut f = Fake {
            calls: vec![],
            bad_count: true,
            invalid: true,
        };
        let s = read(
            &mut f,
            0,
            "NVIDIA GeForce RTX 5060 Laptop GPU",
            1,
            &mut Some(7),
        );
        assert!(s.values["GpuCoreTemperature"].value.is_none());
        assert_eq!(s.values["GpuFrequency"].value, Some(2250.));
        assert!(s.errors.contains_key("thermal"));
    }
    #[test]
    fn layouts_have_real_alignment_and_no_clock_setters() {
        for f in [
            SensorFunction::Memory,
            SensorFunction::Fans,
            SensorFunction::Voltage,
        ] {
            let mut p = SensorPacket::new(f);
            assert_eq!(p.as_mut_ptr() as usize % 8, 0);
            p.validate(f).unwrap();
            p.set(0, 0);
            assert!(p.validate(f).is_err());
        }
        let mut s = GpuSample::new(0, "fake", 0);
        assert!(insert_memory(&mut s, 10, 11, "fake").is_err());
        assert!(s.values.is_empty());
    }
}
