//! Pure integration of the verified policy and native session. No timer or I/O is created here.
use anyhow::{Context, Result, ensure};
use jiks_core::{
    settings::ApplicationThermalSettings,
    thermal::{
        ApplyStatus, ControlStatus, Epoch, PowerCapability, PowerCommand, PowerEnvelope,
        TemperatureReading, ThermalInput, ThermalSupervisor, ThermalTick,
    },
};
use jiks_platform::{
    CpuSnapshot, Envelope, HardwareAction, TemperatureSample,
    domain::{RangeCapability, custom_cpu_capabilities},
};
use serde_json::Value;

pub struct Prepared {
    pub settings: ApplicationThermalSettings,
    pub capability: PowerCapability,
    pub baseline: CpuSnapshot,
}
impl Prepared {
    pub fn from_discovery(settings: ApplicationThermalSettings, discovery: &Value) -> Result<Self> {
        settings.validate_control()?;
        let rows: Vec<RangeCapability> =
            serde_json::from_value(discovery["ranges"].clone()).context("缺少完整CPU固件范围")?;
        let caps = custom_cpu_capabilities(&rows, &[])?;
        let get = |key: &str| {
            discovery["limits"][key]
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .context("CPU限制读数无效")
        };
        let baseline = CpuSnapshot {
            raw_mode: discovery["raw_mode"]
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .context("缺少性能模式")?,
            power: Envelope {
                long_watts: get("long_watts")?,
                short_watts: get("short_watts")?,
            },
            temperature_limit: get("temperature_limit")?,
        };
        caps.validate(baseline.power, baseline.temperature_limit)?;
        ensure!(
            caps.temperature.contains(settings.firmware_limit_c as u32),
            "温限不符合本机固件步长"
        );
        let capability = PowerCapability {
            min_long: f64::from(caps.long.min),
            max_long: f64::from(caps.long.max),
            min_short: f64::from(caps.short.min),
            max_short: f64::from(caps.short.max),
            step: f64::from(caps.long.step),
        };
        ensure!(
            capability.is_aligned(envelope(baseline.power)),
            "功耗基线不在已验证的固件网格上"
        );
        Ok(Self {
            settings,
            capability,
            baseline,
        })
    }
    pub fn activate(self, verified: CpuSnapshot, epoch: Epoch, now: u64) -> Result<Controller> {
        ensure!(
            verified.raw_mode == 255 && verified.power == self.baseline.power,
            "启用期间模式或功耗基线发生变化，不能创建温控预算"
        );
        ensure!(
            verified.temperature_limit <= self.settings.firmware_limit_c as u32
                && verified.temperature_limit <= 88,
            "固件温限未得到核对"
        );
        let supervisor = ThermalSupervisor::new(
            self.capability,
            envelope(verified.power),
            epoch,
            self.settings.options.clone(),
            now,
        )?;
        Ok(Controller {
            supervisor,
            epoch,
            settings: self.settings,
            pending: None,
            pending_at: 0,
            pending_unknown: false,
        })
    }
    pub fn observe(self, epoch: Epoch, now: u64) -> Result<Controller> {
        let supervisor = ThermalSupervisor::new(
            self.capability,
            envelope(self.baseline.power),
            epoch,
            self.settings.options.clone(),
            now,
        )?;
        Ok(Controller {
            supervisor,
            epoch,
            settings: self.settings,
            pending: None,
            pending_at: 0,
            pending_unknown: false,
        })
    }
}
pub fn envelope(value: Envelope) -> PowerEnvelope {
    PowerEnvelope::new(f64::from(value.long_watts), f64::from(value.short_watts))
}

pub struct Controller {
    supervisor: ThermalSupervisor,
    pub epoch: Epoch,
    pub settings: ApplicationThermalSettings,
    pending: Option<PowerCommand>,
    pending_at: u64,
    pending_unknown: bool,
}
impl Controller {
    pub fn sample(
        &mut self,
        sample: Option<&TemperatureSample>,
        now: u64,
    ) -> Result<(ThermalTick, Option<HardwareAction>)> {
        self.sample_sources(None, sample, now)
    }
    /// Real Intel DTS package/core fields keep their own original request tick.
    /// Firmware fallback stays a separate source; it is never called core-max.
    pub fn sample_sources(
        &mut self,
        intel: Option<&jiks_platform::intel_sensors::IntelSample>,
        sample: Option<&TemperatureSample>,
        now: u64,
    ) -> Result<(ThermalTick, Option<HardwareAction>)> {
        // Timestamp from REQUEST, never from a delayed WMI response's completion.
        let firmware = sample.map(|v| TemperatureReading {
            celsius: f64::from(v.celsius),
            sampled_at_ms: v.requested_at_tick_ms,
        });
        let tick = self.supervisor.evaluate(
            ThermalInput {
                package: intel
                    .and_then(|s| {
                        s.package_temperature_celsius
                            .map(|celsius| TemperatureReading {
                                celsius,
                                sampled_at_ms: s.requested_at_tick_ms,
                            })
                    })
                    .unwrap_or(TemperatureReading::MISSING),
                core_max: intel
                    .and_then(|s| {
                        s.maximum_core_temperature_celsius
                            .map(|celsius| TemperatureReading {
                                celsius,
                                sampled_at_ms: s.requested_at_tick_ms,
                            })
                    })
                    .unwrap_or(TemperatureReading::MISSING),
                firmware,
            },
            now,
            self.epoch,
        )?;
        let action = tick.command.map(|command| {
            self.pending = Some(command);
            self.pending_at = now;
            self.pending_unknown = false;
            if tick.verify_only {
                HardwareAction::ReadCpu
            } else {
                HardwareAction::SetCpuPower {
                    power: Envelope {
                        long_watts: command.target.long_watts as u32,
                        short_watts: command.target.short_watts as u32,
                    },
                    deadline_tick_ms: command.expires_at_ms,
                }
            }
        });
        Ok((tick, action))
    }
    pub fn shadow_sample(
        &mut self,
        sample: Option<&TemperatureSample>,
        now: u64,
    ) -> Result<ThermalTick> {
        self.shadow_sources(None, sample, now)
    }
    pub fn shadow_sources(
        &mut self,
        intel: Option<&jiks_platform::intel_sensors::IntelSample>,
        sample: Option<&TemperatureSample>,
        now: u64,
    ) -> Result<ThermalTick> {
        let (tick, _) = self.sample_sources(intel, sample, now)?;
        // No physical job exists in observation mode. Resolve its pure proposal without a setter.
        if let Some(command) = self.pending.take() {
            self.supervisor.acknowledge(
                command,
                ApplyStatus::Verified,
                Some(command.target),
                self.epoch,
            )?;
        }
        Ok(tick)
    }
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn check_timeout(&mut self, now: u64) -> Result<bool> {
        if let Some(command) = self.pending
            && !self.pending_unknown
            && now.saturating_sub(self.pending_at) >= self.settings.options.command_timeout_ms
        {
            self.supervisor.acknowledge(
                command,
                ApplyStatus::UnknownCompletion,
                None,
                self.epoch,
            )?;
            self.pending_unknown = true;
            return Ok(true);
        }
        Ok(false)
    }
    pub fn complete(&mut self, result: Result<CpuSnapshot>) -> Result<ControlStatus> {
        let command = self.pending.take().context("温控没有对应的待确认操作")?;
        if self.pending_unknown {
            self.supervisor.physical_completion_observed(command)?;
            self.pending_unknown = false;
            return Ok(ControlStatus::ActuatorFault);
        }
        let readback = result
            .as_ref()
            .ok()
            .filter(|s| s.raw_mode == 255 && s.temperature_limit <= 88)
            .map(|s| envelope(s.power));
        Ok(self.supervisor.acknowledge(
            command,
            if result.is_ok() {
                ApplyStatus::Verified
            } else {
                ApplyStatus::Failed
            },
            readback,
            self.epoch,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiks_core::settings::SettingsDocument;
    use serde_json::json;
    fn prepared() -> Prepared {
        let settings =
            ApplicationThermalSettings::from_document(&SettingsDocument::parse("{}").unwrap())
                .unwrap();
        Prepared::from_discovery(settings,&json!({"raw_mode":255,"limits":{"long_watts":70,"short_watts":125,"temperature_limit":100},
            "ranges":[{"IDs":0x0102ff00u32,"MinValue":50,"MaxValue":135,"Step":1,"DefaultValue":70},
            {"IDs":0x0101ff00u32,"MinValue":60,"MaxValue":195,"Step":1,"DefaultValue":125},
            {"IDs":0x0104ff00u32,"MinValue":85,"MaxValue":100,"Step":1,"DefaultValue":100}]})).unwrap()
    }
    fn active() -> Controller {
        let p = prepared();
        let mut s = p.baseline.clone();
        s.temperature_limit = 88;
        p.activate(s, Epoch { policy: 1, mode: 1 }, 1000).unwrap()
    }
    #[test]
    fn native_core_and_package_temperatures_remain_distinct_from_firmware_fallback() {
        let mut controller = active();
        let intel:jiks_platform::intel_sensors::IntelSample=serde_json::from_value(json!({"requested_at_tick_ms":1001,"completed_at_tick_ms":1002,"cores":[],"package_temperature_celsius":89.,"maximum_core_temperature_celsius":95.,"average_core_temperature_celsius":75.,"conservative_temperature_celsius":95.,"package_power_watts":null,"core_power_watts":null,"dram_power_watts":null,"platform_power_watts":null,"power_interval_start_tick_ms":null,"core_vid_average_volts":null,"core_vid_maximum_volts":null,"source":"purefixture genuine DTS fields","errors":[]})).unwrap();
        let firmware = TemperatureSample {
            celsius: 55,
            requested_at_tick_ms: 1001,
            completed_at_tick_ms: 1002,
            source: "aggregate fixture".into(),
        };
        let (tick, _) = controller
            .sample_sources(Some(&intel), Some(&firmware), 1003)
            .unwrap();
        assert_eq!(
            tick.source,
            jiks_core::thermal::TemperatureSource::PackageAndCoreMaximum
        );
        assert_eq!(tick.observed_temperature_c, Some(95.));
        assert!(tick.target.unwrap().long_watts <= 70.);
    }
    #[test]
    fn enabling_does_not_create_a_larger_budget_or_accept_a_changed_mode() {
        let p = prepared();
        let mut s = p.baseline.clone();
        s.temperature_limit = 88;
        s.power.long_watts = 71;
        assert!(p.activate(s, Epoch { policy: 1, mode: 1 }, 1000).is_err());
        let mut c = active();
        let sample = TemperatureSample {
            celsius: 96,
            requested_at_tick_ms: 1001,
            completed_at_tick_ms: 1002,
            source: "aggregate".into(),
        };
        let (t, a) = c.sample(Some(&sample), 1003).unwrap();
        assert!(t.target.unwrap().long_watts <= 70.);
        assert!(a.is_some());
    }
    #[test]
    fn slow_sensor_completion_is_not_fresh_and_never_fakes_core_max() {
        let mut c = active();
        let sample = TemperatureSample {
            celsius: 40,
            requested_at_tick_ms: 1,
            completed_at_tick_ms: 9999,
            source: "aggregate".into(),
        };
        let (t, _) = c.sample(Some(&sample), 9999).unwrap();
        assert_eq!(t.observed_temperature_c, None);
        assert_eq!(
            t.source,
            jiks_core::thermal::TemperatureSource::LenovoFirmwareAggregate
        );
        assert_eq!(t.target.unwrap().long_watts, 50.);
    }
    #[test]
    fn late_completion_keeps_fault_and_cannot_raise_budget() {
        let mut c = active();
        let (_, a) = c.sample(None, 1001).unwrap();
        assert!(a.is_some());
        assert!(c.check_timeout(3000).unwrap());
        assert!(c.sample(None, 3001).is_err());
        let result = CpuSnapshot {
            raw_mode: 255,
            power: Envelope {
                long_watts: 50,
                short_watts: 60,
            },
            temperature_limit: 88,
        };
        assert_eq!(
            c.complete(Ok(result)).unwrap(),
            ControlStatus::ActuatorFault
        );
        let (t, _) = c.sample(None, 3002).unwrap();
        assert_eq!(t.target.unwrap().long_watts, 50.);
    }
    #[test]
    fn shadow_proposals_never_have_a_pending_physical_job() {
        let p = prepared();
        let mut c = p.observe(Epoch { policy: 1, mode: 1 }, 0).unwrap();
        assert!(c.shadow_sample(None, 10).unwrap().target.is_some());
        assert!(!c.pending());
        assert!(c.shadow_sample(None, 20).is_ok());
    }
}
