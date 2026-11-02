//! CCD display controls, ported from LLT.WindowsDisplayAPI 1.3.17-pre.38
//! commit 5905022a0acdb3801e6ac2405219207b5d296899. Undocumented DPI packets
//! use that fixed fork's exact 32/24-byte ABI; no registry or shell bridge.
use crate::{domain::ExplicitAction, ownership::OwnerToken};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    mem::{size_of, zeroed},
    ptr,
};
use windows_sys::Win32::{Devices::Display::*, Foundation::LUID};

const DPI_PERCENTAGES: [u32; 12] = [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];
#[repr(C)]
#[derive(Clone, Copy)]
struct ColorInfo2 {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    value: u32,
    encoding: u32,
    bits: u32,
    active: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct ColorSet {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    value: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct DpiGet {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    minimum: i32,
    current: i32,
    maximum: i32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct DpiSet {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    relative: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColorStatus {
    pub hdr_supported: bool,
    pub hdr_enabled: bool,
    pub acm_supported: bool,
    pub acm_enabled: bool,
    pub limited_by_policy: bool,
    pub active_mode: u32,
    pub bits_per_channel: u32,
    pub source: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DpiStatus {
    pub current_percent: u32,
    pub recommended_percent: u32,
    pub available_percentages: Vec<u32>,
}
#[derive(Debug, Clone, Serialize)]
pub struct DisplayControlStatus {
    pub monitor_path: String,
    pub name: String,
    pub source_name: String,
    pub internal: bool,
    pub color: Option<ColorStatus>,
    pub dpi: Option<DpiStatus>,
    pub refresh: Option<crate::dynamic_refresh::DrrStatus>,
    pub errors: Vec<String>,
}
fn header<T>(kind: i32, adapter: LUID, id: u32) -> DISPLAYCONFIG_DEVICE_INFO_HEADER {
    DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: kind,
        size: size_of::<T>() as u32,
        adapterId: adapter,
        id,
    }
}
fn text(value: &[u16]) -> String {
    String::from_utf16_lossy(&value[..value.iter().position(|v| *v == 0).unwrap_or(value.len())])
}
fn snapshot(flags: u32) -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>)> {
    for _ in 0..4 {
        let (mut path_count, mut mode_count) = (0, 0);
        let code = unsafe { GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count) };
        ensure!(
            code == 0 && path_count <= 1024 && mode_count <= 4096,
            "GetDisplayConfigBufferSizes failed/out of bound ({code})"
        );
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        let code = unsafe {
            QueryDisplayConfig(
                flags,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                ptr::null_mut(),
            )
        };
        if code == 122 {
            continue;
        }
        ensure!(
            code == 0
                && (path_count as usize) <= paths.len()
                && (mode_count as usize) <= modes.len(),
            "QueryDisplayConfig failed ({code})"
        );
        paths.truncate(path_count as usize);
        modes.truncate(mode_count as usize);
        return Ok((paths, modes));
    }
    anyhow::bail!("Display topology did not settle within four snapshots")
}
fn paths() -> Result<Vec<DISPLAYCONFIG_PATH_INFO>> {
    Ok(snapshot(QDC_ONLY_ACTIVE_PATHS)?.0)
}
pub(crate) fn refresh_state(name: &str) -> Result<crate::dynamic_refresh::RefreshRate> {
    let (paths, modes) =
        snapshot(QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE | QDC_VIRTUAL_REFRESH_RATE_AWARE)?;
    let matching = paths
        .iter()
        .filter(|p| {
            target_name(p).is_ok_and(|n| text(&n.monitorDevicePath).eq_ignore_ascii_case(name))
        })
        .collect::<Vec<_>>();
    ensure!(matching.len() == 1, "CCD refresh target absent/ambiguous");
    decode_refresh(matching[0], &modes)
}
pub(crate) fn current_timing(name: &str) -> Result<crate::devices::DisplayMode> {
    let source = source_name(&find_path(name)?)?;
    crate::devices::displays()?
        .into_iter()
        .find(|d| d.name == source)
        .and_then(|d| d.current)
        .context("Selected display current timing unavailable")
}
fn decode_refresh(
    path: &DISPLAYCONFIG_PATH_INFO,
    modes: &[DISPLAYCONFIG_MODE_INFO],
) -> Result<crate::dynamic_refresh::RefreshRate> {
    let index = unsafe { path.targetInfo.Anonymous.modeInfoIdx };
    let index = if path.flags & 8 != 0 {
        index >> 16
    } else {
        index
    };
    let mode = modes
        .get(index as usize)
        .context("CCD physical timing mode unavailable")?;
    ensure!(
        mode.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET
            && mode.id == path.targetInfo.id
            && mode.adapterId.LowPart == path.targetInfo.adapterId.LowPart
            && mode.adapterId.HighPart == path.targetInfo.adapterId.HighPart,
        "CCD physical timing identity mismatch"
    );
    let signal = unsafe { mode.Anonymous.targetMode.targetVideoSignalInfo };
    let hz = |r: DISPLAYCONFIG_RATIONAL| -> Result<u32> {
        ensure!(r.Denominator > 0, "CCD refresh denominator is zero");
        let v = r.Numerator as f64 / r.Denominator as f64;
        ensure!(v > 0. && v <= 1000., "CCD refresh out of bound");
        Ok(v.round() as u32)
    };
    let frequency = hz(signal.vSyncFreq)?;
    let is_dynamic = path.flags & 0x10 != 0;
    let base_frequency = if is_dynamic {
        hz(path.targetInfo.refreshRate)?
    } else {
        0
    };
    ensure!(
        !is_dynamic || base_frequency < frequency,
        "CCD boost flag has no lower virtual refresh rate"
    );
    Ok(crate::dynamic_refresh::RefreshRate {
        frequency,
        is_dynamic,
        base_frequency,
    })
}
fn target_name(path: &DISPLAYCONFIG_PATH_INFO) -> Result<DISPLAYCONFIG_TARGET_DEVICE_NAME> {
    let mut value = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: header::<DISPLAYCONFIG_TARGET_DEVICE_NAME>(
            2,
            path.targetInfo.adapterId,
            path.targetInfo.id,
        ),
        ..Default::default()
    };
    let code = unsafe { DisplayConfigGetDeviceInfo(&mut value.header) };
    ensure!(code == 0, "Display target name unavailable ({code})");
    Ok(value)
}
fn source_name(path: &DISPLAYCONFIG_PATH_INFO) -> Result<String> {
    let mut value = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: header::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(
            1,
            path.sourceInfo.adapterId,
            path.sourceInfo.id,
        ),
        ..Default::default()
    };
    let code = unsafe { DisplayConfigGetDeviceInfo(&mut value.header) };
    ensure!(code == 0, "Display source name unavailable ({code})");
    Ok(text(&value.viewGdiDeviceName))
}
fn find_path(name: &str) -> Result<DISPLAYCONFIG_PATH_INFO> {
    ensure!(
        !name.is_empty(),
        "An explicit monitor device path is required"
    );
    let found = paths()?
        .into_iter()
        .filter_map(|p| {
            target_name(&p)
                .ok()
                .map(|n| (p, text(&n.monitorDevicePath)))
        })
        .filter(|(_, n)| n.eq_ignore_ascii_case(name))
        .collect::<Vec<_>>();
    ensure!(
        found.len() == 1,
        "Monitor path is absent/ambiguous after topology refresh"
    );
    Ok(found[0].0)
}
fn decode_color(value: ColorInfo2) -> ColorStatus {
    ColorStatus {
        hdr_supported: value.value & 0x10 != 0,
        hdr_enabled: value.value & 0x20 != 0 && value.active == 2,
        acm_supported: value.value & 0x40 != 0,
        acm_enabled: value.value & 0x80 != 0,
        limited_by_policy: value.value & 8 != 0,
        active_mode: value.active,
        bits_per_channel: value.bits,
        source: "DisplayConfigGetDeviceInfo type15; separate HDR/WCG API".into(),
    }
}
fn color(path: &DISPLAYCONFIG_PATH_INFO) -> Result<ColorStatus> {
    let mut value: ColorInfo2 = unsafe { zeroed() };
    value.header = header::<ColorInfo2>(15, path.targetInfo.adapterId, path.targetInfo.id);
    let code = unsafe { DisplayConfigGetDeviceInfo(&mut value.header) };
    ensure!(
        code == 0 && value.active <= 2,
        "Separate HDR/ACM capability API unavailable/invalid ({code}); legacy ambiguous-color setter is disabled"
    );
    Ok(decode_color(value))
}
fn decode_dpi(value: DpiGet) -> Result<DpiStatus> {
    ensure!(
        value.minimum <= 0
            && value.maximum >= 0
            && value.current >= value.minimum
            && value.current <= value.maximum,
        "Invalid DPI relative capability range"
    );
    let recommended = value.minimum.checked_neg().context("DPI step overflow")?;
    let current = recommended
        .checked_add(value.current)
        .context("DPI current overflow")?;
    let maximum = recommended
        .checked_add(value.maximum)
        .context("DPI maximum overflow")?;
    ensure!(
        maximum >= 0 && (maximum as usize) < DPI_PERCENTAGES.len() && current >= 0,
        "DPI capability exceeds the fixed verified scale table"
    );
    Ok(DpiStatus {
        current_percent: DPI_PERCENTAGES[current as usize],
        recommended_percent: DPI_PERCENTAGES[recommended as usize],
        available_percentages: DPI_PERCENTAGES[..=maximum as usize].to_vec(),
    })
}
fn dpi(path: &DISPLAYCONFIG_PATH_INFO) -> Result<DpiStatus> {
    let mut value: DpiGet = unsafe { zeroed() };
    value.header = header::<DpiGet>(-3, path.sourceInfo.adapterId, path.sourceInfo.id);
    let code = unsafe { DisplayConfigGetDeviceInfo(&mut value.header) };
    ensure!(code == 0, "Display DPI capability unavailable ({code})");
    decode_dpi(value)
}
pub fn inventory() -> Result<Vec<DisplayControlStatus>> {
    let mut result = vec![];
    for path in paths()? {
        let target = target_name(&path)?;
        let mut errors = vec![];
        let color = match color(&path) {
            Ok(v) => Some(v),
            Err(e) => {
                errors.push(e.to_string());
                None
            }
        };
        let dpi = match dpi(&path) {
            Ok(v) => Some(v),
            Err(e) => {
                errors.push(e.to_string());
                None
            }
        };
        let source = source_name(&path)?;
        let internal = matches!(
            path.targetInfo.outputTechnology,
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
                | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
                | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED
        );
        let refresh = if internal {
            match crate::devices::displays()?
                .into_iter()
                .find(|d| d.name == source)
                .and_then(|d| d.current)
            {
                Some(current) => match crate::dynamic_refresh::read(
                    &text(&target.monitorDevicePath),
                    current.width,
                    current.height,
                ) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        errors.push(format!("DRR: {e:#}"));
                        None
                    }
                },
                None => None,
            }
        } else {
            None
        };
        result.push(DisplayControlStatus {
            monitor_path: text(&target.monitorDevicePath),
            name: text(&target.monitorFriendlyDeviceName),
            source_name: source,
            internal,
            color,
            dpi,
            refresh,
            errors,
        });
    }
    Ok(result)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DisplayAction {
    RefreshRate {
        monitor_path: String,
        width: u32,
        height: u32,
        rate: crate::dynamic_refresh::RefreshRate,
    },
    Hdr {
        monitor_path: String,
        enabled: bool,
    },
    Acm {
        monitor_path: String,
        enabled: bool,
    },
    Dpi {
        monitor_path: String,
        percent: u32,
    },
}
pub(crate) fn write(
    owner: &OwnerToken,
    action: &ExplicitAction,
    request: DisplayAction,
) -> Result<serde_json::Value> {
    match request {
        DisplayAction::RefreshRate {
            monitor_path,
            width,
            height,
            rate,
        } => crate::dynamic_refresh::write(owner, action, &monitor_path, width, height, rate),
        DisplayAction::Hdr {
            monitor_path,
            enabled,
        } => set_color(owner, action, &monitor_path, enabled, true),
        DisplayAction::Acm {
            monitor_path,
            enabled,
        } => set_color(owner, action, &monitor_path, enabled, false),
        DisplayAction::Dpi {
            monitor_path,
            percent,
        } => {
            let path = find_path(&monitor_path)?;
            let before = dpi(&path)?;
            ensure!(
                before.available_percentages.contains(&percent),
                "DPI is not an advertised percentage"
            );
            let index = DPI_PERCENTAGES.iter().position(|p| *p == percent).unwrap() as i32;
            let recommended = DPI_PERCENTAGES
                .iter()
                .position(|p| *p == before.recommended_percent)
                .unwrap() as i32;
            let value = DpiSet {
                header: header::<DpiSet>(-4, path.sourceInfo.adapterId, path.sourceInfo.id),
                relative: index - recommended,
            };
            action.check_write_boundary()?;
            let code = unsafe { DisplayConfigSetDeviceInfo(&value.header) };
            ensure!(code == 0, "Display DPI setter rejected ({code})");
            let observed = dpi(&find_path(&monitor_path)?)?;
            ensure!(
                observed.current_percent == percent,
                "Display DPI readback mismatch"
            );
            Ok(serde_json::to_value(observed)?)
        }
    }
}
fn set_color(
    owner: &OwnerToken,
    action: &ExplicitAction,
    name: &str,
    enabled: bool,
    hdr: bool,
) -> Result<serde_json::Value> {
    let path = find_path(name)?;
    let before = color(&path)?;
    ensure!(
        if hdr {
            before.hdr_supported
        } else {
            before.acm_supported
        },
        "Requested color feature is not supported by this monitor"
    );
    ensure!(
        !enabled || !before.limited_by_policy,
        "Advanced color is disabled by Windows policy"
    );
    let source = source_name(&path)?;
    let old = crate::devices::displays()?
        .into_iter()
        .find(|d| d.name == source)
        .and_then(|d| d.current)
        .context("Cannot preserve current display timing")?;
    let value = ColorSet {
        header: header::<ColorSet>(
            if hdr { 16 } else { 17 },
            path.targetInfo.adapterId,
            path.targetInfo.id,
        ),
        value: u32::from(enabled),
    };
    action.check_write_boundary()?;
    let code = unsafe { DisplayConfigSetDeviceInfo(&value.header) };
    ensure!(
        code == 0,
        "Separate display HDR/ACM setter rejected ({code}); no ambiguous legacy fallback was attempted"
    );
    let current_source = source_name(&find_path(name)?)?;
    let current = crate::devices::displays()?
        .into_iter()
        .find(|d| d.name == current_source)
        .and_then(|d| d.current)
        .context("Display disappeared after color change")?;
    if (current.width, current.height, current.hertz) != (old.width, old.height, old.hertz) {
        crate::devices::display_write(
            owner,
            action,
            &current_source,
            old.width,
            old.height,
            old.hertz,
        )?;
    }
    let observed = color(&find_path(name)?)?;
    ensure!(
        if hdr {
            observed.hdr_enabled == enabled
        } else {
            observed.acm_enabled == enabled
        },
        "Display color readback mismatch"
    );
    Ok(serde_json::to_value(observed)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn virtual_ccd_readback_uses_physical_and_base_frequencies_and_exact_target_index() {
        let mut path = DISPLAYCONFIG_PATH_INFO::default();
        path.flags = 1 | 8 | 0x10;
        path.targetInfo.id = 17;
        path.targetInfo.adapterId.LowPart = 9;
        path.targetInfo.Anonymous.modeInfoIdx = 0xffff;
        path.targetInfo.refreshRate = DISPLAYCONFIG_RATIONAL {
            Numerator: 120,
            Denominator: 1,
        };
        let mut mode = DISPLAYCONFIG_MODE_INFO::default();
        mode.infoType = DISPLAYCONFIG_MODE_INFO_TYPE_TARGET;
        mode.id = 17;
        mode.adapterId.LowPart = 9;
        mode.Anonymous.targetMode.targetVideoSignalInfo.vSyncFreq = DISPLAYCONFIG_RATIONAL {
            Numerator: 240,
            Denominator: 1,
        };
        assert_eq!(
            decode_refresh(&path, &[mode]).unwrap(),
            crate::dynamic_refresh::RefreshRate {
                frequency: 240,
                is_dynamic: true,
                base_frequency: 120
            }
        );
        path.targetInfo.Anonymous.modeInfoIdx = 1 << 16;
        assert!(decode_refresh(&path, &[mode]).is_err());
        path.targetInfo.Anonymous.modeInfoIdx = 0xffff;
        path.targetInfo.refreshRate.Denominator = 0;
        assert!(decode_refresh(&path, &[mode]).is_err());
    }
    #[test]
    fn fixed_ccd_abi_sizes_and_dpi_grid_are_checked_without_display_io() {
        assert_eq!(size_of::<DISPLAYCONFIG_DEVICE_INFO_HEADER>(), 20);
        assert_eq!(size_of::<ColorInfo2>(), 36);
        assert_eq!(size_of::<ColorSet>(), 24);
        assert_eq!(size_of::<DpiGet>(), 32);
        assert_eq!(size_of::<DpiSet>(), 24);
        let mut value: DpiGet = unsafe { zeroed() };
        value.minimum = -2;
        value.current = 0;
        value.maximum = 3;
        let result = decode_dpi(value).unwrap();
        assert_eq!(result.current_percent, 150);
        assert_eq!(result.recommended_percent, 150);
        assert_eq!(
            result.available_percentages,
            vec![100, 125, 150, 175, 200, 225]
        );
        value.current = 4;
        assert!(decode_dpi(value).is_err());
        value.minimum = i32::MIN;
        assert!(decode_dpi(value).is_err());
    }
    #[test]
    fn modern_color_bits_separate_policy_hdr_and_acm() {
        let value = ColorInfo2 {
            header: unsafe { zeroed() },
            value: 0x10 | 0x20 | 0x40 | 0x80 | 8,
            encoding: 0,
            bits: 10,
            active: 2,
        };
        let status = decode_color(value);
        assert!(
            status.hdr_supported
                && status.hdr_enabled
                && status.acm_supported
                && status.acm_enabled
                && status.limited_by_policy
        );
        assert!(!decode_color(ColorInfo2 { active: 1, ..value }).hdr_enabled);
    }
}
