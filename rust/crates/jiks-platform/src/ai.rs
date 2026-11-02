//! Explicit Lenovo balanced-mode AI submodes. A process/game policy belongs to
//! runtime; discovering this feature never enables it or enumerates processes.
use crate::{
    domain::{ExplicitAction, monotonic_ms},
    ownership::OwnerToken,
    wmi_native::{MethodInput, WmiFacade},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntelligentProcess {
    pub processname: String,
    pub mode: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiSnapshot {
    pub submode: u32,
    pub raw_mode: u32,
    pub ac_connected: bool,
    pub modes: Vec<u32>,
    pub process_modes: Vec<IntelligentProcess>,
    pub process_list_error: Option<String>,
}
pub fn validate(snapshot: &AiSnapshot, target: u32, cpu_active: bool) -> Result<()> {
    ensure!(
        !cpu_active,
        "Stop and verify CPU restoration before changing AI firmware policy"
    );
    ensure!(
        snapshot.raw_mode == 2,
        "AI submode is writable only in balanced firmware mode"
    );
    ensure!(
        target == 0 || snapshot.ac_connected,
        "AC power is required to enable AI submode"
    );
    ensure!(
        snapshot.modes.contains(&target),
        "AI target is not the original 0/1 mode or an advertised process submode"
    );
    Ok(())
}
pub trait AiFirmware {
    fn guard(&mut self, deadline: u64) -> Result<()>;
    fn read(&mut self) -> Result<AiSnapshot>;
    fn validate_write(&mut self, target: u32) -> Result<()>;
    fn write(&mut self, target: u32, expected: &AiSnapshot, deadline: u64) -> Result<()>;
}
pub fn execute(
    f: &mut impl AiFirmware,
    target: u32,
    deadline: u64,
    cpu_active: bool,
) -> Result<AiSnapshot> {
    f.guard(deadline)?;
    let before = f.read()?;
    validate(&before, target, cpu_active)?;
    f.validate_write(target)?;
    if before.submode == target {
        return Ok(before);
    }
    f.guard(deadline)?;
    let latest = f.read()?;
    validate(&latest, target, cpu_active)?;
    ensure!(
        before.submode == latest.submode && before.raw_mode == latest.raw_mode,
        "AI policy changed externally before write"
    );
    f.write(target, &latest, deadline)?;
    let actual = f.read()?;
    ensure!(
        actual.submode == target && actual.raw_mode == before.raw_mode,
        "AI readback mismatch after attempted setter; no retry/rollback was issued"
    );
    Ok(actual)
}
fn params(
    v: impl IntoIterator<Item = (&'static str, MethodInput)>,
) -> BTreeMap<String, MethodInput> {
    v.into_iter().map(|(k, v)| (k.into(), v)).collect()
}
fn game(f: &WmiFacade, m: &str) -> Result<u32> {
    crate::system_controls::integer(
        &f.method_read("LENOVO_GAMEZONE_DATA", m, BTreeMap::new())?,
        "Data",
    )
}
pub(crate) fn read(f: &WmiFacade) -> Result<AiSnapshot> {
    let submode = game(f, "GetIntelligentSubMode")?;
    let raw_mode = game(f, "GetSmartFanMode")?;
    let mut modes = BTreeSet::from([0, 1]);
    let (process_modes, process_list_error) = match f
        .query::<IntelligentProcess>("SELECT processname,mode FROM LENOVO_INTELLIGENT_OP_LIST")
    {
        Ok(rows) => {
            for row in &rows {
                modes.insert(row.mode);
            }
            (rows, None)
        }
        Err(e) => (Vec::new(), Some(format!("{e:#}"))),
    };
    Ok(AiSnapshot {
        submode,
        raw_mode,
        ac_connected: crate::ac_connected()?,
        modes: modes.into_iter().collect(),
        process_modes,
        process_list_error,
    })
}
pub(crate) struct NativeAi<'a> {
    pub facade: &'a WmiFacade,
    pub owner: &'a OwnerToken,
    pub action: &'a ExplicitAction,
}
impl AiFirmware for NativeAi<'_> {
    fn guard(&mut self, d: u64) -> Result<()> {
        crate::ensure_no_other_controllers()?;
        ensure!(monotonic_ms() < d, "AI action deadline expired");
        Ok(())
    }
    fn read(&mut self) -> Result<AiSnapshot> {
        read(self.facade)
    }
    fn validate_write(&mut self, target: u32) -> Result<()> {
        self.facade.validate_inputs(
            "LENOVO_GAMEZONE_DATA",
            "SetIntelligentSubMode",
            &params([("Data", MethodInput::U32(target))]),
        )
    }
    fn write(&mut self, target: u32, expected: &AiSnapshot, d: u64) -> Result<()> {
        self.facade.method_write_guarded(
            self.owner,
            self.action,
            "LENOVO_GAMEZONE_DATA",
            "SetIntelligentSubMode",
            params([("Data", MethodInput::U32(target))]),
            || {
                crate::ensure_no_other_controllers()?;
                ensure!(monotonic_ms() < d, "AI deadline expired before ExecMethod");
                let latest = read(self.facade)?;
                validate(&latest, target, false)?;
                ensure!(
                    latest.submode == expected.submode && latest.raw_mode == expected.raw_mode,
                    "AI mode changed at invocation boundary"
                );
                ensure!(monotonic_ms() < d, "AI deadline expired during validation");
                Ok(())
            },
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        snapshot: AiSnapshot,
        writes: Vec<u32>,
        now: u64,
        alter_mode: bool,
    }
    impl AiFirmware for Fake {
        fn guard(&mut self, d: u64) -> Result<()> {
            ensure!(self.now < d, "expired");
            Ok(())
        }
        fn read(&mut self) -> Result<AiSnapshot> {
            Ok(self.snapshot.clone())
        }
        fn validate_write(&mut self, _: u32) -> Result<()> {
            Ok(())
        }
        fn write(&mut self, v: u32, _: &AiSnapshot, _: u64) -> Result<()> {
            self.writes.push(v);
            self.snapshot.submode = v;
            if self.alter_mode {
                self.snapshot.raw_mode = 3
            }
            Ok(())
        }
    }
    fn fake() -> Fake {
        Fake {
            snapshot: AiSnapshot {
                submode: 0,
                raw_mode: 2,
                ac_connected: true,
                modes: vec![0, 1, 7],
                process_modes: vec![],
                process_list_error: None,
            },
            writes: vec![],
            now: 0,
            alter_mode: false,
        }
    }
    #[test]
    fn unsupported_expired_cpu_or_dc_enable_never_write() {
        for test in 0..4 {
            let mut f = fake();
            let (v, d, active) = match test {
                0 => (2, 1, false),
                1 => (1, 0, false),
                2 => (1, 1, true),
                _ => (1, 1, false),
            };
            if test == 3 {
                f.snapshot.ac_connected = false;
            }
            assert!(execute(&mut f, v, d, active).is_err());
            assert!(f.writes.is_empty());
        }
    }
    #[test]
    fn advertised_process_submode_is_valid_and_disable_on_dc_works() {
        let mut f = fake();
        assert_eq!(execute(&mut f, 7, 1, false).unwrap().submode, 7);
        f.snapshot.ac_connected = false;
        assert_eq!(execute(&mut f, 0, 1, false).unwrap().submode, 0);
        assert_eq!(f.writes, vec![7, 0]);
    }
    #[test]
    fn mode_change_after_setter_cannot_report_success() {
        let mut f = fake();
        f.alter_mode = true;
        assert!(execute(&mut f, 1, 1, false).is_err());
        assert_eq!(f.writes, vec![1]);
    }
}
