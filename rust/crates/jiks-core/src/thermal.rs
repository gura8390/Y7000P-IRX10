//! Deterministic CPU-only thermal policy. No timers, threads, settings IO or firmware calls.
use crate::{CoreError, CoreResult};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PowerEnvelope {
    pub long_watts: f64,
    pub short_watts: f64,
}
impl PowerEnvelope {
    pub const fn new(long_watts: f64, short_watts: f64) -> Self {
        Self {
            long_watts,
            short_watts,
        }
    }
    pub fn min(self, other: Self) -> Self {
        Self::new(
            self.long_watts.min(other.long_watts),
            self.short_watts.min(other.short_watts),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PowerCapability {
    pub min_long: f64,
    pub max_long: f64,
    pub min_short: f64,
    pub max_short: f64,
    pub step: f64,
}
fn decimal(value: f64) -> Option<Decimal> {
    if !value.is_finite() {
        return None;
    }
    Decimal::from_str(&value.to_string()).ok()
}
fn number(value: Decimal) -> Option<f64> {
    value
        .to_string()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}
impl PowerCapability {
    pub const Y7000P: Self = Self {
        min_long: 50.,
        max_long: 135.,
        min_short: 60.,
        max_short: 195.,
        step: 1.,
    };
    pub fn contains(self, value: PowerEnvelope) -> bool {
        [
            self.min_long,
            self.max_long,
            self.min_short,
            self.max_short,
            self.step,
            value.long_watts,
            value.short_watts,
        ]
        .iter()
        .all(|&v| decimal(v).is_some())
            && self.step > 0.
            && self.min_long > 0.
            && self.min_short > 0.
            && self.max_long >= self.min_long
            && self.max_short >= self.min_short
            && value.long_watts >= self.min_long
            && value.long_watts <= self.max_long
            && value.short_watts >= self.min_short
            && value.short_watts <= self.max_short
            && value.long_watts <= value.short_watts
    }
    pub fn round_down(self, value: PowerEnvelope) -> CoreResult<PowerEnvelope> {
        if !self.contains(value) {
            return Err(CoreError::invalid("unsupported power envelope"));
        }
        let step = decimal(self.step).unwrap();
        let lo = decimal(self.min_long).unwrap();
        let so = decimal(self.min_short).unwrap();
        let l = decimal(value.long_watts).unwrap();
        let s = decimal(value.short_watts).unwrap();
        let floor_grid = |origin: Decimal, ceiling: Decimal| -> Option<Decimal> {
            let n = ceiling.checked_sub(origin)?.checked_div(step)?.floor();
            origin.checked_add(n.checked_mul(step)?)
        };
        let mut short =
            floor_grid(so, s).ok_or_else(|| CoreError::invalid("power grid overflow"))?;
        if number(short).is_none_or(|v| v > value.short_watts) {
            short = short
                .checked_sub(step)
                .ok_or_else(|| CoreError::invalid("power grid overflow"))?;
        }
        let ceiling = l.min(short);
        if ceiling < lo {
            return Err(CoreError::invalid("no feasible long/short grid pair"));
        }
        let mut long =
            floor_grid(lo, ceiling).ok_or_else(|| CoreError::invalid("power grid overflow"))?;
        if number(long).is_none_or(|v| v > value.long_watts) {
            long = long
                .checked_sub(step)
                .ok_or_else(|| CoreError::invalid("power grid overflow"))?;
        }
        let result = PowerEnvelope::new(
            number(long).ok_or_else(|| CoreError::invalid("power conversion"))?,
            number(short).ok_or_else(|| CoreError::invalid("power conversion"))?,
        );
        if !self.contains(result)
            || result.long_watts > value.long_watts
            || result.short_watts > value.short_watts
        {
            return Err(CoreError::invalid(
                "no legal power envelope under the ceiling",
            ));
        }
        Ok(result)
    }
    pub fn is_aligned(self, value: PowerEnvelope) -> bool {
        self.round_down(value).is_ok_and(|v| v == value)
    }
    pub fn minimum(self) -> CoreResult<PowerEnvelope> {
        if !self.contains(PowerEnvelope::new(self.min_long, self.max_short)) {
            return Err(CoreError::invalid("capability has no legal envelope"));
        }
        let step = decimal(self.step).unwrap();
        let origin = decimal(self.min_short).unwrap();
        let ceiling = decimal(self.min_long.max(self.min_short)).unwrap();
        let n = ceiling
            .checked_sub(origin)
            .and_then(|v| v.checked_div(step))
            .ok_or_else(|| CoreError::invalid("grid overflow"))?
            .ceil();
        let short = origin
            .checked_add(
                n.checked_mul(step)
                    .ok_or_else(|| CoreError::invalid("grid overflow"))?,
            )
            .ok_or_else(|| CoreError::invalid("grid overflow"))?;
        let result = PowerEnvelope::new(
            self.min_long,
            number(short).ok_or_else(|| CoreError::invalid("grid conversion"))?,
        );
        if self.is_aligned(result) {
            Ok(result)
        } else {
            Err(CoreError::invalid("capability minimum is not aligned"))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Epoch {
    pub policy: u64,
    pub mode: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PowerCommand {
    pub target: PowerEnvelope,
    pub epoch: Epoch,
    pub expires_at_ms: u64,
}
pub type PowerConstraint = PowerCommand;
pub fn resolve_power(
    user: PowerEnvelope,
    constraints: &[PowerConstraint],
    capability: PowerCapability,
    now: u64,
    epoch: Epoch,
    expires_at_ms: u64,
) -> CoreResult<PowerCommand> {
    if !capability.contains(user) || expires_at_ms <= now {
        return Err(CoreError::invalid("baseline or deadline"));
    }
    let mut target = user;
    let mut expiry = expires_at_ms;
    for constraint in constraints {
        if constraint.epoch != epoch || constraint.expires_at_ms <= now {
            return Err(CoreError::Superseded);
        }
        if !capability.contains(constraint.target) {
            return Err(CoreError::invalid("constraint outside capability"));
        }
        target = target.min(constraint.target);
        expiry = expiry.min(constraint.expires_at_ms);
    }
    Ok(PowerCommand {
        target: capability.round_down(target)?,
        epoch,
        expires_at_ms: expiry,
    })
}
impl PowerCommand {
    pub fn is_current(self, capability: PowerCapability, now: u64, epoch: Epoch) -> bool {
        self.expires_at_ms > now && self.epoch == epoch && capability.is_aligned(self.target)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TemperatureReading {
    pub celsius: f64,
    pub sampled_at_ms: u64,
}
impl TemperatureReading {
    pub const MISSING: Self = Self {
        celsius: f64::NAN,
        sampled_at_ms: 0,
    };
    pub fn is_fresh(self, now: u64, max_age: u64) -> bool {
        self.celsius.is_finite()
            && self.celsius > 0.
            && self.celsius < 150.
            && self.sampled_at_ms <= now
            && now - self.sampled_at_ms <= max_age
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThermalPhase {
    Normal,
    Warm,
    Hot,
    Recovering,
    SensorFault,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThermalOptions {
    pub target_c: f64,
    pub warm_c: f64,
    pub recover_c: f64,
    pub freshness_ms: u64,
    pub recovery_ms: u64,
    pub adjustment_ms: u64,
    pub minimum_fraction: f64,
    pub reduction: f64,
    pub warm_reduction: f64,
    pub recovery_step: f64,
}
impl Default for ThermalOptions {
    fn default() -> Self {
        Self {
            target_c: 90.,
            warm_c: 87.,
            recover_c: 86.,
            freshness_ms: 2000,
            recovery_ms: 20000,
            adjustment_ms: 2000,
            minimum_fraction: 0.35,
            reduction: 0.10,
            warm_reduction: 0.10,
            recovery_step: 0.02,
        }
    }
}
impl ThermalOptions {
    pub fn application_default() -> Self {
        Self {
            target_c: 88.,
            warm_c: 85.,
            recover_c: 82.,
            ..Self::default()
        }
    }
    pub fn validate(&self) -> CoreResult<()> {
        let fractions = [
            self.minimum_fraction,
            self.reduction,
            self.warm_reduction,
            self.recovery_step,
        ];
        if ![self.target_c, self.warm_c, self.recover_c]
            .iter()
            .all(|v| v.is_finite())
            || self.recover_c <= 0.
            || self.recover_c >= self.warm_c
            || self.warm_c >= self.target_c
            || self.target_c >= 150.
            || self.freshness_ms == 0
            || self.recovery_ms == 0
            || self.adjustment_ms == 0
            || !fractions
                .iter()
                .all(|v| v.is_finite() && *v > 0. && *v <= 1.)
        {
            return Err(CoreError::invalid("thermal policy options"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThermalDecision {
    pub phase: ThermalPhase,
    pub power_fraction: f64,
    pub request_cooling: bool,
    pub cool_since_ms: Option<u64>,
    pub last_adjustment_ms: u64,
    pub evaluated_at_ms: u64,
}
#[derive(Debug, Clone)]
pub struct ThermalPolicy {
    options: ThermalOptions,
}
impl ThermalPolicy {
    pub fn new(options: ThermalOptions) -> CoreResult<Self> {
        options.validate()?;
        Ok(Self { options })
    }
    pub fn evaluate(
        &self,
        package: TemperatureReading,
        core: TemperatureReading,
        now: u64,
        previous: Option<&ThermalDecision>,
    ) -> ThermalDecision {
        self.evaluate_temperature(
            package.celsius.max(core.celsius),
            package.is_fresh(now, self.options.freshness_ms)
                && core.is_fresh(now, self.options.freshness_ms),
            now,
            previous,
        )
    }
    pub fn evaluate_firmware(
        &self,
        reading: TemperatureReading,
        now: u64,
        previous: Option<&ThermalDecision>,
    ) -> ThermalDecision {
        self.evaluate_temperature(
            reading.celsius,
            reading.is_fresh(now, self.options.freshness_ms),
            now,
            previous,
        )
    }
    fn evaluate_temperature(
        &self,
        temperature: f64,
        healthy: bool,
        now: u64,
        previous: Option<&ThermalDecision>,
    ) -> ThermalDecision {
        let valid = previous.is_none_or(|p| {
            p.power_fraction.is_finite()
                && p.power_fraction > 0.
                && p.power_fraction <= 1.
                && p.last_adjustment_ms <= p.evaluated_at_ms
                && p.cool_since_ms.is_none_or(|t| t <= p.evaluated_at_ms)
        });
        let mut fraction = if valid {
            previous
                .map_or(1., |p| p.power_fraction)
                .clamp(self.options.minimum_fraction, 1.)
        } else {
            self.options.minimum_fraction
        };
        let mut last = if valid {
            previous.map_or(now, |p| p.last_adjustment_ms).min(now)
        } else {
            now
        };
        let healthy = healthy && valid && previous.is_none_or(|p| now >= p.evaluated_at_ms);
        let mut result = ThermalDecision {
            phase: ThermalPhase::SensorFault,
            power_fraction: fraction,
            request_cooling: true,
            cool_since_ms: None,
            last_adjustment_ms: last,
            evaluated_at_ms: now,
        };
        if !healthy {
            return result;
        }
        if temperature >= self.options.target_c {
            if previous.is_none()
                || fraction == 1.
                || now.saturating_sub(last) >= self.options.adjustment_ms
            {
                fraction = (fraction - self.options.reduction).max(self.options.minimum_fraction);
                last = now;
            }
            result.phase = ThermalPhase::Hot;
        } else if temperature >= self.options.warm_c {
            if fraction == 1. {
                fraction = (1. - self.options.warm_reduction).max(self.options.minimum_fraction);
                last = now;
            }
            result.phase = ThermalPhase::Warm;
        } else if temperature > self.options.recover_c {
            result.phase = ThermalPhase::Recovering;
            result.request_cooling = fraction < 1.;
        } else {
            let cool_since = previous.and_then(|p| p.cool_since_ms).unwrap_or(now);
            if now.saturating_sub(cool_since) >= self.options.recovery_ms
                && now.saturating_sub(last) >= self.options.adjustment_ms
            {
                fraction = (fraction + self.options.recovery_step).min(1.);
                last = now;
            }
            result.phase = if fraction >= 1. {
                ThermalPhase::Normal
            } else {
                ThermalPhase::Recovering
            };
            result.request_cooling = fraction < 1.;
            result.cool_since_ms = Some(cool_since);
        }
        result.power_fraction = fraction;
        result.last_adjustment_ms = last;
        result
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThermalControlOptions {
    pub policy: ThermalOptions,
    pub command_timeout_ms: u64,
    pub command_lifetime_ms: u64,
    pub readback_interval_ms: u64,
    pub prediction_ms: u64,
    pub maximum_prediction_c: f64,
}
impl Default for ThermalControlOptions {
    fn default() -> Self {
        Self {
            policy: ThermalOptions::application_default(),
            command_timeout_ms: 1500,
            command_lifetime_ms: 2000,
            readback_interval_ms: 1000,
            prediction_ms: 1000,
            maximum_prediction_c: 5.,
        }
    }
}
impl ThermalControlOptions {
    pub fn validate(&self) -> CoreResult<()> {
        self.policy.validate()?;
        if self.command_timeout_ms == 0
            || self.command_timeout_ms > i32::MAX as u64
            || self.command_lifetime_ms == 0
            || self.command_lifetime_ms > i32::MAX as u64
            || self.readback_interval_ms == 0
            || self.prediction_ms > self.policy.freshness_ms
            || !self.maximum_prediction_c.is_finite()
            || !(0.0..=20.0).contains(&self.maximum_prediction_c)
        {
            return Err(CoreError::invalid("thermal supervisor options"));
        }
        Ok(())
    }
    pub fn validate_machine(&self, sampling_ms: u64, firmware_limit_c: i32) -> CoreResult<()> {
        self.validate()?;
        if !(100..=1000).contains(&sampling_ms)
            || firmware_limit_c > 88
            || firmware_limit_c < 1
            || self.policy.target_c > f64::from(firmware_limit_c)
            || self.policy.warm_c > 85.
        {
            return Err(CoreError::invalid("Y7000P thermal settings"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TemperatureSource {
    Unavailable,
    PackageAndCoreMaximum,
    LenovoFirmwareAggregate,
    FirmwareWithPartialHardware,
}
#[derive(Debug, Clone, Copy)]
pub struct ThermalInput {
    pub package: TemperatureReading,
    pub core_max: TemperatureReading,
    pub firmware: Option<TemperatureReading>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApplyStatus {
    Rejected,
    Verified,
    Failed,
    UnknownCompletion,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlStatus {
    Holding,
    Verified,
    SensorFault,
    ActuatorFault,
    UnknownCompletion,
    Superseded,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThermalTick {
    pub status: ControlStatus,
    pub decision: Option<ThermalDecision>,
    pub target: Option<PowerEnvelope>,
    pub command: Option<PowerCommand>,
    pub verify_only: bool,
    pub observed_temperature_c: Option<f64>,
    pub predicted_rise_c: f64,
    pub source: TemperatureSource,
}

/// One pure evaluation at a time. The platform executes a returned command and must acknowledge it.
/// A pending command prevents another write until completion; UnknownCompletion retains that ownership.
#[derive(Debug, Clone)]
pub struct ThermalSupervisor {
    capability: PowerCapability,
    baseline: PowerEnvelope,
    minimum: PowerEnvelope,
    epoch: Epoch,
    options: ThermalControlOptions,
    policy: ThermalPolicy,
    decision: Option<ThermalDecision>,
    last_verified: PowerEnvelope,
    recovery_budget: PowerEnvelope,
    fault_ceiling: Option<PowerEnvelope>,
    actuator_fault: bool,
    last_recovery: u64,
    last_verification: u64,
    previous_temperature: Option<(f64, u64)>,
    predicted_rise: f64,
    previous_source: TemperatureSource,
    previous_partial_mask: u8,
    pending: Option<PowerCommand>,
    pending_unknown: bool,
}
impl ThermalSupervisor {
    pub fn new(
        capability: PowerCapability,
        baseline: PowerEnvelope,
        epoch: Epoch,
        options: ThermalControlOptions,
        now: u64,
    ) -> CoreResult<Self> {
        options.validate()?;
        if !capability.is_aligned(baseline) {
            return Err(CoreError::invalid("verified aligned baseline required"));
        }
        Ok(Self {
            capability,
            baseline,
            minimum: capability.minimum()?,
            epoch,
            policy: ThermalPolicy::new(options.policy.clone())?,
            options,
            decision: None,
            last_verified: baseline,
            recovery_budget: baseline,
            fault_ceiling: None,
            actuator_fault: false,
            last_recovery: now,
            last_verification: now,
            previous_temperature: None,
            predicted_rise: 0.,
            previous_source: TemperatureSource::Unavailable,
            previous_partial_mask: 0,
            pending: None,
            pending_unknown: false,
        })
    }
    pub fn has_actuator_fault(&self) -> bool {
        self.actuator_fault
    }
    pub fn has_pending_physical_io(&self) -> bool {
        self.pending.is_some()
    }
    pub fn baseline(&self) -> PowerEnvelope {
        self.baseline
    }
    pub fn evaluate(
        &mut self,
        input: ThermalInput,
        now: u64,
        epoch: Epoch,
    ) -> CoreResult<ThermalTick> {
        if epoch != self.epoch {
            return Ok(ThermalTick {
                status: ControlStatus::Superseded,
                decision: self.decision.clone(),
                target: None,
                command: None,
                verify_only: false,
                observed_temperature_c: None,
                predicted_rise_c: 0.,
                source: TemperatureSource::Unavailable,
            });
        }
        if self.pending.is_some() {
            return Err(CoreError::Capacity(
                "physical command/readback still pending".into(),
            ));
        }
        let package_ok = input
            .package
            .is_fresh(now, self.options.policy.freshness_ms);
        let core_ok = input
            .core_max
            .is_fresh(now, self.options.policy.freshness_ms);
        let mut source = TemperatureSource::PackageAndCoreMaximum;
        let mut partial_mask = 0;
        let mut reading = TemperatureReading {
            celsius: input.package.celsius.max(input.core_max.celsius),
            sampled_at_ms: input
                .package
                .sampled_at_ms
                .min(input.core_max.sampled_at_ms),
        };
        let mut fresh = package_ok && core_ok;
        if !fresh && let Some(firmware) = input.firmware {
            source = TemperatureSource::LenovoFirmwareAggregate;
            reading = firmware;
            fresh = firmware.is_fresh(now, self.options.policy.freshness_ms);
            if fresh && (package_ok || core_ok) {
                source = TemperatureSource::FirmwareWithPartialHardware;
                partial_mask = u8::from(package_ok) | (u8::from(core_ok) << 1);
                if package_ok && input.package.celsius > reading.celsius {
                    reading = input.package;
                }
                if core_ok && input.core_max.celsius > reading.celsius {
                    reading = input.core_max;
                }
            }
        }
        if source != self.previous_source || partial_mask != self.previous_partial_mask {
            self.previous_temperature = None;
            self.predicted_rise = 0.;
            if let Some(decision) = self.decision.as_mut() {
                decision.cool_since_ms = None;
            }
            self.previous_source = source;
            self.previous_partial_mask = partial_mask;
        }
        let observed = fresh.then_some(reading.celsius);
        if fresh {
            if let Some((previous, tick)) = self.previous_temperature {
                if reading.sampled_at_ms > tick
                    && reading.sampled_at_ms - tick <= self.options.policy.freshness_ms
                {
                    self.predicted_rise = ((reading.celsius - previous)
                        * self.options.prediction_ms as f64
                        / (reading.sampled_at_ms - tick) as f64)
                        .clamp(0., self.options.maximum_prediction_c);
                } else if reading.sampled_at_ms < tick {
                    self.predicted_rise = 0.;
                }
            } else {
                self.predicted_rise = 0.;
            }
            if self
                .previous_temperature
                .is_none_or(|(_, tick)| tick != reading.sampled_at_ms)
            {
                self.previous_temperature = Some((reading.celsius, reading.sampled_at_ms));
            }
        } else {
            self.previous_temperature = None;
            self.predicted_rise = 0.;
        }
        let predicted = TemperatureReading {
            celsius: reading.celsius + self.predicted_rise,
            ..reading
        };
        let decision = if source == TemperatureSource::PackageAndCoreMaximum {
            self.policy.evaluate(
                TemperatureReading {
                    celsius: input.package.celsius + self.predicted_rise,
                    ..input.package
                },
                TemperatureReading {
                    celsius: input.core_max.celsius + self.predicted_rise,
                    ..input.core_max
                },
                now,
                self.decision.as_ref(),
            )
        } else {
            self.policy
                .evaluate_firmware(predicted, now, self.decision.as_ref())
        };
        let mut desired;
        if decision.phase == ThermalPhase::SensorFault {
            desired = self.minimum;
            self.recovery_budget = self.minimum;
            self.last_recovery = now;
        } else {
            desired = self.capability.round_down(PowerEnvelope::new(
                (self.baseline.long_watts * decision.power_fraction).max(self.minimum.long_watts),
                (self.baseline.short_watts * decision.power_fraction).max(self.minimum.short_watts),
            ))?;
            self.recovery_budget = self.recovery_budget.min(desired);
            if decision
                .cool_since_ms
                .is_some_and(|t| now.saturating_sub(t) >= self.options.policy.recovery_ms)
                && now.saturating_sub(self.last_recovery) >= self.options.policy.adjustment_ms
            {
                self.recovery_budget = PowerEnvelope::new(
                    (self.recovery_budget.long_watts
                        + self.baseline.long_watts * self.options.policy.recovery_step)
                        .min(self.baseline.long_watts),
                    (self.recovery_budget.short_watts
                        + self.baseline.short_watts * self.options.policy.recovery_step)
                        .min(self.baseline.short_watts),
                );
                self.last_recovery = now;
            }
            desired = self
                .capability
                .round_down(desired.min(self.recovery_budget))?;
        }
        if let Some(ceiling) = self.fault_ceiling {
            desired = self.capability.round_down(desired.min(ceiling))?;
        }
        let unchanged = desired == self.last_verified;
        let mut status = if self.actuator_fault {
            ControlStatus::ActuatorFault
        } else if decision.phase == ThermalPhase::SensorFault {
            ControlStatus::SensorFault
        } else {
            ControlStatus::Holding
        };
        let command = if unchanged
            && now >= self.last_verification
            && now - self.last_verification < self.options.readback_interval_ms
        {
            None
        } else {
            let expiry = now.saturating_add(self.options.command_lifetime_ms);
            let command = resolve_power(
                self.baseline,
                &[PowerConstraint {
                    target: desired,
                    epoch: self.epoch,
                    expires_at_ms: expiry,
                }],
                self.capability,
                now,
                self.epoch,
                expiry,
            )?;
            self.last_verification = now;
            self.pending = Some(command);
            self.pending_unknown = false;
            if !self.actuator_fault && decision.phase != ThermalPhase::SensorFault {
                status = ControlStatus::Holding;
            }
            Some(command)
        };
        self.decision = Some(decision.clone());
        Ok(ThermalTick {
            status,
            decision: Some(decision),
            target: Some(desired),
            command,
            verify_only: unchanged,
            observed_temperature_c: observed,
            predicted_rise_c: self.predicted_rise,
            source,
        })
    }
    pub fn acknowledge(
        &mut self,
        command: PowerCommand,
        result: ApplyStatus,
        readback: Option<PowerEnvelope>,
        epoch: Epoch,
    ) -> CoreResult<ControlStatus> {
        if self.pending != Some(command) {
            return Err(CoreError::Superseded);
        }
        let verified = result == ApplyStatus::Verified
            && readback == Some(command.target)
            && epoch == command.epoch
            && self.capability.is_aligned(command.target);
        if verified && !self.pending_unknown {
            self.pending = None;
            self.last_verified = command.target;
            return Ok(if self.actuator_fault {
                ControlStatus::ActuatorFault
            } else if self
                .decision
                .as_ref()
                .is_some_and(|d| d.phase == ThermalPhase::SensorFault)
            {
                ControlStatus::SensorFault
            } else {
                ControlStatus::Verified
            });
        }
        self.actuator_fault = true;
        let ceiling = self.last_verified.min(command.target);
        self.fault_ceiling = Some(self.fault_ceiling.map_or(ceiling, |old| old.min(ceiling)));
        self.recovery_budget = self.recovery_budget.min(self.fault_ceiling.unwrap());
        if result == ApplyStatus::UnknownCompletion {
            self.pending_unknown = true;
            Ok(ControlStatus::UnknownCompletion)
        } else {
            self.pending = None;
            self.pending_unknown = false;
            Ok(ControlStatus::ActuatorFault)
        }
    }
    /// A late operation is observed, never promoted back to success after its timeout/fault latch.
    pub fn physical_completion_observed(&mut self, command: PowerCommand) -> CoreResult<()> {
        if self.pending != Some(command) || !self.pending_unknown {
            return Err(CoreError::Superseded);
        }
        self.pending = None;
        self.pending_unknown = false;
        Ok(())
    }
}
