//! Pure machine policy and data. This crate cannot invoke firmware or create UI.
use serde::{Deserialize, Serialize};

pub mod automation;
pub mod commands;
pub mod error;
pub mod sensors;
pub mod settings;
pub mod thermal;

pub use error::{CoreError, CoreResult};

pub const TARGET_MANUFACTURER: &str = "LENOVO";
pub const TARGET_MODEL: &str = "83NN";
pub const TARGET_BIOS: &str = "S9CN15WW";
pub const TARGET_CPU: &str = "i9-14900HX";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineIdentity {
    pub manufacturer: String,
    pub model: String,
    pub bios: String,
    pub cpu: String,
    pub logical_processors: u32,
}

impl MachineIdentity {
    pub fn is_supported(&self) -> bool {
        self.manufacturer.eq_ignore_ascii_case(TARGET_MANUFACTURER)
            && self.model == TARGET_MODEL
            && self.bios == TARGET_BIOS
            && self
                .cpu
                .to_ascii_lowercase()
                .contains(&TARGET_CPU.to_ascii_lowercase())
            && self.logical_processors > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_verified_machine_is_accepted() {
        let machine = MachineIdentity {
            manufacturer: "LENOVO".into(),
            model: "83NN".into(),
            bios: "S9CN15WW".into(),
            cpu: "Intel Core i9-14900HX".into(),
            logical_processors: 32,
        };
        assert!(machine.is_supported());
        for changed in [
            MachineIdentity {
                model: "83XX".into(),
                ..machine.clone()
            },
            MachineIdentity {
                bios: "unverified".into(),
                ..machine.clone()
            },
            MachineIdentity {
                cpu: "different CPU".into(),
                ..machine.clone()
            },
            MachineIdentity {
                logical_processors: 0,
                ..machine.clone()
            },
        ] {
            assert!(!changed.is_supported());
        }
    }
}
