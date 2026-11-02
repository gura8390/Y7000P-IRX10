//! Opt-in DDR5 hub temperature through the original signed PawnIO modules.
//! SPDX-License-Identifier: MPL-2.0
//! No EEPROM/PMIC/configuration writes or sensor enable. A confirmed hub's
//! temporary page selector is preserved/restored with an explicit permit. The
//! shared standard global SMBus mutex is held across the complete read batch.
use crate::{
    domain::{ExplicitAction, monotonic_ms},
    ownership::wide,
    wmi_native::WmiFacade,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::ptr;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_ABANDONED, WAIT_OBJECT_0},
    Storage::FileSystem::*,
    System::{
        IO::DeviceIoControl,
        Threading::{
            CreateMutexW, MUTEX_MODIFY_STATE, OpenMutexW, ReleaseMutex, WaitForSingleObject,
        },
    },
};
const I801: &[u8] = include_bytes!("../resources/pawnio/SmbusI801.bin");
const IMC: &[u8] = include_bytes!("../resources/pawnio/SmbusIntelSkylakeIMC.bin");
const LOAD: u32 = 0xa1b22084;
const EXECUTE: u32 = 0xa1b22104;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusKind {
    I801,
    IntelImc(u8),
}
impl BusKind {
    fn label(self) -> String {
        match self {
            Self::I801 => "Intel I801".into(),
            Self::IntelImc(n) => format!("Intel IMC{n}"),
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct Dimm {
    pub bus: String,
    pub address: u8,
    pub ddr5_identity: bool,
    pub thermal_capability: Option<bool>,
    pub enabled: Option<bool>,
    pub celsius: Option<f64>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub requested_at_tick_ms: u64,
    pub completed_at_tick_ms: u64,
    pub maximum_celsius: Option<f64>,
    pub dimms: Vec<Dimm>,
    pub errors: Vec<String>,
    pub source: String,
}
pub fn read_packet(
    kind: BusKind,
    address: u8,
    command: u8,
    word: bool,
) -> Result<(Vec<u8>, usize)> {
    ensure!(
        (0x50..=0x57).contains(&address),
        "DDR5 query address is outside the SPD slots"
    );
    ensure!(
        (!word && [0, 1, 5, 0x0b, 0x1a].contains(&command)) || (word && command == 0x31),
        "Register is not in the DDR5 thermal read allowlist"
    );
    let mut input = vec![
        0u8;
        32 + if matches!(kind, BusKind::I801) {
            5 * 8
        } else {
            9 * 8
        }
    ];
    input[..16].copy_from_slice(b"ioctl_smbus_xfer");
    for (index, value) in [
        address as u64,
        1,
        command as u64,
        if word { 3 } else { 2 },
        0,
    ]
    .into_iter()
    .enumerate()
    {
        input[32 + index * 8..40 + index * 8].copy_from_slice(&value.to_le_bytes());
    }
    Ok((
        input,
        if matches!(kind, BusKind::I801) {
            if word { 16 } else { 8 }
        } else {
            40
        },
    ))
}
pub fn decode_temperature(raw: u16) -> Option<f64> {
    if raw & 0xe000 != 0 {
        return None;
    }
    let c = if raw & 0x1000 != 0 {
        (raw & !0x1000) as f64 / 16. - 256.
    } else {
        raw as f64 / 16.
    };
    (c > 0. && c < 150.).then_some(c)
}
pub trait Port {
    fn tick(&self) -> u64;
    fn read(&mut self, address: u8, command: u8, word: bool, deadline: u64) -> Result<u16>;
}
pub fn read_dimm(port: &mut impl Port, bus: String, address: u8, deadline: u64) -> Dimm {
    let mut out = Dimm {
        bus,
        address,
        ddr5_identity: false,
        thermal_capability: None,
        enabled: None,
        celsius: None,
        error: None,
    };
    let result = (|| {
        let page = port.read(address, 0x0b, false, deadline)?;
        ensure!(
            page & 8 == 0,
            "DDR5 hub is using 16-bit addressing; the current SMBus protocol adapter cannot read that mode"
        );
        ensure!(
            page & 7 == 0,
            "SPD hub is on another virtual page; this read-only adapter does not change page state"
        );
        let most = port.read(address, 0, false, deadline)?;
        let least = port.read(address, 1, false, deadline)?;
        ensure!(
            most == 0x51 && least == 0x18,
            "No confirmed DDR5 hub identity 0x5118 at this slot"
        );
        out.ddr5_identity = true;
        let cap = port.read(address, 5, false, deadline)?;
        out.thermal_capability = Some(cap & 2 != 0);
        ensure!(cap & 2 != 0, "DDR5 hub does not advertise a thermal sensor");
        let enabled = port.read(address, 0x1a, false, deadline)?;
        out.enabled = Some(enabled == 0);
        ensure!(
            enabled == 0,
            "DDR5 hub thermal sensor is disabled; no enable write was issued"
        );
        let raw = port.read(address, 0x31, true, deadline)?;
        ensure!(
            port.tick() < deadline,
            "DDR5 physical read completed after sample deadline"
        );
        out.celsius = Some(decode_temperature(raw).context("Invalid/missing DDR5 thermal sample")?);
        Ok::<_, anyhow::Error>(())
    })();
    if let Err(e) = result {
        out.error = Some(format!("{e:#}"));
    }
    out
}
pub trait PagePort: Port {
    fn guard_selector(&self, cleanup: bool, deadline: u64) -> Result<()>;
    fn write_selector(
        &mut self,
        address: u8,
        saved: u8,
        value: u8,
        cleanup: bool,
        deadline: u64,
    ) -> Result<()>;
}
#[derive(Debug, Clone)]
struct PageRecovery {
    address: u8,
    saved: u8,
    temporary: u8,
}
pub struct PageSession<P: PagePort> {
    port: P,
    pending: Option<PageRecovery>,
}
impl<P: PagePort> PageSession<P> {
    pub fn new(port: P) -> Self {
        Self {
            port,
            pending: None,
        }
    }
    pub fn recovery_required(&self) -> bool {
        self.pending.is_some()
    }
    pub fn recover(&mut self) -> Result<()> {
        let Some(record) = self.pending.clone() else {
            return Ok(());
        };
        let deadline = self.port.tick().saturating_add(30_000);
        let current = self.port.read(record.address, 0x0b, false, deadline)?;
        ensure!(
            current == record.saved as u16 || current == record.temporary as u16,
            "External MR11 change; saved selector must not overwrite another owner's state"
        );
        if current != record.saved as u16 {
            self.port.guard_selector(true, deadline)?;
            self.port
                .write_selector(record.address, record.saved, record.saved, true, deadline)?;
        }
        ensure!(
            self.port.read(record.address, 0x0b, false, deadline)? == record.saved as u16,
            "DDR5 MR11 exact restore readback failed; SMBus ownership remains held"
        );
        self.pending = None;
        Ok(())
    }
    pub fn read(&mut self, bus: String, address: u8, deadline: u64) -> Result<Dimm> {
        ensure!(
            self.pending.is_none(),
            "Prior DDR5 selector restoration requires StopAuxSensors/recovery before sampling"
        );
        let mut row = Dimm {
            bus: bus.clone(),
            address,
            ddr5_identity: false,
            thermal_capability: None,
            enabled: None,
            celsius: None,
            error: None,
        };
        let primary = (|| -> Result<Dimm> {
            let mode = self.port.read(address, 0x0b, false, deadline)?;
            ensure!(
                mode <= 255 && mode & 8 == 0,
                "DDR5 hub 16-bit-addressing mode is outside this SMBus adapter; no page or mode write authorized"
            );
            if mode & 7 == 0 {
                return Ok(read_dimm(&mut self.port, bus, address, deadline));
            }
            // Linux spd5118 and the fixed RAMSPD accessor allow hidden 0/0
            // identity on a nonzero page, then re-check 0x5118 after selection.
            let most = self.port.read(address, 0, false, deadline)?;
            let least = self.port.read(address, 1, false, deadline)?;
            ensure!(
                (most == 0x51 && least == 0x18) || (most == 0 && least == 0),
                "Nonzero-page device is not a confirmed/hidden DDR5 hub candidate; no selector write issued"
            );
            self.port.guard_selector(false, deadline)?;
            let saved = mode as u8;
            let temporary = saved & !7;
            self.pending = Some(PageRecovery {
                address,
                saved,
                temporary,
            });
            self.port
                .write_selector(address, saved, temporary, false, deadline)?;
            ensure!(
                self.port.read(address, 0x0b, false, deadline)? == temporary as u16,
                "Temporary MR11 selector failed exact readback"
            );
            Ok(read_dimm(&mut self.port, bus, address, deadline))
        })();
        let recovered = self.recover();
        if let Err(error) = recovered {
            anyhow::bail!(
                "DDR5 primary={:?}; selector restoration REQUIRED, session+SMBus mutex retained: {error:#}",
                primary.as_ref().err().map(|e| format!("{e:#}"))
            );
        }
        match primary {
            Ok(value) => Ok(value),
            Err(error) => {
                row.error = Some(format!("{error:#}"));
                Ok(row)
            }
        }
    }
}
pub fn selector_packet(
    kind: BusKind,
    address: u8,
    saved: u8,
    value: u8,
    restore: bool,
    protected: bool,
) -> Result<(Vec<u8>, usize)> {
    ensure!(
        (0x50..=0x57).contains(&address)
            && saved & 8 == 0
            && (value == saved & !7 || (restore && value == saved)),
        "Only preserved MR11 page-zero/select-back transactions are allowed"
    );
    ensure!(
        !protected || kind == BusKind::I801,
        "Unexpected SPD write-protection protocol for this bus"
    );
    let mut packet = vec![0u8; 32 + if kind == BusKind::I801 { 40 } else { 72 }];
    packet[..16].copy_from_slice(b"ioctl_smbus_xfer");
    for (index, word) in [
        address as u64,
        if protected { 1 } else { 0 },
        0x0b,
        if protected { 4 } else { 2 },
        value as u64,
    ]
    .into_iter()
    .enumerate()
    {
        packet[32 + index * 8..40 + index * 8].copy_from_slice(&word.to_le_bytes());
    }
    Ok((packet, if kind == BusKind::I801 { 16 } else { 40 }))
}
struct MutexGuard {
    handle: HANDLE,
}
impl MutexGuard {
    fn acquire(deadline: u64) -> Result<Self> {
        let name = wide("Global\\Access_SMBUS.HTP.Method")?;
        // Opening an existing world mutex requires only synchronize+modify,
        // rather than the MUTEX_ALL_ACCESS requested by CreateMutexW.
        let mut handle = unsafe { OpenMutexW(MUTEX_MODIFY_STATE | 0x0010_0000, 0, name.as_ptr()) };
        if handle.is_null() {
            handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
        }
        ensure!(
            !handle.is_null(),
            "Standard global SMBus mutex cannot be opened; no local unsynchronized fallback is allowed"
        );
        let remaining = deadline.saturating_sub(monotonic_ms()).min(100) as u32;
        let wait = unsafe { WaitForSingleObject(handle, remaining) };
        if wait == WAIT_OBJECT_0 {
            return Ok(Self { handle });
        }
        if wait == WAIT_ABANDONED {
            unsafe { ReleaseMutex(handle) };
        }
        unsafe { CloseHandle(handle) };
        anyhow::bail!("Standard SMBus mutex is busy/abandoned/unavailable; no transfer was issued")
    }
}
impl Drop for MutexGuard {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.handle);
            CloseHandle(self.handle)
        };
    }
}
struct NativeBus {
    handle: HANDLE,
    kind: BusKind,
    protected: bool,
    action: Option<ExplicitAction>,
}
impl Drop for NativeBus {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}
impl NativeBus {
    fn execute(&self, name: &[u8], words: &[u64], output: usize) -> Result<Vec<u8>> {
        ensure!(
            name.len() < 32 && words.len() <= 9 && output <= 40,
            "Fixed PawnIO request bound mismatch"
        );
        let mut input = vec![0u8; 32 + words.len() * 8];
        input[..name.len()].copy_from_slice(name);
        for (index, word) in words.iter().enumerate() {
            input[32 + index * 8..40 + index * 8].copy_from_slice(&word.to_le_bytes());
        }
        self.raw_execute(&input, output)
    }
    fn raw_execute(&self, input: &[u8], capacity: usize) -> Result<Vec<u8>> {
        let mut output = vec![0u8; capacity];
        let mut returned = 0;
        ensure!(
            unsafe {
                DeviceIoControl(
                    self.handle,
                    EXECUTE,
                    input.as_ptr().cast(),
                    input.len() as u32,
                    output.as_mut_ptr().cast(),
                    capacity as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            } != 0,
            "PawnIO {} read failed: {}",
            self.kind.label(),
            std::io::Error::last_os_error()
        );
        ensure!(
            returned as usize <= capacity && (returned as usize).is_multiple_of(8),
            "PawnIO SMBus returned a malformed response length"
        );
        output.truncate(returned as usize);
        Ok(output)
    }
    fn open(kind: BusKind, deadline: u64) -> Result<Self> {
        ensure!(
            monotonic_ms() < deadline,
            "DDR5 initialization deadline expired"
        );
        let handle = unsafe {
            CreateFileW(
                wide("\\\\?\\GLOBALROOT\\Device\\PawnIO")?.as_ptr(),
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
            "Existing PawnIO device unavailable: {}",
            std::io::Error::last_os_error()
        );
        let mut bus = Self {
            handle,
            kind,
            protected: false,
            action: None,
        };
        let module = if kind == BusKind::I801 { I801 } else { IMC };
        let mut returned = 0;
        ensure!(
            monotonic_ms() < deadline,
            "DDR5 initialization deadline expired before module load"
        );
        ensure!(
            unsafe {
                DeviceIoControl(
                    handle,
                    LOAD,
                    module.as_ptr().cast(),
                    module.len() as u32,
                    ptr::null_mut(),
                    0,
                    &mut returned,
                    ptr::null_mut(),
                )
            } != 0,
            "Fixed signed {} module load failed: {}",
            kind.label(),
            std::io::Error::last_os_error()
        );
        if let BusKind::IntelImc(index) = kind {
            ensure!(
                index <= 1 && monotonic_ms() < deadline,
                "Invalid/expired IMC module selection"
            );
            bus.execute(b"ioctl_smbus_index", &[index as u64], 0)?;
        }
        ensure!(
            monotonic_ms() < deadline,
            "DDR5 initialization deadline expired before identity"
        );
        let identity = bus.execute(b"ioctl_identity", &[], 24)?;
        ensure!(
            identity.len() == 24,
            "SMBus controller identity is truncated"
        );
        let pci = u64::from_le_bytes(identity[16..24].try_into()?);
        ensure!(
            pci & 0xffff == 0x8086,
            "DDR5 adapter is not an Intel SMBus controller"
        );
        let name = &identity[..8];
        ensure!(
            match kind {
                BusKind::I801 => name == b"i801\0\0\0\0",
                BusKind::IntelImc(_) => name == b"IntelIMC",
            },
            "Unexpected signed SMBus module identity"
        );
        if kind == BusKind::I801 {
            let result = bus.execute(b"ioctl_write_protection", &[], 8)?;
            ensure!(
                result.len() == 8,
                "I801 SPD write-protection response truncated"
            );
            let value = u64::from_le_bytes(result[..8].try_into()?);
            ensure!(value <= 1, "Unknown I801 SPD write-protection state");
            bus.protected = value == 1;
        }
        Ok(bus)
    }
}
impl PagePort for NativeBus {
    fn guard_selector(&self, cleanup: bool, deadline: u64) -> Result<()> {
        ensure!(
            monotonic_ms() < deadline,
            "DDR5 selector operation deadline expired"
        );
        if !cleanup {
            self.action
                .as_ref()
                .context("A page change requires an explicitly enabled sensor request")?
                .check_write_boundary()?;
        }
        // Restoration owns the standard SMBus mutex and only the saved byte;
        // it cannot inherit cancellation/expiry from the original sample.
        Ok(())
    }
    fn write_selector(
        &mut self,
        address: u8,
        saved: u8,
        value: u8,
        cleanup: bool,
        deadline: u64,
    ) -> Result<()> {
        self.guard_selector(cleanup, deadline)?;
        let current = self.read(address, 0xb, false, deadline)?;
        ensure!(
            current == saved as u16 || current == (saved & !7) as u16,
            "MR11 current state is outside this transaction's owned history"
        );
        let (packet, capacity) =
            selector_packet(self.kind, address, saved, value, cleanup, self.protected)?;
        self.guard_selector(cleanup, deadline)?;
        self.raw_execute(&packet, capacity)?;
        ensure!(
            self.read(address, 0xb, false, deadline)? == value as u16,
            "MR11 selector write actual readback mismatch"
        );
        Ok(())
    }
}
impl Port for NativeBus {
    fn tick(&self) -> u64 {
        monotonic_ms()
    }
    fn read(&mut self, address: u8, command: u8, word: bool, deadline: u64) -> Result<u16> {
        ensure!(
            monotonic_ms() < deadline,
            "DDR5 deadline expired before a later SMBus transfer"
        );
        let (input, capacity) = read_packet(self.kind, address, command, word)?;
        let output = self.raw_execute(&input, capacity)?;
        ensure!(output.len() >= 8, "SMBus thermal response is truncated");
        let value = u64::from_le_bytes(output[..8].try_into()?);
        ensure!(
            value
                <= if word {
                    u16::MAX as u64
                } else {
                    u8::MAX as u64
                },
            "SMBus read returned an out-of-range value"
        );
        Ok(value as u16)
    }
}
#[derive(Default)]
pub struct Sampler {
    buses: Vec<PageSession<NativeBus>>,
    initialized: bool,
    initialization_errors: Vec<String>,
    mutex: Option<MutexGuard>,
}
impl Sampler {
    pub fn stop(&mut self) -> Result<()> {
        for bus in &mut self.buses {
            if bus.recovery_required() {
                ensure!(
                    self.mutex.is_some(),
                    "DDR5 recovery lost its SMBus mutex; no restore is authorized"
                );
                bus.recover()?;
            }
        }
        self.buses.clear();
        self.mutex.take();
        self.initialized = false;
        self.initialization_errors.clear();
        Ok(())
    }
    pub fn sample(&mut self, action: &ExplicitAction) -> Result<Snapshot> {
        ensure!(
            !self.buses.iter().any(PageSession::recovery_required),
            "DDR5 selector recovery is pending; StopAuxSensors must confirm restore before further sampling"
        );
        ensure!(self.mutex.is_none(), "SMBus transaction cleanup is pending");
        let requested = monotonic_ms();
        let deadline = requested.saturating_add(2000);
        self.mutex = Some(MutexGuard::acquire(deadline)?);
        let result = self.sample_inner(requested, deadline, action);
        if !self.buses.iter().any(PageSession::recovery_required) {
            self.mutex.take();
        }
        result
    }
    fn sample_inner(
        &mut self,
        requested: u64,
        deadline: u64,
        action: &ExplicitAction,
    ) -> Result<Snapshot> {
        if !self.initialized {
            ensure!(
                crate::read_machine_identity()?.is_supported(),
                "DDR5 reader requires the exact supported 83NN machine"
            );
            let state: Vec<serde_json::Value> = WmiFacade::connect("ROOT\\CIMV2")?
                .query("SELECT State,Started FROM Win32_SystemDriver WHERE Name='PawnIO'")?;
            ensure!(
                state.len() == 1 && state[0]["State"] == "Running" && state[0]["Started"] == true,
                "PawnIO must already be running; no driver will be installed/started"
            );
            for kind in [BusKind::I801, BusKind::IntelImc(0), BusKind::IntelImc(1)] {
                match NativeBus::open(kind, deadline) {
                    Ok(bus) => self.buses.push(PageSession::new(bus)),
                    Err(e) => self
                        .initialization_errors
                        .push(format!("{}: {e:#}", kind.label())),
                }
            }
            self.initialized = true;
        }
        let mut out=Snapshot{requested_at_tick_ms:requested,completed_at_tick_ms:requested,maximum_celsius:None,dimms:vec![],errors:self.initialization_errors.clone(),source:"Fixed RAMSPDToolkit DDR5 hub 0x5118; PawnIO I801/IntelIMC, standard global SMBus mutex, MR11 saved/temporary-page0/exact restore".into()};
        for bus in &mut self.buses {
            bus.port.action = Some(action.clone());
            for address in 0x50..=0x57 {
                if monotonic_ms() >= deadline {
                    out.errors
                        .push("DDR5 sample deadline expired; no later slot/bus was queried".into());
                    break;
                }
                let label = bus.port.kind.label();
                let row = bus.read(label, address, deadline)?;
                if let Some(c) = row.celsius {
                    out.maximum_celsius = Some(out.maximum_celsius.map_or(c, |v| v.max(c)));
                }
                out.dimms.push(row);
            }
        }
        out.completed_at_tick_ms = monotonic_ms();
        Ok(out)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    struct OwnedFake {
        mode: u8,
        writes: Vec<u8>,
        tick: u64,
        expired: bool,
        expire_after_select: bool,
        fail_restore: bool,
        external: bool,
    }
    impl Port for OwnedFake {
        fn tick(&self) -> u64 {
            self.tick
        }
        fn read(&mut self, _: u8, c: u8, _: bool, deadline: u64) -> Result<u16> {
            ensure!(self.tick < deadline, "deadline");
            if c == 0xb {
                return Ok(self.mode as u16);
            }
            if c == 0x31 && self.external {
                self.mode = 0xa5;
            }
            Ok(match c {
                0 => 0x51,
                1 => 0x18,
                5 => 2,
                0x1a => 0,
                0x31 => 50 * 16,
                _ => unreachable!(),
            })
        }
    }
    impl PagePort for OwnedFake {
        fn guard_selector(&self, cleanup: bool, deadline: u64) -> Result<()> {
            ensure!(
                self.tick < deadline && (!self.expired || cleanup),
                "expired request"
            );
            Ok(())
        }
        fn write_selector(
            &mut self,
            _: u8,
            saved: u8,
            value: u8,
            cleanup: bool,
            deadline: u64,
        ) -> Result<()> {
            self.guard_selector(cleanup, deadline)?;
            if cleanup && self.fail_restore {
                anyhow::bail!("unconfirmed restore");
            }
            self.mode = value;
            self.writes.push(value);
            assert_eq!(value & !7, saved & !7);
            if !cleanup && self.expire_after_select {
                self.tick = 10;
                self.expired = true;
            }
            Ok(())
        }
    }
    fn owned() -> OwnedFake {
        OwnedFake {
            mode: 0xa3,
            writes: vec![],
            tick: 1,
            expired: false,
            expire_after_select: false,
            fail_restore: false,
            external: false,
        }
    }
    #[test]
    fn nonzero_page_preserves_mode_bits_and_exactly_restores_after_temperature() {
        let mut s = PageSession::new(owned());
        let row = s.read("fake".into(), 0x50, 10).unwrap();
        assert_eq!(row.celsius, Some(50.));
        assert_eq!(s.port.writes, vec![0xa0, 0xa3]);
        assert_eq!(s.port.mode, 0xa3);
        assert!(!s.recovery_required());
        let (packet, _) = selector_packet(BusKind::I801, 0x50, 0xa3, 0xa0, false, true).unwrap();
        assert_eq!(u64::from_le_bytes(packet[40..48].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(packet[56..64].try_into().unwrap()), 4);
        assert!(selector_packet(BusKind::I801, 0x50, 0xa3, 0xa4, false, false).is_err());
    }
    #[test]
    fn original_deadline_does_not_expire_owned_selector_restoration() {
        let mut f = owned();
        f.expire_after_select = true;
        let mut s = PageSession::new(f);
        let row = s.read("fake".into(), 0x50, 10).unwrap();
        assert!(row.celsius.is_none() && row.error.is_some());
        assert_eq!(s.port.writes, vec![0xa0, 0xa3]);
        assert!(!s.recovery_required());
    }
    #[test]
    fn restore_failure_retains_pending_and_new_reads_cannot_abandon_it() {
        let mut f = owned();
        f.fail_restore = true;
        let mut s = PageSession::new(f);
        assert!(s.read("fake".into(), 0x50, 10).is_err());
        assert!(s.recovery_required());
        assert_eq!(s.port.mode, 0xa0);
        assert!(s.read("fake".into(), 0x51, 10).is_err());
        assert!(s.recover().is_err());
        s.port.fail_restore = false;
        s.recover().unwrap();
        assert!(!s.recovery_required());
        assert_eq!(s.port.mode, 0xa3);
    }
    #[test]
    fn external_selector_or_16bit_mode_is_never_silently_overwritten() {
        let mut f = owned();
        f.external = true;
        let mut s = PageSession::new(f);
        assert!(s.read("fake".into(), 0x50, 10).is_err());
        assert_eq!(s.port.writes, vec![0xa0]);
        assert_eq!(s.port.mode, 0xa5);
        assert!(s.recovery_required());
        let mut f = owned();
        f.mode = 0xab;
        let mut s = PageSession::new(f);
        let row = s.read("fake".into(), 0x50, 10).unwrap();
        assert!(row.error.unwrap().contains("16-bit"));
        assert!(s.port.writes.is_empty());
        assert!(!s.recovery_required());
        let mut f = owned();
        f.expired = true;
        let mut s = PageSession::new(f);
        assert!(s.read("fake".into(), 0x50, 10).unwrap().error.is_some());
        assert!(s.port.writes.is_empty());
    }
    struct Fake {
        cap: u16,
        enabled: u16,
        page: u16,
        reads: Vec<(u8, bool)>,
        tick: u64,
    }
    impl Port for Fake {
        fn tick(&self) -> u64 {
            self.tick
        }
        fn read(&mut self, _: u8, c: u8, w: bool, d: u64) -> Result<u16> {
            ensure!(self.tick < d, "expired");
            self.reads.push((c, w));
            Ok(match c {
                0 => 0x51,
                1 => 0x18,
                5 => self.cap,
                0xb => self.page,
                0x1a => self.enabled,
                0x31 => 50 * 16,
                _ => unreachable!(),
            })
        }
    }
    fn fake() -> Fake {
        Fake {
            cap: 2,
            enabled: 0,
            page: 0,
            reads: vec![],
            tick: 1,
        }
    }
    #[test]
    fn packet_never_exposes_write_or_eeprom_paths_and_uses_true_imc_sizes() {
        let (bytes, len) = read_packet(BusKind::IntelImc(0), 0x50, 0x31, true).unwrap();
        assert_eq!(bytes.len(), 104);
        assert_eq!(len, 40);
        assert_eq!(u64::from_le_bytes(bytes[40..48].try_into().unwrap()), 1);
        assert_eq!(&bytes[72..], &[0; 32]);
        assert!(read_packet(BusKind::I801, 0x50, 0xb, true).is_err());
        assert!(read_packet(BusKind::I801, 0x80, 5, false).is_err());
        assert!(read_packet(BusKind::I801, 0x50, 0x80, false).is_err());
    }
    #[test]
    fn capability_and_enabled_gate_actual_temperature_without_writes() {
        let mut f = fake();
        let row = read_dimm(&mut f, "fixture".into(), 0x50, 10);
        assert_eq!(row.celsius, Some(50.));
        let mut f = fake();
        f.cap = 0;
        let row = read_dimm(&mut f, "fixture".into(), 0x50, 10);
        assert_eq!(row.thermal_capability, Some(false));
        assert!(!f.reads.iter().any(|(c, _)| *c == 0x31));
        let mut f = fake();
        f.enabled = 1;
        assert!(
            read_dimm(&mut f, "fixture".into(), 0x50, 10)
                .celsius
                .is_none()
        );
        assert!(!f.reads.iter().any(|(c, _)| *c == 0x31));
        let mut f = fake();
        f.page = 3;
        assert!(
            read_dimm(&mut f, "fixture".into(), 0x50, 10)
                .error
                .unwrap()
                .contains("page")
        );
        assert_eq!(f.reads, vec![(0xb, false)]);
    }
    #[test]
    fn deadline_equal_causes_zero_transfers_and_raw_thermal_sign_is_checked() {
        let mut f = fake();
        assert!(read_dimm(&mut f, "fixture".into(), 0x50, 1).error.is_some());
        assert!(f.reads.is_empty());
        assert_eq!(decode_temperature(50 * 16), Some(50.));
        assert_eq!(decode_temperature(0x1ff0), None);
        assert_eq!(decode_temperature(0xffff), None);
        assert_eq!(decode_temperature(0), None);
        assert_eq!(I801.len(), 55540);
        assert_eq!(IMC.len(), 19828);
        assert_eq!(
            crate::boot_logo::digest(I801),
            "a0f7d066e7efda28c0e754c1f52dbd8dc280d388ba8575b514a80cb67490530c"
        );
        assert_eq!(
            crate::boot_logo::digest(IMC),
            "2e2d6c1e541fdfe598aa8df4d334e11c491c8a84f23e8256dd18119dd420dbd2"
        );
    }
}
