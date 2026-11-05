//! GDI/native-control UI. Runtime messages are the only hardware boundary.
mod background;
mod a11y;
mod catalog;
#[cfg(debug_assertions)]
mod capture_fixture;
mod device_information;
mod editors;
mod i18n;
mod native;
mod notifications;
mod overlay;
mod presentation;
mod selectors;
mod sensors;
mod status_popup;
pub use crate::ui_model::UiBridge;
pub(crate) fn benchmark_sensor_settings(bridge: UiBridge) -> anyhow::Result<()> {
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        let class = "STATIC".encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let parent = CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            780,
            720,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        );
        anyhow::ensure!(!parent.is_null(), "Cannot create the isolated DPI parent");
        let result = editors::sensor_settings(parent, bridge);
        DestroyWindow(parent);
        result
    }
}

pub fn run(bridge: UiBridge) -> anyhow::Result<()> {
    let exercise = bridge.smoke_exercise_reentrancy;
    let diagnostics = bridge.diagnostics.clone();
    native::run(bridge)?;
    if exercise {
        use std::sync::atomic::Ordering;
        println!("UI_SMOKE_DIAGNOSTICS {}", diagnostics.snapshot());
        anyhow::ensure!(
            diagnostics.main_rebuilds.load(Ordering::Relaxed) >= 3,
            "UI smoke did not exercise the rebuild path."
        );
        anyhow::ensure!(
            diagnostics.reentrant_defaults.load(Ordering::Relaxed) > 0,
            "UI smoke did not observe native callback reentry."
        );
        let opened = diagnostics.modals_opened.load(Ordering::Relaxed);
        anyhow::ensure!(
            opened > 0 && opened == diagnostics.modals_closed.load(Ordering::Relaxed),
            "UI smoke did not complete its nested modal/close path."
        );
        let overlays = diagnostics.overlays_opened.load(Ordering::Relaxed);
        anyhow::ensure!(
            overlays > 0 && overlays == diagnostics.overlays_closed.load(Ordering::Relaxed),
            "UI smoke did not complete OSD creation and destruction."
        );
        anyhow::ensure!(
            diagnostics.shutdown_requests.load(Ordering::Relaxed) == 1,
            "UI smoke shutdown was not requested exactly once."
        );
        anyhow::ensure!(
            diagnostics.ui_effects_completed.load(Ordering::Relaxed) >= 2,
            "UI smoke did not complete two opaque descending-ID UI effects"
        );
        anyhow::ensure!(
            diagnostics.locale_switches.load(Ordering::Relaxed) >= 2,
            "UI smoke did not rebuild actual navigation labels for English and Chinese"
        );
        anyhow::ensure!(
            diagnostics.status_opened.load(Ordering::Relaxed) > 0
                && diagnostics.status_opened.load(Ordering::Relaxed)
                    == diagnostics.status_closed.load(Ordering::Relaxed),
            "UI smoke did not show, refresh and close its live status window"
        );
    }
    Ok(())
}
