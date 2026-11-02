//! Native Windows 11 DRR. Mode-query edits are detached DisplayState properties;
//! only the guarded TryApply crosses the hardware write boundary. The original
//! fixed WindowsDisplayAPI fork uses the same mode property GUID/ratio semantics.
use crate::{domain::ExplicitAction, ownership::OwnerToken};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::{
    Devices::Display::Core::*,
    Foundation::IReference,
    core::{GUID, Interface},
};
const DRR: GUID = GUID::from_u128(0xd2db8e11_6861_4d69_912c_eee5590e1980);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct RefreshRate {
    pub frequency: u32,
    pub is_dynamic: bool,
    pub base_frequency: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrrStatus {
    pub current: RefreshRate,
    pub choices: Vec<RefreshRate>,
    pub source: String,
}
struct Apartment;
impl Apartment {
    fn enter() -> Result<Self> {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}
struct Manager(DisplayManager);
impl Drop for Manager {
    fn drop(&mut self) {
        let _ = self.0.Close();
    }
}
fn rate(value: DisplayPresentationRate) -> Result<f64> {
    let d = value.VerticalSyncRate.Denominator;
    ensure!(d > 0, "Display mode rational denominator is zero");
    let divisor = value.VerticalSyncsPerPresentation.max(1) as f64;
    let f = value.VerticalSyncRate.Numerator as f64 / d as f64 / divisor;
    ensure!(
        f.is_finite() && f > 0. && f <= 1000.,
        "Invalid display presentation rate"
    );
    Ok(f)
}
pub fn pair(physical: f64, low: f64) -> Result<RefreshRate> {
    ensure!(
        physical.is_finite()
            && low.is_finite()
            && physical >= 120.
            && low > 0.
            && low < physical
            && physical <= 1000.,
        "Invalid DRR physical/base pair"
    );
    let frequency = physical.round() as u32;
    let base_frequency = low.round() as u32;
    ensure!(
        (physical - frequency as f64).abs() <= 1.
            && (low - base_frequency as f64).abs() <= 1.
            && base_frequency < frequency,
        "DRR pair cannot be represented by legacy integer Hz"
    );
    Ok(RefreshRate {
        frequency,
        is_dynamic: true,
        base_frequency,
    })
}
fn target_path(state: &DisplayState, name: &str) -> Result<DisplayPath> {
    let targets = state.Targets()?;
    ensure!(
        targets.Size()? <= 1024,
        "Display target count exceeds bound"
    );
    let mut found = Vec::new();
    for i in 0..targets.Size()? {
        let target = targets.GetAt(i)?;
        if target.IsConnected()?
            && target
                .DeviceInterfacePath()?
                .to_string()
                .eq_ignore_ascii_case(name)
        {
            found.push(state.GetPathForTarget(&target)?);
        }
    }
    ensure!(found.len() == 1, "Exact DRR target absent/ambiguous");
    Ok(found.remove(0))
}
fn query(name: &str) -> Result<(Apartment, Manager, DisplayState, DisplayPath)> {
    let apartment = Apartment::enter()?;
    let manager = Manager(DisplayManager::Create(
        DisplayManagerOptions::VirtualRefreshRateAware,
    )?);
    let result = manager.0.TryReadCurrentStateForModeQuery()?;
    ensure!(
        result.ErrorCode()? == DisplayManagerResult::Success,
        "DRR mode query failed {:?}: {:?}",
        result.ErrorCode()?,
        result.ExtendedErrorCode()?
    );
    let state = result.State()?;
    ensure!(
        !state.IsReadOnly()?,
        "Windows returned a read-only DRR mode state"
    );
    let path = target_path(&state, name)?;
    Ok((apartment, manager, state, path))
}
fn modes(
    path: &DisplayPath,
    width: u32,
    height: u32,
) -> Result<
    Vec<(
        RefreshRate,
        DisplayModeInfo,
        IReference<DisplayPresentationRate>,
    )>,
> {
    // These setters update only this detached mode-query object. No TryApply in
    // discovery: Windows hardware/config remains unchanged.
    path.SetPresentationRate(None)?;
    path.SetPhysicalPresentationRate(None)?;
    let modes = path.FindModes(DisplayModeQueryOptions::None)?;
    let current_wire = path.WireFormat()?;
    ensure!(modes.Size()? <= 8192, "DRR mode count exceeds bound");
    let mut out = Vec::new();
    for i in 0..modes.Size()? {
        let mode = modes.GetAt(i)?;
        if !mode.IsWireFormatSupported(&current_wire)? {
            continue;
        }
        let size = mode.SourceResolution()?;
        if size.Width != width as i32 || size.Height != height as i32 {
            continue;
        }
        let props = mode.Properties()?;
        if !props.HasKey(DRR)? {
            continue;
        }
        let boxed = props
            .Lookup(DRR)?
            .cast::<IReference<DisplayPresentationRate>>()?;
        let low = boxed.Value()?;
        let physical = mode.PhysicalPresentationRate()?;
        if let Ok(candidate) = pair(rate(physical)?, rate(low)?) {
            out.push((candidate, mode, boxed));
        }
    }
    Ok(out)
}
pub(crate) fn read(name: &str, width: u32, height: u32) -> Result<DrrStatus> {
    let (_apartment, _manager, _state, path) = query(name)?;
    let mut choices = modes(&path, width, height)?
        .into_iter()
        .map(|(r, _, _)| r)
        .collect::<Vec<_>>();
    choices.sort_by_key(|r| (r.frequency, r.base_frequency));
    choices.dedup();
    let current = crate::display_controls::refresh_state(name)?;
    Ok(DrrStatus{current,choices,source:"Windows DisplayManager DRR mode property D2DB8E11 + independent virtual-aware CCD readback".into()})
}
pub(crate) fn write(
    _owner: &OwnerToken,
    action: &ExplicitAction,
    name: &str,
    width: u32,
    height: u32,
    request: RefreshRate,
) -> Result<serde_json::Value> {
    ensure!(
        request.is_dynamic,
        "Static display refresh uses the existing GDI path"
    );
    ensure!(
        pair(request.frequency as f64, request.base_frequency as f64)? == request,
        "Invalid DRR request"
    );
    let (_apartment, _manager, state, path) = query(name)?;
    let matching = modes(&path, width, height)?
        .into_iter()
        .filter(|(r, _, _)| *r == request)
        .collect::<Vec<_>>();
    ensure!(
        !matching.is_empty(),
        "Windows did not advertise the requested DRR physical/base pair at this resolution"
    );
    let (_, mode, low) = &matching[0];
    let current_wire = path.WireFormat()?;
    path.ApplyPropertiesFromMode(mode)?;
    path.SetWireFormat(&current_wire)?;
    path.SetPresentationRate(low)?;
    // ApplyPropertiesFromMode supplies the physical constraint as an existing
    // WinRT reference. Do not invent a boxed custom struct or a new timing.
    ensure!(
        (rate(path.PhysicalPresentationRate()?.Value()?)? - request.frequency as f64).abs() <= 1.,
        "Windows mode did not retain the requested physical refresh constraint"
    );
    action.check_write_boundary()?;
    let result = state.TryApply(DisplayStateApplyOptions::FailIfStateChanged)?;
    ensure!(
        result.Status()? == DisplayStateOperationStatus::Success,
        "DRR apply failed {:?}, {:?}; no static-success fallback was attempted",
        result.Status()?,
        result.ExtendedErrorCode()?
    );
    for _ in 0..20 {
        let observed = crate::display_controls::refresh_state(name)?;
        let timing = crate::display_controls::current_timing(name)?;
        if observed == request && timing.width == width && timing.height == height {
            return Ok(
                serde_json::json!({"refresh":observed,"width":timing.width,"height":timing.height,"ccd_readback_verified":true}),
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    anyhow::bail!(
        "DRR apply returned success but independent CCD physical/base/boost readback did not match"
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drr_pairs_require_real_high_and_low_not_two_equal_static_rates() {
        assert_eq!(
            pair(239.76, 119.88).unwrap(),
            RefreshRate {
                frequency: 240,
                is_dynamic: true,
                base_frequency: 120
            }
        );
        for (h, l) in [(60., 30.), (120., 120.), (120., 0.), (f64::NAN, 60.)] {
            assert!(pair(h, l).is_err());
        }
        assert!(
            rate(DisplayPresentationRate {
                VerticalSyncRate: windows::Foundation::Numerics::Rational {
                    Numerator: 120,
                    Denominator: 0
                },
                VerticalSyncsPerPresentation: 1
            })
            .is_err()
        );
        let r = RefreshRate {
            frequency: 120,
            is_dynamic: true,
            base_frequency: 60,
        };
        assert_eq!(
            serde_json::to_value(r).unwrap(),
            serde_json::json!({"Frequency":120,"IsDynamic":true,"BaseFrequency":60})
        );
    }
}
