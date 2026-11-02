//! Adapter classification from independent completed getters. No write/probe.
use crate::wmi_native::WmiFacade;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdapterStatus {
    Disconnected,
    Connected,
    ConnectedLowWattage,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerStatus {
    pub ac_connected: Option<bool>,
    pub ac_fit_for_oc: Option<bool>,
    /// Original GetPowerChargeMode.Data, whose normal value is exactly 1.
    pub power_charge_mode: Option<u32>,
    /// Unknown stays null: a denied Lenovo getter cannot prove a normal adapter.
    pub status: Option<AdapterStatus>,
    pub errors: BTreeMap<String, String>,
}
pub fn classify(ac: Option<bool>, fit: Option<bool>, charge: Option<u32>) -> Option<AdapterStatus> {
    match (ac, fit, charge) {
        (Some(false), _, _) => Some(AdapterStatus::Disconnected),
        (Some(true), Some(false), _) => Some(AdapterStatus::ConnectedLowWattage),
        (Some(true), _, Some(value)) if value != 1 => Some(AdapterStatus::ConnectedLowWattage),
        (Some(true), Some(true), Some(1)) => Some(AdapterStatus::Connected),
        _ => None,
    }
}
pub(crate) fn read(facade: &WmiFacade) -> PowerStatus {
    let mut errors = BTreeMap::new();
    fn capture<T>(
        name: &str,
        value: Result<T>,
        errors: &mut BTreeMap<String, String>,
    ) -> Option<T> {
        match value {
            Ok(value) => Some(value),
            Err(error) => {
                errors.insert(name.into(), format!("{error:#}"));
                None
            }
        }
    }
    let ac_connected = capture("ac_connected", crate::ac_connected(), &mut errors);
    let get = |method| {
        facade
            .method_read("LENOVO_GAMEZONE_DATA", method, BTreeMap::new())
            .and_then(|values| crate::system_controls::integer(&values, "Data"))
    };
    let ac_fit_for_oc = capture("ac_fit_for_oc", get("IsACFitForOC"), &mut errors).map(|v| v == 1);
    let power_charge_mode = capture("power_charge_mode", get("GetPowerChargeMode"), &mut errors);
    PowerStatus {
        ac_connected,
        ac_fit_for_oc,
        power_charge_mode,
        status: classify(ac_connected, ac_fit_for_oc, power_charge_mode),
        errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_supply_states_and_unknowns_remain_distinct() {
        assert_eq!(
            classify(Some(false), None, None),
            Some(AdapterStatus::Disconnected)
        );
        assert_eq!(
            classify(Some(true), Some(true), Some(1)),
            Some(AdapterStatus::Connected)
        );
        assert_eq!(
            classify(Some(true), Some(false), None),
            Some(AdapterStatus::ConnectedLowWattage)
        );
        assert_eq!(
            classify(Some(true), None, Some(0)),
            Some(AdapterStatus::ConnectedLowWattage)
        );
        assert_eq!(classify(Some(true), None, Some(1)), None);
        assert_eq!(classify(Some(true), Some(true), None), None);
        assert_eq!(classify(None, Some(true), Some(1)), None);
    }
}
