//! Persist a verified brightness value to AC/DC settings of every current power
//! plan. Enumeration and all reads finish before the first setter.
use crate::{
    domain::ExplicitAction,
    ownership::OwnerToken,
    system_controls::{parse_guid, power_plans},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, ptr};
use windows_sys::{
    Win32::System::Power::{
        PowerReadACValueIndex, PowerReadDCValueIndex, PowerWriteACValueIndex,
        PowerWriteDCValueIndex,
    },
    core::GUID,
};
const VIDEO: GUID = GUID::from_u128(0x7516b95f_f776_4464_8c53_06167f40cc99);
const BRIGHTNESS: GUID = GUID::from_u128(0xaded5e82_b909_4619_9949_f5d71dac0bcb);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Supply {
    Ac,
    Dc,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setting {
    pub plan: String,
    pub supply: Supply,
    pub before: u32,
    pub target: u32,
}
#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub brightness: u32,
    pub verified: Vec<Setting>,
    pub complete: bool,
}
pub trait Port {
    fn plans(&mut self) -> Result<Vec<String>>;
    fn read(&mut self, plan: &str, supply: Supply) -> Result<u32>;
    fn write(&mut self, plan: &str, supply: Supply, value: u32) -> Result<()>;
}
pub fn plan(port: &mut impl Port, brightness: u32) -> Result<Vec<Setting>> {
    ensure!(brightness <= 100, "Brightness must be 0..100");
    let plans = port.plans()?;
    ensure!(
        !plans.is_empty() && plans.len() <= 4096,
        "No power plans or enumeration budget exceeded"
    );
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for id in plans {
        parse_guid(&id)?;
        ensure!(seen.insert(id.to_ascii_lowercase()), "Duplicate power plan");
        for supply in [Supply::Ac, Supply::Dc] {
            let before = port.read(&id, supply)?;
            ensure!(before <= 100, "Existing power-plan brightness is invalid");
            out.push(Setting {
                plan: id.clone(),
                supply,
                before,
                target: brightness,
            });
        }
    }
    Ok(out)
}
pub fn synchronize(
    port: &mut impl Port,
    brightness: u32,
    mut guard: impl FnMut() -> Result<()>,
) -> Result<Outcome> {
    let settings = plan(port, brightness)?;
    let mut verified = Vec::new();
    for setting in settings {
        let result = (|| -> Result<()> {
            guard()?;
            ensure!(
                port.read(&setting.plan, setting.supply)? == setting.before,
                "Power-plan brightness changed externally before mutation"
            );
            if setting.before != setting.target {
                guard()?;
                port.write(&setting.plan, setting.supply, setting.target)?;
            }
            ensure!(
                port.read(&setting.plan, setting.supply)? == setting.target,
                "Power-plan brightness readback mismatch"
            );
            Ok(())
        })();
        result.with_context(||format!("Brightness sync stopped after {} verified AC/DC entries; partial changes remain and were not rolled back",verified.len()))?;
        verified.push(setting);
    }
    Ok(Outcome {
        brightness,
        verified,
        complete: true,
    })
}
struct Native;
impl Port for Native {
    fn plans(&mut self) -> Result<Vec<String>> {
        Ok(power_plans()?.into_iter().map(|plan| plan.id).collect())
    }
    fn read(&mut self, plan: &str, supply: Supply) -> Result<u32> {
        let plan = parse_guid(plan)?;
        let mut value = 0;
        let code = unsafe {
            match supply {
                Supply::Ac => {
                    PowerReadACValueIndex(ptr::null_mut(), &plan, &VIDEO, &BRIGHTNESS, &mut value)
                }
                Supply::Dc => {
                    PowerReadDCValueIndex(ptr::null_mut(), &plan, &VIDEO, &BRIGHTNESS, &mut value)
                }
            }
        };
        ensure!(code == 0, "Read {supply:?} brightness failed ({code})");
        Ok(value)
    }
    fn write(&mut self, plan: &str, supply: Supply, value: u32) -> Result<()> {
        let plan = parse_guid(plan)?;
        let code = unsafe {
            match supply {
                Supply::Ac => {
                    PowerWriteACValueIndex(ptr::null_mut(), &plan, &VIDEO, &BRIGHTNESS, value)
                }
                Supply::Dc => {
                    PowerWriteDCValueIndex(ptr::null_mut(), &plan, &VIDEO, &BRIGHTNESS, value)
                }
            }
        };
        ensure!(code == 0, "Write {supply:?} brightness failed ({code})");
        Ok(())
    }
}
pub(crate) fn apply(
    _owner: &OwnerToken,
    action: &ExplicitAction,
    brightness: u32,
) -> Result<Outcome> {
    synchronize(&mut Native, brightness, || action.check_write_boundary())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    const A: &str = "381b4222-f694-41f0-9685-ff5bb260df2e";
    const B: &str = "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c";
    struct Fake {
        values: BTreeMap<(String, u8), u32>,
        writes: usize,
        fail_read: bool,
        fail_write: Option<usize>,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                values: [
                    ((A.into(), 0), 30),
                    ((A.into(), 1), 40),
                    ((B.into(), 0), 50),
                    ((B.into(), 1), 60),
                ]
                .into(),
                writes: 0,
                fail_read: false,
                fail_write: None,
            }
        }
    }
    impl Port for Fake {
        fn plans(&mut self) -> Result<Vec<String>> {
            Ok(vec![A.into(), B.into()])
        }
        fn read(&mut self, p: &str, s: Supply) -> Result<u32> {
            ensure!(!self.fail_read, "fake read denied");
            Ok(self.values[&(p.into(), s as u8)])
        }
        fn write(&mut self, p: &str, s: Supply, v: u32) -> Result<()> {
            ensure!(self.fail_write != Some(self.writes), "fake setter failure");
            self.writes += 1;
            self.values.insert((p.into(), s as u8), v);
            Ok(())
        }
    }
    #[test]
    fn all_plans_and_both_supplies_are_verified_without_activating_a_scheme() {
        let mut fake = Fake::new();
        let outcome = synchronize(&mut fake, 75, || Ok(())).unwrap();
        assert_eq!(outcome.verified.len(), 4);
        assert_eq!(fake.writes, 4);
        assert!(fake.values.values().all(|v| *v == 75));
    }
    #[test]
    fn preflight_read_failure_and_invalid_values_issue_no_setter() {
        let mut fake = Fake::new();
        fake.fail_read = true;
        assert!(synchronize(&mut fake, 50, || Ok(())).is_err());
        assert_eq!(fake.writes, 0);
        fake.fail_read = false;
        assert!(synchronize(&mut fake, 101, || Ok(())).is_err());
        assert_eq!(fake.writes, 0);
    }
    #[test]
    fn partial_failure_and_revoked_boundary_never_claim_complete() {
        let mut fake = Fake::new();
        fake.fail_write = Some(1);
        let error = synchronize(&mut fake, 80, || Ok(())).unwrap_err();
        assert!(error.to_string().contains("1 verified"));
        assert_eq!(fake.writes, 1);
        let mut fake = Fake::new();
        assert!(synchronize(&mut fake, 80, || anyhow::bail!("revoked")).is_err());
        assert_eq!(fake.writes, 0);
    }
}
