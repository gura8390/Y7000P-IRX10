//! Manual Custom-mode writes. Complete capability/input validation precedes the
//! first setter; no preset defaults or fan tables are invented here.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum GodModeField {
    CpuLongPower,
    CpuShortPower,
    CpuPeakPower,
    CpuTemperature,
    ApuSppt,
    CpuCrossLoading,
    CpuPl2Tau,
    CpuOverclockEnable,
    GpuPowerBoost,
    GpuTgp,
    GpuTemperature,
    GpuAcOffset,
    GpuToCpuBoost,
}
impl GodModeField {
    pub const ALL: [Self; 13] = [
        Self::CpuLongPower,
        Self::CpuShortPower,
        Self::CpuPeakPower,
        Self::CpuTemperature,
        Self::ApuSppt,
        Self::CpuCrossLoading,
        Self::CpuPl2Tau,
        Self::CpuOverclockEnable,
        Self::GpuPowerBoost,
        Self::GpuTgp,
        Self::GpuTemperature,
        Self::GpuAcOffset,
        Self::GpuToCpuBoost,
    ];
    pub fn id(self) -> u32 {
        match self {
            Self::CpuShortPower => 0x01010000,
            Self::CpuLongPower => 0x01020000,
            Self::CpuPeakPower => 0x01030000,
            Self::CpuTemperature => 0x01040000,
            Self::ApuSppt => 0x01050000,
            Self::CpuCrossLoading => 0x01060000,
            Self::CpuPl2Tau => 0x01070000,
            // The original CPU OC method explicitly passes CapabilityID's
            // Custom-tagged ID unchanged; other numeric sliders use active IDs.
            Self::CpuOverclockEnable => 0x0108ff00,
            Self::GpuPowerBoost => 0x02010000,
            Self::GpuTgp => 0x02020000,
            Self::GpuTemperature => 0x02030000,
            Self::GpuAcOffset => 0x02040000,
            Self::GpuToCpuBoost => 0x020b0000,
        }
    }
    pub fn discovery_id(self) -> u32 {
        self.id() | 0xff00
    }
    pub fn is_cpu(self) -> bool {
        self.id() & 0xff000000 == 0x01000000
    }
    pub fn semantic_key(self) -> &'static str {
        match self {
            Self::CpuLongPower => "cpu.pl1",
            Self::CpuShortPower => "cpu.pl2",
            Self::CpuPeakPower => "cpu.peak",
            Self::CpuTemperature => "cpu.temperature",
            Self::ApuSppt => "cpu.sppt",
            Self::CpuCrossLoading => "cpu.cross_loading",
            Self::CpuPl2Tau => "cpu.pl2_tau",
            Self::CpuOverclockEnable => "cpu.overclock_enabled",
            Self::GpuPowerBoost => "gpu.dynamic_boost",
            Self::GpuTgp => "gpu.tgp",
            Self::GpuTemperature => "gpu.temperature",
            Self::GpuAcOffset => "gpu.total_power_target",
            Self::GpuToCpuBoost => "gpu.to_cpu_boost",
        }
    }
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|field| field.semantic_key() == key)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityValue {
    pub field: GodModeField,
    pub value: i32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GodModePatch {
    pub values: Vec<CapabilityValue>,
    pub deadline_tick_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedRange {
    #[serde(rename = "IDs")]
    pub id: u32,
    #[serde(rename = "MinValue")]
    pub min: i32,
    #[serde(rename = "MaxValue")]
    pub max: i32,
    #[serde(rename = "Step")]
    pub step: i32,
    #[serde(rename = "DefaultValue")]
    pub default_value: i32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscreteValue {
    #[serde(rename = "IDs")]
    pub id: u32,
    #[serde(rename = "Value")]
    pub value: i32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitySpec {
    pub field: GodModeField,
    pub min: i32,
    pub max: i32,
    pub step: Option<i32>,
    pub discrete: Vec<i32>,
    pub default_value: Option<i32>,
}
impl CapabilitySpec {
    pub fn contains(&self, value: i32) -> bool {
        self.min <= value
            && value <= self.max
            && self.step.is_none_or(|step| {
                step > 0 && (i64::from(value) - i64::from(self.min)) % i64::from(step) == 0
            })
            && (self.discrete.is_empty() || self.discrete.contains(&value))
    }
}
pub fn capability(
    field: GodModeField,
    ranges: &[SignedRange],
    discrete: &[DiscreteValue],
) -> Result<CapabilitySpec> {
    let rows: Vec<_> = ranges
        .iter()
        .filter(|row| row.id == field.discovery_id())
        .collect();
    ensure!(
        rows.len() <= 1,
        "Duplicate Custom range for {}",
        field.semantic_key()
    );
    let mut values: Vec<_> = discrete
        .iter()
        .filter(|row| row.id == field.discovery_id())
        .map(|row| row.value)
        .collect();
    values.sort_unstable();
    values.dedup();
    let spec = if let Some(row) = rows.first() {
        ensure!(
            row.max >= row.min && (row.step > 0 || !values.is_empty()),
            "Invalid Custom range for {}",
            field.semantic_key()
        );
        CapabilitySpec {
            field,
            min: row.min,
            max: row.max,
            step: (row.step > 0).then_some(row.step),
            discrete: values,
            default_value: Some(row.default_value),
        }
    } else {
        ensure!(
            !values.is_empty(),
            "No Custom range/discrete capability for {}",
            field.semantic_key()
        );
        CapabilitySpec {
            field,
            min: values[0],
            max: *values.last().unwrap(),
            step: None,
            discrete: values,
            default_value: None,
        }
    };
    ensure!(
        spec.discrete.iter().all(|value| spec.contains(*value)),
        "Discrete values conflict with Custom grid for {}",
        field.semantic_key()
    );
    Ok(spec)
}
pub trait GodModeFirmware {
    fn now_tick_ms(&mut self) -> u64 {
        crate::monotonic_ms()
    }
    fn guard(&mut self) -> Result<()>;
    fn mode(&mut self) -> Result<u32>;
    fn capabilities(&mut self) -> Result<(Vec<SignedRange>, Vec<DiscreteValue>)>;
    fn read(&mut self, field: GodModeField) -> Result<i32>;
    fn validate_write(&mut self, field: GodModeField, value: i32) -> Result<()>;
    fn write(&mut self, field: GodModeField, value: i32, deadline_tick_ms: u64) -> Result<()>;
    fn write_checked(
        &mut self,
        field: GodModeField,
        value: i32,
        deadline_tick_ms: u64,
        _expected: &BTreeMap<GodModeField, i32>,
    ) -> Result<()> {
        self.write(field, value, deadline_tick_ms)
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    pub verified: Vec<CapabilityValue>,
    pub attempted: Option<CapabilityValue>,
    pub observed: BTreeMap<GodModeField, i32>,
    pub error: Option<String>,
    pub complete: bool,
}
#[derive(Debug)]
pub struct ApplyFailure(pub ApplyReport);
impl fmt::Display for ApplyFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GodMode apply incomplete; {}",
            serde_json::to_string(&self.0)
                .unwrap_or_else(|_| "unserializable failure report".into())
        )
    }
}
impl std::error::Error for ApplyFailure {}
fn snapshot(
    firmware: &mut impl GodModeFirmware,
    fields: &BTreeSet<GodModeField>,
) -> Result<BTreeMap<GodModeField, i32>> {
    ensure!(
        firmware.mode()? == 255,
        "Custom mode changed; no further field was written"
    );
    let values = fields
        .iter()
        .map(|field| Ok((*field, firmware.read(*field)?)))
        .collect::<Result<_>>()?;
    ensure!(
        firmware.mode()? == 255,
        "Mode changed during Custom readback"
    );
    Ok(values)
}
pub fn execute(
    firmware: &mut impl GodModeFirmware,
    patch: &GodModePatch,
    cpu_session_present: bool,
) -> Result<ApplyReport> {
    ensure!(
        !patch.values.is_empty() && patch.values.len() <= GodModeField::ALL.len(),
        "A GodMode patch must contain explicit fields"
    );
    ensure!(
        !cpu_session_present || !patch.values.iter().any(|value| value.field.is_cpu()),
        "CPU control owns CPU settings; stop and verify restoration before a manual CPU change"
    );
    let mut requested = BTreeMap::new();
    for value in &patch.values {
        ensure!(
            requested.insert(value.field, value.value).is_none(),
            "Duplicate GodMode field {}",
            value.field.semantic_key()
        );
    }
    firmware.guard()?;
    ensure!(
        firmware.now_tick_ms() < patch.deadline_tick_ms,
        "GodMode request expired before validation"
    );
    ensure!(
        firmware.mode()? == 255,
        "Select Custom mode explicitly before applying a preset; no mode/default was changed"
    );
    let (ranges, discrete) = firmware.capabilities()?;
    for (field, value) in &requested {
        let spec = capability(*field, &ranges, &discrete)?;
        ensure!(
            spec.contains(*value),
            "{}={} is outside the discovered Custom grid/discrete values",
            field.semantic_key(),
            value
        );
        firmware.validate_write(*field, *value)?;
    }
    let mut watched = requested.keys().copied().collect::<BTreeSet<_>>();
    watched.extend([
        GodModeField::CpuLongPower,
        GodModeField::CpuShortPower,
        GodModeField::CpuTemperature,
    ]);
    let mut expected = snapshot(firmware, &watched)?;
    let long = requested
        .get(&GodModeField::CpuLongPower)
        .copied()
        .unwrap_or(expected[&GodModeField::CpuLongPower]);
    let short = requested
        .get(&GodModeField::CpuShortPower)
        .copied()
        .unwrap_or(expected[&GodModeField::CpuShortPower]);
    ensure!(
        long > 0 && long <= short,
        "Manual CPU envelope must keep 0 < PL1 <= PL2"
    );
    let power_order = if short < expected[&GodModeField::CpuLongPower] {
        [GodModeField::CpuLongPower, GodModeField::CpuShortPower]
    } else {
        [GodModeField::CpuShortPower, GodModeField::CpuLongPower]
    };
    let mut order = power_order
        .into_iter()
        .filter(|field| requested.contains_key(field))
        .collect::<Vec<_>>();
    order.extend(
        requested
            .keys()
            .copied()
            .filter(|field| !power_order.contains(field)),
    );
    let mut report = ApplyReport {
        verified: vec![],
        attempted: None,
        observed: expected.clone(),
        error: None,
        complete: false,
    };
    for field in order {
        let value = requested[&field];
        if expected[&field] == value {
            continue;
        }
        let result = (|| -> Result<()> {
            firmware.guard()?;
            ensure!(
                snapshot(firmware, &watched)? == expected,
                "A watched firmware field changed externally; no further setter was issued"
            );
            ensure!(
                firmware.now_tick_ms() < patch.deadline_tick_ms,
                "GodMode request expired before next physical setter"
            );
            report.attempted = Some(CapabilityValue { field, value });
            firmware.write_checked(field, value, patch.deadline_tick_ms, &expected)?;
            expected.insert(field, value);
            let observed = snapshot(firmware, &watched)?;
            report.observed = observed.clone();
            ensure!(
                observed == expected,
                "GodMode physical readback differs from the requested/cross-field snapshot"
            );
            report.verified.push(CapabilityValue { field, value });
            report.attempted = None;
            Ok(())
        })();
        if let Err(error) = result {
            if let Ok(observed) = snapshot(firmware, &watched) {
                report.observed = observed;
            }
            report.error = Some(error.to_string());
            return Err(ApplyFailure(report).into());
        }
    }
    report.complete = true;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_overclock_enable_retains_original_capability_id() {
        assert_eq!(GodModeField::CpuOverclockEnable.id(), 0x0108ff00);
        assert_eq!(GodModeField::CpuLongPower.id(), 0x01020000);
        assert!(GodModeField::CpuOverclockEnable.is_cpu());
    }
    use anyhow::Context;
    struct Fake {
        values: BTreeMap<GodModeField, i32>,
        writes: Vec<(GodModeField, i32)>,
        tick: u64,
        fail: Option<GodModeField>,
        advance: u64,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                values: BTreeMap::from([
                    (GodModeField::CpuLongPower, 80),
                    (GodModeField::CpuShortPower, 100),
                    (GodModeField::CpuTemperature, 100),
                    (GodModeField::CpuPeakPower, 120),
                    (GodModeField::GpuTgp, 80),
                ]),
                writes: vec![],
                tick: 10,
                fail: None,
                advance: 0,
            }
        }
    }
    impl GodModeFirmware for Fake {
        fn now_tick_ms(&mut self) -> u64 {
            self.tick
        }
        fn guard(&mut self) -> Result<()> {
            Ok(())
        }
        fn mode(&mut self) -> Result<u32> {
            Ok(255)
        }
        fn capabilities(&mut self) -> Result<(Vec<SignedRange>, Vec<DiscreteValue>)> {
            Ok((
                GodModeField::ALL
                    .into_iter()
                    .map(|field| SignedRange {
                        id: field.discovery_id(),
                        min: 0,
                        max: 200,
                        step: 5,
                        default_value: 100,
                    })
                    .collect(),
                vec![],
            ))
        }
        fn read(&mut self, field: GodModeField) -> Result<i32> {
            self.values
                .get(&field)
                .copied()
                .context("missing fake value")
        }
        fn validate_write(&mut self, _: GodModeField, _: i32) -> Result<()> {
            Ok(())
        }
        fn write(&mut self, field: GodModeField, value: i32, _: u64) -> Result<()> {
            self.writes.push((field, value));
            self.values.insert(field, value);
            self.tick += self.advance;
            ensure!(self.fail != Some(field), "fake setter applied then failed");
            Ok(())
        }
    }
    fn patch(values: &[(GodModeField, i32)]) -> GodModePatch {
        GodModePatch {
            values: values
                .iter()
                .map(|(field, value)| CapabilityValue {
                    field: *field,
                    value: *value,
                })
                .collect(),
            deadline_tick_ms: 100,
        }
    }
    #[test]
    fn full_preflight_rejects_later_bad_field_before_any_setter() {
        let mut f = Fake::new();
        assert!(
            execute(
                &mut f,
                &patch(&[(GodModeField::CpuLongPower, 40), (GodModeField::GpuTgp, 81)]),
                false
            )
            .is_err()
        );
        assert!(f.writes.is_empty());
    }
    #[test]
    fn legal_cpu_order_decreases_and_raises_without_invalid_intermediate() {
        let mut f = Fake::new();
        execute(
            &mut f,
            &patch(&[
                (GodModeField::CpuLongPower, 40),
                (GodModeField::CpuShortPower, 50),
            ]),
            false,
        )
        .unwrap();
        assert_eq!(
            f.writes,
            vec![
                (GodModeField::CpuLongPower, 40),
                (GodModeField::CpuShortPower, 50)
            ]
        );
        f.writes.clear();
        execute(
            &mut f,
            &patch(&[
                (GodModeField::CpuLongPower, 80),
                (GodModeField::CpuShortPower, 100),
            ]),
            false,
        )
        .unwrap();
        assert_eq!(
            f.writes,
            vec![
                (GodModeField::CpuShortPower, 100),
                (GodModeField::CpuLongPower, 80)
            ]
        );
    }
    #[test]
    fn session_blocks_all_cpu_fields_but_keeps_gpu_field_independent() {
        let mut f = Fake::new();
        assert!(execute(&mut f, &patch(&[(GodModeField::CpuPeakPower, 100)]), true).is_err());
        assert!(f.writes.is_empty());
        execute(&mut f, &patch(&[(GodModeField::GpuTgp, 85)]), true).unwrap();
        assert_eq!(f.writes, vec![(GodModeField::GpuTgp, 85)]);
    }
    #[test]
    fn applied_error_reports_attempted_and_observed_without_followup_writes() {
        let mut f = Fake::new();
        f.fail = Some(GodModeField::CpuLongPower);
        let error = execute(
            &mut f,
            &patch(&[
                (GodModeField::CpuLongPower, 40),
                (GodModeField::CpuShortPower, 50),
            ]),
            false,
        )
        .unwrap_err();
        let report = &error.downcast_ref::<ApplyFailure>().unwrap().0;
        assert!(!report.complete);
        assert_eq!(report.attempted.as_ref().unwrap().value, 40);
        assert_eq!(report.observed[&GodModeField::CpuLongPower], 40);
        assert_eq!(f.writes.len(), 1);
    }
    #[test]
    fn exact_custom_discrete_grid_and_duplicates_are_fail_closed() {
        let field = GodModeField::CpuPl2Tau;
        let values = [
            DiscreteValue {
                id: field.discovery_id(),
                value: 0,
            },
            DiscreteValue {
                id: field.discovery_id(),
                value: 20,
            },
        ];
        let spec = capability(field, &[], &values).unwrap();
        assert!(spec.contains(20) && !spec.contains(10));
        assert!(
            capability(
                field,
                &[],
                &[DiscreteValue {
                    id: field.id() | 0x0100,
                    value: 10
                }]
            )
            .is_err()
        );
        let row = SignedRange {
            id: field.discovery_id(),
            min: 0,
            max: 30,
            step: 5,
            default_value: 0,
        };
        assert!(capability(field, &[row.clone(), row], &[]).is_err());
    }
    #[test]
    fn late_first_write_blocks_second_and_unchanged_patch_never_writes_defaults() {
        let mut f = Fake::new();
        f.advance = 90;
        assert!(
            execute(
                &mut f,
                &patch(&[
                    (GodModeField::CpuLongPower, 40),
                    (GodModeField::CpuShortPower, 50)
                ]),
                false
            )
            .is_err()
        );
        assert_eq!(f.writes.len(), 1);
        let mut f = Fake::new();
        execute(&mut f, &patch(&[(GodModeField::GpuTgp, 80)]), false).unwrap();
        assert!(f.writes.is_empty());
    }
}
