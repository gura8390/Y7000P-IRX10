use crate::god_mode::{self, GodModeField, GodModeFirmware, GodModePatch};
use crate::{
    cpu::{CpuFirmware, CpuSession},
    domain::*,
    ownership::{JournalStore, OwnerToken, current_creation_time},
    wmi_native::{MethodInput, WmiFacade},
    worker::{BoundedWorker, Ticket, WorkerState, WorkerStatus},
};
use anyhow::{Context, Result, bail, ensure};
use jiks_core::MachineIdentity;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf, rc::Rc};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS},
    },
};

fn parameters(
    values: impl IntoIterator<Item = (&'static str, MethodInput)>,
) -> BTreeMap<String, MethodInput> {
    values.into_iter().map(|(k, v)| (k.into(), v)).collect()
}
fn cpu_range_query() -> String {
    format!(
        "SELECT IDs,MinValue,MaxValue,Step,DefaultValue FROM LENOVO_CAPABILITY_DATA_01 WHERE IDs={} OR IDs={} OR IDs={}",
        PL1 | 0xff00,
        PL2 | 0xff00,
        CPU_TEMPERATURE_LIMIT | 0xff00
    )
}
fn integer(values: &BTreeMap<String, Value>, field: &str) -> Result<u32> {
    let value = values
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(field))
        .map(|(_, v)| v)
        .with_context(|| format!("Missing firmware output {field}"))?;
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .context("Firmware output is not a nonnegative uint32")
}
fn signed_integer(values: &BTreeMap<String, Value>, field: &str) -> Result<i32> {
    values
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(field))
        .and_then(|(_, value)| value.as_i64())
        .and_then(|v| i32::try_from(v).ok())
        .with_context(|| format!("Missing/invalid signed firmware output {field}"))
}
fn god_value(value: i32) -> MethodInput {
    if value >= 0 {
        MethodInput::U32(value as u32)
    } else {
        MethodInput::I32(value)
    }
}
fn god_parameters(field: GodModeField, value: i32) -> BTreeMap<String, MethodInput> {
    parameters([
        ("IDs", MethodInput::U32(field.id())),
        ("value", god_value(value)),
    ])
}
fn god_capabilities(
    facade: &WmiFacade,
) -> Result<(Vec<god_mode::SignedRange>, Vec<god_mode::DiscreteValue>)> {
    let filter = GodModeField::ALL
        .into_iter()
        .map(|field| format!("IDs={}", field.discovery_id()))
        .collect::<Vec<_>>()
        .join(" OR ");
    Ok((facade.query(&format!("SELECT IDs,MinValue,MaxValue,Step,DefaultValue FROM LENOVO_CAPABILITY_DATA_01 WHERE {filter}"))?,facade.query(&format!("SELECT IDs,Value FROM LENOVO_DISCRETE_DATA WHERE {filter}"))?))
}
fn god_read(facade: &WmiFacade, field: GodModeField) -> Result<i32> {
    signed_integer(
        &facade.method_read(
            "LENOVO_OTHER_METHOD",
            "GetFeatureValue",
            parameters([("IDs", MethodInput::U32(field.id()))]),
        )?,
        "Value",
    )
}
fn god_mode(facade: &WmiFacade) -> Result<u32> {
    integer(
        &facade.method_read("LENOVO_GAMEZONE_DATA", "GetSmartFanMode", BTreeMap::new())?,
        "Data",
    )
}
fn read_power_modes(facade: &WmiFacade) -> Result<crate::power_modes::PowerModes> {
    let current = god_mode(facade)?;
    let smart = facade
        .method_read("LENOVO_GAMEZONE_DATA", "IsSupportSmartFan", BTreeMap::new())
        .and_then(|value| integer(&value, "Data"))
        .ok();
    let zone = facade
        .method_read(
            "LENOVO_OTHER_METHOD",
            "GetFeatureValue",
            parameters([("IDs", MethodInput::U32(0x00090000))]),
        )
        .and_then(|value| integer(&value, "Value"))
        .or_else(|_| {
            facade
                .method_read(
                    "LENOVO_OTHER_METHOD",
                    "Get_Support_LegionZone_Version",
                    BTreeMap::new(),
                )
                .and_then(|value| integer(&value, "Version"))
        })
        .ok();
    let mask = facade
        .method_read(
            "LENOVO_OTHER_METHOD",
            "GetFeatureValue",
            parameters([("IDs", MethodInput::U32(0x00070000))]),
        )
        .and_then(|value| integer(&value, "Value"));
    let (mask, source) = match mask {
        Ok(value) => (Some(value), true),
        Err(_) => (
            facade
                .method_read(
                    "LENOVO_OTHER_METHOD",
                    "GetSupportThermalMode",
                    BTreeMap::new(),
                )
                .and_then(|value| integer(&value, "mode"))
                .ok(),
            false,
        ),
    };
    Ok(crate::power_modes::decode(
        current, mask, smart, zone, source,
    ))
}
struct NativeGodMode<'a> {
    facade: Rc<WmiFacade>,
    owner: &'a OwnerToken,
    action: &'a ExplicitAction,
}
impl GodModeFirmware for NativeGodMode<'_> {
    fn guard(&mut self) -> Result<()> {
        reject_other_controllers()?;
        ensure!(
            ac_connected()?,
            "AC power is required for Custom-mode writes"
        );
        Ok(())
    }
    fn mode(&mut self) -> Result<u32> {
        god_mode(&self.facade)
    }
    fn capabilities(
        &mut self,
    ) -> Result<(Vec<god_mode::SignedRange>, Vec<god_mode::DiscreteValue>)> {
        god_capabilities(&self.facade)
    }
    fn read(&mut self, field: GodModeField) -> Result<i32> {
        god_read(&self.facade, field)
    }
    fn validate_write(&mut self, field: GodModeField, value: i32) -> Result<()> {
        if field == GodModeField::CpuOverclockEnable {
            ensure!(
                (0..=1).contains(&value),
                "CPU overclock enable is a boolean enum"
            );
            ensure!(
                integer(
                    &self.facade.method_read(
                        "LENOVO_GAMEZONE_DATA",
                        "GetBIOSOCMode",
                        BTreeMap::new()
                    )?,
                    "Data"
                )? > 0,
                "BIOS CPU overclock mode is not enabled"
            );
        }
        self.facade.validate_inputs(
            "LENOVO_OTHER_METHOD",
            "SetFeatureValue",
            &god_parameters(field, value),
        )
    }
    fn write(&mut self, field: GodModeField, value: i32, deadline: u64) -> Result<()> {
        self.write_checked(field, value, deadline, &BTreeMap::new())
    }
    fn write_checked(
        &mut self,
        field: GodModeField,
        value: i32,
        deadline: u64,
        expected: &BTreeMap<GodModeField, i32>,
    ) -> Result<()> {
        self.facade.method_write_guarded(
            self.owner,
            self.action,
            "LENOVO_OTHER_METHOD",
            "SetFeatureValue",
            god_parameters(field, value),
            || {
                reject_other_controllers()?;
                ensure!(
                    ac_connected()?,
                    "AC disconnected before the physical Custom setter"
                );
                ensure!(
                    god_mode(&self.facade)? == 255,
                    "Mode changed during WMI method preparation"
                );
                for (field, value) in expected {
                    ensure!(
                        god_read(&self.facade, *field)? == *value,
                        "A watched Custom field changed during WMI method preparation"
                    );
                }
                ensure!(
                    god_mode(&self.facade)? == 255 && monotonic_ms() < deadline,
                    "Mode/deadline changed before the physical Custom setter"
                );
                Ok(())
            },
        )?;
        Ok(())
    }
}
fn read_fan_table(facade: &WmiFacade) -> Result<Vec<u32>> {
    let values = facade.method_read(
        "LENOVO_FAN_METHOD",
        "Fan_Get_Table",
        parameters([
            ("FanID", MethodInput::Byte(1)),
            ("SensorID", MethodInput::Byte(1)),
        ]),
    )?;
    let count = integer(&values, "FanTableSize")? as usize;
    let data = values
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("FanTable"))
        .and_then(|(_, value)| value.as_array())
        .context("Fan table response is not a uint32 array")?;
    ensure!(
        count == 10 && data.len() >= count,
        "Fan table readback must contain ten levels"
    );
    data[..count]
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .context("Invalid fan table level")
        })
        .collect()
}
fn require_lighting(facade: &WmiFacade, id: u32) -> Result<()> {
    if id == 4 {
        let rows: Vec<Value> =
            facade.query("SELECT IDs FROM LENOVO_CAPABILITY_DATA_00 WHERE IDs=100925440")?;
        ensure!(rows.len() == 1, "Power LED capability is absent/ambiguous");
        return Ok(());
    }
    let (control, kind) = match id {
        0 => (0, 1),
        3 | 5 => (1, 0),
        _ => bail!("Unknown lighting identifier"),
    };
    let rows:Vec<Value>=facade.query(&format!("SELECT Lighting_ID FROM LENOVO_LIGHTING_DATA WHERE Lighting_ID={id} AND Control_Interface={control} AND Lighting_Type={kind}"))?;
    ensure!(
        rows.len() == 1,
        "Lighting interface/type capability is absent/ambiguous"
    );
    Ok(())
}
fn require_mapped_capability(facade: &WmiFacade, feature: MappedFeature) -> Result<()> {
    let rows: Vec<Value> = facade.query(&format!(
        "SELECT IDs FROM LENOVO_CAPABILITY_DATA_00 WHERE IDs={}",
        feature.id()
    ))?;
    ensure!(
        rows.len() == 1,
        "Mapped feature is absent/ambiguous in firmware capability discovery"
    );
    Ok(())
}

fn capture_read(result: Result<impl Serialize>) -> Value {
    match result.and_then(|v| Ok(serde_json::to_value(v)?)) {
        Ok(value) => serde_json::json!({"available":true,"value":value}),
        Err(error) => serde_json::json!({"available":false,"error":error.to_string()}),
    }
}

fn read_capabilities(facade: &WmiFacade) -> Value {
    let mut values = serde_json::Map::new();
    for (key, table) in [
        ("flags", "LENOVO_CAPABILITY_DATA_00"),
        ("ranges", "LENOVO_CAPABILITY_DATA_01"),
        ("discrete", "LENOVO_DISCRETE_DATA"),
        ("lighting", "LENOVO_LIGHTING_DATA"),
        ("fans", "LENOVO_FAN_TABLE_DATA"),
    ] {
        values.insert(
            key.into(),
            capture_read(facade.query::<Value>(&format!("SELECT * FROM {table}"))),
        );
    }
    Value::Object(values)
}
pub fn ac_connected() -> Result<bool> {
    let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { GetSystemPowerStatus(&mut status) } != 0,
        "GetSystemPowerStatus failed"
    );
    ensure!(status.ACLineStatus != 255, "AC power state unknown");
    Ok(status.ACLineStatus == 1)
}

pub fn read_machine_identity() -> Result<MachineIdentity> {
    #[derive(Deserialize)]
    struct System {
        #[serde(rename = "Manufacturer")]
        manufacturer: String,
        #[serde(rename = "Model")]
        model: String,
    }
    #[derive(Deserialize)]
    struct Bios {
        #[serde(rename = "SMBIOSBIOSVersion")]
        version: String,
    }
    #[derive(Deserialize)]
    struct Cpu {
        #[serde(rename = "Name")]
        name: String,
        #[serde(rename = "NumberOfLogicalProcessors")]
        logical: u32,
    }
    let connection = WmiFacade::connect("ROOT\\CIMV2")?;
    let mut systems: Vec<System> =
        connection.query("SELECT Manufacturer,Model FROM Win32_ComputerSystem")?;
    let mut bios: Vec<Bios> = connection.query("SELECT SMBIOSBIOSVersion FROM Win32_BIOS")?;
    let mut cpus: Vec<Cpu> =
        connection.query("SELECT Name,NumberOfLogicalProcessors FROM Win32_Processor")?;
    ensure!(
        systems.len() == 1 && bios.len() == 1 && cpus.len() == 1,
        "Expected a single laptop system, BIOS and CPU"
    );
    let system = systems.remove(0);
    let bios = bios.remove(0);
    let cpu = cpus.remove(0);
    Ok(MachineIdentity {
        manufacturer: system.manufacturer.trim().into(),
        model: system.model.trim().into(),
        bios: bios.version.trim().into(),
        cpu: cpu.name.trim().into(),
        logical_processors: cpu.logical,
    })
}

pub fn read_cpu_discovery() -> Result<Value> {
    let facade = WmiFacade::connect("ROOT\\WMI")?;
    let mode = integer(
        &facade.method_read("LENOVO_GAMEZONE_DATA", "GetSmartFanMode", BTreeMap::new())?,
        "Data",
    )?;
    let ranges: Vec<RangeCapability> = facade.query(&cpu_range_query())?;
    let mut limits = BTreeMap::new();
    for (name, id) in [
        ("long_watts", PL1),
        ("short_watts", PL2),
        ("temperature_limit", CPU_TEMPERATURE_LIMIT),
    ] {
        limits.insert(
            name,
            integer(
                &facade.method_read(
                    "LENOVO_OTHER_METHOD",
                    "GetFeatureValue",
                    parameters([("IDs", MethodInput::U32(id))]),
                )?,
                "Value",
            )?,
        );
    }
    let after = integer(
        &facade.method_read("LENOVO_GAMEZONE_DATA", "GetSmartFanMode", BTreeMap::new())?,
        "Data",
    )?;
    ensure!(
        mode == after,
        "Firmware mode changed while sampling CPU discovery"
    );
    Ok(serde_json::json!({"raw_mode":mode,"limits":limits,"ranges":ranges,"read_only":true}))
}

pub(crate) fn reject_other_controllers() -> Result<()> {
    reject_other_controllers_except(&[])
}
pub(crate) fn reject_other_controllers_for_software(
    changes: &BTreeMap<String, bool>,
) -> Result<()> {
    let allowed = ["LenovoVantage.exe", "LegionZone.exe", "LegionSpace.exe"]
        .into_iter()
        .filter(|name| crate::software::scoped_controller_allowed(changes, name))
        .collect::<Vec<_>>();
    reject_other_controllers_except(&allowed)
}
fn reject_other_controllers_except(allowed: &[&str]) -> Result<()> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    ensure!(
        snapshot != INVALID_HANDLE_VALUE,
        "Cannot enumerate competing controller processes"
    );
    struct Handle(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let _handle = Handle(snapshot);
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut present = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    if !present {
        let error = std::io::Error::last_os_error();
        ensure!(
            error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32),
            "Competing controller inventory failed: {error}"
        );
    }
    while present {
        let end = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
        if entry.th32ProcessID != std::process::id()
            && [
                "Lenovo Legion Toolkit.exe",
                "LenovoVantage.exe",
                "LegionSpace.exe",
                "LegionZone.exe",
            ]
            .iter()
            .any(|n| name.eq_ignore_ascii_case(n))
            && !allowed
                .iter()
                .any(|allowed| name.eq_ignore_ascii_case(allowed))
        {
            bail!(
                "Another firmware controller is running: {name} (PID {}). It was not stopped",
                entry.th32ProcessID
            )
        }
        present = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        if !present {
            let error = std::io::Error::last_os_error();
            ensure!(
                error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32),
                "Competing controller inventory continuation failed: {error}"
            );
        }
    }
    Ok(())
}
/// Read-only ToolHelp guard for publishing a precise reason hardware writes are unavailable.
pub fn ensure_no_other_controllers() -> Result<()> {
    reject_other_controllers()
}

struct NativeFirmware {
    facade: Rc<WmiFacade>,
    owner: Rc<OwnerToken>,
    action: ExplicitAction,
    initial_deadline: Rc<std::cell::Cell<Option<u64>>>,
}
impl CpuFirmware for NativeFirmware {
    fn identity(&mut self) -> Result<MachineIdentity> {
        read_machine_identity()
    }
    fn ac_connected(&mut self) -> Result<bool> {
        ac_connected()
    }
    fn capabilities(&mut self) -> Result<CpuCapabilities> {
        #[derive(Deserialize)]
        struct Discrete {
            #[serde(rename = "IDs")]
            id: u32,
        }
        let ranges = self.facade.query(&cpu_range_query())?;
        let discrete: Vec<Discrete> = self.facade.query("SELECT IDs FROM LENOVO_DISCRETE_DATA")?;
        custom_cpu_capabilities(
            &ranges,
            &discrete.into_iter().map(|r| r.id).collect::<Vec<_>>(),
        )
    }
    fn mode(&mut self) -> Result<u32> {
        integer(
            &self
                .facade
                .method_read("LENOVO_GAMEZONE_DATA", "GetSmartFanMode", BTreeMap::new())?,
            "Data",
        )
    }
    fn read(&mut self, id: u32) -> Result<u32> {
        integer(
            &self.facade.method_read(
                "LENOVO_OTHER_METHOD",
                "GetFeatureValue",
                parameters([("IDs", MethodInput::U32(id))]),
            )?,
            "Value",
        )
    }
    fn write(&mut self, id: u32, value: u32) -> Result<()> {
        self.write_before(id, value, None)
    }
    fn write_before(&mut self, id: u32, value: u32, deadline: Option<u64>) -> Result<()> {
        ensure!(
            [PL1, PL2, CPU_TEMPERATURE_LIMIT].contains(&id),
            "Uncoordinated CPU write rejected"
        );
        reject_other_controllers()?;
        self.facade.method_write_guarded(
            &self.owner,
            &self.action,
            "LENOVO_OTHER_METHOD",
            "SetFeatureValue",
            parameters([
                ("IDs", MethodInput::U32(id)),
                ("value", MethodInput::U32(value)),
            ]),
            || {
                reject_other_controllers()?;
                ensure!(
                    deadline
                        .into_iter()
                        .chain(self.initial_deadline.get())
                        .all(|limit| monotonic_ms() < limit),
                    "CPU command expired after WMI metadata preparation; no setter was issued"
                );
                Ok(())
            },
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SpectrumAction {
    ReadActiveProfile {
        path: String,
    },
    ReadEffects {
        path: String,
        profile: u8,
    },
    Reset {
        path: String,
        profile: u8,
    },
    Brightness {
        path: String,
        value: u8,
    },
    SelectProfile {
        path: String,
        profile: u8,
    },
    Effects {
        path: String,
        profile: crate::spectrum::Profile,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HardwareAction {
    ShutdownWorker,
    SetRawMode(u32),
    SetMapped {
        feature: MappedFeature,
        value: u32,
    },
    SetFanTable(Vec<u8>),
    SetLighting {
        lighting_id: u32,
        state_type: u32,
        brightness: u32,
    },
    EnableCpu {
        firmware_limit: u32,
    },
    SetCpuPower {
        power: Envelope,
        /// Absolute GetTickCount64 milliseconds, never Unix/wall time.
        deadline_tick_ms: u64,
    },
    StopCpu,
    ReadCpu,
    ReadCpuTemperature,
    ReadMapped(MappedFeature),
    ReadLighting {
        lighting_id: u32,
    },
    ReadCapabilities,
    ReadPowerModes,
    ReadHybrid,
    SetHybrid {
        mode: crate::hybrid::HybridMode,
        deadline_tick_ms: u64,
    },
    ReadAi,
    SetAiSubmode {
        submode: u32,
        deadline_tick_ms: u64,
    },
    ReadGodMode,
    ApplyGodMode(GodModePatch),
    ReadDevices,
    ReadDeviceInformation,
    ReadNvidia,
    ReadFanTable,
    ReadFirmwareSensors,
    ReadGpuInventory,
    ReadIntegratedGpuInventory,
    ReadIntegratedGpuSensors {
        luid_low: u32,
        luid_high: i32,
        expected_name: String,
    },
    StopGpuSensors,
    ReadGpuSensors {
        index: u32,
        expected_name: String,
    },
    ReadGpuDeviceNodes,
    ReadGpuKeepAwake,
    SetGpuKeepAwake {
        expected_name: String,
        enabled: bool,
    },
    RequestGpuSleep,
    ReadGpuPerformance {
        index: u32,
        expected_name: String,
    },
    SetGpuPerformance {
        index: u32,
        expected_name: String,
        state: Option<u32>,
        deadline_tick_ms: u64,
    },
    ReadGpuProcesses {
        index: u32,
        expected_name: String,
    },
    RestartGpu {
        instance_id: String,
        require_started: bool,
    },
    KillGpuProcesses(Vec<crate::gpu_management::ProcessIdentity>),
    KillGpuApps {
        index: u32,
        expected_name: String,
        instance_id: String,
    },
    SetGpuPreference {
        process: crate::gpu_management::ProcessIdentity,
        preference: crate::gpu_management::GpuPreference,
    },
    ReadGpuOverclock {
        index: u32,
        expected_name: String,
    },
    SetGpuOverclock {
        index: u32,
        expected_name: String,
        requested: crate::gpu::OverclockInfo,
        deadline_tick_ms: u64,
    },
    ReadSystem,
    ReadBootLogo,
    BootLogo(crate::boot_logo::Request),
    ReadAutorun,
    SetAutorun(crate::autorun::Mode),
    StartDriverKeyEvents,
    ReadDriverKeyEvents,
    StopDriverKeyEvents,
    ReadBatteryInformation,
    ReadBatteryHistory {
        reset_on_reboot: bool,
    },
    ReadBatteryTemperature,
    ReadStorageSensors,
    ReadMemorySensors,
    StopAuxSensors,
    ReadSoftware,
    SetSoftware {
        changes: BTreeMap<String, bool>,
        deadline_tick_ms: u64,
    },
    ReadWireless,
    SetWifiEnabled(bool),
    SetAirplaneMode(bool),
    ReadDisplayControls,
    ReadSpectrumCandidates,
    Spectrum(SpectrumAction),
    System(crate::system_controls::SystemAction),
    Display(crate::display_controls::DisplayAction),
    SetSpeakerMute(bool),
    SetMicrophoneMute(bool),
    SetSpeakerVolume(u32),
    SetDisplayMode {
        device: String,
        width: u32,
        height: u32,
        hertz: u32,
    },
    SendKey {
        virtual_key: u16,
        pressed: bool,
    },
    HidFeatureWrite {
        path: String,
        bytes: Vec<u8>,
    },
    Energy(crate::energy::EnergyAction),
    ReadEnergy,
    ChargeThreshold(crate::charge_threshold::Threshold),
    ReadChargeThreshold,
    ReadTelemetry,
    ReadIntelSensors,
}

struct NativeState {
    facade: Rc<WmiFacade>,
    owner: Option<Rc<OwnerToken>>,
    cpu: Option<CpuSession<NativeFirmware>>,
    journal: JournalStore,
    sampler: crate::telemetry::SystemSampler,
    intel_sampler: crate::intel_sensors::IntelSampler,
    gpu_sampler: crate::nvapi_native::GpuSampler,
    intel_gpu_sampler: crate::intel_gpu::IntelGpuSampler,
    awake: Option<crate::gpu_awake::GpuLease>,
    boot: Option<crate::boot_logo::Session<crate::boot_logo::native::NativePort>>,
    driver_keys: Option<crate::driver_keys::Session<crate::driver_keys::NativePort>>,
    memory_sampler: crate::memory_sensors::Sampler,
}
impl NativeState {
    fn boot_root(&self) -> Result<PathBuf> {
        Ok(self
            .journal
            .path()
            .parent()
            .context("Hardware data directory missing")?
            .to_path_buf())
    }
    fn run_boot(
        &mut self,
        request: crate::boot_logo::Request,
        permit: ExplicitAction,
    ) -> Result<Value> {
        use crate::boot_logo::{self as b, BootPort};
        ensure!(
            self.cpu.is_none(),
            "Stop and verify CPU restoration before boot firmware/BCD changes"
        );
        let root = self.boot_root()?;
        if !matches!(request, b::Request::RetryRecovery) {
            permit.check_write_boundary()?;
        }
        let owner = self.owner()?;
        if self.boot.is_some() {
            ensure!(
                matches!(request, b::Request::RetryRecovery),
                "A prior boot transaction/cleanup requires RetryRecovery before another request"
            );
        }
        if self.boot.is_none() {
            let port = b::native::NativePort::new(root.clone(), owner, permit.clone())?;
            let pending = port.pending_record()?;
            ensure!(
                matches!(request, b::Request::RetryRecovery)
                    || pending.is_none() && !port.has_lease(),
                "A prior EFI lease/boot transaction requires explicit RetryRecovery before a new change"
            );
            self.boot = Some(if let Some(record) = pending {
                b::Session::from_record(port, record)?
            } else {
                b::Session::new(port)
            });
        }
        let session = self.boot.as_mut().unwrap();
        session.port.replace_action(permit);
        let result = (|| -> Result<Value> {
            let report = match request {
                b::Request::RetryRecovery => {
                    if session.pending() {
                        session.recover()?
                    } else {
                        session.shutdown()?;
                        b::Report {
                            backup_id: String::new(),
                            state: b::TransactionState::Restored,
                            firmware_verified: false,
                            files_verified: false,
                            animation_verified: false,
                            mount_cleanup_verified: true,
                            permanent_user_change: false,
                        }
                    }
                }
                b::Request::Restore { backup_id } => {
                    ensure!(
                        !session.pending(),
                        "Recover the active boot transaction before restoring another backup"
                    );
                    let record = session.port.backup(&backup_id)?;
                    session.restore_as_new_change(record.baseline, unix_ms())?
                }
                b::Request::Apply {
                    logo,
                    animation_disabled,
                } => {
                    ensure!(
                        !session.pending(),
                        "Recover the active boot transaction before another boot change"
                    );
                    let logo = logo
                        .map(|change| match change {
                            b::LogoChange::Disable => Ok(b::PreparedLogo::Disable),
                            b::LogoChange::Enable { source } => {
                                ensure!(
                                    source.is_absolute()
                                        && source.to_str().is_some_and(|v| !v
                                            .starts_with("\\\\.\\")
                                            && !v.contains('\0')),
                                    "Logo source must be an absolute normal image file"
                                );
                                use std::io::Read;
                                use std::os::windows::fs::OpenOptionsExt;
                                let mut file = std::fs::OpenOptions::new()
                                    .read(true)
                                    .share_mode(
                                        windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ,
                                    )
                                    .open(&source)?;
                                let meta = file.metadata()?;
                                ensure!(
                                    meta.is_file() && meta.len() <= b::MAX_IMAGE_BYTES as u64,
                                    "Logo source is not a bounded regular file"
                                );
                                let mut bytes = Vec::new();
                                file.by_ref()
                                    .take(b::MAX_IMAGE_BYTES as u64 + 1)
                                    .read_to_end(&mut bytes)?;
                                let firmware = session.port.firmware()?;
                                Ok(b::PreparedLogo::Enable(b::prepare_image(
                                    &bytes,
                                    &firmware.validate()?,
                                )?))
                            }
                        })
                        .transpose()?;
                    session.apply(logo, animation_disabled, unix_ms())?
                }
            };
            Ok(serde_json::json!({"operation":report,"status":b::native::status(&root)}))
        })();
        // Fallible rollback/unmount retains the native port and its Rc owner.
        if !session.pending() {
            match session.shutdown() {
                Ok(()) => {
                    self.boot.take();
                }
                Err(cleanup) => {
                    return Err(cleanup.context(format!(
                        "Boot operation error={:?}; EFI cleanup still requires ownership",
                        result.as_ref().err().map(|e| format!("{e:#}"))
                    )));
                }
            }
        }
        result
    }
    fn owner(&mut self) -> Result<Rc<OwnerToken>> {
        self.owner_with_guard(reject_other_controllers)
    }
    fn owner_with_guard(&mut self, guard: impl Fn() -> Result<()>) -> Result<Rc<OwnerToken>> {
        if let Some(owner) = &self.owner {
            guard()?;
            return Ok(owner.clone());
        }
        ensure!(
            read_machine_identity()?.is_supported(),
            "Machine identity is not approved for hardware writes"
        );
        guard()?;
        let owner = OwnerToken::acquire()?;
        if owner.abandoned {
            self.journal.mark_recovery_required("A prior hardware thread/process ended while owning the kernel mutex. Physical restoration is unconfirmed; inspect firmware and journal before clearing this marker.")?;
            bail!(
                "Abandoned firmware owner requires manual baseline/journal review; no write was issued"
            )
        }
        if let Some(prior) = self.journal.read()? {
            ensure!(
                matches!(
                    prior.state,
                    JournalState::Restored | JournalState::ReleasedExternal
                ),
                "Unverified prior owner journal blocks new hardware writes"
            );
        }
        let owner = Rc::new(owner);
        self.owner = Some(owner.clone());
        Ok(owner)
    }
    fn run(&mut self, action: HardwareAction, permit: ExplicitAction) -> Result<Value> {
        if matches!(action, HardwareAction::StopAuxSensors) {
            self.memory_sampler.stop()?;
            self.memory_sampler = Default::default();
            return Ok(
                serde_json::json!({"owned_sensor_handles_released":true,"driver_stopped":false}),
            );
        }
        if matches!(action, HardwareAction::StopDriverKeyEvents) {
            let result = if let Some(session) = self.driver_keys.as_mut() {
                session.stop()?
            } else {
                crate::driver_keys::Snapshot::stopped()
            };
            self.driver_keys.take();
            return Ok(serde_json::to_value(result)?);
        }
        if matches!(action, HardwareAction::ReadDriverKeyEvents) {
            return Ok(serde_json::to_value(
                self.driver_keys
                    .as_mut()
                    .context("Driver key source has not been explicitly started")?
                    .read()?,
            )?);
        }
        if matches!(action, HardwareAction::StopGpuSensors) {
            self.gpu_sampler = Default::default();
            self.intel_gpu_sampler = Default::default();
            return Ok(
                serde_json::json!({"sensor_contexts_released":true,"physical_sleep_verified":false}),
            );
        }
        if matches!(
            action,
            HardwareAction::SetGpuKeepAwake { enabled: false, .. }
        ) {
            self.awake.take();
            return Ok(
                serde_json::json!({"keep_awake_active":false,"context_released":true,"physical_sleep_verified":false}),
            );
        }
        if matches!(action, HardwareAction::StopCpu) {
            return crate::cpu::stop_session(&mut self.cpu);
        }
        match &action {
            HardwareAction::ReadBatteryInformation => {
                return Ok(serde_json::to_value(crate::battery::read_information()?)?);
            }
            HardwareAction::ReadBatteryHistory { reset_on_reboot } => {
                return Ok(serde_json::to_value(crate::battery_history::read(
                    *reset_on_reboot,
                )?)?);
            }
            HardwareAction::ReadBatteryTemperature => {
                return Ok(serde_json::to_value(crate::battery::read_temperature()?)?);
            }
            HardwareAction::ReadStorageSensors => {
                return Ok(serde_json::to_value(crate::storage_sensors::read()?)?);
            }
            HardwareAction::ReadMemorySensors => {
                return Ok(serde_json::to_value(self.memory_sampler.sample(&permit)?)?);
            }
            HardwareAction::ReadDeviceInformation => return crate::device_information::read(),
            HardwareAction::ReadBootLogo => {
                return Ok(crate::boot_logo::native::status(&self.boot_root()?));
            }
            HardwareAction::ReadAutorun => {
                return Ok(serde_json::to_value(crate::autorun::read(
                    &self.boot_root()?,
                )?)?);
            }
            HardwareAction::ReadIntegratedGpuInventory => {
                return Ok(serde_json::to_value(crate::intel_gpu::inventory()?)?);
            }
            HardwareAction::ReadIntegratedGpuSensors {
                luid_low,
                luid_high,
                expected_name,
            } => {
                // Preference changes release the other driver's cached reader
                // on this worker, before opening the newly selected source.
                self.gpu_sampler = Default::default();
                let mut sample =
                    self.intel_gpu_sampler
                        .sample(*luid_low, *luid_high, expected_name)?;
                if sample
                    .values
                    .get("GpuPower")
                    .and_then(|v| v.value)
                    .is_none()
                {
                    match self.intel_sampler.gpu_power() {
                        Ok(Some(watts)) => sample.insert(
                            "GpuPower",
                            watts,
                            "W",
                            "Intel PP1 MSR 0x641 RAPL energy delta",
                        )?,
                        Ok(None) => {}
                        Err(e) => {
                            sample.errors.insert("pp1".into(), format!("{e:#}"));
                        }
                    }
                }
                sample.completed_at_tick_ms = monotonic_ms();
                return Ok(serde_json::to_value(sample)?);
            }
            HardwareAction::ReadWireless => return Ok(crate::wireless::read()),
            HardwareAction::ReadGpuKeepAwake => {
                return Ok(
                    serde_json::json!({"active":self.awake.is_some(),"name":self.awake.as_ref().map(|v|&v.name)}),
                );
            }
            HardwareAction::ReadGpuSensors {
                index,
                expected_name,
            } => {
                self.intel_gpu_sampler = Default::default();
                // Original GPU status classification gives Hybrid Off priority.
                // Use this actor's existing WMI facade, never a UI preference.
                let requested = monotonic_ms();
                let hybrid = crate::hybrid::read(&self.facade);
                let mut sample = self.gpu_sampler.sample(*index, expected_name)?;
                sample.requested_at_tick_ms = requested;
                sample.hybrid_off = hybrid
                    .selected()
                    .map(|mode| mode == crate::hybrid::HybridMode::Off);
                if sample.hybrid_off.is_none() {
                    sample.errors.insert(
                        "hybrid_status".into(),
                        format!("Hybrid state is unknown: {:?}", hybrid.errors),
                    );
                }
                return Ok(serde_json::to_value(sample)?);
            }
            HardwareAction::ReadGpuPerformance {
                index,
                expected_name,
            } => return crate::nvapi_native::read_performance(*index, expected_name),
            HardwareAction::ReadGpuProcesses {
                index,
                expected_name,
            } => return crate::nvml_processes::read(*index, expected_name),
            HardwareAction::ReadGpuDeviceNodes => {
                return Ok(serde_json::to_value(crate::gpu_management::devices()?)?);
            }
            HardwareAction::ReadIntelSensors => {
                return Ok(serde_json::to_value(self.intel_sampler.sample()?)?);
            }
            HardwareAction::ReadHybrid => {
                let snapshot = crate::hybrid::read(&self.facade);
                return Ok(
                    serde_json::json!({"snapshot":snapshot,"modes":snapshot.modes().into_iter().map(|m|serde_json::json!({"raw":m as u32,"name":m.label()})).collect::<Vec<_>>(),"selected":snapshot.selected().map(|m|m as u32),"physical_topology_verified":false}),
                );
            }
            HardwareAction::ReadAi => {
                return Ok(serde_json::to_value(crate::ai::read(&self.facade)?)?);
            }
            HardwareAction::ReadPowerModes => {
                return Ok(serde_json::to_value(read_power_modes(&self.facade)?)?);
            }
            HardwareAction::ReadGodMode => {
                let mode = god_mode(&self.facade)?;
                let (ranges, discrete) = god_capabilities(&self.facade)?;
                let fields=GodModeField::ALL.into_iter().map(|field|serde_json::json!({"field":field,"key":field.semantic_key(),"capability":capture_read(god_mode::capability(field,&ranges,&discrete)),"current":capture_read(god_read(&self.facade,field))})).collect::<Vec<_>>();
                ensure!(
                    god_mode(&self.facade)? == mode,
                    "Mode changed while reading Custom capabilities"
                );
                return Ok(
                    serde_json::json!({"raw_mode":mode,"fields":fields,"read_only":true,"defaults_applied":false}),
                );
            }
            HardwareAction::ReadFanTable => {
                return Ok(serde_json::to_value(read_fan_table(&self.facade)?)?);
            }
            HardwareAction::ReadFirmwareSensors => {
                let requested = monotonic_ms();
                let mut values = serde_json::Map::new();
                for (name, id) in [
                    ("cpu_aggregate_celsius", CPU_TEMPERATURE),
                    ("gpu_aggregate_celsius", 0x05050000),
                    ("pch_celsius", 0x05010000),
                    ("cpu_fan_rpm", 0x04030001),
                    ("gpu_fan_rpm", 0x04030002),
                    ("pch_fan_rpm", 0x04030004),
                ] {
                    values.insert(
                        name.into(),
                        capture_read(
                            self.facade
                                .method_read(
                                    "LENOVO_OTHER_METHOD",
                                    "GetFeatureValue",
                                    parameters([("IDs", MethodInput::U32(id))]),
                                )
                                .and_then(|value| integer(&value, "Value")),
                        ),
                    );
                }
                return Ok(
                    serde_json::json!({"requested_at_tick_ms":requested,"completed_at_tick_ms":monotonic_ms(),"values":values,"source":"Lenovo firmware capability getters; aggregate temperatures, no package/core maximum"}),
                );
            }
            HardwareAction::ReadGpuInventory => {
                return Ok(serde_json::to_value(crate::nvapi_native::inventory()?)?);
            }
            HardwareAction::ReadGpuOverclock {
                index,
                expected_name,
            } => return crate::nvapi_native::read_overclock(*index, expected_name),
            HardwareAction::ReadSpectrumCandidates => {
                let identity = read_machine_identity()?;
                let candidates = crate::devices::hid_interfaces()?
                    .into_iter()
                    .filter(|device| {
                        device.metadata.as_ref().is_some_and(|caps| {
                            crate::spectrum::is_83nn_candidate(
                                &identity,
                                crate::spectrum::HidCandidate {
                                    vendor_id: caps.vendor_id,
                                    product_id: caps.product_id,
                                    feature_report_bytes: caps.feature_length,
                                },
                            )
                        })
                    })
                    .collect::<Vec<_>>();
                return Ok(
                    serde_json::json!({"candidates":candidates,"zones":24,"protocol_probed":false,"discovery_sent_feature_reports":false}),
                );
            }
            HardwareAction::ReadSystem => {
                return Ok(serde_json::json!({
                    "power_status":crate::power_status::read(&self.facade),
                    "ac_connected":capture_read(ac_connected()),
                    "power_plans":capture_read(crate::system_controls::power_plans()),
                    "effective_power_mode":capture_read(crate::system_controls::effective_power_mode()),
                    "power_policy":capture_read(crate::system_controls::power_policy_snapshot()),
                    "brightness":capture_read(crate::system_controls::brightness()),
                    "win_key":capture_read(crate::system_controls::read_switch(crate::system_controls::GameZoneSwitch::WinKey)),
                    "touchpad_lock":capture_read(crate::system_controls::read_switch(crate::system_controls::GameZoneSwitch::TouchpadLock))
                }));
            }
            HardwareAction::ReadSoftware => {
                return Ok(serde_json::to_value(crate::software::read())?);
            }
            HardwareAction::ReadDisplayControls => {
                return Ok(serde_json::to_value(crate::display_controls::inventory()?)?);
            }
            HardwareAction::ReadMapped(feature) => {
                require_mapped_capability(&self.facade, *feature)?;
                let value = integer(
                    &self.facade.method_read(
                        "LENOVO_OTHER_METHOD",
                        "GetFeatureValue",
                        parameters([("IDs", MethodInput::U32(feature.id()))]),
                    )?,
                    "Value",
                )?;
                feature.validate(value)?;
                return Ok(serde_json::json!({"feature":feature,"id":feature.id(),"value":value}));
            }
            HardwareAction::ReadLighting { lighting_id } => {
                ensure!(
                    [0, 3, 4, 5].contains(lighting_id),
                    "Unknown lighting identifier"
                );
                require_lighting(&self.facade, *lighting_id)?;
                return Ok(serde_json::to_value(self.facade.method_read(
                    "LENOVO_LIGHTING_METHOD",
                    "Get_Lighting_Current_Status",
                    parameters([("Lighting_ID", MethodInput::U32(*lighting_id))]),
                )?)?);
            }
            HardwareAction::ReadCapabilities => return Ok(read_capabilities(&self.facade)),
            HardwareAction::ReadDevices => {
                return Ok(serde_json::json!({
                    "displays":capture_read(crate::devices::displays()),
                    "audio":capture_read(crate::devices::audio_endpoints()),
                    "batteries":capture_read(crate::devices::batteries()),
                    "hid":capture_read(crate::devices::hid_inventory_read_only())
                ,"hid_interfaces":capture_read(crate::devices::hid_interfaces())
                }));
            }
            HardwareAction::ReadNvidia => {
                return Ok(serde_json::to_value(crate::nvml::inventory()?)?);
            }
            _ => (),
        }
        if matches!(action, HardwareAction::ReadTelemetry) {
            return Ok(serde_json::to_value(self.sampler.sample())?);
        }
        if matches!(action, HardwareAction::ReadEnergy) {
            return Ok(serde_json::to_value(crate::energy::read_status()?)?);
        }
        if matches!(action, HardwareAction::ReadChargeThreshold) {
            return Ok(serde_json::to_value(crate::charge_threshold::read()?)?);
        }
        if matches!(
            action,
            HardwareAction::ReadCpu | HardwareAction::ReadCpuTemperature
        ) {
            return match (&mut self.cpu, action) {
                (Some(cpu), HardwareAction::ReadCpu) => Ok(serde_json::to_value(cpu.sample()?)?),
                (Some(cpu), _) => Ok(serde_json::to_value(cpu.temperature()?)?),
                (_, HardwareAction::ReadCpu) => read_cpu_discovery(),
                _ => {
                    let requested = monotonic_ms();
                    let celsius = integer(
                        &self.facade.method_read(
                            "LENOVO_OTHER_METHOD",
                            "GetFeatureValue",
                            parameters([("IDs", MethodInput::U32(CPU_TEMPERATURE))]),
                        )?,
                        "Value",
                    )?;
                    Ok(serde_json::to_value(TemperatureSample::firmware(
                        celsius, requested,
                    )?)?)
                }
            };
        }
        if let HardwareAction::BootLogo(request) = action {
            return self.run_boot(request, permit);
        }
        let boot_store = crate::boot_logo::BackupStore::new(self.boot_root()?);
        ensure!(
            self.boot.is_none() && boot_store.pending()?.is_none() && boot_store.lease()?.is_none(),
            "A boot firmware transaction/EFI lease requires verified recovery before another hardware write"
        );
        if !matches!(action, HardwareAction::SendKey { pressed: false, .. }) {
            permit.check_deadline()?;
        }
        let owner = if let HardwareAction::SetSoftware { changes, .. } = &action {
            ensure!(
                self.cpu.is_none(),
                "Stop CPU control before changing vendor services/processes"
            );
            self.owner_with_guard(|| reject_other_controllers_for_software(changes))?
        } else {
            self.owner()?
        };
        match action {
            HardwareAction::StartDriverKeyEvents => {
                if self.driver_keys.is_none() {
                    self.driver_keys = Some(crate::driver_keys::Session::new(
                        crate::driver_keys::NativePort::open(owner, permit)?,
                    ));
                }
                Ok(serde_json::to_value(
                    self.driver_keys.as_mut().unwrap().start()?,
                )?)
            }
            HardwareAction::SetAutorun(mode) => Ok(serde_json::to_value(crate::autorun::write(
                &owner,
                &permit,
                &self.boot_root()?,
                mode,
            )?)?),
            HardwareAction::SetSoftware {
                changes,
                deadline_tick_ms,
            } => {
                ensure!(
                    self.cpu.is_none(),
                    "Stop CPU control before changing vendor services/processes"
                );
                permit.check_deadline()?;
                let outcome = crate::software::apply(&owner, &permit, changes, deadline_tick_ms)?;
                ensure!(
                    outcome.success,
                    "Vendor software change partially failed; completed {} operations; {}. Refresh status before retrying",
                    outcome.completed.len(),
                    outcome.errors.join("; ")
                );
                Ok(serde_json::to_value(outcome)?)
            }
            HardwareAction::SetGpuKeepAwake {
                expected_name,
                enabled: true,
            } => {
                let guard = || {
                    crate::gpu_awake::validate_policy(
                        &crate::hybrid::read(&self.facade),
                        ac_connected()?,
                    )
                };
                guard()?;
                if self
                    .awake
                    .as_ref()
                    .is_some_and(|v| crate::gpu_awake::names_match(&v.name, &expected_name))
                {
                    return Ok(serde_json::json!({"keep_awake_active":true,"changed":false}));
                }
                ensure!(
                    self.awake.is_none(),
                    "Release existing keep-awake context before selecting another adapter"
                );
                self.awake = Some(crate::gpu_awake::GpuLease::open(
                    owner.clone(),
                    &permit,
                    &expected_name,
                    guard,
                )?);
                Ok(
                    serde_json::json!({"keep_awake_active":true,"context_owned":true,"name":self.awake.as_ref().map(|v|&v.name)}),
                )
            }
            HardwareAction::RequestGpuSleep => {
                let request = crate::gpu_sleep::prepare(&self.facade)?;
                permit.check_deadline()?;
                if let Some(cpu) = &mut self.cpu {
                    cpu.verify_ownership()?;
                }
                self.awake.take();
                self.gpu_sampler = Default::default();
                let result = crate::gpu_sleep::execute(&self.facade, &owner, &permit, request)
                    .context(
                        "Keep-awake context released; GPU notification must be checked separately",
                    );
                if let Some(cpu) = &mut self.cpu {
                    cpu.verify_ownership()?;
                }
                result
            }
            HardwareAction::SetGpuPerformance {
                index,
                expected_name,
                state,
                deadline_tick_ms,
            } => {
                if let Some(cpu) = &mut self.cpu {
                    cpu.verify_ownership()?;
                }
                let result = crate::nvapi_native::set_performance(
                    &owner,
                    &permit,
                    index,
                    &expected_name,
                    state,
                    deadline_tick_ms,
                );
                if let Some(cpu) = &mut self.cpu {
                    cpu.verify_ownership()?;
                }
                result
            }
            HardwareAction::RestartGpu { instance_id, require_started } => {
                ensure!(
                    self.cpu.is_none(),
                    "Stop and verify CPU restoration before restarting GPU"
                );
                self.gpu_sampler = Default::default();
                crate::gpu_management::restart(&owner, &permit, &instance_id, require_started)
            }
            HardwareAction::KillGpuProcesses(processes) => {
                crate::gpu_management::kill(&owner, &permit, &processes)
            }
            HardwareAction::KillGpuApps { index, expected_name, instance_id } => {
                permit.check_write_boundary()?;
                let devices = crate::gpu_management::devices()?;
                ensure!(
                    index == 0 && devices.len() == 1 && devices[0].started
                        && devices[0].problem_code == 0
                        && devices[0].instance_id.eq_ignore_ascii_case(&instance_id)
                        && devices[0].description.eq_ignore_ascii_case(&expected_name),
                    "本机独显 PnP 身份或启动状态已变化"
                );
                let hybrid = crate::hybrid::read(&self.facade).selected()
                    .context("无法核对独显混合模式，未结束应用")?;
                if hybrid == crate::hybrid::HybridMode::Off {
                    return Ok(serde_json::to_value(crate::gpu_management::AutomationKillReport::default())?);
                }
                let mut gpu = crate::nvapi_native::NativeNvApi::open(index, &expected_name, None, None, None)?;
                let report = crate::gpu_management::kill_automation_apps(&owner, &permit, || {
                    match crate::gpu_status::coproc(&mut gpu)? {
                        0 | 2 => Ok(vec![]),
                        1 => crate::gpu_status::active_application_pids(&mut gpu),
                        raw => bail!("未识别的独显协处理器状态 {raw}，未结束应用"),
                    }
                })?;
                Ok(serde_json::to_value(report)?)
            }
            HardwareAction::SetGpuPreference {
                process,
                preference,
            } => crate::gpu_management::set_preference(&owner, &permit, &process, preference),
            HardwareAction::SetHybrid {
                mode,
                deadline_tick_ms,
            } => {
                let needs_release = matches!(
                    mode,
                    crate::hybrid::HybridMode::UMA | crate::hybrid::HybridMode::OnIGPUOnly
                ) || mode == crate::hybrid::HybridMode::OnAuto
                    && !ac_connected()?;
                let mut prepare = || -> Result<()> {
                    if needs_release {
                        permit.check_deadline()?;
                        self.awake.take();
                        self.gpu_sampler = Default::default();
                    }
                    Ok(())
                };
                let mut firmware = crate::hybrid::NativeHybrid {
                    facade: &self.facade,
                    owner: &owner,
                    action: &permit,
                    before_execution: Some(&mut prepare),
                };
                Ok(serde_json::to_value(crate::hybrid::execute(
                    &mut firmware,
                    mode,
                    deadline_tick_ms,
                    self.cpu.is_some(),
                )?)?)
            }
            HardwareAction::SetAiSubmode {
                submode,
                deadline_tick_ms,
            } => {
                let mut firmware = crate::ai::NativeAi {
                    facade: &self.facade,
                    owner: &owner,
                    action: &permit,
                };
                Ok(serde_json::to_value(crate::ai::execute(
                    &mut firmware,
                    submode,
                    deadline_tick_ms,
                    self.cpu.is_some(),
                )?)?)
            }
            HardwareAction::ApplyGodMode(patch) => {
                if let Some(cpu) = &mut self.cpu {
                    cpu.verify_ownership()?;
                }
                let mut firmware = NativeGodMode {
                    facade: self.facade.clone(),
                    owner: &owner,
                    action: &permit,
                };
                let result = god_mode::execute(&mut firmware, &patch, self.cpu.is_some());
                if let Some(cpu) = &mut self.cpu {
                    cpu.verify_ownership()?;
                }
                Ok(serde_json::to_value(result?)?)
            }
            HardwareAction::SetGpuOverclock {
                index,
                expected_name,
                requested,
                deadline_tick_ms,
            } => {
                if let Some(cpu) = &mut self.cpu {
                    cpu.verify_ownership()?;
                }
                let facade = self.facade.clone();
                let guard = || -> Result<()> {
                    reject_other_controllers()?;
                    ensure!(
                        monotonic_ms() < deadline_tick_ms,
                        "GPU action deadline expired"
                    );
                    ensure!(
                        ac_connected()?,
                        "AC power is required for GPU overclock writes"
                    );
                    ensure!(
                        integer(
                            &facade.method_read(
                                "LENOVO_GAMEZONE_DATA",
                                "IsSupportGpuOC",
                                BTreeMap::new()
                            )?,
                            "Data"
                        )? > 0,
                        "Lenovo firmware does not advertise GPU OC"
                    );
                    let igpu = integer(
                        &facade.method_read(
                            "LENOVO_OTHER_METHOD",
                            "GetFeatureValue",
                            parameters([("IDs", MethodInput::U32(MappedFeature::IgpuMode.id()))]),
                        )?,
                        "Value",
                    )?;
                    ensure!(
                        igpu != 1,
                        "iGPU-only mode blocks waking/applying dGPU overclock"
                    );
                    ensure!(
                        monotonic_ms() < deadline_tick_ms,
                        "GPU action expired after capability readback"
                    );
                    Ok(())
                };
                guard()?;
                let mut transport = crate::nvapi_native::NativeNvApi::open(
                    index,
                    &expected_name,
                    Some(&owner),
                    Some(&permit),
                    Some(deadline_tick_ms),
                )?;
                let snapshot = crate::gpu::read_snapshot(&mut transport)?;
                let plan = crate::gpu::plan_overclock(&snapshot, requested)?;
                let result = crate::gpu::execute_overclock(&mut transport, &plan, guard);
                if let Some(cpu) = &mut self.cpu {
                    cpu.verify_ownership()?;
                }
                Ok(serde_json::to_value(result?)?)
            }
            HardwareAction::Spectrum(request) => {
                use crate::spectrum::SpectrumTransport;
                match request {
                    SpectrumAction::ReadActiveProfile { path } => {
                        let mut device =
                            crate::devices::SpectrumDevice::open(&owner, &permit, &path)?;
                        // This protocol query includes a request SetFeature. Keep it
                        // on the explicitly authorized writer and retain its handle.
                        Ok(
                            serde_json::json!({"profile":crate::spectrum::read_profile(&mut device)?}),
                        )
                    }
                    SpectrumAction::ReadEffects { path, profile } => {
                        crate::spectrum::effects_request(profile)?;
                        let mut device =
                            crate::devices::SpectrumDevice::open(&owner, &permit, &path)?;
                        Ok(serde_json::to_value(crate::spectrum::read_effects(
                            &mut device,
                            profile,
                        )?)?)
                    }
                    SpectrumAction::Reset { path, profile } => {
                        let frame = crate::spectrum::reset_profile(profile)?;
                        let mut device =
                            crate::devices::SpectrumDevice::open(&owner, &permit, &path)?;
                        device.set_feature(&frame)?;
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        let first = crate::spectrum::read_effects(&mut device, profile)?;
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        let second = crate::spectrum::read_effects(&mut device, profile)?;
                        ensure!(first == second, "Spectrum reset readback did not settle");
                        let mut result = serde_json::to_value(second)?;
                        result["reset_request_accepted"] = Value::Bool(true);
                        result["readback_consistent"] = Value::Bool(true);
                        result["factory_default_equality_verified"] = Value::Bool(false);
                        Ok(result)
                    }
                    SpectrumAction::Brightness { path, value } => {
                        let mut device =
                            crate::devices::SpectrumDevice::open(&owner, &permit, &path)?;
                        crate::spectrum::apply_brightness_verified(
                            &mut device,
                            value,
                            reject_other_controllers,
                        )?;
                        Ok(Value::from(value))
                    }
                    SpectrumAction::SelectProfile { path, profile } => {
                        let frame = crate::spectrum::set_profile(profile)?;
                        let mut device =
                            crate::devices::SpectrumDevice::open(&owner, &permit, &path)?;
                        device.set_feature(&frame)?;
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        ensure!(
                            crate::spectrum::read_profile(&mut device)? == profile,
                            "Spectrum profile selection readback mismatch"
                        );
                        if profile == 0 {
                            Ok(serde_json::json!({"profile":0,"effects":[]}))
                        } else {
                            Ok(serde_json::to_value(crate::spectrum::read_effects(
                                &mut device,
                                profile,
                            )?)?)
                        }
                    }
                    SpectrumAction::Effects { path, profile } => {
                        let mut device =
                            crate::devices::SpectrumDevice::open(&owner, &permit, &path)?;
                        crate::spectrum::apply_profile_verified(
                            &mut device,
                            &profile,
                            reject_other_controllers,
                        )?;
                        Ok(serde_json::to_value(crate::spectrum::Profile::decode(
                            &profile.encode_24_zone()?,
                        )?)?)
                    }
                }
            }
            HardwareAction::EnableCpu { firmware_limit } => {
                ensure!(
                    self.cpu.is_none(),
                    "Stop CPU control before enabling it again"
                );
                let initial_deadline = Rc::new(std::cell::Cell::new(permit.deadline_tick_ms()));
                let mut cpu = CpuSession::open(
                    NativeFirmware {
                        facade: self.facade.clone(),
                        owner,
                        action: permit.for_cpu_session(),
                        initial_deadline: initial_deadline.clone(),
                    },
                    self.journal.clone(),
                    &permit,
                    current_creation_time()?,
                )?;
                // Retain the session even on enable failure, so Stop/drop can recover attempted writes.
                let result = cpu.enable(&permit, firmware_limit);
                initial_deadline.set(None);
                self.cpu = Some(cpu);
                Ok(serde_json::to_value(result?)?)
            }
            HardwareAction::System(request) => {
                if matches!(
                    request,
                    crate::system_controls::SystemAction::PowerPlan(_)
                        | crate::system_controls::SystemAction::PowerMode(_)
                        | crate::system_controls::SystemAction::PowerPolicy(_)
                ) {
                    ensure!(
                        self.cpu.is_none(),
                        "Stop CPU control before changing the Windows power policy"
                    );
                }
                if let crate::system_controls::SystemAction::PowerPolicy(policy) = request {
                    let expected = policy.expected_raw_mode;
                    let facade = &self.facade;
                    crate::system_controls::apply_power_policy_guarded(
                        &owner,
                        &permit,
                        policy,
                        &mut || {
                            let actual = integer(
                                &facade.method_read(
                                    "LENOVO_GAMEZONE_DATA",
                                    "GetSmartFanMode",
                                    BTreeMap::new(),
                                )?,
                                "Data",
                            )?;
                            ensure!(
                                actual == expected,
                                "Firmware mode changed during Windows power policy; no later setter issued"
                            );
                            Ok(())
                        },
                    )
                } else {
                    crate::system_controls::write(&owner, &permit, request)
                }
            }
            HardwareAction::Display(request) => {
                crate::display_controls::write(&owner, &permit, request)
            }
            HardwareAction::SetCpuPower {
                power,
                deadline_tick_ms,
            } => Ok(serde_json::to_value(
                self.cpu
                    .as_mut()
                    .context("CPU control not enabled")?
                    .set_power(&permit, power, deadline_tick_ms)?,
            )?),
            HardwareAction::SetRawMode(mode) => {
                ensure!(
                    self.cpu.is_none(),
                    "Stop CPU control before changing firmware mode"
                );
                ensure!(
                    read_power_modes(&self.facade)?
                        .modes
                        .iter()
                        .any(|supported| supported.raw == mode),
                    "Firmware/compatibility discovery did not confirm this raw mode"
                );
                ensure!(
                    ac_connected()? || ![3, 224, 255].contains(&mode),
                    "AC required for this firmware mode"
                );
                self.facade.method_write(
                    &owner,
                    &permit,
                    "LENOVO_GAMEZONE_DATA",
                    "SetSmartFanMode",
                    parameters([("Data", MethodInput::U32(mode))]),
                )?;
                let observed = integer(
                    &self.facade.method_read(
                        "LENOVO_GAMEZONE_DATA",
                        "GetSmartFanMode",
                        BTreeMap::new(),
                    )?,
                    "Data",
                )?;
                ensure!(observed == mode, "Firmware mode readback mismatch");
                Ok(Value::from(observed))
            }
            HardwareAction::SetMapped { feature, value } => {
                ensure!(
                    !matches!(feature, MappedFeature::IgpuMode) || self.cpu.is_none(),
                    "Stop and verify CPU restoration before changing IGPU routing"
                );
                feature.validate(value)?;
                require_mapped_capability(&self.facade, feature)?;
                self.facade.method_write(
                    &owner,
                    &permit,
                    "LENOVO_OTHER_METHOD",
                    "SetFeatureValue",
                    parameters([
                        ("IDs", MethodInput::U32(feature.id())),
                        ("value", MethodInput::U32(value)),
                    ]),
                )?;
                let observed = integer(
                    &self.facade.method_read(
                        "LENOVO_OTHER_METHOD",
                        "GetFeatureValue",
                        parameters([("IDs", MethodInput::U32(feature.id()))]),
                    )?,
                    "Value",
                )?;
                ensure!(value == observed, "Mapped feature readback mismatch");
                if matches!(feature, MappedFeature::IgpuMode) {
                    let changed = integer(
                        &self.facade.method_read(
                            "LENOVO_OTHER_METHOD",
                            "GetFeatureValue",
                            parameters([("IDs", MethodInput::U32(0x000f0000))]),
                        )?,
                        "Value",
                    )?;
                    ensure!(changed != 0, "iGPU mode firmware dGPU-change check failed");
                }
                Ok(Value::from(observed))
            }
            HardwareAction::SetFanTable(bytes) => {
                validate_fan_table(&bytes)?;
                ensure!(
                    god_mode(&self.facade)? == 255,
                    "Custom mode is required for an explicit manual fan table"
                );
                if let Some(cpu) = &mut self.cpu {
                    cpu.verify_ownership()?;
                }
                let expected: Vec<u32> = (0..10)
                    .map(|i| u32::from(u16::from_le_bytes([bytes[6 + i * 2], bytes[7 + i * 2]])))
                    .collect();
                self.facade.method_write_guarded(
                    &owner,
                    &permit,
                    "LENOVO_FAN_METHOD",
                    "Fan_Set_Table",
                    parameters([("FanTable", MethodInput::Bytes(bytes))]),
                    || {
                        ensure!(
                            god_mode(&self.facade)? == 255,
                            "Mode changed before manual fan-table write"
                        );
                        Ok(())
                    },
                )?;
                for _ in 0..2 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    ensure!(
                        read_fan_table(&self.facade)? == expected,
                        "Fan table readback mismatch; no write retry was issued"
                    );
                }
                if let Some(cpu) = &mut self.cpu {
                    cpu.verify_ownership()?;
                }
                Ok(serde_json::json!({"levels":expected,"verified_reads":2}))
            }
            HardwareAction::SetLighting {
                lighting_id,
                state_type,
                brightness,
            } => {
                ensure!(
                    [0, 3, 4, 5].contains(&lighting_id) && state_type <= 2 && brightness <= 255,
                    "Lighting value outside supported bounds"
                );
                require_lighting(&self.facade, lighting_id)?;
                ensure!(
                    match lighting_id {
                        0 => state_type == 0 && (1..=3).contains(&brightness),
                        3 | 5 => state_type <= 1 && brightness == 0,
                        4 => brightness <= 4,
                        _ => false,
                    },
                    "Lighting state/level is outside the original interface enum"
                );
                self.facade.method_write(
                    &owner,
                    &permit,
                    "LENOVO_LIGHTING_METHOD",
                    "Set_Lighting_Current_Status",
                    parameters([
                        ("Lighting_ID", MethodInput::U32(lighting_id)),
                        ("Current_State_Type", MethodInput::U32(state_type)),
                        ("Current_Brightness_Level", MethodInput::U32(brightness)),
                    ]),
                )?;
                let observed = self.facade.method_read(
                    "LENOVO_LIGHTING_METHOD",
                    "Get_Lighting_Current_Status",
                    parameters([("Lighting_ID", MethodInput::U32(lighting_id))]),
                )?;
                ensure!(
                    integer(&observed, "Current_State_Type")? == state_type
                        && integer(&observed, "Current_Brightness_Level")? == brightness,
                    "Lighting readback mismatch"
                );
                Ok(serde_json::to_value(observed)?)
            }
            HardwareAction::SetSpeakerMute(v) => {
                crate::devices::audio_write(&owner, &permit, false, Some(v), None)
            }
            HardwareAction::SetWifiEnabled(v) => crate::wireless::write_wifi(&owner, &permit, v),
            HardwareAction::SetAirplaneMode(v) => {
                crate::wireless::write_airplane(&owner, &permit, v)
            }
            HardwareAction::SetMicrophoneMute(v) => {
                crate::devices::audio_write(&owner, &permit, true, Some(v), None)
            }
            HardwareAction::SetSpeakerVolume(v) => {
                ensure!(v <= 100, "Volume must be 0..100");
                crate::devices::audio_write(&owner, &permit, false, None, Some(v))
            }
            HardwareAction::SetDisplayMode {
                device,
                width,
                height,
                hertz,
            } => Ok(serde_json::to_value(crate::devices::display_write(
                &owner, &permit, &device, width, height, hertz,
            )?)?),
            HardwareAction::SendKey {
                virtual_key,
                pressed,
            } => {
                crate::devices::send_key(&owner, &permit, virtual_key, pressed)?;
                Ok(Value::Null)
            }
            HardwareAction::HidFeatureWrite { path, bytes } => {
                crate::devices::hid_write(&owner, &permit, &path, &bytes)?;
                Ok(Value::Null)
            }
            HardwareAction::Energy(action) => Ok(serde_json::to_value(crate::energy::write(
                &owner, &permit, &action,
            )?)?),
            HardwareAction::ChargeThreshold(target) => Ok(serde_json::to_value(
                crate::charge_threshold::write(&owner, &permit, target)?,
            )?),
            _ => unreachable!(),
        }
    }
}
impl WorkerState for NativeState {
    fn shutdown(&mut self) -> Result<()> {
        self.memory_sampler.stop()?;
        self.memory_sampler = Default::default();
        if let Some(keys) = self.driver_keys.as_mut() {
            keys.stop()?;
        }
        self.driver_keys.take();
        if let Some(boot) = self.boot.as_mut() {
            boot.shutdown()?;
        }
        self.boot.take();
        if self.owner.is_some() {
            let store = crate::boot_logo::BackupStore::new(self.boot_root()?);
            ensure!(
                store.pending()?.is_none() && store.lease()?.is_none(),
                "Persisted boot recovery is unverified; hardware owner remains held"
            );
        }
        self.awake.take();
        crate::cpu::stop_session(&mut self.cpu)?;
        // stop_session dropped the firmware's Rc first; release the final kernel
        // owner on this same hardware thread before acknowledging the barrier.
        self.owner.take();
        Ok(())
    }
}

pub struct PlatformWorker {
    worker: BoundedWorker<NativeState>,
}
impl PlatformWorker {
    /// Creates only a read connection. Ownership and writes begin only in dispatch.
    pub fn connect(journal_path: impl Into<PathBuf>) -> Result<Self> {
        let journal = JournalStore::new(journal_path);
        Ok(Self {
            worker: BoundedWorker::spawn(move || {
                Ok(NativeState {
                    facade: Rc::new(WmiFacade::connect("ROOT\\WMI")?),
                    owner: None,
                    cpu: None,
                    journal,
                    sampler: crate::telemetry::SystemSampler::default(),
                    intel_sampler: crate::intel_sensors::IntelSampler::default(),
                    gpu_sampler: crate::nvapi_native::GpuSampler::default(),
                    intel_gpu_sampler: crate::intel_gpu::IntelGpuSampler::default(),
                    awake: None,
                    boot: None,
                    driver_keys: None,
                    memory_sampler: Default::default(),
                })
            })?,
        })
    }
    pub fn dispatch(&self, action: HardwareAction, permit: ExplicitAction) -> Result<Ticket> {
        if matches!(action, HardwareAction::ShutdownWorker) {
            return self.shutdown_ticket();
        }
        self.worker.submit(move |state| state.run(action, permit))
    }
    pub fn shutdown_ticket(&self) -> Result<Ticket> {
        self.worker.shutdown_ticket()
    }
    pub fn status(&self) -> WorkerStatus {
        self.worker.status()
    }
}
