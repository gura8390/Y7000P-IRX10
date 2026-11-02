//! Sampling deadlines and refresh ownership, independent of any device/UI runtime.
use crate::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SensorItem {
    CpuUtilization,
    CpuFrequency,
    CpuMaxFrequency,
    CpuAverageFrequency,
    CpuPMaxFrequency,
    CpuPAverageFrequency,
    CpuEMaxFrequency,
    CpuEAverageFrequency,
    CpuFanSpeed,
    CpuTemperature,
    CpuPower,
    CpuVoltage,
    GpuUtilization,
    GpuFrequency,
    GpuFanSpeed,
    GpuCoreTemperature,
    GpuVramTemperature,
    GpuTemperatures,
    GpuPower,
    GpuVramUsed,
    GpuVramTotal,
    PchFanSpeed,
    PchTemperature,
    BatteryState,
    BatteryLevel,
    MemoryUtilization,
    MemoryUsed,
    MemoryTotal,
    MemoryTemperature,
    Disk1Temperature,
    Disk2Temperature,
    GpuVramUtilization,
}
pub const SENSOR_COUNT: usize = 32;
pub const ALL_SENSOR_ITEMS: [SensorItem; SENSOR_COUNT] = [
    SensorItem::CpuUtilization,
    SensorItem::CpuFrequency,
    SensorItem::CpuMaxFrequency,
    SensorItem::CpuAverageFrequency,
    SensorItem::CpuPMaxFrequency,
    SensorItem::CpuPAverageFrequency,
    SensorItem::CpuEMaxFrequency,
    SensorItem::CpuEAverageFrequency,
    SensorItem::CpuFanSpeed,
    SensorItem::CpuTemperature,
    SensorItem::CpuPower,
    SensorItem::CpuVoltage,
    SensorItem::GpuUtilization,
    SensorItem::GpuFrequency,
    SensorItem::GpuFanSpeed,
    SensorItem::GpuCoreTemperature,
    SensorItem::GpuVramTemperature,
    SensorItem::GpuTemperatures,
    SensorItem::GpuPower,
    SensorItem::GpuVramUsed,
    SensorItem::GpuVramTotal,
    SensorItem::PchFanSpeed,
    SensorItem::PchTemperature,
    SensorItem::BatteryState,
    SensorItem::BatteryLevel,
    SensorItem::MemoryUtilization,
    SensorItem::MemoryUsed,
    SensorItem::MemoryTotal,
    SensorItem::MemoryTemperature,
    SensorItem::Disk1Temperature,
    SensorItem::Disk2Temperature,
    SensorItem::GpuVramUtilization,
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensorSnapshot {
    values: [f32; SENSOR_COUNT],
    sample_ticks: [u64; SENSOR_COUNT],
}
impl Default for SensorSnapshot {
    fn default() -> Self {
        Self {
            values: [-1.; SENSOR_COUNT],
            sample_ticks: [0; SENSOR_COUNT],
        }
    }
}
impl SensorSnapshot {
    pub fn value(&self, item: SensorItem) -> f32 {
        self.values[item as usize]
    }
    pub fn sample_tick(&self, item: SensorItem) -> u64 {
        self.sample_ticks[item as usize]
    }
    pub fn is_fresh(&self, item: SensorItem, now: u64, max_age_ms: u64) -> bool {
        self.value(item).is_finite()
            && self.value(item) >= 0.
            && self.sample_tick(item) <= now
            && now - self.sample_tick(item) <= max_age_ms
    }
    pub fn with_sample(&self, item: SensorItem, value: f32, tick: u64) -> Self {
        self.with_samples(&[(item, value)], tick)
    }
    pub fn with_samples(&self, values: &[(SensorItem, f32)], tick: u64) -> Self {
        let mut copy = self.clone();
        for &(item, value) in values {
            copy.values[item as usize] = value;
            copy.sample_ticks[item as usize] = tick;
        }
        copy
    }
    pub fn with_value(&self, item: SensorItem, value: f32) -> Self {
        let mut copy = self.clone();
        copy.values[item as usize] = value;
        copy
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateScope(u8);
impl UpdateScope {
    pub const NONE: Self = Self(0);
    pub const CPU: Self = Self(1);
    pub const GPU: Self = Self(2);
    pub const MEMORY: Self = Self(4);
    pub const FANS: Self = Self(8);
    pub const STORAGE: Self = Self(16);
    pub const ALL_NON_STORAGE: Self = Self(15);
    pub const ALL: Self = Self(31);
    pub fn from_bits(bits: u8) -> CoreResult<Self> {
        if bits & !31 != 0 {
            Err(CoreError::invalid("unknown sampling scope bits"))
        } else {
            Ok(Self(bits))
        }
    }
    pub const fn bits(self) -> u8 {
        self.0
    }
    pub const fn covers(self, requested: Self) -> bool {
        self.0 & requested.0 == requested.0
    }
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}
impl std::ops::BitOr for UpdateScope {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}
impl std::ops::BitOrAssign for UpdateScope {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(rhs);
    }
}

#[derive(Debug, Clone, Copy)]
struct Subscription {
    interval_ms: u64,
    scope: UpdateScope,
    due_ms: u64,
}
#[derive(Debug, Clone, Default)]
pub struct SensorScheduler {
    subscriptions: BTreeMap<u64, Subscription>,
}
impl SensorScheduler {
    pub fn len(&self) -> usize {
        self.subscriptions.len()
    }
    pub fn is_empty(&self) -> bool {
        self.subscriptions.is_empty()
    }
    pub fn set(
        &mut self,
        owner: u64,
        interval_ms: u64,
        scope: UpdateScope,
        now: u64,
    ) -> CoreResult<()> {
        if interval_ms == 0
            || interval_ms > i32::MAX as u64
            || scope == UpdateScope::NONE
            || scope.bits() & !UpdateScope::ALL.bits() != 0
        {
            return Err(CoreError::invalid("sampling interval or scope"));
        }
        self.subscriptions.insert(
            owner,
            Subscription {
                interval_ms: interval_ms.max(100),
                scope,
                due_ms: now,
            },
        );
        Ok(())
    }
    pub fn remove(&mut self, owner: u64) -> bool {
        self.subscriptions.remove(&owner).is_some()
    }
    pub fn clear(&mut self) {
        self.subscriptions.clear();
    }
    pub fn take_due(&mut self, now: u64) -> UpdateScope {
        let mut scope = UpdateScope::NONE;
        for subscription in self.subscriptions.values_mut() {
            if now < subscription.due_ms {
                continue;
            }
            scope |= subscription.scope;
            let periods = (now - subscription.due_ms) / subscription.interval_ms + 1;
            subscription.due_ms = subscription
                .due_ms
                .saturating_add(periods.saturating_mul(subscription.interval_ms));
        }
        scope
    }
    pub fn next_delay_ms(&self, now: u64) -> u64 {
        self.subscriptions
            .values()
            .map(|s| s.due_ms.saturating_sub(now))
            .min()
            .unwrap_or(i32::MAX as u64)
    }
}

pub fn subscription_scope(
    items: &[SensorItem],
    providers: &[(UpdateScope, Vec<SensorItem>)],
) -> UpdateScope {
    let mut scope = UpdateScope::NONE;
    for (provider_scope, provided) in providers {
        if items.iter().any(|i| provided.contains(i)) {
            scope |= *provider_scope;
        }
    }
    for item in items {
        scope |= match item {
            SensorItem::GpuTemperatures => UpdateScope::GPU,
            SensorItem::CpuFanSpeed
            | SensorItem::GpuFanSpeed
            | SensorItem::PchFanSpeed
            | SensorItem::PchTemperature
            | SensorItem::BatteryState
            | SensorItem::BatteryLevel => UpdateScope::FANS,
            _ => UpdateScope::NONE,
        };
    }
    scope
}
pub fn interval_or_default_ms(seconds: f64, default_ms: u64) -> CoreResult<u64> {
    if default_ms == 0 || default_ms > i32::MAX as u64 {
        return Err(CoreError::invalid("refresh default interval"));
    }
    if !seconds.is_finite() || seconds < 0.0000001 || seconds > i32::MAX as f64 / 1000. {
        return Ok(default_ms);
    }
    Ok((seconds * 1000.).ceil().max(1.) as u64)
}

#[derive(Debug, Clone)]
pub struct RefreshTicket {
    pub snapshot: Arc<SensorSnapshot>,
    pub generation: u64,
    flight_id: u64,
}
/// The runtime holds the ticket across ALL awaited backend reads. Cancellation never releases it.
#[derive(Debug, Default)]
pub struct LatestRefresh {
    enabled: bool,
    generation: u64,
    interval_ms: u64,
    next_refresh_ms: u64,
    pending: Option<Arc<SensorSnapshot>>,
    flight: Option<u64>,
    next_flight_id: u64,
}
impl LatestRefresh {
    pub fn activate(&mut self, interval_ms: u64, now: u64) -> CoreResult<()> {
        if interval_ms == 0 || interval_ms > i32::MAX as u64 {
            return Err(CoreError::invalid("refresh interval"));
        }
        self.generation = self.generation.wrapping_add(1);
        self.enabled = true;
        self.pending = None;
        self.interval_ms = interval_ms.max(100);
        self.next_refresh_ms = now;
        Ok(())
    }
    pub fn deactivate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.enabled = false;
        self.pending = None;
    }
    pub fn offer(&mut self, snapshot: Arc<SensorSnapshot>) {
        if self.enabled {
            self.pending = Some(snapshot);
        }
    }
    pub fn take(&mut self, now: u64) -> Option<RefreshTicket> {
        if !self.enabled || self.flight.is_some() || now < self.next_refresh_ms {
            return None;
        }
        let snapshot = self.pending.take()?;
        self.next_flight_id = self.next_flight_id.wrapping_add(1);
        self.flight = Some(self.next_flight_id);
        self.next_refresh_ms = now.saturating_add(self.interval_ms);
        Some(RefreshTicket {
            snapshot,
            generation: self.generation,
            flight_id: self.next_flight_id,
        })
    }
    pub fn is_current(&self, ticket: &RefreshTicket) -> bool {
        self.enabled
            && ticket.generation == self.generation
            && self.flight == Some(ticket.flight_id)
    }
    pub fn complete(&mut self, ticket: &RefreshTicket) -> CoreResult<()> {
        if self.flight != Some(ticket.flight_id) {
            return Err(CoreError::Superseded);
        }
        self.flight = None;
        Ok(())
    }
    pub fn in_flight(&self) -> bool {
        self.flight.is_some()
    }
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}
