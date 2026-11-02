use jiks_core::commands::*;
use jiks_core::sensors::*;
use jiks_core::thermal::*;
use std::sync::Arc;

fn epoch() -> Epoch {
    Epoch { policy: 1, mode: 1 }
}
fn sample(celsius: f64, tick: u64) -> TemperatureReading {
    TemperatureReading {
        celsius,
        sampled_at_ms: tick,
    }
}
fn input(celsius: f64, tick: u64) -> ThermalInput {
    ThermalInput {
        package: sample(celsius, tick),
        core_max: sample(celsius, tick),
        firmware: None,
    }
}

#[test]
fn application_defaults_and_invalid_options_are_explicit() {
    assert_eq!(ThermalOptions::default().target_c, 90.);
    let defaults = ThermalControlOptions::default();
    assert_eq!(
        (
            defaults.policy.target_c,
            defaults.policy.warm_c,
            defaults.policy.recover_c
        ),
        (88., 85., 82.)
    );
    assert!(defaults.validate_machine(250, 88).is_ok());
    for invalid in [f64::NAN, f64::INFINITY, -1., 0., 85.] {
        let mut changed = defaults.clone();
        changed.policy.target_c = invalid;
        assert!(changed.validate().is_err());
    }
    for (interval, limit) in [(0, 88), (99, 88), (1001, 88), (250, 89), (250, 87)] {
        assert!(defaults.validate_machine(interval, limit).is_err());
    }
}

#[test]
fn independent_grid_oracle_covers_4680_ceilings_without_overshoot() {
    let caps = [
        PowerCapability::Y7000P,
        PowerCapability {
            min_long: 3.25,
            max_long: 10.25,
            min_short: 3.5,
            max_short: 13.5,
            step: 0.5,
        },
        PowerCapability {
            min_long: 5.,
            max_long: 20.,
            min_short: 3.,
            max_short: 25.,
            step: 2.,
        },
    ];
    let mut seed = 0x0123456789abcdef_u64;
    let mut checked = 0;
    for capability in caps {
        let long_grid: Vec<_> = (0..1000)
            .map(|n| capability.min_long + n as f64 * capability.step)
            .take_while(|v| *v <= capability.max_long)
            .collect();
        let short_grid: Vec<_> = (0..1000)
            .map(|n| capability.min_short + n as f64 * capability.step)
            .take_while(|v| *v <= capability.max_short)
            .collect();
        let minimum = capability.minimum().unwrap();
        assert!(capability.is_aligned(minimum));
        for _ in 0..1560 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let a = (seed >> 11) as f64 / ((1_u64 << 53) as f64);
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let b = (seed >> 11) as f64 / ((1_u64 << 53) as f64);
            let l = capability.min_long + (capability.max_long - capability.min_long) * a;
            let s =
                (capability.min_short + (capability.max_short - capability.min_short) * b).max(l);
            let ceiling = PowerEnvelope::new(l, s.min(capability.max_short));
            if !capability.contains(ceiling) {
                continue;
            }
            let oracle = short_grid.iter().rev().find_map(|&short| {
                if short > ceiling.short_watts {
                    return None;
                }
                long_grid
                    .iter()
                    .rev()
                    .find(|&&long| long <= ceiling.long_watts && long <= short)
                    .map(|&long| PowerEnvelope::new(long, short))
            });
            let actual = capability.round_down(ceiling);
            match oracle {
                Some(expected) => {
                    let actual = actual.unwrap();
                    assert_eq!(actual, expected);
                    assert!(
                        actual.long_watts <= ceiling.long_watts
                            && actual.short_watts <= ceiling.short_watts
                    );
                }
                None => assert!(actual.is_err()),
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 4680);
    let fractional = PowerCapability {
        min_long: 0.1,
        max_long: 1.1,
        min_short: 0.2,
        max_short: 1.2,
        step: 0.1,
    };
    assert_eq!(
        fractional.round_down(PowerEnvelope::new(0.5, 0.7)).unwrap(),
        PowerEnvelope::new(0.5, 0.7)
    );
    assert_eq!(
        fractional
            .round_down(PowerEnvelope::new(0.499999999, 0.699999999))
            .unwrap(),
        PowerEnvelope::new(0.4, 0.6)
    );
}

#[test]
fn constraints_reject_expired_or_wrong_epoch_and_never_raise_budget() {
    let capability = PowerCapability::Y7000P;
    let baseline = PowerEnvelope::new(70., 125.);
    let constraint = PowerConstraint {
        target: PowerEnvelope::new(60.5, 100.9),
        epoch: epoch(),
        expires_at_ms: 1000,
    };
    let command = resolve_power(baseline, &[constraint], capability, 100, epoch(), 2000).unwrap();
    assert_eq!(command.target, PowerEnvelope::new(60., 100.));
    assert_eq!(command.expires_at_ms, 1000);
    assert!(resolve_power(baseline, &[constraint], capability, 1000, epoch(), 2000).is_err());
    assert!(
        resolve_power(
            baseline,
            &[constraint],
            capability,
            0,
            Epoch { mode: 2, ..epoch() },
            2000
        )
        .is_err()
    );
}

#[test]
fn policy_warm_hysteresis_cooling_and_clock_regression() {
    let policy = ThermalPolicy::new(ThermalOptions::application_default()).unwrap();
    let warm = policy.evaluate(sample(85., 0), sample(83., 0), 0, None);
    assert_eq!(warm.phase, ThermalPhase::Warm);
    assert!((warm.power_fraction - 0.9).abs() < 1e-12);
    let held = policy.evaluate(sample(86., 250), sample(84., 250), 250, Some(&warm));
    assert_eq!(held.power_fraction, warm.power_fraction);
    let hot = policy.evaluate(sample(88., 2000), sample(86., 2000), 2000, Some(&held));
    assert!((hot.power_fraction - 0.8).abs() < 1e-12);
    let regressed = policy.evaluate(sample(60., 1000), sample(60., 1000), 1000, Some(&hot));
    assert_eq!(regressed.phase, ThermalPhase::SensorFault);
    let cool = policy.evaluate(sample(60., 2250), sample(60., 2250), 2250, Some(&hot));
    assert_eq!(cool.cool_since_ms, Some(2250));
    let band = policy.evaluate(sample(83., 2300), sample(83., 2300), 2300, Some(&cool));
    assert_eq!(band.cool_since_ms, None);
}

#[test]
fn partial_hardware_hotspot_is_not_hidden_by_firmware_and_source_is_honest() {
    let mut supervisor = ThermalSupervisor::new(
        PowerCapability::Y7000P,
        PowerEnvelope::new(70., 125.),
        epoch(),
        ThermalControlOptions::default(),
        0,
    )
    .unwrap();
    let tick = supervisor
        .evaluate(
            ThermalInput {
                package: TemperatureReading::MISSING,
                core_max: sample(100., 0),
                firmware: Some(sample(60., 0)),
            },
            0,
            epoch(),
        )
        .unwrap();
    assert_eq!(tick.source, TemperatureSource::FirmwareWithPartialHardware);
    assert_eq!(tick.observed_temperature_c, Some(100.));
    assert_eq!(tick.decision.unwrap().phase, ThermalPhase::Hot);
    let command = tick.command.unwrap();
    supervisor
        .acknowledge(
            command,
            ApplyStatus::Verified,
            Some(command.target),
            epoch(),
        )
        .unwrap();
    let tick = supervisor
        .evaluate(
            ThermalInput {
                package: TemperatureReading::MISSING,
                core_max: TemperatureReading::MISSING,
                firmware: Some(sample(60., 250)),
            },
            250,
            epoch(),
        )
        .unwrap();
    assert_eq!(tick.source, TemperatureSource::LenovoFirmwareAggregate);
    assert_eq!(tick.predicted_rise_c, 0.);
}

#[test]
fn stale_data_requests_physical_minimum_and_recovery_is_slow() {
    let capability = PowerCapability::Y7000P;
    let baseline = PowerEnvelope::new(70., 125.);
    let mut supervisor = ThermalSupervisor::new(
        capability,
        baseline,
        epoch(),
        ThermalControlOptions::default(),
        0,
    )
    .unwrap();
    let tick = supervisor
        .evaluate(
            ThermalInput {
                package: TemperatureReading::MISSING,
                core_max: TemperatureReading::MISSING,
                firmware: None,
            },
            0,
            epoch(),
        )
        .unwrap();
    assert_eq!(tick.target, Some(PowerEnvelope::new(50., 60.)));
    assert_eq!(tick.status, ControlStatus::SensorFault);
    if let Some(command) = tick.command {
        supervisor
            .acknowledge(
                command,
                ApplyStatus::Verified,
                Some(command.target),
                epoch(),
            )
            .unwrap();
    }
    for now in (250..=20_250).step_by(250) {
        let tick = supervisor.evaluate(input(60., now), now, epoch()).unwrap();
        let target = tick.target.unwrap();
        if now < 20_250 {
            assert_eq!(target, PowerEnvelope::new(50., 60.));
        } else {
            assert!(target.long_watts <= 51.4 && target.short_watts <= 62.5);
        }
        if let Some(command) = tick.command {
            supervisor
                .acknowledge(
                    command,
                    ApplyStatus::Verified,
                    Some(command.target),
                    epoch(),
                )
                .unwrap();
        }
    }
}

#[test]
fn actuator_failure_latches_and_unknown_completion_keeps_physical_owner() {
    let mut supervisor = ThermalSupervisor::new(
        PowerCapability::Y7000P,
        PowerEnvelope::new(70., 125.),
        epoch(),
        ThermalControlOptions::default(),
        0,
    )
    .unwrap();
    let tick = supervisor.evaluate(input(90., 0), 0, epoch()).unwrap();
    let command = tick.command.unwrap();
    assert_eq!(
        supervisor
            .acknowledge(command, ApplyStatus::UnknownCompletion, None, epoch())
            .unwrap(),
        ControlStatus::UnknownCompletion
    );
    assert!(supervisor.has_pending_physical_io());
    assert!(supervisor.evaluate(input(60., 250), 250, epoch()).is_err());
    supervisor.physical_completion_observed(command).unwrap();
    assert!(!supervisor.has_pending_physical_io());
    assert!(supervisor.has_actuator_fault());
    let tick = supervisor.evaluate(input(60., 500), 500, epoch()).unwrap();
    assert!(tick.target.unwrap().long_watts <= command.target.long_watts);
    if let Some(next) = tick.command {
        assert_eq!(
            supervisor
                .acknowledge(next, ApplyStatus::Verified, Some(next.target), epoch())
                .unwrap(),
            ControlStatus::ActuatorFault
        );
    }
    let superseded = supervisor
        .evaluate(input(60., 750), 750, Epoch { mode: 2, ..epoch() })
        .unwrap();
    assert_eq!(superseded.status, ControlStatus::Superseded);
    assert!(superseded.command.is_none());
}

#[test]
fn executor_capacity_admission_deadline_and_timeout_ownership() {
    let capability = PowerCapability::Y7000P;
    let command = PowerCommand {
        target: PowerEnvelope::new(60., 100.),
        epoch: epoch(),
        expires_at_ms: 2000,
    };
    let mut queue = CommandQueue::default();
    let mut ids = Vec::new();
    for _ in 0..8 {
        ids.push(
            queue
                .enqueue(command, capability, 0, epoch(), 1500)
                .unwrap(),
        );
    }
    assert!(
        queue
            .enqueue(command, capability, 0, epoch(), 1500)
            .is_err()
    );
    assert!(matches!(
        queue.poll(capability, 0, epoch()),
        Admission::Started(_)
    ));
    assert_eq!(
        queue.stop_waiting(ids[0]).unwrap(),
        ApplyStatus::UnknownCompletion
    );
    assert_eq!(queue.poll(capability, 100, epoch()), Admission::Busy);
    assert_eq!(
        queue
            .finish_physical(ids[0], Ok(command.target), capability, epoch())
            .unwrap(),
        ApplyStatus::UnknownCompletion
    );
    for id in &ids[1..] {
        assert_eq!(
            queue.poll(capability, 1600, epoch()),
            Admission::Rejected { id: *id }
        );
    }
    assert!(queue.is_empty());
    assert!(!queue.owns_physical_io());
}

#[test]
fn snapshots_are_independent_and_retain_metric_sample_age() {
    let original = SensorSnapshot::default().with_sample(SensorItem::CpuTemperature, 85., 100);
    let changed = original
        .clone()
        .with_sample(SensorItem::CpuTemperature, 100., 200);
    assert_eq!(original.value(SensorItem::CpuTemperature), 85.);
    assert_eq!(original.sample_tick(SensorItem::CpuTemperature), 100);
    assert_eq!(changed.value(SensorItem::CpuTemperature), 100.);
    assert!(!original.is_fresh(SensorItem::CpuTemperature, 3000, 2000));
    let values: Vec<_> = ALL_SENSOR_ITEMS
        .iter()
        .enumerate()
        .map(|(i, &item)| (item, i as f32))
        .collect();
    let batch = original.with_samples(&values, 500);
    for (item, value) in values {
        assert_eq!(batch.value(item), value);
        assert_eq!(batch.sample_tick(item), 500);
    }
}

#[test]
fn independent_deadlines_match_cpu601_disk2_and_skip_long_pause_debt() {
    let mut scheduler = SensorScheduler::default();
    scheduler.set(1, 100, UpdateScope::CPU, 0).unwrap();
    scheduler.set(2, 60_000, UpdateScope::STORAGE, 0).unwrap();
    let (mut cpu, mut disk) = (0, 0);
    for now in (0..=60_000).step_by(100) {
        let due = scheduler.take_due(now);
        if due.covers(UpdateScope::CPU) {
            cpu += 1;
        }
        if due.covers(UpdateScope::STORAGE) {
            disk += 1;
        }
    }
    assert_eq!((cpu, disk), (601, 2));
    scheduler.take_due(3_600_000);
    assert_eq!(scheduler.take_due(3_600_000), UpdateScope::NONE);
    assert!(!UpdateScope::CPU.covers(UpdateScope::ALL));
    assert!(UpdateScope::ALL.covers(UpdateScope::CPU));
    assert_eq!(
        subscription_scope(
            &[SensorItem::BatteryLevel, SensorItem::GpuTemperatures],
            &[]
        ),
        UpdateScope::FANS | UpdateScope::GPU
    );
}

#[test]
fn refresh_keeps_one_real_flight_across_hide_show_and_latest_pending() {
    let mut refresh = LatestRefresh::default();
    refresh.activate(100, 0).unwrap();
    refresh.offer(Arc::new(SensorSnapshot::default()));
    let old = refresh.take(0).unwrap();
    refresh.deactivate();
    refresh.activate(100, 1).unwrap();
    for value in 1..=100 {
        refresh.offer(Arc::new(SensorSnapshot::default().with_sample(
            SensorItem::CpuTemperature,
            value as f32,
            value,
        )));
    }
    assert!(!refresh.is_current(&old));
    assert!(refresh.take(1000).is_none());
    assert!(refresh.in_flight());
    refresh.complete(&old).unwrap();
    let latest = refresh.take(1000).unwrap();
    assert_eq!(latest.snapshot.value(SensorItem::CpuTemperature), 100.);
    assert!(refresh.is_current(&latest));
    assert!(refresh.complete(&old).is_err());
    assert!(refresh.in_flight());
    refresh.complete(&latest).unwrap();
    for invalid in [
        f64::NAN,
        f64::INFINITY,
        f64::MIN_POSITIVE,
        -1.,
        0.,
        f64::MAX,
    ] {
        assert_eq!(interval_or_default_ms(invalid, 1000).unwrap(), 1000);
    }
}
