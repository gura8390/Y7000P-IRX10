//! Firmware mux/IGPU routes. Discovery uses WMI getters only; no GPU library is
//! loaded. BIOS save acknowledges a pending selection, never a physical reboot.
use crate::{
    domain::{ExplicitAction, monotonic_ms},
    ownership::OwnerToken,
    wmi_native::{MethodInput, WmiFacade},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fmt};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
pub enum HybridMode {
    On = 0,
    OnIGPUOnly = 1,
    OnAuto = 2,
    UMA = 3,
    Off = 4,
}
impl HybridMode {
    pub fn from_raw(v: u32) -> Result<Self> {
        Ok(match v {
            0 => Self::On,
            1 => Self::OnIGPUOnly,
            2 => Self::OnAuto,
            3 => Self::UMA,
            4 => Self::Off,
            _ => anyhow::bail!("Unknown hybrid mode"),
        })
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::On => "混合模式",
            Self::OnIGPUOnly => "混合 · 仅核显",
            Self::OnAuto => "混合 · 自动",
            Self::UMA => "UMA（需重启）",
            Self::Off => "独显直连（可能需重启）",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BiosGraphics {
    Switchable,
    Uma,
    Discrete,
}
impl BiosGraphics {
    pub fn value(self) -> &'static str {
        match self {
            Self::Switchable => "Switchable Graphics",
            Self::Uma => "UMA Graphics",
            Self::Discrete => "Discrete Graphics",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "Switchable Graphics" => Some(Self::Switchable),
            "UMA Graphics" => Some(Self::Uma),
            "Discrete Graphics" => Some(Self::Discrete),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IgpuBackend {
    GameZone,
    Capability,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HybridSnapshot {
    #[serde(default)]
    pub gsync_supported: Option<bool>,
    #[serde(default)]
    pub igpu_supported: Option<bool>,
    pub gsync: Option<bool>,
    pub igpu_mode: Option<u32>,
    pub igpu_backend: Option<IgpuBackend>,
    pub bios_current: Option<BiosGraphics>,
    pub bios_selections: Vec<BiosGraphics>,
    pub errors: BTreeMap<String, String>,
}
impl HybridSnapshot {
    pub fn modes(&self) -> Vec<HybridMode> {
        let mut modes = Vec::new();
        // A failing getter on a supported/unknown companion route is not proof
        // that the route is absent. Otherwise an IGPU write could be advertised
        // as full Hybrid while the discrete mux remains selected.
        if (self.gsync.is_none() && self.gsync_supported != Some(false))
            || (self.igpu_mode.is_none() && self.igpu_supported != Some(false))
        {
            return modes;
        }
        if self.gsync.is_some() || self.igpu_mode.is_some() {
            modes.push(HybridMode::On);
        }
        if self.igpu_mode.is_some() {
            modes.extend([HybridMode::OnIGPUOnly, HybridMode::OnAuto]);
        }
        if self.gsync.is_some() {
            modes.push(HybridMode::Off);
        }
        if !modes.is_empty()
            && self.bios_current.is_some()
            && self.bios_selections.contains(&BiosGraphics::Uma)
        {
            modes.push(HybridMode::UMA);
        }
        modes
    }
    pub fn selected(&self) -> Option<HybridMode> {
        if self.bios_current == Some(BiosGraphics::Uma) {
            return Some(HybridMode::UMA);
        }
        if self.gsync == Some(true) {
            return Some(HybridMode::Off);
        }
        if self.gsync.is_none() && self.gsync_supported != Some(false) {
            return None;
        }
        match self.igpu_mode {
            Some(0) => Some(HybridMode::On),
            Some(1) => Some(HybridMode::OnIGPUOnly),
            Some(2) => Some(HybridMode::OnAuto),
            None if self.gsync == Some(false) => Some(HybridMode::On),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HybridStep {
    GSync(bool),
    Igpu(u32),
    BiosSet(BiosGraphics),
    BiosSave(BiosGraphics),
}
pub fn plan(snapshot: &HybridSnapshot, target: HybridMode) -> Result<Vec<HybridStep>> {
    ensure!(
        snapshot.modes().contains(&target),
        "Hybrid mode was not advertised by completed firmware discovery"
    );
    if target == HybridMode::UMA || snapshot.bios_current == Some(BiosGraphics::Uma) {
        let value = match target {
            HybridMode::UMA => BiosGraphics::Uma,
            HybridMode::Off => BiosGraphics::Discrete,
            _ => BiosGraphics::Switchable,
        };
        ensure!(
            snapshot.bios_selections.contains(&value),
            "Exact BIOS GraphicsDevice selection is absent"
        );
        return Ok(if snapshot.bios_current == Some(value) {
            vec![]
        } else {
            vec![HybridStep::BiosSet(value), HybridStep::BiosSave(value)]
        });
    }
    let mut steps = Vec::new();
    let gsync = target == HybridMode::Off;
    if let Some(current) = snapshot.gsync {
        if current != gsync {
            steps.push(HybridStep::GSync(gsync));
        }
    }
    let igpu = match target {
        HybridMode::OnIGPUOnly => 1,
        HybridMode::OnAuto => 2,
        _ => 0,
    };
    if let Some(current) = snapshot.igpu_mode {
        if current != igpu {
            steps.push(HybridStep::Igpu(igpu));
        }
    }
    Ok(steps)
}
pub trait HybridFirmware {
    fn before_execution(&mut self) -> Result<()> {
        Ok(())
    }
    fn guard(&mut self, deadline: u64) -> Result<()>;
    fn read(&mut self) -> Result<HybridSnapshot>;
    fn validate(&mut self, step: &HybridStep) -> Result<()>;
    fn write(&mut self, step: &HybridStep, expected: &HybridSnapshot, deadline: u64) -> Result<()>;
}
#[derive(Debug, Clone, Serialize)]
pub struct HybridResult {
    pub requested: HybridMode,
    pub observed: HybridSnapshot,
    pub completed_steps: Vec<HybridStep>,
    pub pending_reboot: bool,
    pub configuration_verified: bool,
    pub physical_topology_verified: bool,
    pub remaining_after_reboot: bool,
}
#[derive(Debug, Serialize)]
pub struct HybridFailure {
    pub attempted: Option<HybridStep>,
    pub completed_steps: Vec<HybridStep>,
    pub observed: Option<HybridSnapshot>,
    pub error: String,
}
impl fmt::Display for HybridFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Hybrid operation failed; no rollback/retry was issued: {}",
            serde_json::to_string(self).unwrap_or_else(|_| self.error.clone())
        )
    }
}
impl std::error::Error for HybridFailure {}
pub fn execute(
    f: &mut impl HybridFirmware,
    target: HybridMode,
    deadline: u64,
    cpu_active: bool,
) -> Result<HybridResult> {
    ensure!(
        !cpu_active,
        "Stop and verify CPU restoration before changing GPU mux routes"
    );
    f.guard(deadline)?;
    let mut expected = f.read()?;
    let steps = plan(&expected, target)?;
    for step in &steps {
        f.validate(step)?;
    }
    f.guard(deadline)?;
    f.before_execution()?;
    let mut completed = Vec::new();
    let mut attempted = None;
    let mut bios_written = false;
    let mut gsync_written = false;
    let result = (|| -> Result<()> {
        for step in &steps {
            f.guard(deadline)?;
            let actual = f.read()?;
            ensure!(
                same_state(&actual, &expected),
                "External mux/IGPU/BIOS change invalidated the operation"
            );
            attempted = Some(step.clone());
            f.write(step, &expected, deadline)?;
            match *step {
                HybridStep::BiosSet(v) => {
                    let actual = f.read()?;
                    let mut staged = expected.clone();
                    staged.bios_current = Some(v);
                    ensure!(
                        same_state(&actual, &expected) || same_state(&actual, &staged),
                        "BIOS staging changed unrelated mux state"
                    );
                    expected = actual;
                } // Save below is still mandatory.
                HybridStep::BiosSave(v) => {
                    let actual = f.read()?;
                    ensure!(
                        actual.bios_current == Some(v),
                        "Saved BIOS selection readback mismatch; reboot was not requested"
                    );
                    expected = actual;
                    bios_written = true;
                }
                HybridStep::GSync(v) => {
                    let actual = f.read()?;
                    ensure!(
                        actual.gsync == Some(v)
                            && actual.igpu_mode == expected.igpu_mode
                            && actual.bios_current == expected.bios_current,
                        "GSync/cross-field readback mismatch"
                    );
                    expected = actual;
                    gsync_written = true;
                }
                HybridStep::Igpu(v) => {
                    let actual = f.read()?;
                    ensure!(
                        actual.igpu_mode == Some(v)
                            && actual.gsync == expected.gsync
                            && actual.bios_current == expected.bios_current,
                        "IGPU/cross-field readback mismatch"
                    );
                    expected = actual;
                }
            }
            completed.push(step.clone());
            attempted = None;
        }
        Ok(())
    })();
    if let Err(error) = result {
        return Err(HybridFailure {
            attempted,
            completed_steps: completed,
            observed: f.read().ok(),
            error: format!("{error:#}"),
        }
        .into());
    }
    let pending = bios_written || gsync_written;
    Ok(HybridResult {
        requested: target,
        observed: expected,
        completed_steps: completed,
        pending_reboot: pending,
        configuration_verified: true,
        physical_topology_verified: false,
        remaining_after_reboot: bios_written
            && matches!(target, HybridMode::OnIGPUOnly | HybridMode::OnAuto),
    })
}
fn same_state(a: &HybridSnapshot, b: &HybridSnapshot) -> bool {
    a.gsync_supported == b.gsync_supported
        && a.igpu_supported == b.igpu_supported
        && a.gsync == b.gsync
        && a.igpu_mode == b.igpu_mode
        && a.igpu_backend == b.igpu_backend
        && a.bios_current == b.bios_current
        && a.bios_selections == b.bios_selections
}
fn params(
    v: impl IntoIterator<Item = (&'static str, MethodInput)>,
) -> BTreeMap<String, MethodInput> {
    v.into_iter().map(|(k, v)| (k.into(), v)).collect()
}
fn number(map: &BTreeMap<String, Value>, name: &str) -> Result<u32> {
    crate::system_controls::integer(map, name)
}
fn game(f: &WmiFacade, method: &str) -> Result<u32> {
    number(
        &f.method_read("LENOVO_GAMEZONE_DATA", method, BTreeMap::new())?,
        "Data",
    )
}
fn mapped(f: &WmiFacade, id: u32) -> Result<u32> {
    number(
        &f.method_read(
            "LENOVO_OTHER_METHOD",
            "GetFeatureValue",
            params([("IDs", MethodInput::U32(id))]),
        )?,
        "Value",
    )
}
fn output_string<'a>(map: &'a BTreeMap<String, Value>, name: &str) -> Result<&'a str> {
    map.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| v.as_str())
        .with_context(|| format!("Missing string firmware output {name}"))
}
pub(crate) fn read(f: &WmiFacade) -> HybridSnapshot {
    let mut s = HybridSnapshot::default();
    match game(f, "IsSupportGSync") {
        Ok(v) => s.gsync_supported = Some(v > 0),
        Err(e) => {
            s.errors.insert("gsync_support".into(), format!("{e:#}"));
        }
    }
    let gsync = (|| -> Result<bool> {
        ensure!(
            s.gsync_supported == Some(true),
            "GSync capability absent/unknown"
        );
        let value = game(f, "GetGSyncStatus")?;
        ensure!(value <= 1, "Unknown GSync state");
        Ok(value == 1)
    })();
    match gsync {
        Ok(v) => s.gsync = Some(v),
        Err(e) => {
            s.errors.insert("gsync".into(), format!("{e:#}"));
        }
    }
    let igpu = (|| -> Result<(u32, IgpuBackend)> {
        let legacy = game(f, "IsSupportIGPUMode");
        if let Err(error) = &legacy {
            s.errors.insert("igpu_support".into(), format!("{error:#}"));
        }
        if legacy.as_ref().is_ok_and(|v| *v > 0) {
            s.igpu_supported = Some(true);
            let v = game(f, "GetIGPUModeStatus")?;
            ensure!(v <= 2, "Unknown IGPU state");
            return Ok((v, IgpuBackend::GameZone));
        }
        let rows: Vec<Value> =
            f.query("SELECT IDs FROM LENOVO_CAPABILITY_DATA_00 WHERE IDs=65536")?;
        if rows.is_empty() && matches!(legacy, Ok(0)) {
            s.igpu_supported = Some(false);
        }
        ensure!(rows.len() == 1, "IGPU capability absent/ambiguous");
        s.igpu_supported = Some(true);
        let v = mapped(f, 0x00010000)?;
        ensure!(v <= 2, "Unknown IGPU state");
        Ok((v, IgpuBackend::Capability))
    })();
    match igpu {
        Ok((v, b)) => {
            s.igpu_mode = Some(v);
            s.igpu_backend = Some(b)
        }
        Err(e) => {
            s.errors.insert("igpu".into(), format!("{e:#}"));
        }
    }
    let bios = (|| -> Result<(BiosGraphics, Vec<BiosGraphics>)> {
        let rows: Vec<Value> = f.query("SELECT CurrentSetting FROM Lenovo_BiosSetting")?;
        let values = rows
            .iter()
            .filter_map(|row| row.get("CurrentSetting").and_then(Value::as_str))
            .filter_map(|raw| raw.strip_prefix("GraphicsDevice,"))
            .collect::<Vec<_>>();
        ensure!(values.len() == 1, "BIOS GraphicsDevice absent/ambiguous");
        let current = BiosGraphics::parse(values[0].split(',').next().unwrap_or(""))
            .context("Unknown BIOS GraphicsDevice value")?;
        let result = f.method_read(
            "Lenovo_GetBiosSelections",
            "GetBiosSelections",
            params([("Item", MethodInput::String("GraphicsDevice".into()))]),
        )?;
        let selections = output_string(&result, "Selections")?
            .split(',')
            .map(str::trim)
            .filter_map(BiosGraphics::parse)
            .collect::<Vec<_>>();
        ensure!(
            !selections.is_empty(),
            "No exact known BIOS graphics selections"
        );
        Ok((current, selections))
    })();
    match bios {
        Ok((v, opts)) => {
            s.bios_current = Some(v);
            s.bios_selections = opts
        }
        Err(e) => {
            s.errors.insert("bios".into(), format!("{e:#}"));
        }
    }
    s
}
pub(crate) struct NativeHybrid<'a> {
    pub facade: &'a WmiFacade,
    pub owner: &'a OwnerToken,
    pub action: &'a ExplicitAction,
    pub before_execution: Option<&'a mut dyn FnMut() -> Result<()>>,
}
impl NativeHybrid<'_> {
    fn method(
        &self,
        step: &HybridStep,
        backend: Option<IgpuBackend>,
    ) -> Result<(&'static str, &'static str, BTreeMap<String, MethodInput>)> {
        Ok(match step {
            HybridStep::GSync(v) => (
                "LENOVO_GAMEZONE_DATA",
                "SetGSyncStatus",
                params([("Data", MethodInput::U32(u32::from(*v)))]),
            ),
            HybridStep::Igpu(v) => match backend.context("IGPU backend missing")? {
                IgpuBackend::GameZone => (
                    "LENOVO_GAMEZONE_DATA",
                    "SetIGPUModeStatus",
                    params([("mode", MethodInput::U32(*v))]),
                ),
                IgpuBackend::Capability => (
                    "LENOVO_OTHER_METHOD",
                    "SetFeatureValue",
                    params([
                        ("IDs", MethodInput::U32(0x00010000)),
                        ("value", MethodInput::U32(*v)),
                    ]),
                ),
            },
            HybridStep::BiosSet(v) => (
                "Lenovo_SetBiosSetting",
                "SetBiosSetting",
                params([(
                    "parameter",
                    MethodInput::String(format!("GraphicsDevice,{},", v.value())),
                )]),
            ),
            HybridStep::BiosSave(_) => (
                "Lenovo_SaveBiosSettings",
                "SaveBiosSettings",
                BTreeMap::new(),
            ),
        })
    }
}
impl HybridFirmware for NativeHybrid<'_> {
    fn before_execution(&mut self) -> Result<()> {
        if let Some(prepare) = self.before_execution.take() {
            prepare()?;
        }
        Ok(())
    }
    fn guard(&mut self, deadline: u64) -> Result<()> {
        crate::ensure_no_other_controllers()?;
        ensure!(monotonic_ms() < deadline, "Mux action deadline expired");
        Ok(())
    }
    fn read(&mut self) -> Result<HybridSnapshot> {
        Ok(read(self.facade))
    }
    fn validate(&mut self, step: &HybridStep) -> Result<()> {
        let s = read(self.facade);
        let (c, m, p) = self.method(step, s.igpu_backend)?;
        self.facade.validate_inputs(c, m, &p)
    }
    fn write(&mut self, step: &HybridStep, expected: &HybridSnapshot, deadline: u64) -> Result<()> {
        let (c, m, p) = self.method(step, expected.igpu_backend)?;
        self.facade
            .method_write_guarded(self.owner, self.action, c, m, p, || {
                crate::ensure_no_other_controllers()?;
                ensure!(
                    monotonic_ms() < deadline,
                    "Mux deadline expired before ExecMethod"
                );
                ensure!(
                    same_state(&read(self.facade), expected),
                    "Mux state changed at firmware invocation boundary"
                );
                ensure!(
                    monotonic_ms() < deadline,
                    "Mux deadline expired during validation"
                );
                Ok(())
            })?;
        if matches!(step, HybridStep::Igpu(_))
            && expected.igpu_backend == Some(IgpuBackend::Capability)
        {
            ensure!(
                mapped(self.facade, 0x000f0000)? != 0,
                "IGPU firmware dGPU-change check failed"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        s: HybridSnapshot,
        writes: Vec<HybridStep>,
        now: u64,
        fail_validate: bool,
        fail_write: Option<usize>,
        external: bool,
    }
    impl HybridFirmware for Fake {
        fn guard(&mut self, d: u64) -> Result<()> {
            ensure!(self.now < d, "expired");
            Ok(())
        }
        fn read(&mut self) -> Result<HybridSnapshot> {
            Ok(self.s.clone())
        }
        fn validate(&mut self, step: &HybridStep) -> Result<()> {
            ensure!(
                !self.fail_validate || !matches!(step, HybridStep::Igpu(_)),
                "metadata"
            );
            Ok(())
        }
        fn write(&mut self, step: &HybridStep, _: &HybridSnapshot, _: u64) -> Result<()> {
            self.writes.push(step.clone());
            ensure!(
                self.fail_write != Some(self.writes.len()),
                "setter uncertain"
            );
            match *step {
                HybridStep::GSync(v) => self.s.gsync = Some(v),
                HybridStep::Igpu(v) => self.s.igpu_mode = Some(v),
                HybridStep::BiosSet(_) => {}
                HybridStep::BiosSave(v) => self.s.bios_current = Some(v),
            };
            if self.external {
                self.s.igpu_mode = Some(2)
            }
            Ok(())
        }
    }
    fn fake() -> Fake {
        Fake {
            s: HybridSnapshot {
                gsync: Some(false),
                igpu_mode: Some(1),
                igpu_backend: Some(IgpuBackend::GameZone),
                ..Default::default()
            },
            ..Default::default()
        }
    }
    #[test]
    fn missing_capability_is_not_a_guessed_mux() {
        let s = HybridSnapshot {
            gsync_supported: Some(false),
            igpu_mode: Some(0),
            ..Default::default()
        };
        assert_eq!(
            s.modes(),
            vec![HybridMode::On, HybridMode::OnIGPUOnly, HybridMode::OnAuto]
        );
        assert!(plan(&s, HybridMode::Off).is_err());
        assert!(plan(&s, HybridMode::UMA).is_err());
        let mut unknown = s;
        unknown.gsync_supported = None;
        assert!(unknown.modes().is_empty());
    }
    #[test]
    fn all_metadata_precedes_first_write() {
        let mut f = fake();
        f.fail_validate = true;
        assert!(execute(&mut f, HybridMode::Off, 10, false).is_err());
        assert!(f.writes.is_empty());
    }
    #[test]
    fn deadline_equal_and_cpu_session_issue_zero_writes() {
        let mut f = fake();
        assert!(execute(&mut f, HybridMode::Off, 0, false).is_err());
        assert!(execute(&mut f, HybridMode::Off, 10, true).is_err());
        assert!(f.writes.is_empty());
    }
    #[test]
    fn partial_mux_failure_is_not_swallowed_or_rolled_back() {
        let mut f = fake();
        f.fail_write = Some(2);
        let e = execute(&mut f, HybridMode::Off, 10, false).unwrap_err();
        let e = e.downcast_ref::<HybridFailure>().unwrap();
        assert_eq!(e.completed_steps, vec![HybridStep::GSync(true)]);
        assert_eq!(e.attempted, Some(HybridStep::Igpu(0)));
        assert_eq!(f.writes.len(), 2);
    }
    #[test]
    fn mux_flag_readback_is_not_physical_topology_proof() {
        let mut f = fake();
        let r = execute(&mut f, HybridMode::Off, 10, false).unwrap();
        assert!(r.pending_reboot && r.configuration_verified && !r.physical_topology_verified);
        assert_eq!(f.writes, vec![HybridStep::GSync(true), HybridStep::Igpu(0)]);
    }
    #[test]
    fn bios_set_save_and_selection_are_exact_and_pending() {
        let mut f = fake();
        f.s.bios_current = Some(BiosGraphics::Switchable);
        f.s.bios_selections = vec![BiosGraphics::Switchable, BiosGraphics::Uma];
        let r = execute(&mut f, HybridMode::UMA, 10, false).unwrap();
        assert!(r.pending_reboot && !r.physical_topology_verified);
        assert_eq!(
            f.writes,
            vec![
                HybridStep::BiosSet(BiosGraphics::Uma),
                HybridStep::BiosSave(BiosGraphics::Uma)
            ]
        );
        f.writes.clear();
        assert!(plan(&f.s, HybridMode::Off).is_err());
        assert!(f.writes.is_empty());
    }
    #[test]
    fn external_cross_field_change_stops_remaining_setters() {
        let mut f = fake();
        f.external = true;
        assert!(execute(&mut f, HybridMode::Off, 10, false).is_err());
        assert_eq!(f.writes.len(), 1);
    }
}
