//! Fixed-fork NVAPI P-state selection. No GPU initialization on import.
//! ABI source: gpu::ABI_SOURCE, PrivateRatedTdpControlV1 (12 bytes, v1),
//! PrivateClockBoostLockV2 (780 bytes, v2), PerformanceStates20ClockEntryV1.
use crate::gpu::{self, AbiBuffer, NvApiTransport, NvFunction};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
pub const SET_RATED_TDP: u32 = 0xc9e9bb33;
pub const ENABLE_DYNAMIC_PSTATES: u32 = 0xfa579a0f;
/// Public NVIDIA ABI: https://docs.nvidia.com/nvapi/group__gpupstate.html
/// QueryInterface ID: NVIDIA/nvapi nvapi_interface.h. A read, never a lock/setter.
pub const GET_CURRENT_PSTATE: u32 = 0x927da4f6;
pub(crate) fn current(mut call: impl FnMut(&mut u32) -> Result<()>) -> Result<u32> {
    let mut value = u32::MAX;
    call(&mut value)?;
    ensure!(
        value < 16,
        "Current GPU P-state is undefined or unknown ({value:#x})"
    );
    Ok(value)
}
#[cfg(test)]
#[test]
fn current_pstate_requires_actual_output_never_zero_initialization() {
    assert!(current(|_| Ok(())).is_err());
    assert!(
        current(|value| {
            *value = 16;
            Ok(())
        })
        .is_err()
    );
    for actual in [0, 8, 15] {
        assert_eq!(
            current(|value| {
                *value = actual;
                Ok(())
            })
            .unwrap(),
            actual
        );
    }
    assert!(
        current(|value| {
            *value = 0;
            anyhow::bail!("getter refused")
        })
        .is_err()
    );
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PerformanceOption {
    pub state: u32,
    pub graphics_max_frequency_khz: u32,
}
pub fn options(raw: &AbiBuffer) -> Result<Vec<PerformanceOption>> {
    let parsed = gpu::Pstates::decode(raw.clone())?;
    let raw = &parsed.raw;
    let states = raw.get(8);
    let clocks = raw.get(12);
    let mut result = Vec::new();
    for index in 0..states as usize {
        let base = 20 + index * 456;
        let state = raw.get(base);
        ensure!(
            state < 16 && !result.iter().any(|v: &PerformanceOption| v.state == state),
            "Invalid/duplicate P-state ID"
        );
        let mut graphics = None;
        for clock in 0..clocks as usize {
            let offset = base + 8 + clock * 44;
            if raw.get(offset) != 0 {
                continue;
            }
            ensure!(graphics.is_none(), "Duplicate graphics clock in P-state");
            let frequency = match raw.get(offset + 4) {
                0 => raw.get(offset + 24),
                1 => {
                    let min = raw.get(offset + 24);
                    let max = raw.get(offset + 28);
                    ensure!(min <= max, "Invalid P-state frequency range");
                    max
                }
                _ => anyhow::bail!("Unknown P-state clock union discriminant"),
            };
            ensure!(
                frequency > 0,
                "P-state has no advertised graphics frequency"
            );
            graphics = Some(frequency);
        }
        if let Some(graphics_max_frequency_khz) = graphics {
            result.push(PerformanceOption {
                state,
                graphics_max_frequency_khz,
            });
        }
    }
    result.sort_by_key(|v| v.state);
    Ok(result)
}
pub fn rated_tdp(enabled: bool) -> AbiBuffer {
    let mut p = AbiBuffer::new(12, 1);
    p.set(4, 1);
    p.set(8, if enabled { 3 } else { 0 });
    p
}
fn locks(raw: &AbiBuffer) -> Result<Vec<[u32; 6]>> {
    raw.validate(gpu::LOCK_BYTES, 2)?;
    let count = raw.get(8) as usize;
    ensure!(count <= 32, "Clock-lock count exceeds fixed ABI");
    let mut result = Vec::new();
    for i in 0..count {
        let base = 12 + i * 24;
        let entry = std::array::from_fn(|word| raw.get(base + word * 4));
        ensure!(
            !result.iter().any(|v: &[u32; 6]| v[0] == entry[0]),
            "Duplicate clock-lock domain"
        );
        result.push(entry)
    }
    Ok(result)
}
pub fn lock_packet(before: &AbiBuffer, state: Option<&PerformanceOption>) -> Result<AbiBuffer> {
    let mut values = locks(before)?;
    values.retain(|v| ![0, 1, 4, 5].contains(&v[0]));
    for domain in [0, 1, 4, 5] {
        let (mode, value) = match state {
            None => (0, 0),
            Some(p) if domain == 0 || domain == 1 => (2, p.graphics_max_frequency_khz),
            Some(p) => (1, p.state),
        };
        values.push([domain, 0, mode, 0, value, 0]);
    }
    ensure!(
        values.len() <= 32,
        "P-state lock would exceed fixed domain array"
    );
    let mut raw = AbiBuffer::new(gpu::LOCK_BYTES, 2);
    raw.set(8, values.len() as u32);
    for (i, value) in values.iter().enumerate() {
        for (word, value) in value.iter().enumerate() {
            raw.set(12 + i * 24 + word * 4, *value)
        }
    }
    Ok(raw)
}
fn verify_locks(expected: &AbiBuffer, observed: &AbiBuffer) -> Result<()> {
    let expected = locks(expected)?;
    let observed = locks(observed)?;
    ensure!(
        expected.len() == observed.len(),
        "Clock-lock domain set changed after planning"
    );
    for value in expected {
        let actual = observed
            .iter()
            .find(|v| v[0] == value[0])
            .context("Clock-lock domain absent on readback")?;
        ensure!(
            actual == &value,
            "P-state/other clock-lock domain readback mismatch"
        );
    }
    Ok(())
}
pub trait PerformanceTransport: NvApiTransport {
    fn guard(&mut self, deadline: u64, locking: bool) -> Result<()>;
    fn preflight(&mut self, dynamic: bool) -> Result<()>;
    fn set_rated_tdp(&mut self, payload: &mut AbiBuffer) -> Result<()>;
    fn enable_dynamic(&mut self) -> Result<()>;
}
pub fn read_locks(f: &mut impl NvApiTransport) -> Result<AbiBuffer> {
    let mut p = AbiBuffer::new(gpu::LOCK_BYTES, 2);
    f.call(NvFunction::GetClockBoostLock, &mut p)?;
    locks(&p)?;
    Ok(p)
}
pub fn read(f: &mut impl NvApiTransport) -> Result<serde_json::Value> {
    let states = gpu::read_pstates(f)?;
    let opts = options(&states.raw)?;
    let raw = read_locks(f)?;
    Ok(
        serde_json::json!({"states":opts,"locks":locks(&raw)?,"source":gpu::ABI_SOURCE,"gpu_initialized_explicitly":true}),
    )
}
pub fn execute(
    f: &mut impl PerformanceTransport,
    state: Option<u32>,
    deadline: u64,
) -> Result<serde_json::Value> {
    f.guard(deadline, state.is_some())?;
    let states = gpu::read_pstates(f)?;
    let opts = options(&states.raw)?;
    let target = state
        .map(|id| {
            opts.iter()
                .find(|v| v.state == id)
                .context("P-state is not advertised by this physical GPU")
        })
        .transpose()?;
    let before = read_locks(f)?;
    let expected = lock_packet(&before, target)?;
    let mut tdp = rated_tdp(state == Some(0));
    f.preflight(state.is_none())?;
    let mut completed = Vec::new();
    let mut attempted = "rated_tdp";
    let apply = (|| -> Result<()> {
        f.guard(deadline, state.is_some())?;
        verify_locks(&before, &read_locks(f)?)?;
        f.set_rated_tdp(&mut tdp)?;
        completed.push("rated_tdp_request");
        attempted = "clock_lock";
        f.guard(deadline, state.is_some())?;
        verify_locks(&before, &read_locks(f)?)?;
        let mut write = expected.clone();
        f.call(NvFunction::SetClockBoostLock, &mut write)?;
        verify_locks(&expected, &read_locks(f)?)?;
        completed.push("clock_lock_verified");
        if state.is_none() {
            attempted = "dynamic_pstates";
            f.guard(deadline, false)?;
            f.enable_dynamic()?;
            completed.push("dynamic_pstates_request");
            verify_locks(&expected, &read_locks(f)?)?;
        }
        Ok(())
    })();
    if let Err(error) = apply {
        anyhow::bail!(
            "GPU performance operation failed after {:?}; attempted {}; no retry/rollback: {error:#}",
            completed,
            attempted
        )
    }
    Ok(
        serde_json::json!({"requested_state":state,"selected_frequency_khz":target.map(|v|v.graphics_max_frequency_khz),"clock_lock_readback_verified":true,"unrelated_domains_preserved":true,"rated_tdp_request_accepted":true,"rated_tdp_readback_verified":false,"dynamic_enable_request_accepted":state.is_none(),"instantaneous_pstate_verified":false,"completed":completed}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    fn states() -> AbiBuffer {
        let mut p = AbiBuffer::new(gpu::PSTATE_V1_BYTES, 1);
        p.set(8, 1);
        p.set(12, 1);
        p.set(20, 0);
        p.set(28, 0);
        p.set(32, 1);
        p.set(52, 210000);
        p.set(56, 2400000);
        p
    }
    #[test]
    fn frequency_comes_from_clock_union_not_delta_range() {
        let mut p = states();
        p.set_i32(44, -500000);
        p.set_i32(48, 1000000);
        assert_eq!(options(&p).unwrap()[0].graphics_max_frequency_khz, 2400000);
        p.set(32, 0);
        p.set(52, 1500000);
        assert_eq!(options(&p).unwrap()[0].graphics_max_frequency_khz, 1500000);
    }
    #[test]
    fn fixed_tdp_and_lock_packets_preserve_voltage_domain_six() {
        assert_eq!(rated_tdp(true).words(), &[0x1000c, 1, 3]);
        assert_eq!(rated_tdp(false).words(), &[0x1000c, 1, 0]);
        let before = gpu::voltage_lock(Some(900000));
        let p = lock_packet(
            &before,
            Some(&PerformanceOption {
                state: 2,
                graphics_max_frequency_khz: 1500000,
            }),
        )
        .unwrap();
        let entries = locks(&p).unwrap();
        assert_eq!(entries.len(), 5);
        assert_eq!(
            entries.iter().find(|v| v[0] == 6).unwrap(),
            &[6, 0, 3, 0, 900000, 0]
        );
        assert_eq!(entries.iter().find(|v| v[0] == 0).unwrap()[4], 1500000);
        assert_eq!(entries.iter().find(|v| v[0] == 4).unwrap()[4], 2);
        let reset = lock_packet(&p, None).unwrap();
        assert_eq!(
            locks(&reset).unwrap().iter().find(|v| v[0] == 6).unwrap()[4],
            900000
        );
    }
    struct Fake {
        raw: AbiBuffer,
        locks: AbiBuffer,
        writes: Vec<&'static str>,
        budget: u32,
        preflight_fails: bool,
    }
    impl NvApiTransport for Fake {
        fn call(&mut self, f: NvFunction, p: &mut AbiBuffer) -> Result<()> {
            match f {
                NvFunction::GetPstates20 => *p = self.raw.clone(),
                NvFunction::GetClockBoostLock => *p = self.locks.clone(),
                NvFunction::SetClockBoostLock => {
                    self.writes.push("lock");
                    self.locks = p.clone()
                }
                _ => anyhow::bail!("unexpected fake call"),
            };
            Ok(())
        }
    }
    impl PerformanceTransport for Fake {
        fn guard(&mut self, _: u64, _: bool) -> Result<()> {
            ensure!(self.budget > 0, "expired");
            Ok(())
        }
        fn preflight(&mut self, _: bool) -> Result<()> {
            ensure!(!self.preflight_fails, "missing function");
            Ok(())
        }
        fn set_rated_tdp(&mut self, _: &mut AbiBuffer) -> Result<()> {
            self.writes.push("tdp");
            self.budget -= 1;
            Ok(())
        }
        fn enable_dynamic(&mut self) -> Result<()> {
            self.writes.push("dynamic");
            Ok(())
        }
    }
    fn fake() -> Fake {
        Fake {
            raw: states(),
            locks: gpu::voltage_lock(Some(900000)),
            writes: vec![],
            budget: 10,
            preflight_fails: false,
        }
    }
    #[test]
    fn unsupported_target_or_late_preflight_never_writes() {
        let mut f = fake();
        assert!(execute(&mut f, Some(15), 10).is_err());
        assert!(f.writes.is_empty());
        f.preflight_fails = true;
        assert!(execute(&mut f, Some(0), 10).is_err());
        assert!(f.writes.is_empty());
    }
    #[test]
    fn late_tdp_completion_stops_clock_write_and_reports_partial() {
        let mut f = fake();
        f.budget = 1;
        assert!(execute(&mut f, Some(0), 10).is_err());
        assert_eq!(f.writes, vec!["tdp"]);
    }
    #[test]
    fn reset_verifies_domains_but_does_not_claim_instantaneous_pstate() {
        let mut f = fake();
        let r = execute(&mut f, None, 10).unwrap();
        assert_eq!(f.writes, vec!["tdp", "lock", "dynamic"]);
        assert_eq!(r["clock_lock_readback_verified"], true);
        assert_eq!(r["instantaneous_pstate_verified"], false);
        assert_eq!(r["rated_tdp_readback_verified"], false);
    }
}
