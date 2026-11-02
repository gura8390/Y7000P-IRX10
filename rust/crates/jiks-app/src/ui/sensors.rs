//! Presentation-only sensor grouping. Values are supplied by the runtime cache.
use crate::ui_model::{UiMetric, UiModel};
use serde_json::Value;
use std::collections::BTreeSet;
pub(super) struct Group {
    pub title: String,
    pub metrics: Vec<UiMetric>,
}
pub(super) fn canonical(key: &str) -> String {
    let compact = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    match compact.as_str() {
        "cputemp" => "cputemperature",
        "cpufan" => "cpufanspeed",
        "cpuusage" => "cpuutilization",
        "gputemp" | "gputemperature" => "gpucoretemperature",
        "gpuusage" => "gpuutilization",
        "gpufan" => "gpufanspeed",
        "gpuvramtemp" => "gpuvramtemperature",
        "gpuvramusage" => "gpuvramutilization",
        "batterypercent" => "batterylevel",
        "memoryusage" => "memoryutilization",
        "memorytemp" => "memorytemperature",
        "pchtemp" => "pchtemperature",
        "pchfan" => "pchfanspeed",
        "disk1temp" => "disk1temperature",
        "disk2temp" => "disk2temperature",
        _ => return compact,
    }
    .into()
}
fn get<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
    let object = root.as_object()?;
    object.get(key).or_else(|| {
        object
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    })
}
fn doc<'a>(model: &'a UiModel, key: &str) -> Option<&'a Value> {
    model.documents.get(key).map(|d| &d.value)
}
pub(super) fn groups(model: &UiModel) -> Vec<Group> {
    if doc(model, "dashboard")
        .and_then(|d| get(d, "ShowSensors"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        return vec![];
    }
    let modern = model
        .value("sensors.new_dashboard")
        .as_bool()
        .unwrap_or(false);
    let settings = if modern {
        doc(model, "sensor_dashboard")
    } else {
        None
    };
    if settings
        .and_then(|d| get(d, "ShowSensors"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        return vec![];
    }
    let visible = settings
        .and_then(|d| get(d, "VisibleItems"))
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(canonical)
                .collect::<BTreeSet<_>>()
        });
    let configured = settings
        .and_then(|d| get(d, "Groups"))
        .and_then(Value::as_array);
    let defaults = [
        (
            "CPU",
            vec![
                "CpuUtilization",
                "CpuFrequency",
                "CpuFanSpeed",
                "CpuTemperature",
                "CpuPower",
                "CpuVoltage",
            ],
        ),
        (
            "GPU",
            vec![
                "GpuUtilization",
                "GpuVramUtilization",
                "GpuFrequency",
                "GpuFanSpeed",
                "GpuCoreTemperature",
                "GpuVramTemperature",
                "GpuTemperatures",
                "GpuPower",
            ],
        ),
        ("Motherboard", vec!["PchFanSpeed", "PchTemperature"]),
        ("Battery", vec!["BatteryState", "BatteryLevel"]),
        (
            "Memory",
            vec![
                "MemoryUtilization",
                "MemoryUsed",
                "MemoryTotal",
                "MemoryTemperature",
            ],
        ),
        ("Disk", vec!["Disk1Temperature", "Disk2Temperature"]),
    ];
    let groups = if let Some(groups) = configured {
        groups
            .iter()
            .filter_map(|group| {
                Some((
                    get(group, "CustomName")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_owned)
                        .or_else(|| {
                            get(group, "Type").and_then(|v| {
                                v.as_str().map(str::to_owned).or_else(|| {
                                    v.as_u64()
                                        .and_then(|index| {
                                            [
                                                "CPU",
                                                "GPU",
                                                "Motherboard",
                                                "Battery",
                                                "Memory",
                                                "Disk",
                                            ]
                                            .get(index as usize)
                                        })
                                        .map(|s| (*s).to_owned())
                                })
                            })
                        })?,
                    get(group, "Items")?
                        .as_array()?
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                    get(group,"CustomName").and_then(Value::as_str).is_none_or(|name|name.trim().is_empty()),
                ))
            })
            .collect::<Vec<_>>()
    } else {
        defaults
            .into_iter()
            .map(|(title, items)| {
                (
                    title.to_owned(),
                    items.into_iter().map(str::to_owned).collect(),
                    true,
                )
            })
            .collect()
    };
    let mut used = BTreeSet::new();
    let mut output = Vec::new();
    for (title, items, product_title) in groups {
        let mut metrics = Vec::new();
        for item in items {
            let item = canonical(&item);
            if ["fps","lowfps","frametime"].contains(&item.as_str()) { continue; }
            if visible.as_ref().is_some_and(|v| !v.contains(&item)) {
                continue;
            }
            if let Some(metric) = model.metrics.iter().find(|m| canonical(&m.key) == item) {
                if used.insert(metric.key.clone()) {
                    metrics.push(metric.clone());
                }
            }
        }
        if !metrics.is_empty() {
            output.push(Group {
                title: if product_title {super::i18n::tr(match title.as_str() {
                    "Motherboard" => "主板",
                    "Battery" => "电池",
                    "Memory" => "内存",
                    "Disk" => "磁盘",
                    _ => &title,
                }).into_owned()}else{title},
                metrics,
            });
        }
    }
    if configured.is_none() && visible.is_none() {
        let metrics = model
            .metrics
            .iter()
            .filter(|m| !used.contains(&m.key) && !["fps","lowfps","frametime"].contains(&canonical(&m.key).as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !metrics.is_empty() {
            output.push(Group {
                title: super::i18n::tr("其他传感器").into_owned(),
                metrics,
            });
        }
    }
    output
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_model::UiDocument;
    use serde_json::json;
    #[test]
    fn configured_order_and_visibility_are_respected() {
        let mut m = UiModel::default();
        m.values.insert("sensors.new_dashboard".into(), json!(true));
        m.metrics = vec![
            UiMetric {
                key: "cpu.temp".into(),
                value: "60°C".into(),
                ..Default::default()
            },
            UiMetric {
                key: "CpuPower".into(),
                value: "25W".into(),
                ..Default::default()
            },
            UiMetric {
                key: "gpu.temperature".into(),
                value: "50°C".into(),
                ..Default::default()
            },
        ];
        m.documents.insert("sensor_dashboard".into(),UiDocument{value:json!({"Groups":[{"Type":"GPU","Items":["GpuCoreTemperature"]},{"Type":"CPU","Items":["CpuPower","CpuTemperature"]}],"VisibleItems":["GpuCoreTemperature","CpuTemperature"]}),..Default::default()});
        let groups = groups(&m);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].title, "GPU");
        assert_eq!(groups[1].metrics.len(), 1);
        assert_eq!(groups[1].metrics[0].key, "cpu.temp");
    }
    #[test]
    fn custom_sensor_group_title_and_numeric_group_type_are_rendered() {
        let mut model = UiModel::default();
        model
            .values
            .insert("sensors.new_dashboard".into(), json!(true));
        model.metrics.push(UiMetric {
            key: "CpuTemperature".into(),
            value: "60 °C".into(),
            ..Default::default()
        });
        model.documents.insert("sensor_dashboard".into(),UiDocument{value:json!({"Groups":[{"Type":0,"CustomName":"自定义热点","Items":["CpuTemperature"]}]}),..Default::default()});
        let rendered = groups(&model);
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].title, "自定义热点");
        assert_eq!(rendered[0].metrics[0].value, "60 °C");
    }
    #[test]
    fn missing_values_are_never_fabricated() {
        assert!(groups(&UiModel::default()).is_empty());
    }
}
