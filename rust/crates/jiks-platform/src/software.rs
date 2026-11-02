//! Explicit vendor software administration. Reading never changes services,
//! startup entries or tasks. All setters use a preflighted, finite plan.
use crate::{
    domain::{ExplicitAction, monotonic_ms},
    gpu_management::ProcessIdentity,
    ownership::OwnerToken,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
mod native;

#[derive(Debug, Clone, Copy)]
pub struct ProductSpec {
    pub id: &'static str,
    pub services: &'static [&'static str],
    pub processes: &'static [&'static str],
    pub tasks: &'static [&'static str],
    pub startup_names: &'static [&'static str],
}
pub const PRODUCTS: &[ProductSpec] = &[
    ProductSpec {
        id: "vantage",
        services: &["ImControllerService", "LenovoVantageService"],
        processes: &[
            "BGHelper",
            "Lenovo.Modern.ImController",
            "Lenovo.Vantage",
            "LenovoVantage",
            "QSHelper",
            "ScheduleEventAction",
        ],
        tasks: &[
            "Lenovo\\BatteryGauge",
            "Lenovo\\ImController",
            "Lenovo\\ImController\\Plugins",
            "Lenovo\\ImController\\TimeBasedEvents",
            "Lenovo\\UDC",
            "Lenovo\\Vantage",
            "Lenovo\\Vantage\\Schedule",
        ],
        startup_names: &["LenovoVantageToolbar", "LenovoVantage"],
    },
    ProductSpec {
        id: "zone",
        services: &["LZService", "StreamingService"],
        processes: &[
            "BorderlessSpace",
            "ContextMenuInstaller",
            "DoudouAI",
            "LegionZone",
            "LZAgent",
            "LZInstall",
            "LZMain",
            "lzolhelp64",
            "LZService",
            "LZStrategy",
            "LZTray",
            "LZUpdate",
            "NvOcScanner",
            "StreamingDiagnosis",
            "StreamingHost",
            "StreamingService",
        ],
        tasks: &[],
        startup_names: &[],
    },
    ProductSpec {
        id: "space",
        services: &["DAService"],
        processes: &[
            "Bino3D",
            "LegionGameWidget",
            "LegionSpace",
            "LegionSpaceComponent",
            "LegionSpaceToast",
            "LSDaemon",
        ],
        tasks: &[],
        startup_names: &[],
    },
    ProductSpec {
        id: "hotkeys",
        services: &["LenovoFnAndFunctionKeys"],
        processes: &[
            "LenovoUtilityUI",
            "LenovoUtilityService",
            "LenovoSmartKey",
            "utility",
        ],
        tasks: &[],
        startup_names: &[],
    },
];
fn product(id: &str) -> Result<&'static ProductSpec> {
    PRODUCTS
        .iter()
        .find(|p| p.id == id)
        .with_context(|| format!("Unknown software product {id}"))
}
pub(crate) fn scoped_controller_allowed(changes: &BTreeMap<String, bool>, name: &str) -> bool {
    [
        ("vantage", "LenovoVantage.exe"),
        ("zone", "LegionZone.exe"),
        ("space", "LegionSpace.exe"),
    ]
    .into_iter()
    .any(|(id, expected)| changes.get(id) == Some(&true) && name.eq_ignore_ascii_case(expected))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Enabled,
    Disabled,
    NotFound,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceState {
    pub name: String,
    pub running: bool,
    pub state: u32,
    pub start_type: u32,
    pub can_stop: bool,
    pub executable: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskState {
    pub path: String,
    pub enabled: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupState {
    pub hive: String,
    pub name: String,
    pub executable: String,
    pub enabled: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UwpState {
    pub family: String,
    pub enabled: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductState {
    pub status: Status,
    pub services: Vec<ServiceState>,
    pub tasks: Vec<TaskState>,
    pub startup: Vec<StartupState>,
    pub uwp: Vec<UwpState>,
    pub processes: Vec<ProcessIdentity>,
    pub errors: Vec<String>,
}
impl Default for ProductState {
    fn default() -> Self {
        Self {
            status: Status::NotFound,
            services: vec![],
            tasks: vec![],
            startup: vec![],
            uwp: vec![],
            processes: vec![],
            errors: vec![],
        }
    }
}
impl ProductState {
    pub fn recompute(&mut self) {
        self.status = if !self.errors.is_empty()
            || self.services.iter().any(|s| s.state != 1 && s.state != 4)
        {
            Status::Unknown
        } else if self.services.is_empty()
            && self.tasks.is_empty()
            && self.startup.is_empty()
            && self.uwp.is_empty()
            && self.processes.is_empty()
        {
            Status::NotFound
        } else if self.services.iter().any(|s| s.running || s.start_type != 4)
            || self.tasks.iter().any(|t| t.enabled)
            || self.startup.iter().any(|s| s.enabled)
            || self.uwp.iter().any(|u| u.enabled)
            || !self.processes.is_empty()
        {
            Status::Enabled
        } else {
            Status::Disabled
        };
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SoftwareSnapshot {
    pub products: BTreeMap<String, ProductState>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operation {
    Task {
        product: String,
        path: String,
        enabled: bool,
    },
    Service {
        product: String,
        name: String,
        enabled: bool,
    },
    Process {
        product: String,
        identity: ProcessIdentity,
    },
    Startup {
        product: String,
        hive: String,
        name: String,
        enabled: bool,
        executable: String,
    },
    Uwp {
        product: String,
        family: String,
        enabled: bool,
    },
}
#[derive(Debug, Clone)]
pub struct Plan {
    pub changes: BTreeMap<String, bool>,
    pub operations: Vec<Operation>,
}
pub fn plan(snapshot: &SoftwareSnapshot, changes: &BTreeMap<String, bool>) -> Result<Plan> {
    ensure!(
        !changes.is_empty() && changes.len() <= PRODUCTS.len(),
        "Select 1..4 software products"
    );
    let mut operations = Vec::new();
    for (id, disabled) in changes {
        let spec = product(id)?;
        let state = snapshot
            .products
            .get(id)
            .context("Software snapshot missing")?;
        ensure!(
            state.errors.is_empty() && state.status != Status::Unknown,
            "Software {id} state is unknown: {}",
            state.errors.join("; ")
        );
        for task in &state.tasks {
            let folder = task
                .path
                .rsplit_once('\\')
                .map(|(folder, _)| folder.trim_start_matches('\\'))
                .context("Invalid task path")?;
            ensure!(
                spec.tasks
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(folder)),
                "Task escaped the fixed vendor folders"
            );
            operations.push(Operation::Task {
                product: id.clone(),
                path: task.path.clone(),
                enabled: !*disabled,
            });
        }
        for service in &state.services {
            ensure!(
                spec.services
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&service.name)),
                "Service escaped the fixed vendor names"
            );
            ensure!(
                !*disabled || !service.running || service.can_stop,
                "Service {} cannot stop without restarting Windows",
                service.name
            );
            operations.push(Operation::Service {
                product: id.clone(),
                name: service.name.clone(),
                enabled: !*disabled,
            });
        }
        if *disabled {
            for identity in &state.processes {
                ensure!(
                    exact_process(spec, &identity.executable),
                    "Process escaped the exact vendor basename list"
                );
                ensure!(
                    identity.pid > 4 && identity.creation_filetime > 0,
                    "Process identity missing"
                );
                operations.push(Operation::Process {
                    product: id.clone(),
                    identity: identity.clone(),
                });
            }
        }
        for startup in &state.startup {
            ensure!(
                id == "vantage" || id == "zone",
                "Product has no legacy Run entry scope"
            );
            ensure!(
                startup.hive == "HKCU" || startup.hive == "HKLM",
                "Invalid Run entry hive"
            );
            ensure!(
                !startup.name.is_empty() && !startup.name.contains('\0'),
                "Invalid Run entry name"
            );
            operations.push(Operation::Startup {
                product: id.clone(),
                hive: startup.hive.clone(),
                name: startup.name.clone(),
                executable: startup.executable.clone(),
                enabled: !*disabled,
            });
        }
        for uwp in &state.uwp {
            ensure!(
                id == "hotkeys" && uwp.family.to_ascii_lowercase().contains("lenovoutility"),
                "UWP identity escaped Lenovo Utility"
            );
            operations.push(Operation::Uwp {
                product: id.clone(),
                family: uwp.family.clone(),
                enabled: !*disabled,
            });
        }
    }
    ensure!(
        operations.len() <= 1024,
        "Software operation budget exceeded"
    );
    Ok(Plan {
        changes: changes.clone(),
        operations,
    })
}
pub fn exact_process(spec: &ProductSpec, path: &str) -> bool {
    let name = path.rsplit(['\\', '/']).next().unwrap_or("");
    let name = if name
        .get(name.len().saturating_sub(4)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".exe"))
    {
        &name[..name.len() - 4]
    } else {
        name
    };
    spec.processes
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
}
pub fn startup_approved(enabled: bool, filetime: u64) -> [u8; 12] {
    let mut value = [0u8; 12];
    value[..4].copy_from_slice(&(if enabled { 2u32 } else { 3 }).to_le_bytes());
    if !enabled {
        value[4..].copy_from_slice(&filetime.to_le_bytes());
    }
    value
}
pub fn uwp_startup_state(enabled: bool) -> u32 {
    if enabled { 2 } else { 1 }
}
#[derive(Debug, Clone, Serialize)]
pub struct ApplyOutcome {
    pub success: bool,
    pub completed: Vec<Operation>,
    pub errors: Vec<String>,
    pub snapshot: SoftwareSnapshot,
}
pub trait SoftwarePort {
    type Prepared;
    fn snapshot(&mut self) -> SoftwareSnapshot;
    /// Must open and validate every target/permission before the first mutation.
    fn prepare(&mut self, plan: &Plan) -> Result<Self::Prepared>;
    fn execute(
        &mut self,
        prepared: &mut Self::Prepared,
        operation: &Operation,
        guard: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()>;
}
pub fn apply_with(
    port: &mut impl SoftwarePort,
    changes: BTreeMap<String, bool>,
    deadline: u64,
    mut guard: impl FnMut() -> Result<()>,
) -> Result<ApplyOutcome> {
    ensure!(
        deadline > monotonic_ms() && deadline.saturating_sub(monotonic_ms()) <= 60000,
        "Software deadline must be within 60 seconds"
    );
    let before = port.snapshot();
    let plan = plan(&before, &changes)?;
    guard()?;
    let mut prepared = port.prepare(&plan)?;
    let mut completed = Vec::new();
    let mut errors = Vec::new();
    for operation in &plan.operations {
        let result = (|| {
            ensure!(
                monotonic_ms() < deadline,
                "Software operation deadline expired"
            );
            guard()?;
            port.execute(&mut prepared, operation, &mut guard)
        })();
        if let Err(error) = result {
            errors.push(format!("{error:#}"));
            break;
        }
        completed.push(operation.clone());
    }
    let snapshot = port.snapshot();
    for (id, disabled) in &changes {
        let state = snapshot
            .products
            .get(id)
            .context("Software readback missing")?;
        let expected = if *disabled {
            Status::Disabled
        } else {
            Status::Enabled
        };
        if state.status != expected && state.status != Status::NotFound {
            errors.push(format!(
                "{id} actual readback is {:?}; expected {expected:?}: {}",
                state.status,
                state.errors.join("; ")
            ));
        }
    }
    Ok(ApplyOutcome {
        success: errors.is_empty(),
        completed,
        errors,
        snapshot,
    })
}
pub fn read() -> SoftwareSnapshot {
    native::NativeSoftware::new().snapshot()
}
pub(crate) fn apply(
    _owner: &OwnerToken,
    permit: &ExplicitAction,
    changes: BTreeMap<String, bool>,
    deadline: u64,
) -> Result<ApplyOutcome> {
    let scope = changes.clone();
    apply_with(
        &mut native::NativeSoftware::new(),
        changes,
        deadline,
        || {
            permit.check_deadline()?;
            crate::native::reject_other_controllers_for_software(&scope)?;
            permit.check_deadline()?;
            ensure!(monotonic_ms() < deadline, "Software deadline expired");
            Ok(())
        },
    )
}

#[cfg(test)]
mod tests;
