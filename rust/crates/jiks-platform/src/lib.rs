//! Native Windows hardware boundaries. Nothing probes or writes merely on import.
pub mod ai;
pub mod audio_control;
pub mod autorun;
mod aux_io;
pub mod battery;
pub mod battery_history;
pub mod boot_logo;
pub mod brightness_sync;
mod cpu;
pub mod device_information;
pub mod devices;
pub mod display_controls;
pub mod domain;
pub mod driver_keys;
pub mod dynamic_refresh;
pub mod energy;
pub mod firmware_events;
pub mod fps;
pub mod god_mode;
pub mod gpu;
pub mod gpu_awake;
pub mod gpu_management;
pub mod gpu_pstate;
pub mod gpu_sensors;
pub mod gpu_sleep;
pub mod gpu_status;
pub mod hybrid;
mod igcl;
pub mod intel_gpu;
pub mod intel_sensors;
pub mod memory_sensors;
mod native;
pub mod nvapi_native;
pub mod nvml;
pub mod nvml_processes;
mod nvml_sensors;
mod ownership;
pub mod power_modes;
pub mod power_status;
pub mod software;
pub mod spectrum;
pub mod storage_sensors;
pub mod system_controls;
pub mod telemetry;
pub mod wireless;
pub mod wmi_native;
pub mod worker;
pub use domain::{
    CpuCapabilities, CpuSnapshot, Envelope, ExplicitAction, TemperatureSample, monotonic_ms,
};
pub use native::{
    HardwareAction, PlatformWorker, SpectrumAction, ac_connected, ensure_no_other_controllers,
    read_cpu_discovery, read_machine_identity,
};
pub use ownership::JournalStore;
pub use worker::{Completion, Ticket, WorkerStatus};
