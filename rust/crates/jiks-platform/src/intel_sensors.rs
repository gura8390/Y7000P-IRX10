//! Native, opt-in CPU sensor reads through the already-running PawnIO driver.
//! No driver service is installed/started. Default construction and metadata
//! projection perform no I/O. Only the fixed signed IntelMSR module is loaded
//! into this handle; only a fixed set of read MSRs can be executed.
//!
//! Register decoding follows Intel SDM and the original LLT LHM fork's
//! IntelCpu.cs (ad760baba99c7c4ad6c86c5c57fb3ba766a7ee71). LHM is MPL-2.0;
//! this file's corresponding source is distributed under MPL-2.0 as well.
//! SPDX-License-Identifier: MPL-2.0
use crate::{domain::monotonic_ms, ownership::wide, wmi_native::WmiFacade};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    },
    System::{
        IO::DeviceIoControl,
        Threading::{
            ALL_PROCESSOR_GROUPS, GetActiveProcessorCount, GetActiveProcessorGroupCount,
            GetCurrentThread, SetThreadAffinityMask,
        },
    },
};

const MODULE: &[u8] = include_bytes!("../resources/pawnio/IntelMSR.bin");
const IOCTL_LOAD: u32 = (41394u32 << 16) | (0x821 << 2);
const IOCTL_EXECUTE: u32 = (41394u32 << 16) | (0x841 << 2);
const THERMAL_TARGET: u32 = 0x1a2;
const CORE_THERMAL: u32 = 0x19c;
const PACKAGE_THERMAL: u32 = 0x1b1;
const PERF_STATUS: u32 = 0x198;
const RAPL_UNIT: u32 = 0x606;
const PACKAGE_ENERGY: u32 = 0x611;
const CORE_ENERGY: u32 = 0x639;
const DRAM_ENERGY: u32 = 0x619;
const PLATFORM_ENERGY: u32 = 0x64d;
const GPU_ENERGY: u32 = 0x641;
const ALLOWED: [u32; 10] = [
    THERMAL_TARGET,
    CORE_THERMAL,
    PACKAGE_THERMAL,
    PERF_STATUS,
    RAPL_UNIT,
    PACKAGE_ENERGY,
    CORE_ENERGY,
    DRAM_ENERGY,
    PLATFORM_ENERGY,
    GPU_ENERGY,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoreKind {
    Performance,
    Efficiency,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreAddress {
    pub physical_id: u32,
    pub logical_processor: u32,
    pub kind: CoreKind,
    pub bus_frequency_mhz: Option<u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreSample {
    pub address: CoreAddress,
    pub temperature_celsius: Option<f64>,
    pub distance_to_tjmax_celsius: Option<f64>,
    pub tjmax_celsius: Option<f64>,
    pub ratio_frequency_mhz: Option<f64>,
    pub requested_vid_volts: Option<f64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntelSample {
    pub requested_at_tick_ms: u64,
    pub completed_at_tick_ms: u64,
    pub cores: Vec<CoreSample>,
    pub package_temperature_celsius: Option<f64>,
    pub maximum_core_temperature_celsius: Option<f64>,
    pub average_core_temperature_celsius: Option<f64>,
    pub conservative_temperature_celsius: Option<f64>,
    pub package_power_watts: Option<f64>,
    pub core_power_watts: Option<f64>,
    pub dram_power_watts: Option<f64>,
    pub platform_power_watts: Option<f64>,
    pub power_interval_start_tick_ms: Option<u64>,
    pub core_vid_average_volts: Option<f64>,
    pub core_vid_maximum_volts: Option<f64>,
    pub source: String,
    pub errors: Vec<String>,
}
impl IntelSample {
    /// A control sample uses the original request tick for freshness. Reusing a
    /// cached snapshot cannot reset its age. Missing both sources is an error.
    pub fn temperature_sample(&self) -> Result<crate::TemperatureSample> {
        let value = self
            .conservative_temperature_celsius
            .context("No valid core/package temperature")?;
        ensure!(
            value > 0.0 && value < 150.0 && value.is_finite(),
            "Invalid conservative CPU temperature"
        );
        ensure!(
            self.completed_at_tick_ms >= self.requested_at_tick_ms,
            "Invalid sensor timestamps"
        );
        Ok(crate::TemperatureSample {
            celsius: value.ceil() as u32,
            requested_at_tick_ms: self.requested_at_tick_ms,
            completed_at_tick_ms: self.completed_at_tick_ms,
            source: format!(
                "PawnIO Intel DTS max(package={:?}, valid-core-max={:?})",
                self.package_temperature_celsius, self.maximum_core_temperature_celsius
            ),
        })
    }
}
pub trait MsrReader {
    fn read(&mut self, core: &CoreAddress, index: u32) -> Result<u64>;
    fn tick_ms(&self) -> u64;
}
#[derive(Debug, Clone, Copy)]
struct EnergyPoint {
    value: u32,
    tick: u64,
}
#[derive(Default)]
pub struct SensorHistory {
    energy: BTreeMap<u32, EnergyPoint>,
}

pub fn decode_temperature(target: u64, status: u64) -> Option<(f64, f64, f64)> {
    let tj = ((target >> 16) & 0xff) as f64;
    if status & (1 << 31) == 0 || !(40.0..=125.0).contains(&tj) {
        return None;
    }
    let distance = ((status >> 16) & 0x7f) as f64;
    let value = tj - distance;
    (0.0..=125.0)
        .contains(&value)
        .then_some((value, distance, tj))
}
pub fn decode_vid(status: u64) -> Option<f64> {
    // Upper word is the requested VID. The lower word includes ratio/status;
    // treating it as a fallback VID manufactures a voltage on unsupported CPUs.
    let bits = (status >> 32) & 0xffff;
    (bits > 0)
        .then_some(bits as f64 / 8192.0)
        .filter(|v| (0.0..=3.0).contains(v))
}
pub fn energy_delta_watts(
    previous: u32,
    current: u32,
    unit_exponent: u32,
    elapsed_ms: u64,
) -> Option<f64> {
    if !(10..=2000).contains(&elapsed_ms) || unit_exponent > 31 {
        return None;
    }
    // A 32-bit counter wraps modulo 2^32. A longer interval can wrap repeatedly;
    // do not present an old baseline as a fresh power measurement after resume.
    let joules = current.wrapping_sub(previous) as f64 * 2.0f64.powi(-(unit_exponent as i32));
    let watts = joules * 1000.0 / elapsed_ms as f64;
    watts
        .is_finite()
        .then_some(watts)
        .filter(|v| (0.0..=10000.0).contains(v))
}
fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let v = values.filter(|v| v.is_finite()).collect::<Vec<_>>();
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}
fn maximum(values: impl Iterator<Item = f64>) -> Option<f64> {
    values.filter(|v| v.is_finite()).reduce(f64::max)
}
pub fn sample_with(
    reader: &mut impl MsrReader,
    cores: &[CoreAddress],
    history: &mut SensorHistory,
) -> Result<IntelSample> {
    ensure!(
        !cores.is_empty() && cores.len() <= 64,
        "Physical CPU topology is empty/unsupported"
    );
    let requested = reader.tick_ms();
    let mut errors = Vec::new();
    let mut samples = Vec::new();
    for core in cores {
        let target = reader.read(core, THERMAL_TARGET);
        let thermal = reader.read(core, CORE_THERMAL);
        let perf = reader.read(core, PERF_STATUS);
        let temperatures = target
            .as_ref()
            .ok()
            .zip(thermal.as_ref().ok())
            .and_then(|(t, s)| decode_temperature(*t, *s));
        for (name, result) in [
            ("TjMax", &target),
            ("DTS", &thermal),
            ("PERF_STATUS", &perf),
        ] {
            if let Err(e) = result {
                errors.push(format!("core{} {name}: {e:#}", core.physical_id));
            }
        }
        let ratio = perf
            .as_ref()
            .ok()
            .map(|v| (v >> 8) & 0xff)
            .filter(|v| *v > 0);
        samples.push(CoreSample {
            address: core.clone(),
            temperature_celsius: temperatures.map(|v| v.0),
            distance_to_tjmax_celsius: temperatures.map(|v| v.1),
            tjmax_celsius: temperatures.map(|v| v.2),
            ratio_frequency_mhz: ratio
                .zip(core.bus_frequency_mhz)
                .map(|(r, b)| r as f64 * b as f64),
            requested_vid_volts: perf.ok().and_then(decode_vid),
        });
    }
    let first = &cores[0];
    let package = reader
        .read(first, THERMAL_TARGET)
        .ok()
        .zip(reader.read(first, PACKAGE_THERMAL).ok())
        .and_then(|(t, s)| decode_temperature(t, s))
        .map(|v| v.0);
    let mut powers = BTreeMap::new();
    let mut interval_start = None;
    let exponent = reader
        .read(first, RAPL_UNIT)
        .ok()
        .map(|v| ((v >> 8) & 31) as u32);
    for id in [PACKAGE_ENERGY, CORE_ENERGY, DRAM_ENERGY, PLATFORM_ENERGY] {
        let point = match reader.read(first, id) {
            Ok(v) => EnergyPoint {
                value: v as u32,
                tick: reader.tick_ms(),
            },
            Err(e) => {
                history.energy.remove(&id);
                errors.push(format!("energy {id:#x}: {e:#}"));
                continue;
            }
        };
        if let Some(prior) = history.energy.insert(id, point) {
            if let Some(power) = exponent.and_then(|e| {
                point
                    .tick
                    .checked_sub(prior.tick)
                    .and_then(|dt| energy_delta_watts(prior.value, point.value, e, dt))
            }) {
                powers.insert(id, power);
                interval_start =
                    Some(interval_start.map_or(prior.tick, |v: u64| v.min(prior.tick)));
            }
        }
    }
    let max = maximum(samples.iter().filter_map(|c| c.temperature_celsius));
    Ok(IntelSample{requested_at_tick_ms:requested,completed_at_tick_ms:reader.tick_ms(),maximum_core_temperature_celsius:max,average_core_temperature_celsius:mean(samples.iter().filter_map(|c|c.temperature_celsius)),conservative_temperature_celsius:maximum(max.into_iter().chain(package)),package_temperature_celsius:package,package_power_watts:powers.get(&PACKAGE_ENERGY).copied(),core_power_watts:powers.get(&CORE_ENERGY).copied(),dram_power_watts:powers.get(&DRAM_ENERGY).copied(),platform_power_watts:powers.get(&PLATFORM_ENERGY).copied(),power_interval_start_tick_ms:interval_start,core_vid_average_volts:mean(samples.iter().filter_map(|c|c.requested_vid_volts)),core_vid_maximum_volts:maximum(samples.iter().filter_map(|c|c.requested_vid_volts)),cores:samples,errors,source:"PawnIO 0.2.11 IntelMSR readonly; Intel DTS core/package, RAPL 32-bit energy deltas, requested VID (not measured Vcore), PERF_STATUS ratio × CPUID bus MHz (not effective clock)".into()})
}

fn read_packet(index: u32) -> Result<[u8; 40]> {
    ensure!(
        ALLOWED.contains(&index),
        "Register is not in the sensor-only read allowlist"
    );
    let mut p = [0u8; 40];
    let name = b"ioctl_read_msr";
    p[..name.len()].copy_from_slice(name);
    p[32..].copy_from_slice(&u64::from(index).to_le_bytes());
    Ok(p)
}
struct PawnMsr {
    handle: HANDLE,
}
impl Drop for PawnMsr {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}
impl PawnMsr {
    fn open() -> Result<Self> {
        // Deliberately no service installation/start or broad device probing.
        let wmi = WmiFacade::connect("ROOT\\CIMV2")?;
        let state: Vec<serde_json::Value> =
            wmi.query("SELECT State,Started FROM Win32_SystemDriver WHERE Name='PawnIO'")?;
        ensure!(
            state.len() == 1
                && state[0].get("State").and_then(serde_json::Value::as_str) == Some("Running")
                && state[0].get("Started").and_then(serde_json::Value::as_bool) == Some(true),
            "PawnIO is not already running; no driver was installed/started"
        );
        ensure!(
            crate::read_machine_identity()?.is_supported(),
            "Exact supported machine identity is required for Intel sensor adapter"
        );
        let path = wide("\\\\?\\GLOBALROOT\\Device\\PawnIO")?;
        // Original PawnIo.cs uses numeric FileAccess.ReadWrite=3, not generic
        // access bits. Preserve this vendor device ABI exactly.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                3,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        ensure!(
            handle != INVALID_HANDLE_VALUE,
            "Open existing PawnIO: {}",
            std::io::Error::last_os_error()
        );
        let session = Self { handle };
        let mut returned = 0;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_LOAD,
                MODULE.as_ptr().cast(),
                MODULE.len() as u32,
                ptr::null_mut(),
                0,
                &mut returned,
                ptr::null_mut(),
            )
        };
        ensure!(
            ok != 0,
            "Load fixed signed IntelMSR module into existing handle: {}",
            std::io::Error::last_os_error()
        );
        Ok(session)
    }
}
struct Affinity(usize);
impl Affinity {
    fn enter(logical: u32) -> Result<Self> {
        ensure!(logical < usize::BITS, "Affinity overflow");
        let prior = unsafe { SetThreadAffinityMask(GetCurrentThread(), 1usize << logical) };
        ensure!(
            prior != 0,
            "Set sensor thread affinity: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(prior))
    }
}
impl Drop for Affinity {
    fn drop(&mut self) {
        unsafe {
            SetThreadAffinityMask(GetCurrentThread(), self.0);
        }
    }
}
impl MsrReader for PawnMsr {
    fn tick_ms(&self) -> u64 {
        monotonic_ms()
    }
    fn read(&mut self, core: &CoreAddress, index: u32) -> Result<u64> {
        let input = read_packet(index)?;
        let _affinity = Affinity::enter(core.logical_processor)?;
        let mut output = 0u64;
        let mut returned = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_EXECUTE,
                input.as_ptr().cast(),
                input.len() as u32,
                (&mut output as *mut u64).cast(),
                8,
                &mut returned,
                ptr::null_mut(),
            )
        };
        ensure!(
            ok != 0,
            "PawnIO MSR {index:#x}: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            returned == 8,
            "PawnIO returned a truncated/malformed MSR response ({returned} bytes)"
        );
        Ok(output)
    }
}
fn topology() -> Result<Vec<CoreAddress>> {
    ensure!(
        unsafe { GetActiveProcessorGroupCount() } == 1,
        "Multi-group topology has not been validated for this exact adapter"
    );
    let count = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
    ensure!(
        (1..=64).contains(&count),
        "Unsupported logical processor count"
    );
    let mut cores = Vec::new();
    let mut seen = BTreeSet::new();
    for logical in 0..count {
        let _affinity = Affinity::enter(logical)?;
        let cpuid = |leaf, sub| std::arch::x86_64::__cpuid_count(leaf, sub);
        let vendor = cpuid(0, 0);
        let mut name = Vec::new();
        for v in [vendor.ebx, vendor.edx, vendor.ecx] {
            name.extend_from_slice(&v.to_le_bytes());
        }
        ensure!(
            name == b"GenuineIntel" && vendor.eax >= 0x1a,
            "Intel hybrid CPUID metadata is unavailable"
        );
        let version = cpuid(1, 0).eax;
        let family = (version >> 8) & 15;
        let model = ((version >> 4) & 15) | ((version >> 12) & 0xf0);
        ensure!(
            family == 6 && matches!(model, 0xb7 | 0xba | 0xbf),
            "Intel family/model is outside the reviewed Raptor Lake adapter"
        );
        let smt = cpuid(0xb, 0);
        ensure!(smt.ebx > 0, "Missing x2APIC topology");
        let shift = smt.eax & 31;
        let physical_id = smt.edx >> shift;
        if !seen.insert(physical_id) {
            continue;
        }
        let kind = match cpuid(0x1a, 0).eax >> 24 {
            0x40 => CoreKind::Performance,
            0x20 => CoreKind::Efficiency,
            _ => CoreKind::Unknown,
        };
        let bus = cpuid(0x16, 0).ecx & 0xffff;
        cores.push(CoreAddress {
            physical_id,
            logical_processor: logical,
            kind,
            bus_frequency_mhz: (bus > 0 && bus <= 1000).then_some(bus),
        });
    }
    Ok(cores)
}
#[derive(Default)]
pub struct IntelSampler {
    native: Option<PawnMsr>,
    cores: Vec<CoreAddress>,
    history: SensorHistory,
}
impl IntelSampler {
    /// Only PP1 energy/unit getters, never CPU temperature, power-limit, or OC.
    pub fn gpu_power(&mut self) -> Result<Option<f64>> {
        if self.native.is_none() {
            self.cores = topology()?;
            self.native = Some(PawnMsr::open()?);
        }
        gpu_power_with(
            self.native.as_mut().context("Intel module missing")?,
            self.cores.first().context("Intel CPU address missing")?,
            &mut self.history,
        )
    }
    /// Explicit opt-in only. Never call from startup discovery/doctor/benchmark.
    pub fn sample(&mut self) -> Result<IntelSample> {
        if self.native.is_none() {
            self.cores = topology()?;
            self.native = Some(PawnMsr::open()?);
        }
        sample_with(
            self.native
                .as_mut()
                .context("Intel module session missing")?,
            &self.cores,
            &mut self.history,
        )
    }
}
pub fn gpu_power_with(
    reader: &mut impl MsrReader,
    core: &CoreAddress,
    history: &mut SensorHistory,
) -> Result<Option<f64>> {
    let exponent = ((reader.read(core, RAPL_UNIT)? >> 8) & 31) as u32;
    let value = reader.read(core, GPU_ENERGY)? as u32;
    let point = EnergyPoint {
        value,
        tick: reader.tick_ms(),
    };
    Ok(history.energy.insert(GPU_ENERGY, point).and_then(|prior| {
        point
            .tick
            .checked_sub(prior.tick)
            .and_then(|dt| energy_delta_watts(prior.value, point.value, exponent, dt))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        values: BTreeMap<(u32, u32), u64>,
        tick: u64,
        reads: Vec<u32>,
    }
    impl MsrReader for Fake {
        fn tick_ms(&self) -> u64 {
            self.tick
        }
        fn read(&mut self, c: &CoreAddress, i: u32) -> Result<u64> {
            self.reads.push(i);
            self.values
                .get(&(c.physical_id, i))
                .copied()
                .context("unsupported fake MSR")
        }
    }
    fn core(id: u32, kind: CoreKind) -> CoreAddress {
        CoreAddress {
            physical_id: id,
            logical_processor: id,
            kind,
            bus_frequency_mhz: Some(100),
        }
    }
    #[test]
    fn fixed_packet_cannot_encode_a_setter_or_power_limit_register() {
        assert!(read_packet(GPU_ENERGY).is_ok());
        let p = read_packet(0x19c).unwrap();
        assert_eq!(&p[..15], b"ioctl_read_msr\0");
        assert_eq!(u64::from_le_bytes(p[32..40].try_into().unwrap()), 0x19c);
        for id in [0x610, 0x150, 0x1a4] {
            assert!(read_packet(id).is_err())
        }
        assert_eq!(IOCTL_LOAD, 0xa1b22084);
        assert_eq!(IOCTL_EXECUTE, 0xa1b22104);
        assert_eq!(MODULE.len(), 5324);
    }
    #[test]
    fn digital_thermal_valid_bit_tjmax_and_invalid_distance_are_checked() {
        assert_eq!(
            decode_temperature(100 << 16, (1 << 31) | (12 << 16)),
            Some((88.0, 12.0, 100.0))
        );
        assert!(decode_temperature(100 << 16, 12 << 16).is_none());
        assert!(decode_temperature(0, 1 << 31).is_none());
        assert!(decode_temperature(100 << 16, (1 << 31) | (127 << 16)).is_none());
    }
    #[test]
    fn vid_never_uses_frequency_status_as_fallback_voltage() {
        assert_eq!(decode_vid(8192u64 << 32), Some(1.0));
        assert!(decode_vid(0x00002c00).is_none());
    }
    #[test]
    fn rapl_wrap_monotonic_interval_and_resume_reset_are_exact() {
        assert_eq!(energy_delta_watts(u32::MAX - 9, 10, 0, 1000), Some(20.0));
        assert_eq!(energy_delta_watts(0, 1638400, 14, 1000), Some(100.0));
        for interval in [0, 9, 2001, u64::MAX] {
            assert!(energy_delta_watts(0, 1, 14, interval).is_none());
        }
    }
    #[test]
    fn gpu_pp1_power_reads_no_temperature_or_limit_registers() {
        let core = core(0, CoreKind::Performance);
        let mut fake = Fake {
            values: BTreeMap::from([((0, RAPL_UNIT), 14 << 8), ((0, GPU_ENERGY), 0)]),
            tick: 100,
            reads: vec![],
        };
        let mut history = SensorHistory::default();
        assert_eq!(
            gpu_power_with(&mut fake, &core, &mut history).unwrap(),
            None
        );
        fake.values.insert((0, GPU_ENERGY), 16384);
        fake.tick = 200;
        assert_eq!(
            gpu_power_with(&mut fake, &core, &mut history).unwrap(),
            Some(10.)
        );
        assert!(
            fake.reads
                .iter()
                .all(|id| [RAPL_UNIT, GPU_ENERGY].contains(id))
        );
    }
    #[test]
    fn failed_core_does_not_dilute_average_or_manufacture_power() {
        let cores = vec![
            core(0, CoreKind::Performance),
            core(1, CoreKind::Efficiency),
        ];
        let mut f = Fake {
            tick: 100,
            reads: vec![],
            values: BTreeMap::from([
                ((0, THERMAL_TARGET), 100 << 16),
                ((0, CORE_THERMAL), (1 << 31) | (10 << 16)),
                ((0, PACKAGE_THERMAL), (1 << 31) | (12 << 16)),
                ((0, PERF_STATUS), (8192u64 << 32) | (44 << 8)),
                ((0, RAPL_UNIT), 14 << 8),
                ((0, PACKAGE_ENERGY), 10000),
            ]),
        };
        let mut h = SensorHistory::default();
        let a = sample_with(&mut f, &cores, &mut h).unwrap();
        assert_eq!(a.average_core_temperature_celsius, Some(90.0));
        assert_eq!(a.conservative_temperature_celsius, Some(90.0));
        assert_eq!(a.package_temperature_celsius, Some(88.0));
        let control = a.temperature_sample().unwrap();
        assert_eq!(control.celsius, 90);
        assert_eq!(control.requested_at_tick_ms, 100);
        assert_eq!(a.cores[0].ratio_frequency_mhz, Some(4400.0));
        assert!(a.package_power_watts.is_none());
        assert!(a.cores[1].temperature_celsius.is_none());
        f.tick = 1100;
        f.values.insert((0, PACKAGE_ENERGY), 10000 + 1638400);
        let b = sample_with(&mut f, &cores, &mut h).unwrap();
        assert_eq!(b.package_power_watts, Some(100.0));
        assert!(f.reads.iter().all(|id| ALLOWED.contains(id)));
    }
}
