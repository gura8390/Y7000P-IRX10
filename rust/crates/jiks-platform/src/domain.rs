//! Owned, serializable boundary values. No COM pointers leave a hardware thread.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const PL1: u32 = 0x0102_0000;
pub const PL2: u32 = 0x0101_0000;
pub const CPU_TEMPERATURE_LIMIT: u32 = 0x0104_0000;
pub const CPU_TEMPERATURE: u32 = 0x0504_0000;
pub const CUSTOM_MODE: u32 = 255;

/// The same boot-relative clock used for command deadlines and sample freshness.
/// Wall-clock corrections never extend a hardware command's execution window.
pub fn monotonic_ms() -> u64 {
    unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone)]
pub struct ExplicitAction {
    description: String,
    deadline_tick_ms: Option<u64>,
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}
impl ExplicitAction {
    /// Construct only from an explicit user action, never from discovery or paint.
    pub fn new(description: impl Into<String>) -> Result<Self> {
        let description = description.into();
        ensure!(
            !description.trim().is_empty(),
            "A hardware write needs an explicit action description"
        );
        Ok(Self {
            description,
            deadline_tick_ms: None,
            cancelled: None,
        })
    }
    pub fn description(&self) -> &str {
        &self.description
    }
    pub fn with_deadline(description: impl Into<String>, deadline_tick_ms: u64) -> Result<Self> {
        let mut action = Self::new(description)?;
        action.deadline_tick_ms = Some(deadline_tick_ms);
        Ok(action)
    }
    pub fn deadline_tick_ms(&self) -> Option<u64> {
        self.deadline_tick_ms
    }
    pub fn with_cancellation(
        mut self,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        self.cancelled = Some(cancelled);
        self
    }
    pub fn check_deadline_at(&self, now: u64) -> Result<()> {
        ensure!(
            !self
                .cancelled
                .as_ref()
                .is_some_and(|v| v.load(std::sync::atomic::Ordering::Acquire)),
            "Explicit action was cancelled; no later setter was issued"
        );
        ensure!(
            self.deadline_tick_ms.is_none_or(|deadline| now < deadline),
            "Explicit action expired before physical write; no later setter was issued"
        );
        Ok(())
    }
    pub fn check_deadline(&self) -> Result<()> {
        self.check_deadline_at(monotonic_ms())
    }
    pub(crate) fn check_write_boundary(&self) -> Result<()> {
        self.check_deadline()?;
        crate::ensure_no_other_controllers()?;
        self.check_deadline()
    }
    /// The long-lived CPU session has its own per-command deadline and verified
    /// restore obligation. The enabling UI command cannot expire that obligation.
    pub(crate) fn for_cpu_session(&self) -> Self {
        Self {
            description: self.description.clone(),
            deadline_tick_ms: None,
            cancelled: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub long_watts: u32,
    pub short_watts: u32,
}
impl Envelope {
    pub fn valid(self) -> bool {
        self.long_watts > 0 && self.long_watts <= self.short_watts
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub min: u32,
    pub max: u32,
    pub step: u32,
}
impl Range {
    pub fn contains(self, value: u32) -> bool {
        self.min > 0
            && self.max >= self.min
            && self.step > 0
            && value >= self.min
            && value <= self.max
            && (value - self.min).is_multiple_of(self.step)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuCapabilities {
    pub long: Range,
    pub short: Range,
    pub temperature: Range,
}
impl CpuCapabilities {
    pub fn validate(&self, value: Envelope, temperature: u32) -> Result<()> {
        ensure!(
            value.valid()
                && self.long.contains(value.long_watts)
                && self.short.contains(value.short_watts)
                && self.temperature.contains(temperature),
            "CPU values do not match the discovered firmware grid"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuSnapshot {
    pub raw_mode: u32,
    pub power: Envelope,
    pub temperature_limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemperatureSample {
    pub celsius: u32,
    pub requested_at_tick_ms: u64,
    pub completed_at_tick_ms: u64,
    pub source: String,
}
impl TemperatureSample {
    pub(crate) fn firmware(celsius: u32, requested_at_tick_ms: u64) -> Result<Self> {
        ensure!(celsius > 0 && celsius < 150, "Invalid CPU temperature");
        Ok(Self {
            celsius,
            requested_at_tick_ms,
            completed_at_tick_ms: monotonic_ms(),
            source: "Lenovo aggregate CPU temperature (0x05040000); not core/package maximum"
                .into(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeCapability {
    #[serde(rename = "IDs")]
    pub id: u32,
    #[serde(rename = "MinValue")]
    pub min: u32,
    #[serde(rename = "MaxValue")]
    pub max: u32,
    #[serde(rename = "Step")]
    pub step: u32,
    #[serde(rename = "DefaultValue")]
    pub default_value: u32,
}

pub fn custom_cpu_capabilities(
    rows: &[RangeCapability],
    discrete_ids: &[u32],
) -> Result<CpuCapabilities> {
    fn one(rows: &[RangeCapability], id: u32, discrete: &[u32]) -> Result<Range> {
        let matches: Vec<_> = rows.iter().filter(|r| r.id == (id | 0xff00)).collect();
        ensure!(
            matches.len() == 1,
            "Expected exactly one Custom-mode capability for {id:#x}"
        );
        ensure!(
            !discrete.iter().any(|d| d & 0xffff00ff == id),
            "Discrete CPU capabilities need a separate adapter"
        );
        let row = matches[0];
        let result = Range {
            min: row.min,
            max: row.max,
            step: row.step,
        };
        ensure!(result.contains(result.min), "Invalid CPU capability range");
        Ok(result)
    }
    let value = CpuCapabilities {
        long: one(rows, PL1, discrete_ids)?,
        short: one(rows, PL2, discrete_ids)?,
        temperature: one(rows, CPU_TEMPERATURE_LIMIT, discrete_ids)?,
    };
    ensure!(
        value.long.step == value.short.step,
        "Different PL1/PL2 steps are not validated"
    );
    Ok(value)
}

/// Native.cs FanTable.GetBytes: two bytes + uint32 count + ten little-endian uint16 levels.
pub fn validate_fan_table(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() == 64
            && bytes[0] == 1
            && bytes[1] == 0
            && u32::from_le_bytes(bytes[2..6].try_into().unwrap()) == 10,
        "Invalid Lenovo 64-byte fan table frame"
    );
    ensure!(
        bytes[26..].iter().all(|b| *b == 0),
        "Reserved fan-table bytes must be zero"
    );
    for (i, minimum) in [1u16, 1, 1, 1, 1, 1, 1, 1, 3, 5].into_iter().enumerate() {
        let offset = 6 + i * 2;
        let level = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        ensure!(
            (minimum..=10).contains(&level),
            "Fan level outside the verified Legion minimum/maximum"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum MappedFeature {
    Overdrive,
    FlipToStart,
    IgpuMode,
    InstantBootAc,
    InstantBootUsb,
    FanFullSpeed,
    GodModeFnQ,
    PowerLedSync,
}
impl MappedFeature {
    pub fn id(self) -> u32 {
        match self {
            Self::Overdrive => 0x001a0000,
            Self::FlipToStart => 0x00030000,
            Self::IgpuMode => 0x00010000,
            Self::InstantBootAc => 0x03010001,
            Self::InstantBootUsb => 0x03010002,
            Self::FanFullSpeed => 0x04020000,
            Self::GodModeFnQ => 0x00100000,
            Self::PowerLedSync => 0x06040000,
        }
    }
    pub fn validate(self, value: u32) -> Result<()> {
        let maximum = if matches!(self, Self::IgpuMode) { 2 } else { 1 };
        if matches!(self, Self::PowerLedSync) {
            ensure!(
                [0, 2].contains(&value),
                "Power LED sync must use the documented 0/2 enum"
            );
            return Ok(());
        }
        ensure!(
            value <= maximum,
            "Mapped-feature value outside its verified enum"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalState {
    Acquired,
    Controlling,
    WriteIntent,
    Restoring,
    Restored,
    ReleasedExternal,
    Faulted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuJournal {
    pub schema: u32,
    pub process_id: u32,
    pub process_created_100ns: u64,
    pub updated_unix_ms: u64,
    pub action: String,
    pub state: JournalState,
    pub baseline: CpuSnapshot,
    pub expected: CpuSnapshot,
    pub long_intents: Vec<u32>,
    pub short_intents: Vec<u32>,
    pub temperature_intents: Vec<u32>,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_deadline_is_strict_and_session_cleanup_does_not_inherit_it() {
        let action = ExplicitAction::with_deadline("fixture", 50).unwrap();
        assert!(action.check_deadline_at(49).is_ok());
        assert!(action.check_deadline_at(50).is_err());
        assert!(action.clone().check_deadline_at(51).is_err());
        assert!(action.for_cpu_session().check_deadline_at(u64::MAX).is_ok());
    }
    #[test]
    fn range_rejects_invalid_or_off_grid_values() {
        assert!(
            Range {
                min: 10,
                max: 100,
                step: 5
            }
            .contains(15)
        );
        assert!(
            !Range {
                min: 10,
                max: 100,
                step: 0
            }
            .contains(15)
        );
        assert!(
            !Range {
                min: 10,
                max: 100,
                step: 5
            }
            .contains(16)
        );
        assert!(
            !Envelope {
                long_watts: 90,
                short_watts: 80
            }
            .valid()
        );
    }
    #[test]
    fn fan_frame_preserves_count_width_and_reserved_bytes() {
        let mut bytes = vec![0u8; 64];
        bytes[0] = 1;
        bytes[2..6].copy_from_slice(&10u32.to_le_bytes());
        for i in 0..10 {
            bytes[6 + i * 2..8 + i * 2].copy_from_slice(&10u16.to_le_bytes());
        }
        assert!(validate_fan_table(&bytes).is_ok());
        bytes[2] = 9;
        assert!(validate_fan_table(&bytes).is_err());
        bytes[2] = 10;
        bytes[63] = 1;
        assert!(validate_fan_table(&bytes).is_err());
    }
}
