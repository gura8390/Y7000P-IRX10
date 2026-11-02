//! Pure display preferences. Raw Celsius, sensor timestamps and controller
//! inputs never pass through this module or change when a user selects °F.
use crate::service::Metric;
use jiks_core::settings::{JsonNode, SettingsDocument};
use serde_json::Value;
use std::collections::BTreeMap;
#[derive(Clone, Copy)]
struct View<'a> {
    application: &'a JsonNode,
    sensors: &'a JsonNode,
}
fn field<'a>(documents: View<'a>, document: &str, key: &str) -> Option<&'a JsonNode> {
    (if document == "application" {
        documents.application
    } else {
        documents.sensors
    })
    .get_ci(key)
}
fn flag(documents: View<'_>, document: &str, key: &str, default: bool) -> bool {
    field(documents, document, key)
        .and_then(JsonNode::as_bool)
        .unwrap_or(default)
}
fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}
fn selection(documents: View<'_>, key: &str, names: &[&str]) -> usize {
    field(documents, "hardware_sensors", key)
        .and_then(|v| {
            v.as_str()
                .and_then(|s| names.iter().position(|name| name.eq_ignore_ascii_case(s)))
                .or_else(|| v.as_i64().and_then(|n| usize::try_from(n).ok()))
        })
        .filter(|i| *i < names.len())
        .unwrap_or(0)
}
fn replace(metrics: &mut Vec<Metric>, key: &str, label: &str, value: String, detail: &str) {
    let metric = Metric {
        key: key.into(),
        label: label.into(),
        value,
        detail: detail.into(),
    };
    if let Some(old) = metrics.iter_mut().find(|m| m.key == key) {
        *old = metric;
    } else {
        metrics.push(metric);
    }
}
fn formatted(value: Option<f64>, unit: &str, precision: usize) -> String {
    value
        .map(|n| format!("{n:.precision$} {unit}"))
        .unwrap_or_else(|| "—".into())
}
fn fresh(value: &Value) -> bool {
    value
        .get("requested_at_tick_ms")
        .and_then(Value::as_u64)
        .zip(value.get("completed_at_tick_ms").and_then(Value::as_u64))
        .is_some_and(|(start, end)| end >= start && end - start <= 3000)
}
pub(crate) fn apply(
    metrics: &mut Vec<Metric>,
    cache: &BTreeMap<String, Value>,
    application: &SettingsDocument,
    sensor_settings: &SettingsDocument,
) {
    let documents = View {
        application: application.root(),
        sensors: sensor_settings.root(),
    };
    let sensors = flag(documents, "application", "EnableHardwareSensors", false);
    if !sensors {
        metrics.retain(|m| m.key == "BatteryLevel" || m.key == "BatteryState");
        return;
    }
    let average = flag(
        documents,
        "hardware_sensors",
        "ShowCpuAverageFrequency",
        false,
    );
    let source_key = if average {
        "CpuAverageFrequency"
    } else {
        "CpuMaxFrequency"
    };
    if let Some(source) = metrics.iter().find(|m| m.key == source_key).cloned() {
        replace(
            metrics,
            "CpuFrequency",
            if average {
                "CPU 平均频率"
            } else {
                "CPU 最高频率"
            },
            source.value,
            &source.detail,
        );
    }
    let temp = selection(
        documents,
        "CpuTemperatureSource",
        &["Auto", "Average", "Package", "CoreMax"],
    );
    if let Some(intel) = cache.get("intel").filter(|v| fresh(v)) {
        let (key, label) = match temp {
            1 => ("average_core_temperature_celsius", "CPU 核心平均温度"),
            2 => ("package_temperature_celsius", "CPU 封装温度"),
            3 => ("maximum_core_temperature_celsius", "CPU 最高核心温度"),
            _ => (
                "conservative_temperature_celsius",
                "CPU 温度（封装/核心最大值）",
            ),
        };
        replace(
            metrics,
            "CpuTemperature",
            label,
            formatted(number(intel.get(key)), "°C", 0),
            "Intel DTS；此选择仅影响显示，控制输入始终采用原始°C与热点最大值",
        );
        let mode = selection(documents, "CpuVoltageMode", &["Average", "Maximum", "Core"]);
        let value = if mode == 2 {
            let index = field(documents, "hardware_sensors", "CpuVoltageCoreIndex")
                .and_then(JsonNode::as_i64)
                .and_then(|i| usize::try_from(i).ok())
                .unwrap_or(0);
            intel
                .get("cores")
                .and_then(Value::as_array)
                .and_then(|cores| cores.get(index))
                .and_then(|c| number(c.get("requested_vid_volts")))
        } else {
            number(intel.get(if mode == 1 {
                "core_vid_maximum_volts"
            } else {
                "core_vid_average_volts"
            }))
        };
        replace(
            metrics,
            "CpuVoltage",
            match mode {
                1 => "CPU 最高请求 VID",
                2 => "CPU 选定核心请求 VID",
                _ => "CPU 平均请求 VID",
            },
            formatted(value, "V", 3),
            "处理器请求VID；并非实测Vcore。缺少所选核心或读数时显示—",
        );
    } else if temp != 0 {
        replace(
            metrics,
            "CpuTemperature",
            "CPU 选定温度源",
            "—".into(),
            "所选Intel核心/封装温度源当前没有有效样本；未用聚合值代替",
        );
    }
    if let Some(sample) = cache.get("telemetry") {
        if let (Some(total), Some(used)) = (
            sample.get("memory_total_bytes").and_then(Value::as_u64),
            sample.get("memory_used_bytes").and_then(Value::as_u64),
        ) {
            if total > 0 && used <= total {
                let gb = flag(
                    documents,
                    "hardware_sensors",
                    "DisplayMemoryInGigabytes",
                    false,
                );
                replace(
                    metrics,
                    "MemoryUtilization",
                    if gb {
                        "内存使用"
                    } else {
                        "内存使用率"
                    },
                    if gb {
                        format!(
                            "{:.2} / {:.2} GiB",
                            used as f64 / 1073741824.,
                            total as f64 / 1073741824.
                        )
                    } else {
                        format!("{:.1}%", used as f64 * 100. / total as f64)
                    },
                    "GlobalMemoryStatusEx；显示格式不改变原始字节值",
                );
            }
        }
    }
    // A selected integrated GPU must never be silently replaced with the cached
    // discrete GPU. The backend supplies an explicitly selected sample.
    if flag(documents, "hardware_sensors", "SelectedGpuIsIgpu", false)
        && cache
            .get("gpu.sensors")
            .is_some_and(|v| v.get("is_igpu").and_then(Value::as_bool) != Some(true))
    {
        metrics.retain(|m| !m.key.starts_with("Gpu"));
        replace(
            metrics,
            "GpuUtilization",
            "所选集成显卡",
            "—".into(),
            "当前缓存不属于选定集显；等待该设备的原生采样，未显示独显读数冒充",
        );
    }
    let fahrenheit = field(documents, "application", "TemperatureUnit").is_some_and(|v| {
        v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("F")) || v.as_i64() == Some(1)
    });
    if fahrenheit {
        for metric in metrics {
            if metric.key.contains("Temperature") {
                metric.value = to_fahrenheit(&metric.value);
            }
        }
    }
}
fn to_fahrenheit(text: &str) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(index) = rest.find("°C") {
        let head = &rest[..index];
        let number_end = head.trim_end().len();
        let number_start = head[..number_end]
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_ascii_digit() && !matches!(c, '.' | '-' | '+'))
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        if let Ok(value) = head[number_start..number_end].parse::<f64>() {
            result.push_str(&head[..number_start]);
            result.push_str(&format!("{:.0} °F", value * 1.8 + 32.));
        } else {
            result.push_str(head);
            result.push_str("°C");
        }
        rest = &rest[index + "°C".len()..];
    }
    result.push_str(rest);
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_actions::Documents;
    use jiks_core::settings::SettingsDocument;
    use serde_json::json;
    fn apply(metrics: &mut Vec<Metric>, cache: &BTreeMap<String, Value>, documents: &Documents) {
        super::apply(
            metrics,
            cache,
            &documents["application"],
            &documents["hardware_sensors"],
        );
    }
    fn docs(sensor: Value, unit: &str) -> Documents {
        BTreeMap::from([
            (
                "application".into(),
                SettingsDocument::parse(
                    &json!({"EnableHardwareSensors":true,"TemperatureUnit":unit}).to_string(),
                )
                .unwrap(),
            ),
            (
                "hardware_sensors".into(),
                SettingsDocument::parse(&sensor.to_string()).unwrap(),
            ),
        ])
    }
    fn cache() -> BTreeMap<String, Value> {
        BTreeMap::from([
            (
                "intel".into(),
                json!({"requested_at_tick_ms":10,"completed_at_tick_ms":11,"package_temperature_celsius":88,"maximum_core_temperature_celsius":95,"average_core_temperature_celsius":75,"conservative_temperature_celsius":95,"core_vid_average_volts":1.1,"core_vid_maximum_volts":1.3,"cores":[{"requested_vid_volts":1.2},{"requested_vid_volts":1.0}]}),
            ),
            (
                "telemetry".into(),
                json!({"memory_total_bytes":17179869184u64,"memory_used_bytes":8589934592u64}),
            ),
        ])
    }
    #[test]
    fn display_temperature_source_and_unit_do_not_mutate_raw_hotspot() {
        let raw = cache();
        let before = raw.clone();
        let mut metrics = vec![];
        apply(
            &mut metrics,
            &raw,
            &docs(json!({"CpuTemperatureSource":"Average"}), "F"),
        );
        assert_eq!(
            metrics
                .iter()
                .find(|m| m.key == "CpuTemperature")
                .unwrap()
                .value,
            "167 °F"
        );
        assert_eq!(raw, before);
        assert_eq!(raw["intel"]["maximum_core_temperature_celsius"], 95);
        assert_eq!(
            to_fahrenheit("核心 60 °C / 显存 70 °C"),
            "核心 140 °F / 显存 158 °F"
        );
    }
    #[test]
    fn voltage_modes_core_and_memory_units_use_distinct_actual_fields() {
        let raw = cache();
        for (mode, index, expected) in [
            ("Average", 0, "1.100 V"),
            ("Maximum", 0, "1.300 V"),
            ("Core", 1, "1.000 V"),
            ("Core", 20, "—"),
        ] {
            let mut metrics = vec![];
            apply(
                &mut metrics,
                &raw,
                &docs(
                    json!({"CpuVoltageMode":mode,"CpuVoltageCoreIndex":index,"DisplayMemoryInGigabytes":true}),
                    "C",
                ),
            );
            assert_eq!(
                metrics
                    .iter()
                    .find(|m| m.key == "CpuVoltage")
                    .unwrap()
                    .value,
                expected
            );
            assert_eq!(
                metrics
                    .iter()
                    .find(|m| m.key == "MemoryUtilization")
                    .unwrap()
                    .value,
                "8.00 / 16.00 GiB"
            );
        }
    }
    #[test]
    fn frequency_preference_selects_the_matching_source_without_relabeling_max_as_average() {
        let raw = cache();
        for (average, expected) in [(false, "5000 MHz"), (true, "3200 MHz")] {
            let mut metrics = vec![
                Metric {
                    key: "CpuMaxFrequency".into(),
                    value: "5000 MHz".into(),
                    ..Default::default()
                },
                Metric {
                    key: "CpuAverageFrequency".into(),
                    value: "3200 MHz".into(),
                    ..Default::default()
                },
            ];
            apply(
                &mut metrics,
                &raw,
                &docs(json!({"ShowCpuAverageFrequency":average}), "C"),
            );
            assert_eq!(
                metrics
                    .iter()
                    .find(|m| m.key == "CpuFrequency")
                    .unwrap()
                    .value,
                expected
            );
        }
    }
}
