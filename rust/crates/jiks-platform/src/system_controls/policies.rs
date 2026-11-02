use super::*;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegOpenKeyExW,
    RegSetValueExW,
};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerOverlay {
    pub ac: WindowsPowerMode,
    pub dc: WindowsPowerMode,
    pub use_ac: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowsPowerPolicy {
    pub expected_raw_mode: u32,
    pub plan: Option<String>,
    pub ensure_balanced_plan: bool,
    pub overlay: Option<PowerOverlay>,
    pub overlay_only_if_balanced: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerPolicyPlan {
    pub id: String,
    pub name: String,
    pub active: bool,
    pub based_on_balanced: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerPolicySnapshot {
    pub plans: Vec<PowerPolicyPlan>,
    pub battery_saver: Option<bool>,
    pub ac_connected: Option<bool>,
    pub effective_power_mode: Option<String>,
    pub overlay_ac: Option<String>,
    pub overlay_dc: Option<String>,
    pub errors: BTreeMap<String, String>,
}
const BALANCED: GUID = GUID::from_u128(0x381b4222_f694_41f0_9685_ff5bb260df2e);
const NO_SUBGROUP: GUID = GUID::from_u128(0xfea3413e_7e05_4911_9a71_700331f1c294);
const PERSONALITY: GUID = GUID::from_u128(0x245d8541_3943_4422_b025_13a784f679b7);
fn personality(id: &str) -> Result<bool> {
    let scheme = parse_guid(id)?;
    let mut value = 0;
    let code = unsafe {
        PowerReadACValueIndex(
            ptr::null_mut(),
            &scheme,
            &NO_SUBGROUP,
            &PERSONALITY,
            &mut value,
        )
    };
    if code != 0 && same_guid(scheme, BALANCED) {
        return Ok(true);
    }
    ensure!(code == 0, "Power-plan personality read failed ({code})");
    Ok(value == 2)
}
fn configured(ac: bool) -> Result<String> {
    let library = OverlayLibrary::open()?;
    let symbol = if ac {
        b"PowerGetUserConfiguredACPowerMode\0".as_slice()
    } else {
        b"PowerGetUserConfiguredDCPowerMode\0".as_slice()
    };
    let get: unsafe extern "system" fn(*mut GUID) -> u32 =
        unsafe { std::mem::transmute(library.symbol(symbol)?) };
    let mut value = unsafe { zeroed() };
    let code = unsafe { get(&mut value) };
    ensure!(code == 0, "Configured overlay read failed ({code})");
    Ok(guid_string(value))
}
pub fn power_policy_snapshot() -> Result<PowerPolicySnapshot> {
    let mut errors = BTreeMap::new();
    fn capture<T>(
        name: &str,
        result: Result<T>,
        errors: &mut BTreeMap<String, String>,
    ) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                errors.insert(name.into(), format!("{error:#}"));
                None
            }
        }
    }
    let mut plans = Vec::new();
    for plan in power_plans()? {
        let based = capture(
            &format!("personality.{}", plan.id),
            personality(&plan.id),
            &mut errors,
        );
        plans.push(PowerPolicyPlan {
            id: plan.id,
            name: plan.name,
            active: plan.active,
            based_on_balanced: based,
        });
    }
    let mut status: SYSTEM_POWER_STATUS = unsafe { zeroed() };
    let known = unsafe { GetSystemPowerStatus(&mut status) } != 0;
    let (battery_saver, ac_connected) = if known {
        (
            Some(status.SystemStatusFlag != 0),
            match status.ACLineStatus {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            },
        )
    } else {
        errors.insert(
            "power_status".into(),
            format!("{}", std::io::Error::last_os_error()),
        );
        (None, None)
    };
    let effective_power_mode = capture("effective_power_mode", effective_power_mode(), &mut errors);
    let overlay_ac = capture("overlay_ac", configured(true), &mut errors);
    let overlay_dc = capture("overlay_dc", configured(false), &mut errors);
    Ok(PowerPolicySnapshot {
        plans,
        battery_saver,
        ac_connected,
        effective_power_mode,
        overlay_ac,
        overlay_dc,
        errors,
    })
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}
fn overlay_key() -> Result<Key> {
    let mut key = ptr::null_mut();
    let code = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            wide("SYSTEM\\CurrentControlSet\\Control\\Power\\User\\PowerSchemes")?.as_ptr(),
            0,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            &mut key,
        )
    };
    ensure!(
        code == 0,
        "Open overlay registry for mutation failed ({code})"
    );
    Ok(Key(key))
}
fn set_registry(key: &Key, ac: bool, mode: WindowsPowerMode) -> Result<()> {
    let value = wide(&guid_string(mode.guid()))?;
    let name = wide(if ac {
        "ActiveOverlayAcPowerScheme"
    } else {
        "ActiveOverlayDcPowerScheme"
    })?;
    let code = unsafe {
        RegSetValueExW(
            key.0,
            name.as_ptr(),
            0,
            REG_SZ,
            value.as_ptr().cast(),
            (value.len() * 2) as u32,
        )
    };
    ensure!(code == 0, "Overlay registry write failed ({code})");
    ensure!(
        configured(ac)? == guid_string(mode.guid()),
        "Persistent overlay readback mismatch"
    );
    Ok(())
}
fn supply_guard(action: &ExplicitAction, expected_ac: bool) -> Result<()> {
    action.check_write_boundary()?;
    let mut status: SYSTEM_POWER_STATUS = unsafe { zeroed() };
    ensure!(
        unsafe { GetSystemPowerStatus(&mut status) } != 0,
        "Power supply state unknown before policy write"
    );
    ensure!(
        status.SystemStatusFlag == 0,
        "Battery saver became active before policy write"
    );
    ensure!(
        status.ACLineStatus <= 1 && expected_ac == (status.ACLineStatus == 1),
        "AC/DC changed while applying Windows power policy"
    );
    action.check_deadline()
}
pub(super) fn apply(
    _owner: &OwnerToken,
    action: &ExplicitAction,
    policy: WindowsPowerPolicy,
    external_guard: &mut dyn FnMut() -> Result<()>,
) -> Result<Value> {
    ensure!(
        [1, 2, 3, 224, 255].contains(&policy.expected_raw_mode),
        "Unknown firmware mode in Windows power policy"
    );
    external_guard()?;
    let before = power_policy_snapshot()?;
    let current = before
        .plans
        .iter()
        .find(|plan| plan.active)
        .context("Active power plan is unknown")?;
    let mut target = policy.plan.as_deref().unwrap_or(&current.id).to_owned();
    if policy.ensure_balanced_plan {
        let based = current
            .based_on_balanced
            .context("Active power-plan personality is unknown")?;
        if !based {
            target = guid_string(BALANCED);
        }
    }
    parse_guid(&target)?;
    let target_plan = before
        .plans
        .iter()
        .find(|plan| plan.id.eq_ignore_ascii_case(&target))
        .context("Configured Windows power plan no longer exists")?;
    let mut overlay = policy.overlay.clone();
    if overlay.is_some()
        && policy.overlay_only_if_balanced
        && !target_plan
            .based_on_balanced
            .context("Target power-plan personality is unknown")?
    {
        overlay = None;
    }
    let saver = if overlay.is_some() {
        Some(
            before
                .battery_saver
                .context("Battery saver is unknown; no policy setter issued")?,
        )
    } else {
        None
    };
    if policy.ensure_balanced_plan && saver == Some(true) {
        return Ok(
            serde_json::json!({"verified":true,"deferred_battery_saver":true,"overlay_applied":false,"snapshot":before}),
        );
    }
    let overlay = if saver == Some(true) { None } else { overlay };
    // Resolve all native entry points, configured-value getters and registry
    // permissions before changing even the base scheme.
    let prepared = if let Some(overlay) = &overlay {
        ensure!(
            before.ac_connected == Some(overlay.use_ac),
            "AC/DC policy input is stale or unknown"
        );
        ensure!(
            before.overlay_ac.is_some()
                && before.overlay_dc.is_some()
                && before.effective_power_mode.is_some(),
            "Overlay getters are unavailable; no policy setter issued"
        );
        let library = OverlayLibrary::open()?;
        let set: unsafe extern "system" fn(GUID) -> u32 =
            unsafe { std::mem::transmute(library.symbol(b"PowerSetActiveOverlayScheme\0")?) };
        Some((library, set, overlay_key()?))
    } else {
        None
    };
    if !target.eq_ignore_ascii_case(&current.id) {
        external_guard()?;
        if let Some(overlay) = &overlay {
            supply_guard(action, overlay.use_ac)?;
        }
        action.check_write_boundary()?;
        let code = unsafe { PowerSetActiveScheme(ptr::null_mut(), &parse_guid(&target)?) };
        ensure!(
            code == 0,
            "Power-plan change failed ({code}); completed changes remain"
        );
        ensure!(
            same_guid(active_plan()?, parse_guid(&target)?),
            "Base power-plan readback mismatch"
        );
    }
    if let (Some(overlay), Some((_library, set, key))) = (&overlay, &prepared) {
        external_guard()?;
        supply_guard(action, overlay.use_ac)?;
        let mode = if overlay.use_ac {
            overlay.ac
        } else {
            overlay.dc
        };
        let code = unsafe { set(mode.guid()) };
        ensure!(
            code == 0,
            "Set active overlay failed ({code}); base plan may already have changed"
        );
        ensure!(
            effective_power_mode()? == guid_string(mode.guid()),
            "Active overlay readback mismatch"
        );
        external_guard()?;
        supply_guard(action, overlay.use_ac)?;
        set_registry(key, true, overlay.ac)
            .context("AC overlay write failed; completed policy changes remain")?;
        external_guard()?;
        supply_guard(action, overlay.use_ac)?;
        set_registry(key, false, overlay.dc)
            .context("DC overlay write failed; completed policy changes remain")?;
    }
    let after = power_policy_snapshot()?;
    ensure!(
        after
            .plans
            .iter()
            .any(|plan| plan.active && plan.id.eq_ignore_ascii_case(&target)),
        "Final Windows power-plan readback mismatch"
    );
    if let Some(overlay) = &overlay {
        ensure!(
            after.overlay_ac.as_deref() == Some(guid_string(overlay.ac.guid()).as_str())
                && after.overlay_dc.as_deref() == Some(guid_string(overlay.dc.guid()).as_str())
                && after.effective_power_mode.as_deref()
                    == Some(
                        guid_string(
                            if overlay.use_ac {
                                overlay.ac
                            } else {
                                overlay.dc
                            }
                            .guid()
                        )
                        .as_str()
                    ),
            "Final Windows AC/DC/effective overlay readback mismatch"
        );
    }
    Ok(
        serde_json::json!({"verified":true,"overlay_applied":overlay.is_some(),"deferred_battery_saver":saver==Some(true),"snapshot":after}),
    )
}
