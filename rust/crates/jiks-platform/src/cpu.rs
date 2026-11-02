//! Firmware-independent ownership/write/readback state machine, shared with fake tests.
use crate::{domain::*, ownership::JournalStore};
use anyhow::{Result, bail, ensure};
use jiks_core::MachineIdentity;

pub trait CpuFirmware {
    fn now_tick_ms(&mut self) -> u64 {
        monotonic_ms()
    }
    fn identity(&mut self) -> Result<MachineIdentity>;
    fn ac_connected(&mut self) -> Result<bool>;
    fn capabilities(&mut self) -> Result<CpuCapabilities>;
    fn mode(&mut self) -> Result<u32>;
    fn read(&mut self, id: u32) -> Result<u32>;
    fn write(&mut self, id: u32, value: u32) -> Result<()>;
    fn write_before(&mut self, id: u32, value: u32, deadline: Option<u64>) -> Result<()> {
        ensure!(
            deadline.is_none_or(|limit| self.now_tick_ms() < limit),
            "CPU command expired at the firmware setter boundary"
        );
        self.write(id, value)
    }
}

pub(crate) struct CpuSession<F: CpuFirmware> {
    firmware: F,
    caps: CpuCapabilities,
    journal: CpuJournal,
    store: JournalStore,
    active: bool,
    restoration_failure: Option<String>,
}
impl<F: CpuFirmware> CpuSession<F> {
    pub(crate) fn open(
        mut firmware: F,
        store: JournalStore,
        action: &ExplicitAction,
        created: u64,
    ) -> Result<Self> {
        ensure!(
            firmware.identity()?.is_supported(),
            "Only verified LENOVO 83NN/S9CN15WW/i9-14900HX may write firmware"
        );
        ensure!(
            firmware.ac_connected()?,
            "AC power is required for active CPU control"
        );
        if let Some(prior) = store.read()? {
            ensure!(
                matches!(
                    prior.state,
                    JournalState::Restored | JournalState::ReleasedExternal
                ),
                "Prior owner did not verify restoration. Inspect the journal and firmware before any new control"
            );
        }
        let caps = firmware.capabilities()?;
        let baseline = read_snapshot(&mut firmware)?;
        ensure!(
            baseline.raw_mode == CUSTOM_MODE,
            "Select Custom mode explicitly before starting CPU control; no mode change was made"
        );
        caps.validate(baseline.power, baseline.temperature_limit)?;
        let journal = CpuJournal {
            schema: 1,
            process_id: std::process::id(),
            process_created_100ns: created,
            updated_unix_ms: unix_ms(),
            action: action.description().into(),
            state: JournalState::Acquired,
            baseline: baseline.clone(),
            expected: baseline,
            long_intents: vec![],
            short_intents: vec![],
            temperature_intents: vec![],
            error: None,
        };
        store.write(&journal)?;
        Ok(Self {
            firmware,
            caps,
            journal,
            store,
            active: false,
            restoration_failure: None,
        })
    }
    fn persist(&mut self, state: JournalState) -> Result<()> {
        self.journal.state = state;
        self.journal.updated_unix_ms = unix_ms();
        self.store.write(&self.journal)
    }
    fn compact_intents(&mut self) {
        for (values, current) in [
            (
                &mut self.journal.long_intents,
                self.journal.expected.power.long_watts,
            ),
            (
                &mut self.journal.short_intents,
                self.journal.expected.power.short_watts,
            ),
            (
                &mut self.journal.temperature_intents,
                self.journal.expected.temperature_limit,
            ),
        ] {
            if !values.is_empty() {
                values.clear();
                values.push(current);
            }
        }
    }
    fn verify_current(&mut self) -> Result<()> {
        let current = read_snapshot(&mut self.firmware)?;
        ensure!(
            current == self.journal.expected,
            "Firmware mode/limits changed externally; no further field was written"
        );
        Ok(())
    }
    pub(crate) fn enable(
        &mut self,
        action: &ExplicitAction,
        temperature: u32,
    ) -> Result<CpuSnapshot> {
        ensure!(!self.active, "CPU control already enabled");
        ensure!(
            self.restoration_failure.is_none()
                && self.journal.error.is_none()
                && self.journal.state == JournalState::Acquired,
            "A failed/restored CPU session cannot be enabled again; open a verified new session"
        );
        let temperature = temperature.min(self.journal.baseline.temperature_limit);
        ensure!(
            temperature <= 88 && self.caps.temperature.contains(temperature),
            "Temperature must be on its firmware grid and <=88C"
        );
        self.journal.action = action.description().into();
        self.verify_current()?;
        self.write_field(CPU_TEMPERATURE_LIMIT, temperature)?;
        self.verify_current()?;
        self.active = true;
        self.compact_intents();
        self.persist(JournalState::Controlling)?;
        Ok(self.journal.expected.clone())
    }
    fn write_field(&mut self, id: u32, value: u32) -> Result<()> {
        self.write_field_before(id, value, None)
    }
    fn write_field_before(&mut self, id: u32, value: u32, deadline: Option<u64>) -> Result<()> {
        self.verify_current()?;
        self.check_deadline(deadline)?;
        match id {
            PL1 => self.journal.long_intents.push(value),
            PL2 => self.journal.short_intents.push(value),
            CPU_TEMPERATURE_LIMIT => self.journal.temperature_intents.push(value),
            _ => bail!("Not a coordinated CPU field"),
        }
        self.persist(JournalState::WriteIntent)?; // Intent is durable BEFORE the physical call.
        // Getter/journal I/O may itself be slow. Recheck immediately before every setter.
        self.check_deadline(deadline)?;
        if let Err(error) = self.firmware.write_before(id, value, deadline) {
            self.journal.error = Some(error.to_string());
            let _ = self.persist(JournalState::Faulted);
            return Err(error);
        }
        ensure!(
            self.firmware.read(id)? == value,
            "CPU field readback mismatch; ownership remains held"
        );
        match id {
            PL1 => self.journal.expected.power.long_watts = value,
            PL2 => self.journal.expected.power.short_watts = value,
            _ => self.journal.expected.temperature_limit = value,
        }
        self.verify_current()?;
        self.persist(JournalState::Controlling)
    }
    fn check_deadline(&mut self, deadline: Option<u64>) -> Result<()> {
        ensure!(
            deadline.is_none_or(|limit| self.firmware.now_tick_ms() < limit),
            "CPU command expired before the next physical setter; no further field was written"
        );
        Ok(())
    }
    pub(crate) fn set_power(
        &mut self,
        action: &ExplicitAction,
        target: Envelope,
        deadline_tick_ms: u64,
    ) -> Result<CpuSnapshot> {
        ensure!(
            self.active,
            "Enable CPU control before writing power limits"
        );
        ensure!(
            self.restoration_failure.is_none()
                && self.journal.error.is_none()
                && self.journal.state == JournalState::Controlling,
            "Prior actuator/restoration failure must be resolved before new power commands"
        );
        ensure!(
            self.firmware.now_tick_ms() < deadline_tick_ms,
            "Expired CPU command; no write was issued"
        );
        self.caps
            .validate(target, self.journal.expected.temperature_limit)?;
        ensure!(
            target.long_watts <= self.journal.baseline.power.long_watts
                && target.short_watts <= self.journal.baseline.power.short_watts,
            "Power target exceeds the verified user baseline"
        );
        self.journal.action = action.description().into();
        self.verify_current()?;
        if let Err(error) = self.write_envelope_before(target, Some(deadline_tick_ms)) {
            self.journal.error = Some(error.to_string());
            let _ = self.persist(JournalState::Faulted);
            return Err(error);
        }
        self.compact_intents();
        self.persist(JournalState::Controlling)?;
        Ok(self.journal.expected.clone())
    }
    fn write_envelope(&mut self, target: Envelope) -> Result<()> {
        self.write_envelope_before(target, None)
    }
    fn write_envelope_before(&mut self, target: Envelope, deadline: Option<u64>) -> Result<()> {
        let order = if target.short_watts < self.journal.expected.power.long_watts {
            [PL1, PL2]
        } else {
            [PL2, PL1]
        };
        for id in order {
            let value = if id == PL1 {
                target.long_watts
            } else {
                target.short_watts
            };
            let current = if id == PL1 {
                self.journal.expected.power.long_watts
            } else {
                self.journal.expected.power.short_watts
            };
            if value != current {
                self.write_field_before(id, value, deadline)?;
            }
        }
        Ok(())
    }
    pub(crate) fn sample(&mut self) -> Result<CpuSnapshot> {
        read_snapshot(&mut self.firmware)
    }
    pub(crate) fn verify_ownership(&mut self) -> Result<()> {
        self.verify_current()
    }
    pub(crate) fn temperature(&mut self) -> Result<TemperatureSample> {
        let requested_at_tick_ms = monotonic_ms();
        let value = self.firmware.read(CPU_TEMPERATURE)?;
        TemperatureSample::firmware(value, requested_at_tick_ms)
    }
    pub(crate) fn restore(&mut self) -> Result<CpuSnapshot> {
        ensure!(
            self.restoration_failure.is_none(),
            "Previous CPU restoration failed; inspect the journal and firmware before restarting"
        );
        let result = self.restore_inner();
        if let Err(error) = &result {
            self.restoration_failure = Some(error.to_string());
            self.journal.error = Some(error.to_string());
            let _ = self.persist(JournalState::Faulted);
        }
        result
    }
    fn restore_inner(&mut self) -> Result<CpuSnapshot> {
        if matches!(
            self.journal.state,
            JournalState::Restored | JournalState::ReleasedExternal
        ) {
            return read_snapshot(&mut self.firmware);
        }
        self.persist(JournalState::Restoring)?;
        let current = read_snapshot(&mut self.firmware)?;
        if current.raw_mode != CUSTOM_MODE {
            self.active = false;
            self.persist(JournalState::ReleasedExternal)?;
            return Ok(current);
        }
        let old = &self.journal.baseline;
        let target = Envelope {
            long_watts: if self
                .journal
                .long_intents
                .contains(&current.power.long_watts)
            {
                old.power.long_watts
            } else {
                current.power.long_watts
            },
            short_watts: if self
                .journal
                .short_intents
                .contains(&current.power.short_watts)
            {
                old.power.short_watts
            } else {
                current.power.short_watts
            },
        };
        let temperature = if self
            .journal
            .temperature_intents
            .contains(&current.temperature_limit)
        {
            old.temperature_limit
        } else {
            current.temperature_limit
        };
        self.caps.validate(target, temperature)?; // Check the ENTIRE restore before changing a field.
        self.journal.expected = current;
        self.write_envelope(target)?;
        if temperature != self.journal.expected.temperature_limit {
            self.write_field(CPU_TEMPERATURE_LIMIT, temperature)?;
        }
        self.verify_current()?;
        self.active = false;
        self.journal.error = None;
        // Preserved external values are not restoration of the original baseline.
        // Record the distinction even though this session has released its fields.
        self.persist(if self.journal.expected == self.journal.baseline {
            JournalState::Restored
        } else {
            JournalState::ReleasedExternal
        })?;
        Ok(self.journal.expected.clone())
    }
}

/// A no-session stop is pure and idempotent. Failed restoration retains the
/// session (and its owner/intents) for truthful shutdown status.
pub(crate) fn stop_session<F: CpuFirmware>(
    session: &mut Option<CpuSession<F>>,
) -> Result<serde_json::Value> {
    if let Some(cpu) = session {
        let snapshot = cpu.restore()?;
        *session = None;
        Ok(serde_json::to_value(snapshot)?)
    } else {
        Ok(serde_json::Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use std::{
        collections::BTreeMap,
        path::PathBuf,
        sync::{Arc, Mutex},
    };
    #[derive(Clone)]
    struct Fake {
        state: Arc<Mutex<State>>,
    }
    struct State {
        values: BTreeMap<u32, u32>,
        writes: Vec<(u32, u32)>,
        mode: u32,
        fail: Option<(u32, u32)>,
        apply_then_fail: bool,
        tick: u64,
        advance_after_write: u64,
    }
    impl CpuFirmware for Fake {
        fn now_tick_ms(&mut self) -> u64 {
            self.state.lock().unwrap().tick
        }
        fn identity(&mut self) -> Result<MachineIdentity> {
            Ok(MachineIdentity {
                manufacturer: "LENOVO".into(),
                model: "83NN".into(),
                bios: "S9CN15WW".into(),
                cpu: "i9-14900HX".into(),
                logical_processors: 32,
            })
        }
        fn ac_connected(&mut self) -> Result<bool> {
            Ok(true)
        }
        fn capabilities(&mut self) -> Result<CpuCapabilities> {
            Ok(CpuCapabilities {
                long: Range {
                    min: 10,
                    max: 100,
                    step: 1,
                },
                short: Range {
                    min: 10,
                    max: 150,
                    step: 1,
                },
                temperature: Range {
                    min: 40,
                    max: 110,
                    step: 1,
                },
            })
        }
        fn mode(&mut self) -> Result<u32> {
            Ok(self.state.lock().unwrap().mode)
        }
        fn read(&mut self, id: u32) -> Result<u32> {
            self.state
                .lock()
                .unwrap()
                .values
                .get(&id)
                .copied()
                .context("missing fake value")
        }
        fn write(&mut self, id: u32, value: u32) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            state.writes.push((id, value));
            state.tick += state.advance_after_write;
            let fail = state.fail == Some((id, value));
            if !fail || state.apply_then_fail {
                state.values.insert(id, value);
            }
            if fail {
                bail!("injected fake setter failure")
            }
            Ok(())
        }
    }
    struct Fixture {
        root: PathBuf,
        store: JournalStore,
        fake: Fake,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "jiks-platform-fixture-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let fake = Fake {
                state: Arc::new(Mutex::new(State {
                    values: BTreeMap::from([
                        (PL1, 80),
                        (PL2, 100),
                        (CPU_TEMPERATURE_LIMIT, 100),
                        (CPU_TEMPERATURE, 50),
                    ]),
                    writes: vec![],
                    mode: 255,
                    fail: None,
                    apply_then_fail: false,
                    tick: 100,
                    advance_after_write: 0,
                })),
            };
            Self {
                store: JournalStore::new(root.join("journal.json")),
                root,
                fake,
            }
        }
        fn open(&self) -> CpuSession<Fake> {
            CpuSession::open(
                self.fake.clone(),
                self.store.clone(),
                &ExplicitAction::new("fixture only").unwrap(),
                0,
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let parent = std::env::temp_dir();
            assert!(
                self.root.starts_with(parent)
                    && self
                        .root
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("jiks-platform-fixture-")
            );
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn action() -> ExplicitAction {
        ExplicitAction::new("fake verification").unwrap()
    }
    #[test]
    fn ordered_decrease_and_verified_restore() {
        let f = Fixture::new();
        let mut cpu = f.open();
        cpu.enable(&action(), 88).unwrap();
        f.fake.state.lock().unwrap().writes.clear();
        cpu.set_power(
            &action(),
            Envelope {
                long_watts: 40,
                short_watts: 50,
            },
            monotonic_ms() + 1000,
        )
        .unwrap();
        assert_eq!(
            f.fake.state.lock().unwrap().writes,
            vec![(PL1, 40), (PL2, 50)]
        );
        let restored = cpu.restore().unwrap();
        assert_eq!(
            restored.power,
            Envelope {
                long_watts: 80,
                short_watts: 100
            }
        );
        assert_eq!(restored.temperature_limit, 100);
        assert_eq!(
            f.store.read().unwrap().unwrap().state,
            JournalState::Restored
        );
        assert_eq!(
            &f.fake.state.lock().unwrap().writes[2..],
            &[(PL2, 100), (PL1, 80), (CPU_TEMPERATURE_LIMIT, 100)]
        );
    }
    #[test]
    fn failed_setter_that_applied_is_still_owned() {
        let f = Fixture::new();
        let mut cpu = f.open();
        cpu.enable(&action(), 88).unwrap();
        {
            let mut state = f.fake.state.lock().unwrap();
            state.fail = Some((PL1, 40));
            state.apply_then_fail = true;
        }
        assert!(
            cpu.set_power(
                &action(),
                Envelope {
                    long_watts: 40,
                    short_watts: 50
                },
                monotonic_ms() + 1000
            )
            .is_err()
        );
        f.fake.state.lock().unwrap().fail = None;
        let restored = cpu.restore().unwrap();
        assert_eq!(restored.power.long_watts, 80);
        assert_eq!(restored.temperature_limit, 100);
    }
    #[test]
    fn expired_command_and_external_change_do_not_write() {
        let f = Fixture::new();
        let mut cpu = f.open();
        cpu.enable(&action(), 88).unwrap();
        let before = f.fake.state.lock().unwrap().writes.len();
        assert!(
            cpu.set_power(
                &action(),
                Envelope {
                    long_watts: 40,
                    short_watts: 50
                },
                0
            )
            .is_err()
        );
        assert_eq!(before, f.fake.state.lock().unwrap().writes.len());
        f.fake.state.lock().unwrap().values.insert(PL1, 60);
        assert!(
            cpu.set_power(
                &action(),
                Envelope {
                    long_watts: 40,
                    short_watts: 50
                },
                monotonic_ms() + 1000
            )
            .is_err()
        );
        assert_eq!(before, f.fake.state.lock().unwrap().writes.len());
        assert_eq!(cpu.restore().unwrap().power.long_watts, 60);
        assert_eq!(
            f.store.read().unwrap().unwrap().state,
            JournalState::ReleasedExternal
        );
    }
    #[test]
    fn failed_restoration_is_latched_and_blocks_reacquisition() {
        let f = Fixture::new();
        let mut cpu = f.open();
        cpu.enable(&action(), 88).unwrap();
        f.fake.state.lock().unwrap().fail = Some((CPU_TEMPERATURE_LIMIT, 100));
        assert!(cpu.restore().is_err());
        f.fake.state.lock().unwrap().fail = None;
        let before = f.fake.state.lock().unwrap().writes.len();
        assert!(cpu.restore().is_err());
        assert_eq!(before, f.fake.state.lock().unwrap().writes.len());
        assert!(CpuSession::open(f.fake.clone(), f.store.clone(), &action(), 0).is_err());
    }
    #[test]
    fn first_setter_that_returns_late_blocks_the_second_but_keeps_restore_ownership() {
        let f = Fixture::new();
        let mut cpu = f.open();
        cpu.enable(&action(), 88).unwrap();
        {
            let mut state = f.fake.state.lock().unwrap();
            state.writes.clear();
            state.advance_after_write = 101;
        }
        assert!(
            cpu.set_power(
                &action(),
                Envelope {
                    long_watts: 40,
                    short_watts: 50
                },
                200
            )
            .is_err()
        );
        assert_eq!(f.fake.state.lock().unwrap().writes, vec![(PL1, 40)]);
        assert_eq!(
            f.store.read().unwrap().unwrap().state,
            JournalState::Faulted
        );
        let restored = cpu.restore().unwrap();
        assert_eq!(
            restored.power,
            Envelope {
                long_watts: 80,
                short_watts: 100
            }
        );
        assert_eq!(restored.temperature_limit, 100);
    }
    #[test]
    fn command_at_exact_expiry_issues_no_physical_write() {
        let f = Fixture::new();
        let mut cpu = f.open();
        cpu.enable(&action(), 88).unwrap();
        f.fake.state.lock().unwrap().writes.clear();
        assert!(
            cpu.set_power(
                &action(),
                Envelope {
                    long_watts: 40,
                    short_watts: 50
                },
                100
            )
            .is_err()
        );
        assert!(f.fake.state.lock().unwrap().writes.is_empty());
        cpu.restore().unwrap();
        assert!(cpu.enable(&action(), 88).is_err());
    }
    #[test]
    fn stop_without_session_uses_no_firmware_and_failure_retains_session() {
        let mut empty: Option<CpuSession<Fake>> = None;
        assert_eq!(stop_session(&mut empty).unwrap(), serde_json::Value::Null);
        assert_eq!(stop_session(&mut empty).unwrap(), serde_json::Value::Null);
        let f = Fixture::new();
        let mut cpu = f.open();
        cpu.enable(&action(), 88).unwrap();
        f.fake.state.lock().unwrap().fail = Some((CPU_TEMPERATURE_LIMIT, 100));
        let mut session = Some(cpu);
        assert!(stop_session(&mut session).is_err());
        assert!(session.is_some());
        assert_eq!(
            f.store.read().unwrap().unwrap().state,
            JournalState::Faulted
        );
    }
    #[test]
    fn journal_commit_sharing_failure_prevents_the_physical_setter() {
        use std::os::windows::fs::OpenOptionsExt;
        let f = Fixture::new();
        let mut cpu = f.open();
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
            .open(f.store.path())
            .unwrap();
        assert!(cpu.enable(&action(), 88).is_err());
        assert!(f.fake.state.lock().unwrap().writes.is_empty());
        drop(locked);
        let restored = cpu.restore().unwrap();
        assert_eq!(restored.temperature_limit, 100);
        assert!(f.fake.state.lock().unwrap().writes.is_empty());
        assert_eq!(
            f.store.read().unwrap().unwrap().state,
            JournalState::Restored
        );
    }
}

pub(crate) fn read_snapshot<F: CpuFirmware>(firmware: &mut F) -> Result<CpuSnapshot> {
    let raw_mode = firmware.mode()?;
    let snapshot = CpuSnapshot {
        raw_mode,
        power: Envelope {
            long_watts: firmware.read(PL1)?,
            short_watts: firmware.read(PL2)?,
        },
        temperature_limit: firmware.read(CPU_TEMPERATURE_LIMIT)?,
    };
    ensure!(
        firmware.mode()? == raw_mode,
        "Mode changed while sampling CPU limits"
    );
    Ok(snapshot)
}
