//! Shared cached sensor sources follow the fastest *active* display consumer.
//! Slow auxiliary sources retain their own 2s schedule; CPU thermal control keeps
//! its independent sampling/freshness deadlines and does not depend on the UI.
use super::*;

pub(super) struct SensorDemand {
    pub(super) full_view: bool,
    pub(super) status_visible: bool,
    pub(super) interval_ms: Option<u64>,
}

fn interval_ms(document: &SettingsDocument, key: &str) -> u64 {
    (document
        .root()
        .get_ci(key)
        .and_then(JsonNode::as_f64)
        .filter(|value| value.is_finite())
        .unwrap_or(1.)
        .clamp(0.1, 60.)
        * 1000.) as u64
}

impl Engine {
    pub(super) fn sensor_demand(&self) -> Result<SensorDemand> {
        let modern = self
            .repository
            .document("application")?
            .root()
            .get_ci("UseNewSensorDashboard")
            .and_then(JsonNode::as_bool)
            == Some(true);
        let dashboard = self.repository.document("dashboard")?;
        let selected = if modern {
            self.repository.document("sensor_dashboard")?
        } else {
            dashboard
        };
        let dashboard_shown = [dashboard, selected].iter().all(|document| {
            document
                .root()
                .get_ci("ShowSensors")
                .and_then(JsonNode::as_bool)
                != Some(false)
        });
        let (main, battery, osd, status) = {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let visible =
                state.values.get("ui.main_visible").and_then(Value::as_bool) == Some(true);
            let page = state.values.get("ui.page").and_then(Value::as_str);
            (
                visible && page == Some("dashboard") && dashboard_shown,
                visible && page == Some("battery"),
                state.values.get("osd.actual_visible").and_then(Value::as_bool) == Some(true),
                state.values.get("ui.status_visible").and_then(Value::as_bool) == Some(true),
            )
        };
        let mut interval = None;
        let mut include = |active, value| {
            if active {
                interval = Some(interval.map_or(value, |current: u64| current.min(value)));
            }
        };
        include(
            main || battery || status,
            interval_ms(selected, "SensorsRefreshIntervalSeconds"),
        );
        include(
            osd,
            interval_ms(self.repository.document("osd")?, "OsdRefreshInterval"),
        );
        // The built-in exporter publishes every second even with the main
        // window hidden. An inactive dashboard must not slow its source cache.
        include(self.hwinfo_consumer(), 1000);
        Ok(SensorDemand {
            full_view: main || battery || osd,
            status_visible: status,
            interval_ms: interval,
        })
    }

    pub(super) fn sensor_consumers(&self) -> Result<bool> {
        Ok(self.sensor_demand()?.interval_ms.is_some())
    }

    pub(super) fn display_interval_ms(&self) -> Result<u64> {
        // No active display consumer creates no periodic metric request. This
        // fallback only coalesces incidental discovery/explicit read results.
        Ok(self.sensor_demand()?.interval_ms.unwrap_or(1000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::lifecycle_tests::Fixture;

    fn save(engine: &mut Engine, id: &str, value: Value) {
        engine.repository.save(id, SettingsDocument::parse(&value.to_string()).unwrap()).unwrap();
    }
    fn setup() -> Fixture {
        let mut f = Fixture::new();
        save(&mut f.engine, "application", json!({"EnableHardwareSensors":true}));
        save(&mut f.engine, "dashboard", json!({"SensorsRefreshIntervalSeconds":60}));
        save(&mut f.engine, "sensor_dashboard", json!({"SensorsRefreshIntervalSeconds":2}));
        save(&mut f.engine, "osd", json!({"OsdRefreshInterval":0.1}));
        f
    }
    #[test]
    fn visible_osd_samples_at_its_interval_without_waiting_for_hidden_dashboard() {
        let mut f = setup();
        f.engine.value("osd.actual_visible", json!(true));
        assert_eq!(f.engine.sensor_demand().unwrap().interval_ms, Some(100));
        f.engine.periodic().unwrap();
        assert!(f.engine.read_queue.iter().any(|row| row.0 == "telemetry"));
        assert!(f.engine.read_queue.iter().any(|row| row.0 == "intel"));
        assert!(f.engine.read_queue.iter().any(|row| row.0 == "firmware"));
        assert!(f.log.lock().unwrap().is_empty(), "schedule-only test must not dispatch physical IO");
    }
    #[test]
    fn fastest_active_consumer_wins_but_hidden_osd_does_not_raise_polling_rate() {
        let mut f = setup();
        f.engine.value("ui.main_visible", json!(true));
        f.engine.value("ui.page", json!("dashboard"));
        assert_eq!(f.engine.sensor_demand().unwrap().interval_ms, Some(60000));
        f.engine.value("osd.actual_visible", json!(true));
        assert_eq!(f.engine.sensor_demand().unwrap().interval_ms, Some(100));
        f.engine.value("osd.actual_visible", json!(false));
        save(&mut f.engine, "integrations", json!({"HWiNFO":true}));
        assert_eq!(f.engine.sensor_demand().unwrap().interval_ms, Some(1000));
        save(&mut f.engine, "integrations", json!({"HWiNFO":false}));
        assert_eq!(f.engine.sensor_demand().unwrap().interval_ms, Some(60000));
    }
    #[test]
    fn faster_new_consumer_shortens_already_pending_sample_and_presentation_deadlines() {
        let mut f = setup();
        f.engine.value("ui.main_visible", json!(true));
        f.engine.value("ui.page", json!("dashboard"));
        let slow = monotonic_ms().saturating_add(60000);
        f.engine.next_metrics = slow;
        f.engine.next_presentation = slow;
        f.engine.value("osd.actual_visible", json!(true));
        f.engine.periodic().unwrap();
        let latest = monotonic_ms().saturating_add(100);
        assert!(f.engine.next_metrics <= latest && f.engine.next_metrics < slow);
        assert!(f.engine.next_presentation <= latest && f.engine.next_presentation < slow);
        assert!(f.engine.read_queue.is_empty(), "an upcoming deadline is shortened, not bypassed");
    }
    #[test]
    fn disabled_dashboard_sensors_are_not_a_consumer_but_status_and_battery_still_are() {
        let mut f = setup();
        f.engine.value("ui.main_visible", json!(true));
        f.engine.value("ui.page", json!("dashboard"));
        save(&mut f.engine, "dashboard", json!({"ShowSensors":false,"SensorsRefreshIntervalSeconds":60}));
        let demand = f.engine.sensor_demand().unwrap();
        assert!(!demand.full_view && demand.interval_ms.is_none());
        f.engine.periodic().unwrap();
        assert!(f.engine.read_queue.is_empty());
        f.engine.value("ui.status_visible", json!(true));
        let demand = f.engine.sensor_demand().unwrap();
        assert!(demand.status_visible && !demand.full_view);
        assert_eq!(demand.interval_ms, Some(60000));
        f.engine.value("ui.status_visible", json!(false));
        f.engine.value("ui.page", json!("battery"));
        assert!(f.engine.sensor_demand().unwrap().full_view);
    }
    #[test]
    fn modern_dashboard_visibility_and_interval_follow_the_selected_document() {
        let mut f = setup();
        save(&mut f.engine, "application", json!({"EnableHardwareSensors":true,"UseNewSensorDashboard":true}));
        f.engine.value("ui.main_visible", json!(true));
        f.engine.value("ui.page", json!("dashboard"));
        assert_eq!(f.engine.sensor_demand().unwrap().interval_ms, Some(2000));
        save(&mut f.engine, "sensor_dashboard", json!({"ShowSensors":false,"SensorsRefreshIntervalSeconds":2}));
        assert_eq!(f.engine.sensor_demand().unwrap().interval_ms, None);
        f.engine.value("osd.actual_visible", json!(true));
        assert_eq!(f.engine.sensor_demand().unwrap().interval_ms, Some(100));
    }
    #[test]
    fn no_visible_consumer_creates_no_metrics_work_and_osd_never_changes_thermal_deadlines() {
        let mut f = setup();
        f.engine.periodic().unwrap();
        assert!(f.engine.sensor_demand().unwrap().interval_ms.is_none());
        assert!(f.engine.read_queue.is_empty());
        let thermal = monotonic_ms().saturating_add(60000);
        f.engine.next_temperature = thermal;
        f.engine.next_control = thermal;
        f.engine.value("osd.actual_visible", json!(true));
        f.engine.periodic().unwrap();
        assert_eq!(f.engine.next_temperature, thermal);
        assert_eq!(f.engine.next_control, thermal);
        assert!(f.engine.controller.is_none() && f.engine.shadow.is_none());
        assert!(f.engine.read_queue.iter().all(|row| matches!(row.1,
            A::ReadTelemetry | A::ReadIntelSensors | A::ReadNvidia | A::ReadFirmwareSensors)));
    }
    #[test]
    fn changing_visible_osd_interval_retimes_sampling_without_granting_hardware_permission() {
        let mut f = setup();
        f.engine.value("osd.actual_visible", json!(true));
        save(&mut f.engine, "osd", json!({"OsdRefreshInterval":60}));
        let slow = monotonic_ms().saturating_add(60000);
        f.engine.next_metrics = slow;
        f.engine.next_presentation = slow;
        save(&mut f.engine, "osd", json!({"OsdRefreshInterval":0.1}));
        f.engine.periodic().unwrap();
        assert!(f.engine.next_metrics <= monotonic_ms().saturating_add(100));
        assert!(f.engine.next_presentation <= monotonic_ms().saturating_add(100));
        f.engine.read_only = true;
        f.engine.next_metrics = 0;
        f.engine.periodic().unwrap();
        assert!(f.engine.read_queue.is_empty());
        f.engine.read_only = false;
        save(&mut f.engine, "application", json!({"EnableHardwareSensors":false}));
        f.engine.periodic().unwrap();
        assert!(f.engine.read_queue.is_empty());
    }
}
