//! Read-only OS telemetry. Constructing the sampler performs no native call.
//! CPU clock values are Windows power-manager reports, not effective core clocks.
use crate::domain::monotonic_ms;
use serde::{Deserialize, Serialize};
use std::mem::{size_of, zeroed};
use windows_sys::Win32::{
    Foundation::FILETIME,
    System::{
        Power::{CallNtPowerInformation, PROCESSOR_POWER_INFORMATION, ProcessorInformation},
        SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
        Threading::{ALL_PROCESSOR_GROUPS, GetActiveProcessorCount, GetSystemTimes},
    },
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemSample {
    pub requested_at_tick_ms: u64,
    pub completed_at_tick_ms: u64,
    pub logical_processors: u32,
    pub cpu_utilization_percent: Option<f64>,
    pub utilization_interval_start_tick_ms: Option<u64>,
    pub os_reported_frequency_mhz: Option<f64>,
    pub per_cpu_os_reported_mhz: Vec<u32>,
    pub memory_total_bytes: Option<u64>,
    pub memory_available_bytes: Option<u64>,
    pub memory_used_bytes: Option<u64>,
    pub cpu_power_watts: Option<f64>,
    pub cpu_voltage_volts: Option<f64>,
    pub source: String,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct CpuTimes {
    idle: u64,
    kernel: u64,
    user: u64,
    tick: u64,
}
impl CpuTimes {
    fn utilization_since(self, prior: Self) -> Option<f64> {
        if self.tick <= prior.tick {
            return None;
        }
        let idle = self.idle.checked_sub(prior.idle)?;
        // GetSystemTimes kernel time INCLUDES idle; do not add idle a second time.
        let total = self
            .kernel
            .checked_sub(prior.kernel)?
            .checked_add(self.user.checked_sub(prior.user)?)?;
        let busy = total.checked_sub(idle)?;
        (total > 0).then(|| busy as f64 * 100.0 / total as f64)
    }
}
fn filetime(value: FILETIME) -> u64 {
    u64::from(value.dwLowDateTime) | (u64::from(value.dwHighDateTime) << 32)
}

#[derive(Default)]
pub struct SystemSampler {
    previous: Option<CpuTimes>,
}
impl SystemSampler {
    /// Run on a background worker. Independent getter failures leave other metrics intact.
    pub fn sample(&mut self) -> SystemSample {
        let requested = monotonic_ms();
        let count = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
        let mut result = SystemSample {
            requested_at_tick_ms: requested,
            completed_at_tick_ms: requested,
            logical_processors: count,
            cpu_utilization_percent: None,
            utilization_interval_start_tick_ms: None,
            os_reported_frequency_mhz: None,
            per_cpu_os_reported_mhz: vec![],
            memory_total_bytes: None,
            memory_available_bytes: None,
            memory_used_bytes: None,
            cpu_power_watts: None,
            cpu_voltage_volts: None,
            source: "GetSystemTimes / GlobalMemoryStatusEx / CallNtPowerInformation(ProcessorInformation); clock is OS-reported, not effective; CPU power/voltage unavailable".into(),
            errors: vec![],
        };
        // GetSystemTimes is group-relative above 64 processors. Do not label a group
        // as total machine utilization. The approved 83NN has 32 logical processors.
        if (1..=64).contains(&count) {
            let (mut idle, mut kernel, mut user) = unsafe { (zeroed(), zeroed(), zeroed()) };
            if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } != 0 {
                let current = CpuTimes {
                    idle: filetime(idle),
                    kernel: filetime(kernel),
                    user: filetime(user),
                    tick: requested,
                };
                if let Some(previous) = self.previous {
                    result.cpu_utilization_percent = current.utilization_since(previous);
                    if result.cpu_utilization_percent.is_some() {
                        result.utilization_interval_start_tick_ms = Some(previous.tick);
                    }
                }
                self.previous = Some(current);
            } else {
                self.previous = None;
                result.errors.push(format!(
                    "GetSystemTimes: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut processors: Vec<PROCESSOR_POWER_INFORMATION> =
                (0..count).map(|_| unsafe { zeroed() }).collect();
            let bytes = (processors.len() * size_of::<PROCESSOR_POWER_INFORMATION>()) as u32;
            let status = unsafe {
                CallNtPowerInformation(
                    ProcessorInformation,
                    std::ptr::null(),
                    0,
                    processors.as_mut_ptr().cast(),
                    bytes,
                )
            };
            if status >= 0
                && processors.iter().enumerate().all(|(index, p)| {
                    p.Number == index as u32 && p.CurrentMhz > 0 && p.CurrentMhz <= 20000
                })
            {
                result.per_cpu_os_reported_mhz = processors.iter().map(|p| p.CurrentMhz).collect();
                result.os_reported_frequency_mhz = Some(
                    result
                        .per_cpu_os_reported_mhz
                        .iter()
                        .map(|v| u64::from(*v))
                        .sum::<u64>() as f64
                        / f64::from(count),
                );
            } else {
                result.errors.push(format!(
                    "ProcessorInformation unavailable or invalid (NTSTATUS {status:#x})"
                ));
            }
        } else {
            self.previous = None;
            result.errors.push(
                "OS CPU sampler supports one processor group with 1..64 logical processors".into(),
            );
        }
        let mut memory: MEMORYSTATUSEX = unsafe { zeroed() };
        memory.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut memory) } != 0
            && memory.ullTotalPhys > 0
            && memory.ullAvailPhys <= memory.ullTotalPhys
        {
            result.memory_total_bytes = Some(memory.ullTotalPhys);
            result.memory_available_bytes = Some(memory.ullAvailPhys);
            result.memory_used_bytes = Some(memory.ullTotalPhys - memory.ullAvailPhys);
        } else {
            result.errors.push(format!(
                "GlobalMemoryStatusEx unavailable/invalid: {}",
                std::io::Error::last_os_error()
            ));
        }
        result.completed_at_tick_ms = monotonic_ms();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utilization_counts_idle_once_and_rejects_reset_or_empty_intervals() {
        let before = CpuTimes {
            idle: 10,
            kernel: 20,
            user: 30,
            tick: 1,
        };
        let after = CpuTimes {
            idle: 40,
            kernel: 80,
            user: 70,
            tick: 2,
        };
        assert_eq!(after.utilization_since(before), Some(70.0));
        assert_eq!(before.utilization_since(after), None);
        assert_eq!(before.utilization_since(before), None);
        assert_eq!(
            CpuTimes { idle: 111, ..after }.utilization_since(before),
            None
        );
        assert_eq!(
            CpuTimes { tick: 2, ..before }.utilization_since(before),
            None
        );
    }
}
