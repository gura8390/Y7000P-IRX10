//! Explicit GPU status queries; constructing packets or classifying samples does no IO.
//!
//! ABI source: LenovoLegionToolkit-Team/NvAPIWrapper at the fixed commit below,
//! Native/Helpers/FunctionId.cs, Native/Delegates/GPU.cs, and the structures
//! CoprocInfoV8/V7, PrivateActiveApplicationV2, LongString and DisplayIdsV2.
//! Classification follows the old GPUController.RefreshStateAsync ordering;
//! unlike its catch fallbacks, unavailable getters remain unknown.
use crate::gpu::{AbiBuffer, NvStatus};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const ABI_SOURCE: &str = "https://github.com/LenovoLegionToolkit-Team/NvAPIWrapper/tree/0bc0ff2ef72dec39a8882c0d2b4259bc0c6448bc";
pub const GET_COPROC_INFO: u32 = 0x0191_85be;
pub const QUERY_ACTIVE_APPS: u32 = 0x65b1_c5f5;
pub const GET_CONNECTED_DISPLAY_IDS: u32 = 0x0078_dba2;
pub const MAX_APPLICATIONS: usize = 128;
pub const MAX_DISPLAYS: u32 = 1024;
pub const APPLICATION_VERSION: u32 = (2 << 16) | 264;
pub const DISPLAY_VERSION: u32 = (3 << 16) | 16;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuState {
    #[default]
    Unknown,
    PoweredOff,
    Active,
    Inactive,
    MonitorConnected,
}

/// Small status metadata only; process names, paths and PIDs never enter IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuStatusSnapshot {
    pub state: GpuState,
    pub raw_coproc_state: Option<u8>,
    pub filtered_process_count: Option<u32>,
    pub display_connected: Option<bool>,
    pub requested_at_tick_ms: u64,
    pub completed_at_tick_ms: u64,
    pub errors: BTreeMap<String, String>,
}
impl GpuStatusSnapshot {
    /// `hybrid_off` must come from an actual HybridMode getter, never settings.
    /// The caller applies its own freshness gate to both observations.
    pub fn classified(&self, hybrid_off: Option<bool>) -> GpuState {
        classify(
            self.raw_coproc_state,
            self.filtered_process_count,
            self.display_connected,
            hybrid_off,
        )
    }
}

pub fn classify(
    coproc: Option<u8>,
    processes: Option<u32>,
    display_connected: Option<bool>,
    hybrid_off: Option<bool>,
) -> GpuState {
    if matches!(coproc, Some(0 | 2)) {
        return GpuState::PoweredOff;
    }
    if hybrid_off == Some(true) {
        return GpuState::MonitorConnected;
    }
    match (processes, display_connected, hybrid_off) {
        (Some(count), _, Some(false)) if count > 0 => GpuState::Active,
        (Some(0), Some(true), _) => GpuState::MonitorConnected,
        (Some(0), Some(false), Some(false)) => GpuState::Inactive,
        _ => GpuState::Unknown,
    }
}

/// The fork's PrivateActiveApplicationV2 is DWORD version + DWORD pid +
/// LongString's 256 ANSI bytes. Native writes only into this owned fixed array.
#[repr(C)]
#[derive(Clone)]
pub struct ActiveApplication {
    pub version: u32,
    pub process_id: u32,
    pub process_name: [u8; 256],
}
impl Default for ActiveApplication {
    fn default() -> Self {
        Self {
            version: APPLICATION_VERSION,
            process_id: 0,
            process_name: [0; 256],
        }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DisplayId {
    pub version: u32,
    pub connection_type: u32,
    pub display_id: u32,
    pub flags: u32,
}
impl Default for DisplayId {
    fn default() -> Self {
        Self {
            version: DISPLAY_VERSION,
            connection_type: 0,
            display_id: 0,
            flags: 0,
        }
    }
}

/// Implemented by the already-selected, thread-bound native NVAPI session.
/// These are read methods only; no library or device is opened by this module.
pub trait StatusTransport {
    fn coproc_info(&mut self, packet: &mut AbiBuffer) -> Result<()>;
    fn active_applications(
        &mut self,
        apps: &mut [ActiveApplication],
        count: &mut u32,
    ) -> Result<()>;
    fn process_exists(&mut self, process_id: u32) -> Result<bool>;
    /// Same flags=0/count query as the original connected-display fallback.
    fn connected_display_count(&mut self) -> Result<u32>;
}

pub(crate) fn coproc(t: &mut impl StatusTransport) -> Result<u8> {
    for (bytes, version) in [(108, 8), (96, 7)] {
        let mut p = AbiBuffer::new(bytes, version);
        match t.coproc_info(&mut p) {
            Ok(()) => {
                p.validate(bytes, version)?;
                // FieldOffset(0x24) is a BYTE, not the whole DWORD.
                return Ok((p.get(0x24) & 0xff) as u8);
            }
            Err(e) if e.downcast_ref::<NvStatus>().is_some_and(|s| s.0 == -9) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(NvStatus(-9).into())
}

pub(crate) fn active_application_pids(t: &mut impl StatusTransport) -> Result<Vec<u32>> {
    let mut apps = vec![ActiveApplication::default(); MAX_APPLICATIONS];
    let mut count = apps.len() as u32;
    t.active_applications(&mut apps, &mut count)?;
    ensure!(
        count as usize <= apps.len(),
        "NVAPI active application count exceeds packet capacity"
    );
    let mut active = Vec::new();
    for app in apps.iter().take(count as usize) {
        ensure!(
            app.version == APPLICATION_VERSION,
            "NVAPI active application version mismatch"
        );
        let end = app
            .process_name
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| anyhow::anyhow!("NVAPI active application name is unterminated"))?;
        let name = &app.process_name[..end];
        // The original comparer is OrdinalIgnoreCase; these two excluded
        // executable names are ASCII and include the .exe extension.
        if name.eq_ignore_ascii_case(b"dwm.exe") || name.eq_ignore_ascii_case(b"explorer.exe") {
            continue;
        }
        ensure!(
            app.process_id > 0 && app.process_id <= i32::MAX as u32,
            "NVAPI active application has an invalid process ID"
        );
        active.push(app.process_id);
    }
    Ok(active)
}

fn active_count(t: &mut impl StatusTransport) -> Result<u32> {
    let mut count = 0;
    for pid in active_application_pids(t)? {
        if t.process_exists(pid)? {
            count += 1;
        }
    }
    Ok(count)
}

/// Uses current observations only. A failed query is retained verbatim and
/// cannot become a false PoweredOff/Inactive. Coprocessor-confirmed sleep skips
/// process/display queries to avoid unnecessary extra GPU activity.
pub fn read(t: &mut impl StatusTransport, requested_at_tick_ms: u64) -> GpuStatusSnapshot {
    let mut out = GpuStatusSnapshot {
        state: GpuState::Unknown,
        raw_coproc_state: None,
        filtered_process_count: None,
        display_connected: None,
        requested_at_tick_ms,
        completed_at_tick_ms: requested_at_tick_ms,
        errors: BTreeMap::new(),
    };
    match coproc(t) {
        Ok(raw) => out.raw_coproc_state = Some(raw),
        Err(e) => {
            out.errors.insert("coproc_info".into(), format!("{e:#}"));
        }
    }
    if !matches!(out.raw_coproc_state, Some(0 | 2)) {
        match active_count(t) {
            Ok(count) => out.filtered_process_count = Some(count),
            Err(e) => {
                out.errors
                    .insert("active_applications".into(), format!("{e:#}"));
            }
        }
        match t.connected_display_count() {
            Ok(count) if count <= MAX_DISPLAYS => out.display_connected = Some(count > 0),
            Ok(_) => {
                out.errors.insert(
                    "connected_displays".into(),
                    "NVAPI connected display count exceeds bound".into(),
                );
            }
            Err(e) => {
                out.errors
                    .insert("connected_displays".into(), format!("{e:#}"));
            }
        }
    }
    out.state = out.classified(None);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        raw: u8,
        coproc_error: Option<i32>,
        fallback: bool,
        calls: Vec<&'static str>,
        names: Vec<(u32, Vec<u8>)>,
        missing: bool,
        process_error: bool,
        display: Option<u32>,
        overflow: bool,
    }
    impl StatusTransport for Fake {
        fn coproc_info(&mut self, p: &mut AbiBuffer) -> Result<()> {
            self.calls.push(if p.version() == 8 { "v8" } else { "v7" });
            if let Some(e) = self.coproc_error {
                return Err(NvStatus(e).into());
            }
            if self.fallback && p.version() == 8 {
                return Err(NvStatus(-9).into());
            }
            p.set(0x24, 0xaabb_cc00 | self.raw as u32);
            Ok(())
        }
        fn active_applications(
            &mut self,
            apps: &mut [ActiveApplication],
            count: &mut u32,
        ) -> Result<()> {
            self.calls.push("apps");
            assert_eq!(*count, 128);
            assert_eq!(apps.len(), 128);
            for (a, (pid, name)) in apps.iter_mut().zip(&self.names) {
                a.process_id = *pid;
                a.process_name[..name.len()].copy_from_slice(name);
            }
            *count = if self.overflow {
                129
            } else {
                self.names.len() as u32
            };
            Ok(())
        }
        fn process_exists(&mut self, _: u32) -> Result<bool> {
            if self.process_error {
                anyhow::bail!("process query access denied")
            }
            Ok(!self.missing)
        }
        fn connected_display_count(&mut self) -> Result<u32> {
            self.calls.push("displays");
            self.display
                .ok_or_else(|| anyhow::anyhow!("NVAPI original display error -42"))
        }
    }
    #[test]
    fn fixed_fork_status_abi_and_v8_to_v7_only_on_version_error() {
        assert_eq!(std::mem::size_of::<ActiveApplication>(), 264);
        assert_eq!(std::mem::offset_of!(ActiveApplication, process_id), 4);
        assert_eq!(std::mem::offset_of!(ActiveApplication, process_name), 8);
        assert_eq!(std::mem::size_of::<DisplayId>(), 16);
        assert_eq!(std::mem::offset_of!(DisplayId, flags), 12);
        let mut f = Fake {
            raw: 3,
            fallback: true,
            display: Some(0),
            ..Default::default()
        };
        let s = read(&mut f, 99);
        assert_eq!(s.raw_coproc_state, Some(3));
        assert_eq!(f.calls, ["v8", "v7", "apps", "displays"]);
        f.calls.clear();
        f.coproc_error = Some(-42);
        let s = read(&mut f, 99);
        assert!(!f.calls.contains(&"v7"));
        assert!(s.errors["coproc_info"].contains("-42"));
    }
    #[test]
    fn gpu_status_truth_table_preserves_old_precedence_and_unknowns() {
        for raw in [0, 2] {
            assert_eq!(
                classify(Some(raw), Some(4), Some(true), Some(false)),
                GpuState::PoweredOff
            );
        }
        for raw in [None, Some(1), Some(3), Some(255)] {
            assert_eq!(
                classify(raw, Some(4), Some(false), Some(true)),
                GpuState::MonitorConnected
            );
            assert_eq!(
                classify(raw, Some(4), Some(true), Some(false)),
                GpuState::Active
            );
            assert_eq!(
                classify(raw, Some(0), Some(true), Some(false)),
                GpuState::MonitorConnected
            );
            assert_eq!(
                classify(raw, Some(0), Some(false), Some(false)),
                GpuState::Inactive
            );
            assert_eq!(classify(raw, Some(4), Some(false), None), GpuState::Unknown);
            assert_eq!(
                classify(raw, None, Some(false), Some(false)),
                GpuState::Unknown
            );
            assert_eq!(classify(raw, Some(0), None, Some(false)), GpuState::Unknown);
        }
    }
    #[test]
    fn confirmed_sleep_skips_other_queries_and_keeps_timestamps() {
        for raw in [0, 2] {
            let mut f = Fake {
                raw,
                ..Default::default()
            };
            let s = read(&mut f, 123);
            assert_eq!(s.state, GpuState::PoweredOff);
            assert_eq!(f.calls, ["v8"]);
            assert_eq!(s.requested_at_tick_ms, 123);
            assert_eq!(s.completed_at_tick_ms, 123);
            assert!(s.errors.is_empty());
        }
    }
    #[test]
    fn live_nonexcluded_processes_use_exact_exe_ascii_case_and_no_private_metadata() {
        let mut f = Fake {
            raw: 3,
            display: Some(0),
            names: vec![
                (1, b"DWM.ExE".to_vec()),
                (2, b"EXPLORER.exe".to_vec()),
                (3, b"dwm".to_vec()),
                (4, b"game.exe".to_vec()),
            ],
            ..Default::default()
        };
        let s = read(&mut f, 1);
        assert_eq!(s.filtered_process_count, Some(2));
        assert_eq!(s.classified(Some(false)), GpuState::Active);
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("game.exe"));
        assert!(!json.contains("process_id"));
        f.missing = true;
        assert_eq!(read(&mut f, 2).filtered_process_count, Some(0));
    }
    #[test]
    fn failed_or_malformed_queries_are_unknown_and_preserve_original_error() {
        let mut f = Fake {
            coproc_error: Some(-220),
            names: vec![(42, b"a.exe".to_vec())],
            process_error: true,
            ..Default::default()
        };
        let s = read(&mut f, 1);
        assert_eq!(s.state, GpuState::Unknown);
        assert_eq!(s.classified(Some(false)), GpuState::Unknown);
        assert_eq!(s.errors.len(), 3);
        assert!(s.errors["coproc_info"].contains("-220"));
        assert!(s.errors["active_applications"].contains("access denied"));
        assert!(s.errors["connected_displays"].contains("-42"));
        f.coproc_error = None;
        f.raw = 3;
        f.overflow = true;
        f.display = Some(MAX_DISPLAYS + 1);
        let s = read(&mut f, 2);
        assert!(s.filtered_process_count.is_none());
        assert!(s.display_connected.is_none());
        f.overflow = false;
        f.process_error = false;
        f.names = vec![(42, vec![b'a'; 256])];
        assert!(read(&mut f, 3).errors["active_applications"].contains("unterminated"));
    }
}
