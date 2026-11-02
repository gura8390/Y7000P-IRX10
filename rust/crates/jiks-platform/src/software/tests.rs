use super::*;
use anyhow::bail;
fn snapshot() -> SoftwareSnapshot {
    let mut snapshot = SoftwareSnapshot {
        products: PRODUCTS
            .iter()
            .map(|p| (p.id.into(), ProductState::default()))
            .collect(),
    };
    let state = snapshot.products.get_mut("vantage").unwrap();
    state.services.push(ServiceState {
        name: "ImControllerService".into(),
        running: true,
        state: 4,
        start_type: 2,
        can_stop: true,
        executable: "C:\\Program Files\\Lenovo\\ImController.exe".into(),
    });
    state.tasks.push(TaskState {
        path: "\\Lenovo\\Vantage\\Fixture".into(),
        enabled: true,
    });
    state.recompute();
    snapshot
}
struct Fake {
    snapshot: SoftwareSnapshot,
    prepare_fail: bool,
    fail_at: Option<usize>,
    writes: Vec<Operation>,
}
impl Fake {
    fn new() -> Self {
        Self {
            snapshot: snapshot(),
            prepare_fail: false,
            fail_at: None,
            writes: vec![],
        }
    }
}
impl SoftwarePort for Fake {
    type Prepared = ();
    fn snapshot(&mut self) -> SoftwareSnapshot {
        self.snapshot.clone()
    }
    fn prepare(&mut self, _: &Plan) -> Result<()> {
        ensure!(!self.prepare_fail, "synthetic permission failure");
        Ok(())
    }
    fn execute(
        &mut self,
        _: &mut (),
        operation: &Operation,
        guard: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        guard()?;
        if self.fail_at == Some(self.writes.len()) {
            bail!("synthetic setter failure");
        }
        match operation {
            Operation::Task {
                product, enabled, ..
            } => {
                self.snapshot.products.get_mut(product).unwrap().tasks[0].enabled = *enabled;
            }
            Operation::Service {
                product, enabled, ..
            } => {
                let service = &mut self.snapshot.products.get_mut(product).unwrap().services[0];
                service.running = *enabled;
                service.state = if *enabled { 4 } else { 1 };
                service.start_type = if *enabled { 2 } else { 4 };
            }
            _ => {}
        }
        self.writes.push(operation.clone());
        for state in self.snapshot.products.values_mut() {
            state.recompute();
        }
        Ok(())
    }
}
fn changes() -> BTreeMap<String, bool> {
    [("vantage".into(), true)].into()
}
#[test]
fn fixed_products_and_seven_task_folders_match_original_scope_without_amd_driver() {
    assert!(scoped_controller_allowed(&changes(), "LENOVOVANTAGE.EXE"));
    assert!(!scoped_controller_allowed(&changes(), "LegionSpace.exe"));
    assert!(!scoped_controller_allowed(
        &changes(),
        "Lenovo Legion Toolkit.exe"
    ));
    assert!(!scoped_controller_allowed(
        &[("vantage".into(), false)].into(),
        "LenovoVantage.exe"
    ));
    assert_eq!(PRODUCTS.len(), 4);
    assert_eq!(PRODUCTS[0].tasks.len(), 7);
    assert!(
        PRODUCTS
            .iter()
            .all(|p| !p.services.iter().any(|s| s.contains("AMDRyzen")))
    );
}
#[test]
fn process_matching_is_exact_and_case_insensitive_never_prefix_based() {
    let spec = product("vantage").unwrap();
    assert!(exact_process(spec, "C:\\Fixture\\LENOVOVANTAGE.ExE"));
    assert!(!exact_process(spec, "C:\\Fixture\\LenovoVantageEvil.exe"));
    assert!(!exact_process(spec, "C:\\Fixture\\prefixLenovoVantage.exe"));
}
#[test]
fn unknown_getter_and_pending_service_are_not_not_found() {
    let mut state = ProductState::default();
    state.errors.push("synthetic access denied".into());
    state.recompute();
    assert_eq!(state.status, Status::Unknown);
    let mut state = snapshot().products.remove("vantage").unwrap();
    state.services[0].state = 2;
    state.recompute();
    assert_eq!(state.status, Status::Unknown);
}
#[test]
fn full_plan_preflight_failure_has_no_mutation() {
    let mut fake = Fake::new();
    fake.prepare_fail = true;
    assert!(apply_with(&mut fake, changes(), monotonic_ms() + 60000, || Ok(())).is_err());
    assert!(fake.writes.is_empty());
    let mut bad = changes();
    bad.insert("unrelated".into(), true);
    fake.prepare_fail = false;
    assert!(apply_with(&mut fake, bad, monotonic_ms() + 60000, || Ok(())).is_err());
    assert!(fake.writes.is_empty());
}
#[test]
fn task_outside_fixed_folders_and_unstoppable_service_fail_before_first_write() {
    let mut fake = Fake::new();
    fake.snapshot.products.get_mut("vantage").unwrap().tasks[0].path =
        "\\Unrelated\\Fixture".into();
    assert!(apply_with(&mut fake, changes(), monotonic_ms() + 60000, || Ok(())).is_err());
    assert!(fake.writes.is_empty());
    let mut fake = Fake::new();
    fake.snapshot.products.get_mut("vantage").unwrap().services[0].can_stop = false;
    assert!(apply_with(&mut fake, changes(), monotonic_ms() + 60000, || Ok(())).is_err());
    assert!(fake.writes.is_empty());
}
#[test]
fn partial_native_failure_is_not_success_and_retains_completed_actions_and_readback() {
    let mut fake = Fake::new();
    fake.fail_at = Some(1);
    let result = apply_with(&mut fake, changes(), monotonic_ms() + 60000, || Ok(())).unwrap();
    assert!(!result.success);
    assert_eq!(result.completed.len(), 1);
    assert!(!result.snapshot.products["vantage"].tasks[0].enabled);
    assert!(result.snapshot.products["vantage"].services[0].running);
    assert!(!result.errors.is_empty());
}
#[test]
fn successful_disable_requires_actual_readback_and_rechecks_boundary() {
    let mut fake = Fake::new();
    let mut calls = 0;
    let result = apply_with(&mut fake, changes(), monotonic_ms() + 60000, || {
        calls += 1;
        Ok(())
    })
    .unwrap();
    assert!(result.success);
    assert_eq!(result.snapshot.products["vantage"].status, Status::Disabled);
    assert!(calls >= 5);
    let mut fake = Fake::new();
    let mut calls = 0;
    assert!(
        !apply_with(&mut fake, changes(), monotonic_ms() + 60000, || {
            calls += 1;
            ensure!(calls < 2, "synthetic revoked permit");
            Ok(())
        })
        .unwrap()
        .success
    );
    assert!(fake.writes.is_empty());
}
#[test]
fn startup_approval_and_uwp_state_truth_table_match_original_values() {
    assert_eq!(&startup_approved(true, 999)[..4], &2u32.to_le_bytes());
    assert_eq!(&startup_approved(true, 999)[4..], &[0; 8]);
    assert_eq!(&startup_approved(false, 999)[..4], &3u32.to_le_bytes());
    assert_eq!(&startup_approved(false, 999)[4..], &999u64.to_le_bytes());
    assert_eq!(uwp_startup_state(true), 2);
    assert_eq!(uwp_startup_state(false), 1);
}
