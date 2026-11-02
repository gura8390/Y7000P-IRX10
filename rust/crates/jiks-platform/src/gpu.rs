//! GPU OC protocol port. No DLL load, enumeration, probing or writes on import.
//! Private ABI source: LLT.NvAPIWrapper.Net 1.1.28-pre.42 repository commit
//! 0bc0ff2ef72dec39a8882c0d2b4259bc0c6448bc, Native/GPUApi.Performance.cs,
//! Native/GPU/Structures and Helpers/FunctionId.cs. Public bootstrap IDs are
//! additionally checked against NVIDIA/nvapi nvapi_interface.h (2026-11-02).
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{ffi::c_void, fmt};

pub const ABI_SOURCE: &str = "https://github.com/LenovoLegionToolkit-Team/NvAPIWrapper/tree/0bc0ff2ef72dec39a8882c0d2b4259bc0c6448bc";
pub const MAX_PHYSICAL_GPUS: usize = 64;
pub const SHORT_STRING_BYTES: usize = 64;
pub const MAX_CLOCKS: usize = 32;
pub const MAX_DELTAS: usize = 255;
pub const MAX_POINTS: usize = 254;
pub const MASK_BYTES: usize = 4 + 8 * 4 + 8 * 4 + 255 * 24; // 6188
pub const RANGES_BYTES: usize = 4 + 4 + 8 * 4 + 32 * 72; // 2344
pub const TABLE_BYTES: usize = 4 + 8 * 4 + 8 * 4 + 255 * 36; // 9248
pub const POINTS_BYTES: usize = 4 + 17 * 4 + 254 * 28 + 6 * 4; // 7208
pub const LOCK_BYTES: usize = 12 + 32 * 24; // 780
pub const PSTATE_V1_BYTES: usize = 20 + 16 * (8 + 8 * 44 + 4 * 24); // 7316
pub const PSTATE_V2_BYTES: usize = PSTATE_V1_BYTES + 4 + 4 * 24; // 7416

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum NvFunction {
    Initialize = 0x0150e828,
    // The fork incorrectly labels 0xd7c61344 (PCF_Mode1Unload) as NvAPI_Unload.
    // Use NVIDIA's public interface table, never the PCF entry.
    Unload = 0xd22bdd7e,
    EnumPhysicalGpus = 0xe5ac921f,
    GetFullName = 0xceee8e9f,
    GetPstates20 = 0x6ff81213,
    SetPstates20 = 0x0f4dae6b,
    EnableOverclockedPstates = 0xb23b70ee,
    GetClockBoostMask = 0x507b4b59,
    GetClockBoostRanges = 0x64b43a6a,
    GetClockBoostTable = 0x23f1b133,
    SetClockBoostTable = 0x0733e009,
    GetClientClkVfPointsStatus = 0x21537ad4,
    GetClockBoostLock = 0xe440b867,
    SetClockBoostLock = 0x39442cfb,
}
impl NvFunction {
    pub fn is_write(self) -> bool {
        matches!(
            self,
            Self::SetPstates20
                | Self::EnableOverclockedPstates
                | Self::SetClockBoostTable
                | Self::SetClockBoostLock
        )
    }
    pub fn structured(self) -> bool {
        !matches!(
            self,
            Self::Initialize
                | Self::Unload
                | Self::EnumPhysicalGpus
                | Self::GetFullName
                | Self::EnableOverclockedPstates
        )
    }
}
/// Status errors must retain their code so version negotiation never treats an
/// asleep/invalidated GPU as an incompatible structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvStatus(pub i32);
impl fmt::Display for NvStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NVAPI status {}", self.0)
    }
}
impl std::error::Error for NvStatus {}
impl NvStatus {
    pub const INCOMPATIBLE_STRUCTURE_VERSION: i32 = -9;
    pub fn checked(status: i32) -> Result<()> {
        if status == 0 {
            Ok(())
        } else {
            Err(Self(status).into())
        }
    }
}
fn incompatible(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<NvStatus>()
        .is_some_and(|s| s.0 == NvStatus::INCOMPATIBLE_STRUCTURE_VERSION)
}

/// All verified structures contain DWORD fields/arrays only. Vec<u32> keeps the
/// native pointer aligned; no repr(Rust) structs are passed across this ABI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbiBuffer {
    words: Vec<u32>,
}
impl AbiBuffer {
    pub fn new(bytes: usize, version: u32) -> Self {
        assert!(bytes > 0 && bytes.is_multiple_of(4) && bytes <= u16::MAX as usize);
        let mut value = Self {
            words: vec![0; bytes / 4],
        };
        value.words[0] = (version << 16) | bytes as u32;
        value
    }
    pub fn len_bytes(&self) -> usize {
        self.words.len() * 4
    }
    pub fn as_mut_ptr(&mut self) -> *mut c_void {
        self.words.as_mut_ptr().cast()
    }
    pub fn words(&self) -> &[u32] {
        &self.words
    }
    pub fn words_mut(&mut self) -> &mut [u32] {
        &mut self.words
    }
    pub fn version(&self) -> u32 {
        self.words[0] >> 16
    }
    pub fn get(&self, offset: usize) -> u32 {
        self.words[offset / 4]
    }
    pub fn set(&mut self, offset: usize, value: u32) {
        self.words[offset / 4] = value;
    }
    pub fn get_i32(&self, offset: usize) -> i32 {
        self.get(offset) as i32
    }
    pub fn set_i32(&mut self, offset: usize, value: i32) {
        self.set(offset, value as u32);
    }
    pub fn validate(&self, bytes: usize, version: u32) -> Result<()> {
        ensure!(
            self.len_bytes() == bytes && self.words[0] == (version << 16) | bytes as u32,
            "NVAPI response size/version mismatch"
        );
        Ok(())
    }
    pub fn to_le_bytes(&self) -> Vec<u8> {
        self.words
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect()
    }
}
/// Transport is bound to one selected physical GPU and owned by the native
/// worker. call handles only (physicalHandle, void*) functions. Bootstrap and
/// EnumPhysicalGpus/GetFullName have different signatures and are NOT call-able
/// with this trait. EnableOverclockedPstates uses an empty payload sentinel.
pub trait NvApiTransport {
    fn call(&mut self, function: NvFunction, payload: &mut AbiBuffer) -> Result<()>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct OverclockInfo {
    #[serde(rename = "CoreDeltaMhz")]
    pub core_delta_mhz: i32,
    #[serde(rename = "MemoryDeltaMhz")]
    pub memory_delta_mhz: i32,
    #[serde(rename = "VoltageLockMv")]
    pub voltage_lock_mv: i32,
    #[serde(rename = "VoltageCapMv")]
    pub voltage_cap_mv: i32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OffsetRange {
    pub min_mhz: i32,
    pub max_mhz: i32,
    pub driver_reported: bool,
}
impl OffsetRange {
    fn fallback(memory: bool) -> Self {
        Self {
            min_mhz: if memory { -3000 } else { -500 },
            max_mhz: if memory { 3000 } else { 500 },
            driver_reported: false,
        }
    }
    fn clamp(self, mhz: i32) -> i32 {
        mhz.clamp(self.min_mhz, self.max_mhz)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockState {
    pub domain: u32,
    pub delta_khz: i32,
    pub min_khz: i32,
    pub max_khz: i32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pstates {
    pub raw: AbiBuffer,
    pub p0: Vec<ClockState>,
}
impl Pstates {
    pub fn decode(raw: AbiBuffer) -> Result<Self> {
        let version = raw.version();
        ensure!((1..=3).contains(&version), "Unknown Pstate version");
        raw.validate(
            if version == 1 {
                PSTATE_V1_BYTES
            } else {
                PSTATE_V2_BYTES
            },
            version,
        )?;
        let states = raw.get(8) as usize;
        let clocks = raw.get(12) as usize;
        let voltages = raw.get(16) as usize;
        ensure!(
            states <= 16 && clocks <= 8 && voltages <= 4,
            "NVAPI Pstate count exceeds ABI arrays"
        );
        let mut p0 = Vec::new();
        let mut found = false;
        for state in 0..states {
            let start = 20 + state * 456;
            if raw.get(start) != 0 {
                continue;
            }
            ensure!(!found, "Duplicate P0 state");
            found = true;
            for clock in 0..clocks {
                let base = start + 8 + clock * 44;
                let value = ClockState {
                    domain: raw.get(base),
                    delta_khz: raw.get_i32(base + 12),
                    min_khz: raw.get_i32(base + 16),
                    max_khz: raw.get_i32(base + 20),
                };
                ensure!(
                    !p0.iter().any(|c: &ClockState| c.domain == value.domain),
                    "Duplicate P0 clock domain"
                );
                p0.push(value);
            }
        }
        Ok(Self { raw, p0 })
    }
    pub fn range(&self, domain: u32) -> OffsetRange {
        let Some(clock) = self.p0.iter().find(|c| c.domain == domain) else {
            return OffsetRange::fallback(domain == 4);
        };
        let min = clock.min_khz / 1000;
        let max = clock.max_khz / 1000;
        if clock.min_khz == 0 && clock.max_khz == 0 || min > max {
            OffsetRange::fallback(domain == 4)
        } else {
            OffsetRange {
                min_mhz: min,
                max_mhz: max,
                driver_reported: true,
            }
        }
    }
}
pub fn pstate_write(core_khz: i32, memory_khz: i32) -> AbiBuffer {
    let mut p = AbiBuffer::new(PSTATE_V1_BYTES, 1);
    p.set(8, 1);
    p.set(12, 2);
    p.set(28, 0);
    p.set_i32(40, core_khz);
    p.set(72, 4);
    p.set_i32(84, memory_khz);
    p
}
pub fn read_pstates(transport: &mut impl NvApiTransport) -> Result<Pstates> {
    for version in [3, 2, 1] {
        let mut raw = AbiBuffer::new(
            if version == 1 {
                PSTATE_V1_BYTES
            } else {
                PSTATE_V2_BYTES
            },
            version,
        );
        match transport.call(NvFunction::GetPstates20, &mut raw) {
            Ok(()) => return Pstates::decode(raw),
            Err(error) if incompatible(&error) => continue,
            Err(error) => return Err(error),
        }
    }
    bail!("No compatible Pstates20 ABI")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveRange {
    pub domain: u32,
    pub min_khz: i32,
    pub max_khz: i32,
    pub first: usize,
    pub last: usize,
}
pub fn decode_ranges(raw: &AbiBuffer) -> Result<Vec<CurveRange>> {
    raw.validate(RANGES_BYTES, 1)?;
    let count = raw.get(4).count_ones() as usize;
    ensure!(count <= MAX_CLOCKS, "Invalid clock domain mask");
    let mut result = Vec::new();
    for index in 0..count {
        let start = 40 + index * 72;
        let indices = raw.get(start + 48);
        let range = CurveRange {
            domain: raw.get(start + 4),
            max_khz: raw.get_i32(start + 40),
            min_khz: raw.get_i32(start + 44),
            first: (indices & 255) as usize,
            last: ((indices >> 8) & 255) as usize,
        };
        ensure!(
            range.first <= range.last && range.min_khz <= range.max_khz,
            "Invalid V/F curve range"
        );
        ensure!(
            !result.iter().any(|r: &CurveRange| r.domain == range.domain),
            "Duplicate V/F curve domain"
        );
        result.push(range);
    }
    Ok(result)
}
fn read_mask(transport: &mut impl NvApiTransport) -> Result<AbiBuffer> {
    let mut mask = AbiBuffer::new(MASK_BYTES, 1);
    transport.call(NvFunction::GetClockBoostMask, &mut mask)?;
    mask.validate(MASK_BYTES, 1)?;
    Ok(mask)
}
fn curve_request(
    transport: &mut impl NvApiTransport,
    bytes: usize,
    version: u32,
) -> Result<AbiBuffer> {
    let mask = read_mask(transport)?;
    let mut value = AbiBuffer::new(bytes, version);
    value.words_mut()[1..17].copy_from_slice(&mask.words()[1..17]);
    Ok(value)
}
fn read_table(transport: &mut impl NvApiTransport, version: u32) -> Result<AbiBuffer> {
    let mut table = curve_request(transport, TABLE_BYTES, version)?;
    transport.call(NvFunction::GetClockBoostTable, &mut table)?;
    table.validate(TABLE_BYTES, version)?;
    Ok(table)
}
fn read_points(transport: &mut impl NvApiTransport, version: u32) -> Result<AbiBuffer> {
    let mut points = curve_request(transport, POINTS_BYTES, version)?;
    transport.call(NvFunction::GetClientClkVfPointsStatus, &mut points)?;
    points.validate(POINTS_BYTES, version)?;
    Ok(points)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurvePoint {
    pub frequency_khz: u32,
    pub voltage_uv: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurveSnapshot {
    pub version: u32,
    pub ranges: Vec<CurveRange>,
    pub points: Vec<CurvePoint>,
    pub table: AbiBuffer,
}
impl CurveSnapshot {
    pub fn graphics(&self) -> Option<CurveRange> {
        self.ranges
            .iter()
            .copied()
            .find(|r| r.domain == 0 && r.last > 0)
    }
    pub fn memory(&self) -> Option<CurveRange> {
        self.ranges.iter().copied().find(|r| r.domain == 4)
    }
    fn delta(&self, index: usize) -> i32 {
        self.table.get_i32(68 + index * 36 + 20)
    }
    fn settable(&self, index: usize) -> bool {
        self.table.get(68 + index * 36) == 0
    }
    fn stock(&self, index: usize) -> i64 {
        i64::from(self.points[index].frequency_khz) - i64::from(self.delta(index))
    }
    fn first_delta(&self, range: CurveRange) -> Option<i32> {
        (range.first..=range.last.min(MAX_DELTAS - 1))
            .find(|i| self.settable(*i))
            .map(|i| self.delta(i) / 1000)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=2).contains(&self.version),
            "Unsupported private curve ABI version"
        );
        self.table.validate(TABLE_BYTES, self.version)?;
        ensure!(
            self.points.len() == MAX_POINTS,
            "Unexpected curve point count"
        );
        for range in &self.ranges {
            ensure!(
                range.first <= range.last
                    && range.last < MAX_POINTS
                    && range.min_khz <= range.max_khz,
                "Curve range exceeds actual arrays"
            );
        }
        if let (Some(g), Some(m)) = (self.graphics(), self.memory()) {
            ensure!(
                g.last < m.first || m.last < g.first,
                "Overlapping graphics/memory ranges"
            );
        }
        Ok(())
    }
}
pub fn read_curve(transport: &mut impl NvApiTransport) -> Result<CurveSnapshot> {
    let mut raw = AbiBuffer::new(RANGES_BYTES, 1);
    transport.call(NvFunction::GetClockBoostRanges, &mut raw)?;
    let ranges = decode_ranges(&raw)?;
    if !ranges.iter().any(|r| r.domain == 0 && r.last > 0) {
        return Err(NvStatus(-104).into());
    }
    let (version, points) = match read_points(transport, 2) {
        Ok(p) => (2, p),
        Err(error) if incompatible(&error) => (1, read_points(transport, 1)?),
        Err(error) => return Err(error),
    };
    let points = (0..MAX_POINTS)
        .map(|i| CurvePoint {
            frequency_khz: points.get(72 + i * 28),
            voltage_uv: points.get(76 + i * 28),
        })
        .collect();
    let snapshot = CurveSnapshot {
        version,
        ranges,
        points,
        table: read_table(transport, version)?,
    };
    snapshot.validate()?;
    Ok(snapshot)
}
pub fn voltage_lock(voltage_uv: Option<u32>) -> AbiBuffer {
    let mut value = AbiBuffer::new(LOCK_BYTES, 2);
    value.set(8, 1);
    value.set(12, 6);
    value.set(20, if voltage_uv.is_some() { 3 } else { 0 });
    value.set(28, voltage_uv.unwrap_or(0));
    value
}
pub fn read_voltage_lock(transport: &mut impl NvApiTransport) -> Result<Option<u32>> {
    let mut value = voltage_lock(None);
    transport.call(NvFunction::GetClockBoostLock, &mut value)?;
    value.validate(LOCK_BYTES, 2)?;
    let count = value.get(8) as usize;
    ensure!(count <= 32, "Invalid clock lock count");
    let mut result = None;
    let mut found = false;
    for i in 0..count {
        let start = 12 + i * 24;
        if value.get(start) == 6 {
            ensure!(!found, "Duplicate voltage lock");
            found = true;
            let mode = value.get(start + 8);
            ensure!(mode == 0 || mode == 3, "Unsupported voltage lock mode");
            if mode == 3 {
                result = Some(value.get(start + 16));
            }
        }
    }
    Ok(result)
}
#[derive(Debug, Clone)]
pub struct GpuSnapshot {
    pub pstates: Pstates,
    pub curve: Option<CurveSnapshot>,
    pub voltage_lock_uv: Option<u32>,
}
impl GpuSnapshot {
    /// Readback derives cap from the first falling offset as in the fixed fork.
    /// Displayed core delta uses the legacy raw kHz/1000 convention.
    pub fn overclock_info(&self) -> OverclockInfo {
        let mut info = OverclockInfo::default();
        if let Some(curve) = &self.curve {
            if let Some(graphics) = curve.graphics() {
                info.core_delta_mhz = curve.first_delta(graphics).unwrap_or(0);
                for i in graphics.first + 1..=graphics.last.min(MAX_POINTS - 1) {
                    if curve.delta(i) < curve.delta(i - 1) && curve.points[i - 1].voltage_uv > 0 {
                        info.voltage_cap_mv = (curve.points[i - 1].voltage_uv / 1000) as i32;
                        break;
                    }
                }
            }
            if let Some(memory) = curve.memory() {
                info.memory_delta_mhz = curve.first_delta(memory).unwrap_or(0);
            }
        } else {
            info.core_delta_mhz = self
                .pstates
                .p0
                .iter()
                .find(|c| c.domain == 0)
                .map_or(0, |c| c.delta_khz / 1000);
            info.memory_delta_mhz = self
                .pstates
                .p0
                .iter()
                .find(|c| c.domain == 4)
                .map_or(0, |c| c.delta_khz / 1000);
        }
        info.voltage_lock_mv = self.voltage_lock_uv.map_or(0, |uv| (uv / 1000) as i32);
        info
    }
}
/// Availability must be decided by the actor before any read: NVAPI reads can
/// wake a GPU. This is deliberately not an automatic constructor/probe.
pub fn read_snapshot(transport: &mut impl NvApiTransport) -> Result<GpuSnapshot> {
    let pstates = read_pstates(transport)?;
    // Only an explicitly unsupported private interface permits the Pstate-only
    // path. An invalid handle/asleep GPU must remain an error.
    let curve = match read_curve(transport) {
        Ok(curve) => Some(curve),
        Err(error)
            if error
                .downcast_ref::<NvStatus>()
                .is_some_and(|s| matches!(s.0, -3 | -104)) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    let voltage_lock_uv = if curve.is_some() {
        read_voltage_lock(transport)?
    } else {
        None
    };
    Ok(GpuSnapshot {
        pstates,
        curve,
        voltage_lock_uv,
    })
}

#[derive(Debug, Clone)]
pub enum OverclockPlan {
    Pstate {
        core_khz: i32,
        memory_khz: i32,
        target: OverclockInfo,
    },
    Curve {
        baseline: CurveSnapshot,
        table: AbiBuffer,
        lock_uv: Option<u32>,
        target: OverclockInfo,
    },
}
impl OverclockPlan {
    pub fn target(&self) -> OverclockInfo {
        match self {
            Self::Pstate { target, .. } | Self::Curve { target, .. } => *target,
        }
    }
}
/// Build the complete plan before side effects. Unknown range support and
/// driver-owned points cannot silently claim a requested offset/voltage lock.
pub fn plan_overclock(snapshot: &GpuSnapshot, requested: OverclockInfo) -> Result<OverclockPlan> {
    ensure!(
        requested.voltage_lock_mv >= 0 && requested.voltage_cap_mv >= 0,
        "Negative voltage cap/lock"
    );
    let core_range = snapshot.pstates.range(0);
    let memory_range = snapshot.pstates.range(4);
    ensure!(
        core_range.driver_reported || requested.core_delta_mhz == 0,
        "Driver core range unavailable (UI fallback is not a write capability)"
    );
    ensure!(
        memory_range.driver_reported || requested.memory_delta_mhz == 0,
        "Driver memory range unavailable (UI fallback is not a write capability)"
    );
    let target = OverclockInfo {
        core_delta_mhz: core_range.clamp(requested.core_delta_mhz),
        memory_delta_mhz: memory_range.clamp(requested.memory_delta_mhz),
        ..requested
    };
    let core_khz = target
        .core_delta_mhz
        .checked_mul(1000)
        .context("Core offset overflow")?;
    let memory_khz = target
        .memory_delta_mhz
        .checked_mul(1000)
        .context("Memory offset overflow")?;
    let Some(curve) = &snapshot.curve else {
        ensure!(
            target.voltage_lock_mv == 0 && target.voltage_cap_mv == 0,
            "Pstates fallback cannot apply voltage cap/lock"
        );
        ensure!(
            snapshot.pstates.p0.iter().any(|c| c.domain == 0)
                && snapshot.pstates.p0.iter().any(|c| c.domain == 4),
            "P0 graphics/memory absent"
        );
        return Ok(OverclockPlan::Pstate {
            core_khz,
            memory_khz,
            target,
        });
    };
    curve.validate()?;
    let graphics = curve.graphics().context("Graphics curve unavailable")?;
    let memory = curve.memory();
    if target.core_delta_mhz != 0 {
        ensure!(
            curve.first_delta(graphics).is_some(),
            "No settable graphics points"
        );
    }
    if target.memory_delta_mhz != 0 {
        ensure!(
            memory.is_some_and(|m| curve.first_delta(m).is_some()),
            "No settable memory points"
        );
    }
    let multiplier = if curve.version == 2
        || i64::from(graphics.max_khz) != 2 * i64::from(core_range.max_mhz) * 1000
    {
        1
    } else {
        2
    };
    let core_khz = core_khz
        .checked_mul(multiplier)
        .context("Adjusted core offset overflow")?;
    ensure!(
        (graphics.min_khz..=graphics.max_khz).contains(&core_khz),
        "Core offset outside private curve range"
    );
    if let Some(m) = memory {
        ensure!(
            (m.min_khz..=m.max_khz).contains(&memory_khz),
            "Memory offset outside private curve range"
        );
    }
    let voltage_mv = if target.voltage_lock_mv > 0 {
        target.voltage_lock_mv
    } else {
        target.voltage_cap_mv
    };
    let anchor = if voltage_mv > 0 {
        Some(
            (graphics.first..=graphics.last)
                .find(|i| {
                    curve.points[*i].voltage_uv > 0
                        && i64::from(curve.points[*i].voltage_uv / 1000) >= i64::from(voltage_mv)
                })
                .context("No V/F anchor at or above requested voltage")?,
        )
    } else {
        None
    };
    let mut table = curve.table.clone();
    let anchor_stock = anchor.map(|i| curve.stock(i));
    for index in 0..MAX_DELTAS {
        if !curve.settable(index) {
            continue;
        }
        let in_memory = memory.is_some_and(|m| (m.first..=m.last).contains(&index));
        let in_graphics = (graphics.first..=graphics.last).contains(&index);
        let mut delta = curve.delta(index);
        if in_memory {
            delta = memory_khz;
        } else if in_graphics {
            delta = core_khz;
            if let Some(stock) = anchor_stock {
                if curve.stock(index) > stock {
                    let flattened = stock + i64::from(core_khz) - curve.stock(index);
                    // A clipped flatten would not actually provide the requested cap.
                    ensure!(
                        flattened >= i64::from(graphics.min_khz),
                        "Requested voltage cap exceeds flattening range"
                    );
                    delta = i32::try_from(flattened).context("Curve delta overflow")?;
                }
            }
        }
        table.set_i32(68 + index * 36 + 20, delta);
    }
    // Hardware-owned points are untouched by the fixed fork. They must not
    // invalidate a claimed cap/lock by continuing above the plateau.
    if let Some(stock) = anchor_stock {
        let ceiling = stock + i64::from(core_khz);
        for i in graphics.first..=graphics.last {
            ensure!(
                curve.stock(i) + i64::from(table.get_i32(68 + i * 36 + 20)) <= ceiling,
                "Driver-owned point prevents requested cap/lock"
            );
        }
    }
    let lock_uv = if target.voltage_lock_mv > 0 {
        Some(curve.points[anchor.unwrap()].voltage_uv)
    } else {
        None
    };
    Ok(OverclockPlan::Curve {
        baseline: curve.clone(),
        table,
        lock_uv,
        target,
    })
}
/// Native actor supplies a live guard (identity, exclusive owner, vendor tools,
/// dGPU awake, OC capability, deadline) and retains the ticket through readback.
/// Any failed setter/readback is an error; callers must not clear saved settings
/// or report success merely because a transport accepted the bytes.
pub fn execute_overclock(
    transport: &mut impl NvApiTransport,
    plan: &OverclockPlan,
    mut guard: impl FnMut() -> Result<()>,
) -> Result<OverclockInfo> {
    match plan {
        OverclockPlan::Pstate {
            core_khz,
            memory_khz,
            ..
        } => {
            // Enabling OC Pstates is itself a write, not an ignorable probe.
            guard()?;
            let mut sentinel = AbiBuffer::new(4, 0);
            transport.call(NvFunction::EnableOverclockedPstates, &mut sentinel)?;
            guard()?;
            let mut payload = pstate_write(*core_khz, *memory_khz);
            transport.call(NvFunction::SetPstates20, &mut payload)?;
            let actual = read_pstates(transport)?;
            for (domain, target) in [(0, *core_khz), (4, *memory_khz)] {
                ensure!(
                    actual
                        .p0
                        .iter()
                        .find(|c| c.domain == domain)
                        .is_some_and(|c| c.delta_khz == target),
                    "Pstate offset readback mismatch"
                );
            }
        }
        OverclockPlan::Curve {
            baseline,
            table,
            lock_uv,
            ..
        } => {
            // Re-read metadata immediately before write as the fork does. Never
            // replay a stale table over changes made by an external owner.
            let current = read_table(transport, baseline.version)?;
            ensure!(
                current == baseline.table,
                "GPU curve changed after plan creation"
            );
            guard()?;
            let mut payload = table.clone();
            transport.call(NvFunction::SetClockBoostTable, &mut payload)?;
            guard()?;
            let mut lock = voltage_lock(*lock_uv);
            transport.call(NvFunction::SetClockBoostLock, &mut lock)?;
            let actual = read_table(transport, baseline.version)?;
            // Compare each delta, including preserved driver-owned/unrelated
            // points. Other reserved status words may legitimately change.
            for i in 0..MAX_DELTAS {
                ensure!(
                    actual.get_i32(68 + i * 36 + 20) == table.get_i32(68 + i * 36 + 20),
                    "Curve delta readback mismatch at {i}"
                );
            }
            ensure!(
                read_voltage_lock(transport)? == *lock_uv,
                "Voltage lock readback mismatch"
            );
        }
    }
    Ok(plan.target())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pstates() -> Pstates {
        let mut raw = pstate_write(0, 0);
        raw.set_i32(44, -500000);
        raw.set_i32(48, 500000);
        raw.set_i32(88, -3000000);
        raw.set_i32(92, 3000000);
        Pstates::decode(raw).unwrap()
    }
    fn fixture(version: u32) -> GpuSnapshot {
        let mut table = AbiBuffer::new(TABLE_BYTES, version);
        table.set(4, 0x12345678);
        table.set(36, 0xabcd);
        for i in 0..MAX_DELTAS {
            table.set(68 + i * 36 + 4, 0xa5a5);
        }
        let mut points = vec![
            CurvePoint {
                frequency_khz: 0,
                voltage_uv: 0
            };
            MAX_POINTS
        ];
        for (i, point) in points.iter_mut().enumerate().take(4) {
            *point = CurvePoint {
                frequency_khz: 1000000 + i as u32 * 100000,
                voltage_uv: 700000 + i as u32 * 100000,
            };
        }
        GpuSnapshot {
            pstates: pstates(),
            curve: Some(CurveSnapshot {
                version,
                ranges: vec![
                    CurveRange {
                        domain: 0,
                        min_khz: -1000000,
                        max_khz: if version == 1 { 1000000 } else { 500000 },
                        first: 0,
                        last: 3,
                    },
                    CurveRange {
                        domain: 4,
                        min_khz: -3000000,
                        max_khz: 3000000,
                        first: 4,
                        last: 5,
                    },
                ],
                points,
                table,
            }),
            voltage_lock_uv: None,
        }
    }
    #[test]
    fn abi_sizes_versions_offsets_and_official_unload_are_exact() {
        assert_eq!(
            [
                MASK_BYTES,
                RANGES_BYTES,
                TABLE_BYTES,
                POINTS_BYTES,
                LOCK_BYTES,
                PSTATE_V1_BYTES,
                PSTATE_V2_BYTES
            ],
            [6188, 2344, 9248, 7208, 780, 7316, 7416]
        );
        let p = pstate_write(-500000, 3000000);
        assert_eq!(p.get(0), 0x10000 | 7316);
        assert_eq!(p.get(12), 2);
        assert_eq!(p.get_i32(40), -500000);
        assert_eq!(p.get(72), 4);
        assert_eq!(p.get_i32(84), 3000000);
        assert_eq!(
            voltage_lock(Some(900000)).words()[..9],
            [0x20000 | 780, 0, 1, 6, 0, 3, 0, 900000, 0]
        );
        assert_eq!(NvFunction::Unload as u32, 0xd22bdd7e);
        assert_eq!(NvFunction::GetClockBoostTable as u32, 0x23f1b133);
        let mut p = p;
        assert_eq!(p.as_mut_ptr() as usize % 4, 0);
        assert_eq!(p.to_le_bytes().len(), 7316);
    }
    #[test]
    fn adjusted_v1_core_multiplier_and_v2_normal_units() {
        for (version, expected) in [(1, 200000), (2, 100000)] {
            let plan = plan_overclock(
                &fixture(version),
                OverclockInfo {
                    core_delta_mhz: 100,
                    memory_delta_mhz: 200,
                    ..Default::default()
                },
            )
            .unwrap();
            let OverclockPlan::Curve { table, .. } = plan else {
                panic!()
            };
            assert_eq!(table.get_i32(88), expected);
            assert_eq!(table.get_i32(232), 200000);
        }
    }
    #[test]
    fn voltage_anchor_flattens_stock_and_preserves_unrelated_metadata() {
        let snap = fixture(2);
        let plan = plan_overclock(
            &snap,
            OverclockInfo {
                core_delta_mhz: 50,
                voltage_cap_mv: 800,
                ..Default::default()
            },
        )
        .unwrap();
        let OverclockPlan::Curve { table, lock_uv, .. } = plan else {
            panic!()
        };
        assert_eq!(lock_uv, None);
        assert_eq!(
            (0..4)
                .map(|i| table.get_i32(88 + i * 36))
                .collect::<Vec<_>>(),
            vec![50000, 50000, -50000, -150000]
        );
        assert_eq!(table.get(4), 0x12345678);
        assert_eq!(table.get(36), 0xabcd);
        assert_eq!(table.get(72), 0xa5a5);
        let plan = plan_overclock(
            &snap,
            OverclockInfo {
                voltage_lock_mv: 801,
                voltage_cap_mv: 700,
                ..Default::default()
            },
        )
        .unwrap();
        let OverclockPlan::Curve { lock_uv, .. } = plan else {
            panic!()
        };
        assert_eq!(lock_uv, Some(900000));
    }
    #[test]
    fn hardware_owned_points_and_unknown_ranges_fail_honestly() {
        let mut snap = fixture(2);
        snap.curve.as_mut().unwrap().table.set(68 + 3 * 36, 1);
        assert!(
            plan_overclock(
                &snap,
                OverclockInfo {
                    voltage_cap_mv: 700,
                    ..Default::default()
                }
            )
            .is_err()
        );
        let plan = plan_overclock(
            &snap,
            OverclockInfo {
                core_delta_mhz: 100,
                ..Default::default()
            },
        )
        .unwrap();
        let OverclockPlan::Curve { table, .. } = plan else {
            panic!()
        };
        assert_eq!(table.get_i32(88 + 3 * 36), 0);
        assert_eq!(table.get(68 + 3 * 36), 1);
        snap.pstates.p0[0].min_khz = 0;
        snap.pstates.p0[0].max_khz = 0;
        assert!(!snap.pstates.range(0).driver_reported);
        assert!(
            plan_overclock(
                &snap,
                OverclockInfo {
                    core_delta_mhz: 1,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
    #[test]
    fn unsupported_voltage_and_bad_ranges_never_create_a_plan() {
        let mut snap = fixture(2);
        snap.curve = None;
        assert!(
            plan_overclock(
                &snap,
                OverclockInfo {
                    voltage_lock_mv: 900,
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert!(matches!(
            plan_overclock(
                &snap,
                OverclockInfo {
                    core_delta_mhz: 900,
                    memory_delta_mhz: -9000,
                    ..Default::default()
                }
            )
            .unwrap(),
            OverclockPlan::Pstate {
                core_khz: 500000,
                memory_khz: -3000000,
                ..
            }
        ));
        let mut snap = fixture(2);
        snap.curve.as_mut().unwrap().ranges[0].last = 254;
        assert!(plan_overclock(&snap, OverclockInfo::default()).is_err());
        assert!(
            plan_overclock(
                &fixture(2),
                OverclockInfo {
                    voltage_cap_mv: 1100,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
    #[test]
    fn pstate_counts_and_duplicate_domains_are_validated() {
        let mut raw = pstate_write(0, 0);
        raw.set(12, 9);
        assert!(Pstates::decode(raw).is_err());
        let mut raw = pstate_write(0, 0);
        raw.set(72, 0);
        assert!(Pstates::decode(raw).is_err());
        let mut ranges = AbiBuffer::new(RANGES_BYTES, 1);
        ranges.set(4, 1);
        ranges.set(88, 0xff01);
        assert!(decode_ranges(&ranges).is_ok());
        ranges.set(88, 0x01ff);
        assert!(decode_ranges(&ranges).is_err());
    }
    #[test]
    fn version_negotiation_only_retries_incompatible_status() {
        struct Fake {
            calls: Vec<u32>,
            status: i32,
        }
        impl NvApiTransport for Fake {
            fn call(&mut self, _: NvFunction, p: &mut AbiBuffer) -> Result<()> {
                self.calls.push(p.version());
                Err(NvStatus(self.status).into())
            }
        }
        let mut fake = Fake {
            calls: vec![],
            status: -9,
        };
        assert!(read_pstates(&mut fake).is_err());
        assert_eq!(fake.calls, vec![3, 2, 1]);
        let mut fake = Fake {
            calls: vec![],
            status: -13,
        };
        assert!(read_pstates(&mut fake).is_err());
        assert_eq!(fake.calls, vec![3]);
    }
    #[test]
    fn fake_apply_requires_guard_and_verified_readback() {
        struct Fake {
            state: AbiBuffer,
            writes: Vec<NvFunction>,
            lie: bool,
        }
        impl NvApiTransport for Fake {
            fn call(&mut self, f: NvFunction, p: &mut AbiBuffer) -> Result<()> {
                if f.is_write() {
                    self.writes.push(f);
                }
                match f {
                    NvFunction::SetPstates20 => {
                        if !self.lie {
                            self.state = p.clone();
                        }
                    }
                    NvFunction::GetPstates20 => {
                        *p = self.state.clone();
                    }
                    NvFunction::EnableOverclockedPstates => {}
                    _ => bail!("unexpected fake function"),
                };
                Ok(())
            }
        }
        let mut snap = fixture(2);
        snap.curve = None;
        let plan = plan_overclock(
            &snap,
            OverclockInfo {
                core_delta_mhz: 10,
                ..Default::default()
            },
        )
        .unwrap();
        let mut fake = Fake {
            state: snap.pstates.raw.clone(),
            writes: vec![],
            lie: false,
        };
        assert!(execute_overclock(&mut fake, &plan, || bail!("deadline/ownership")).is_err());
        assert!(fake.writes.is_empty());
        assert_eq!(
            execute_overclock(&mut fake, &plan, || Ok(()))
                .unwrap()
                .core_delta_mhz,
            10
        );
        fake.state = snap.pstates.raw;
        fake.lie = true;
        assert!(execute_overclock(&mut fake, &plan, || Ok(())).is_err());
    }
    #[test]
    fn core_settings_format_uses_exact_legacy_property_names() {
        let value = serde_json::to_value(OverclockInfo {
            core_delta_mhz: 100,
            memory_delta_mhz: 200,
            voltage_lock_mv: 900,
            voltage_cap_mv: 0,
        })
        .unwrap();
        assert_eq!(value["CoreDeltaMhz"], 100);
        assert_eq!(value["VoltageLockMv"], 900);
        assert_eq!(
            serde_json::from_value::<OverclockInfo>(value)
                .unwrap()
                .memory_delta_mhz,
            200
        );
    }
}
