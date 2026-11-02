//! Fixed, read-only Lenovo extrinsic WMI event subscriptions. Creation is always
//! explicit on the consuming background thread; no listener starts on import.
use crate::{domain::monotonic_ms, wmi_native::WmiFacade};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{marker::PhantomData, rc::Rc};
use windows::Win32::System::Wmi::{IEnumWbemClassObject, IWbemClassObject};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareEventKind {
    SmartFanMode,
    ThermalMode,
    Lighting,
    KeyboardProfile,
    KeyLock,
    Utility,
    Brightness,
}
impl FirmwareEventKind {
    pub const ALL: [Self; 7] = [
        Self::SmartFanMode,
        Self::ThermalMode,
        Self::Lighting,
        Self::KeyboardProfile,
        Self::KeyLock,
        Self::Utility,
        Self::Brightness,
    ];
    pub fn class(self) -> &'static str {
        match self {
            Self::SmartFanMode => "LENOVO_GAMEZONE_SMART_FAN_MODE_EVENT",
            Self::ThermalMode => "LENOVO_GAMEZONE_THERMAL_MODE_EVENT",
            Self::Lighting => "LENOVO_LIGHTING_EVENT",
            Self::KeyboardProfile => "LENOVO_GAMEZONE_LIGHT_PROFILE_CHANGE_EVENT",
            Self::KeyLock => "LENOVO_GAMEZONE_KEYLOCK_STATUS_EVENT",
            Self::Utility => "LENOVO_UTILITY_EVENT",
            Self::Brightness => "WmiMonitorBrightnessEvent",
        }
    }
    pub fn field(self) -> &'static str {
        match self {
            Self::SmartFanMode | Self::ThermalMode => "mode",
            Self::Lighting => "Key_ID",
            Self::KeyboardProfile => "EventId",
            Self::KeyLock => "KeyLockState",
            Self::Utility => "PressTypeDataVal",
            Self::Brightness => "Brightness",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwareEvent {
    pub kind: FirmwareEventKind,
    pub value: u32,
    pub received_at_tick_ms: u64,
}
impl FirmwareEvent {
    pub fn decode(
        kind: FirmwareEventKind,
        value: &serde_json::Value,
        received_at_tick_ms: u64,
    ) -> Result<Self> {
        let value = value
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .context("Firmware event value is not a nonnegative uint32")?;
        ensure!(
            kind != FirmwareEventKind::Brightness || value <= 100,
            "Brightness event is outside 0..100"
        );
        Ok(Self {
            kind,
            value,
            received_at_tick_ms,
        })
    }
}
pub struct FirmwareSubscription {
    enumerator: IEnumWbemClassObject,
    _connection: WmiFacade,
    kind: FirmwareEventKind,
    _thread: PhantomData<Rc<()>>,
}
impl FirmwareSubscription {
    /// Call only inside the owning background thread. Provider/permission errors
    /// are returned, never converted into an unsupported/false event.
    pub fn open(kind: FirmwareEventKind) -> Result<Self> {
        let connection = WmiFacade::connect("ROOT\\WMI")?;
        let enumerator = connection.subscribe_fixed(kind)?;
        Ok(Self {
            enumerator,
            _connection: connection,
            kind,
            _thread: PhantomData,
        })
    }
    /// A timeout means no event, not cancellation. Drop the subscription on this
    /// thread after the caller's stop flag is observed. No async sink survives it.
    pub fn poll(&mut self, timeout_ms: u32) -> Result<Option<FirmwareEvent>> {
        ensure!(
            timeout_ms <= 250,
            "Firmware event poll timeout exceeds the shutdown bound"
        );
        let mut objects: [Option<IWbemClassObject>; 1] = [None];
        let mut returned = 0;
        let status = unsafe {
            self.enumerator
                .Next(timeout_ms as i32, &mut objects, &mut returned)
        };
        status.ok()?;
        ensure!(
            returned <= 1,
            "Firmware event enumerator exceeded the requested count"
        );
        if returned == 0 {
            ensure!(
                status.0 == 0x00040004,
                "Firmware event stream ended/unexpected empty success ({status:?})"
            );
            return Ok(None);
        }
        let object = objects[0].take().context("Firmware event object missing")?;
        let value = WmiFacade::event_value(&object, self.kind.field())?;
        Ok(Some(FirmwareEvent::decode(
            self.kind,
            &value,
            monotonic_ms(),
        )?))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_class_and_fields_match_original_listeners() {
        assert_eq!(
            FirmwareEventKind::SmartFanMode.class(),
            "LENOVO_GAMEZONE_SMART_FAN_MODE_EVENT"
        );
        assert_eq!(FirmwareEventKind::Utility.field(), "PressTypeDataVal");
        assert_eq!(FirmwareEventKind::ALL.len(), 7);
        let classes = FirmwareEventKind::ALL
            .into_iter()
            .map(|kind| kind.class())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(classes.len(), 7);
    }
    #[test]
    fn raw_224_is_preserved_and_invalid_metadata_does_not_become_default() {
        let event =
            FirmwareEvent::decode(FirmwareEventKind::SmartFanMode, &serde_json::json!(224), 99)
                .unwrap();
        assert_eq!(event.value, 224);
        assert_eq!(event.received_at_tick_ms, 99);
        assert!(
            FirmwareEvent::decode(FirmwareEventKind::Brightness, &serde_json::json!(101), 99)
                .is_err()
        );
        assert!(
            FirmwareEvent::decode(FirmwareEventKind::SmartFanMode, &serde_json::json!(-1), 99)
                .is_err()
        );
        assert!(
            FirmwareEvent::decode(
                FirmwareEventKind::SmartFanMode,
                &serde_json::Value::Null,
                99
            )
            .is_err()
        );
    }
}
