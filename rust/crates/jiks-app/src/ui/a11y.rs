//! Semantic metadata for the system's standard Win32 accessibility providers.
//! Never overrides Value, enabled/checked state, roles, or supported patterns.
//! https://learn.microsoft.com/windows/win32/winauto/using-direct-annotation
use anyhow::{Context, Result, ensure};
use std::{
    cell::RefCell,
    marker::PhantomData,
    ptr::null_mut,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};
use windows::{
    Win32::{
        Foundation::HWND as ComHwnd,
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
            CoCreateInstance, CoInitializeEx, CoUninitialize,
        },
        UI::Accessibility::{
            AutomationId_Property_GUID, CLSID_AccPropServices, HelpText_Property_GUID,
            IAccPropServices, Name_Property_GUID, PROPID_ACC_NAME,
        },
    },
    core::{GUID, PCWSTR},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::{
        Shell::{DefSubclassProc, GetWindowSubclass, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            CHILDID_SELF, GetWindowThreadProcessId, IsWindow, OBJID_CLIENT, WM_NCDESTROY,
        },
    },
};

const SUBCLASS: usize = 0x4A494B53;
static GENERATION: AtomicU64 = AtomicU64::new(1);
const PROPERTIES: [GUID; 4] = [
    AutomationId_Property_GUID,
    Name_Property_GUID,
    PROPID_ACC_NAME,
    HelpText_Property_GUID,
];
struct Services {
    users: usize,
    value: IAccPropServices,
}
#[derive(Default)]
struct Diagnostics {
    failures: u64,
    cleaned: u64,
    last_error: Option<String>,
    property_calls: u64,
}
thread_local! {
    static SERVICES: RefCell<Option<Services>> = const { RefCell::new(None) };
    static DIAGNOSTICS: RefCell<Diagnostics> = RefCell::new(Diagnostics::default());
}

/// Kept on the creating UI thread until its windows have actually been retired.
/// Nested editors and file pickers use the same STA and balance their own entry.
pub(super) struct UiThread {
    _not_send: PhantomData<Rc<()>>,
}
impl UiThread {
    pub(super) fn enter() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE)
                .ok()
                .context("Cannot initialize the accessibility UI apartment")?;
        }
        // CoCreateInstance may pump messages. Never hold the TLS RefMut across
        // that call, including standalone/nested editor initialization.
        let needs_service = SERVICES.with(|slot| slot.borrow().is_none());
        let candidate: Result<Option<IAccPropServices>> = if needs_service {
            unsafe { CoCreateInstance(&CLSID_AccPropServices, None, CLSCTX_INPROC_SERVER) }
                .context("Cannot initialize Windows accessibility annotations")
                .map(Some)
        } else {
            Ok(None)
        };
        let initialized = candidate.and_then(|candidate| {
            SERVICES.with(|slot| -> Result<()> {
                let mut slot = slot.borrow_mut();
                if let Some(services) = slot.as_mut() {
                    services.users += 1;
                } else {
                    let value = candidate
                        .context("Accessibility apartment initialization was interrupted")?;
                    *slot = Some(Services { users: 1, value });
                }
                Ok(())
            })
        });
        if let Err(error) = initialized {
            unsafe { CoUninitialize() };
            return Err(error);
        }
        Ok(Self {
            _not_send: PhantomData,
        })
    }
}
impl Drop for UiThread {
    fn drop(&mut self) {
        SERVICES.with(|slot| {
            let mut slot = slot.borrow_mut();
            if let Some(services) = slot.as_mut() {
                services.users -= 1;
                if services.users == 0 {
                    *slot = None;
                }
            }
        });
        unsafe { CoUninitialize() };
    }
}
struct Annotation {
    services: IAccPropServices,
    id: String,
    generation: u64,
    name: Option<String>,
    help: Option<String>,
    applied: bool,
}
fn report(id: &str, error: impl std::fmt::Display) {
    let message = format!("Accessibility metadata failed for {id}: {error}");
    DIAGNOSTICS.with(|slot| {
        let mut diagnostics = slot.borrow_mut();
        diagnostics.failures += 1;
        if diagnostics.last_error.as_ref() != Some(&message) {
            eprintln!("{message}");
        }
        diagnostics.last_error = Some(message);
    });
}
pub(super) fn diagnostic() -> Option<String> {
    DIAGNOSTICS.with(|slot| slot.borrow().last_error.clone())
}
fn utf16(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
#[derive(Clone, Copy)]
struct Owner {
    pointer: usize,
    generation: u64,
}
fn current(hwnd: HWND, owner: Owner) -> bool {
    unsafe {
        let mut pointer = 0usize;
        GetWindowSubclass(hwnd, Some(subclass), SUBCLASS, &mut pointer) != 0
            && pointer == owner.pointer
            && pointer != 0
            && (*(pointer as *const Annotation)).generation == owner.generation
    }
}
fn write(
    services: &IAccPropServices,
    hwnd: HWND,
    owner: Owner,
    property: GUID,
    value: &str,
) -> Result<()> {
    ensure!(
        current(hwnd, owner),
        "Accessibility HWND was retired during annotation"
    );
    ensure!(
        !value.contains('\0'),
        "Accessibility strings cannot contain NUL"
    );
    DIAGNOSTICS.with(|slot| slot.borrow_mut().property_calls += 1);
    unsafe {
        services.SetHwndPropStr(
            ComHwnd(hwnd),
            OBJID_CLIENT as u32,
            CHILDID_SELF,
            property,
            PCWSTR(utf16(value).as_ptr()),
        )
    }
    .context("SetHwndPropStr")?;
    ensure!(
        current(hwnd, owner),
        "Accessibility HWND was retired during annotation"
    );
    Ok(())
}
fn apply(hwnd: HWND, id: &str, name: Option<&str>, help: Option<&str>) -> Result<()> {
    ensure!(
        !id.trim().is_empty() && !id.contains('\0'),
        "An accessibility ID must be nonempty"
    );
    ensure!(
        name.into_iter()
            .chain(help)
            .all(|value| !value.contains('\0')),
        "Accessibility strings cannot contain NUL"
    );
    ensure!(
        !hwnd.is_null() && unsafe { IsWindow(hwnd) } != 0,
        "Accessibility HWND is unavailable"
    );
    ensure!(
        unsafe { GetWindowThreadProcessId(hwnd, null_mut()) } == unsafe { GetCurrentThreadId() },
        "Accessibility annotations must run on the window's UI thread"
    );
    let services = SERVICES
        .with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|services| services.value.clone())
        })
        .context("Accessibility UI apartment has not been initialized")?;
    let owner = unsafe {
        let mut pointer = 0usize;
        if GetWindowSubclass(hwnd, Some(subclass), SUBCLASS, &mut pointer) != 0 && pointer != 0 {
            let annotation = &mut *(pointer as *mut Annotation);
            if annotation.applied
                && annotation.id == id
                && annotation.name.as_deref() == name
                && annotation.help.as_deref() == help
            {
                return Ok(());
            }
            // No borrow of callback state is held across a COM call, which can
            // synchronously dispatch native window messages.
            annotation.id = id.into();
            annotation.name = name.map(str::to_owned);
            annotation.help = help.map(str::to_owned);
            annotation.applied = false;
            Owner {
                pointer,
                generation: (*(pointer as *const Annotation)).generation,
            }
        } else {
            let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
            let pointer = Box::into_raw(Box::new(Annotation {
                services: services.clone(),
                id: id.into(),
                generation,
                name: name.map(str::to_owned),
                help: help.map(str::to_owned),
                applied: false,
            }));
            if SetWindowSubclass(hwnd, Some(subclass), SUBCLASS, pointer as usize) == 0 {
                drop(Box::from_raw(pointer));
                anyhow::bail!(
                    "Cannot install accessibility metadata cleanup: {}",
                    std::io::Error::last_os_error()
                );
            }
            Owner {
                pointer: pointer as usize,
                generation,
            }
        }
    };
    write(&services, hwnd, owner, AutomationId_Property_GUID, id)?;
    if let Some(name) = name {
        write(&services, hwnd, owner, Name_Property_GUID, name)?;
        write(&services, hwnd, owner, PROPID_ACC_NAME, name)?;
    } else {
        ensure!(
            current(hwnd, owner),
            "Accessibility HWND was retired during annotation"
        );
        unsafe {
            services.ClearHwndProps(
                ComHwnd(hwnd),
                OBJID_CLIENT as u32,
                CHILDID_SELF,
                &[Name_Property_GUID, PROPID_ACC_NAME],
            )
        }
        .context("ClearHwndProps(Name)")?;
    }
    if let Some(help) = help {
        write(&services, hwnd, owner, HelpText_Property_GUID, help)?;
    } else {
        ensure!(
            current(hwnd, owner),
            "Accessibility HWND was retired during annotation"
        );
        unsafe {
            services.ClearHwndProps(
                ComHwnd(hwnd),
                OBJID_CLIENT as u32,
                CHILDID_SELF,
                &[HelpText_Property_GUID],
            )
        }
        .context("ClearHwndProps(HelpText)")?;
    }
    ensure!(
        current(hwnd, owner),
        "Accessibility HWND was retired during annotation"
    );
    unsafe {
        (*(owner.pointer as *mut Annotation)).applied = true;
    }
    Ok(())
}
/// IDs are invariant keys; Name/HelpText use the current UI language. `None`
/// leaves the native text/value provider in charge rather than freezing text.
pub(super) fn annotate(hwnd: HWND, id: &str, name: Option<&str>, help: Option<&str>) -> bool {
    match apply(hwnd, id, name, help) {
        Ok(()) => true,
        Err(error) => {
            report(id, format_args!("{error:#}"));
            false
        }
    }
}
/// Reads only metadata installed by this module, never another process's HWND.
#[cfg(test)]
fn identity(hwnd: HWND) -> Option<String> {
    unsafe {
        if hwnd.is_null() || GetWindowThreadProcessId(hwnd, null_mut()) != GetCurrentThreadId() {
            return None;
        }
        let mut pointer = 0usize;
        (GetWindowSubclass(hwnd, Some(subclass), SUBCLASS, &mut pointer) != 0 && pointer != 0)
            .then(|| (*(pointer as *const Annotation)).id.clone())
    }
}
unsafe extern "system" fn subclass(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    if message == WM_NCDESTROY && data != 0 {
        unsafe {
            // Detach first: nested default handling cannot observe or free the
            // same allocation a second time. Metadata never survives HWND reuse.
            RemoveWindowSubclass(hwnd, Some(subclass), SUBCLASS);
            let annotation = Box::from_raw(data as *mut Annotation);
            if let Err(error) = annotation.services.ClearHwndProps(
                ComHwnd(hwnd),
                OBJID_CLIENT as u32,
                CHILDID_SELF,
                &PROPERTIES,
            ) {
                report(&annotation.id, error);
            }
            DIAGNOSTICS.with(|slot| slot.borrow_mut().cleaned += 1);
            drop(annotation);
        }
    }
    unsafe { DefSubclassProc(hwnd, message, w, l) }
}

/// Escapes only structural delimiters, keeping keys readable without collisions.
pub(super) fn key(scope: &str, role: &str, value: &str) -> String {
    let escape = |s: &str| s.replace('%', "%25").replace('/', "%2F");
    format!("{}/{}/{}", escape(scope), escape(role), escape(value))
}

#[cfg(test)]
#[derive(Debug)]
pub(super) struct Probe {
    pub name: String,
    pub id: String,
    pub help: String,
    pub control_type: i32,
    pub enabled: bool,
    pub offscreen: bool,
    pub value: Option<String>,
    pub read_only: Option<bool>,
    pub toggle: Option<i32>,
    pub invoke: bool,
}
/// Formal UIA reads of exact HWNDs created by the calling test. The UI thread
/// keeps pumping while an independent, windowless MTA client obtains properties.
/// It never activates windows, injects input, or invokes application commands.
#[cfg(test)]
pub(super) fn read_for_test(handles: &[HWND]) -> Result<Vec<Probe>> {
    use windows::{
        Win32::{System::Com::COINIT_MULTITHREADED, UI::Accessibility::*},
        core::Interface,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    let handles: Vec<usize> = handles.iter().map(|handle| *handle as usize).collect();
    let reader = std::thread::spawn(move || -> Result<Vec<Probe>> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        struct Mta;
        impl Drop for Mta {
            fn drop(&mut self) {
                unsafe { CoUninitialize() };
            }
        }
        let _apartment = Mta;
        let automation: IUIAutomation2 =
            unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)? };
        unsafe {
            automation.SetConnectionTimeout(2000)?;
            automation.SetTransactionTimeout(2000)?;
        }
        let client: IUIAutomation = automation.cast()?;
        handles
            .into_iter()
            .map(|handle| -> Result<Probe> {
                unsafe {
                    let element = client.ElementFromHandle(ComHwnd(handle as HWND))?;
                    let value = element
                        .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                        .ok();
                    let toggle = element
                        .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                        .ok();
                    Ok(Probe {
                        name: element.CurrentName()?.to_string(),
                        id: element.CurrentAutomationId()?.to_string(),
                        help: element.CurrentHelpText()?.to_string(),
                        control_type: element.CurrentControlType()?.0,
                        enabled: element.CurrentIsEnabled()?.as_bool(),
                        offscreen: element.CurrentIsOffscreen()?.as_bool(),
                        value: value
                            .as_ref()
                            .map(|pattern| pattern.CurrentValue().map(|value| value.to_string()))
                            .transpose()?,
                        read_only: value
                            .as_ref()
                            .map(|pattern| pattern.CurrentIsReadOnly().map(|value| value.as_bool()))
                            .transpose()?,
                        toggle: toggle
                            .map(|pattern| pattern.CurrentToggleState().map(|value| value.0))
                            .transpose()?,
                        invoke: element
                            .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                            .is_ok(),
                    })
                }
            })
            .collect()
    });
    while !reader.is_finished() {
        unsafe {
            let mut message = std::mem::zeroed();
            while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("UIA reader panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn semantic_ids_preserve_field_identity_and_escape_delimiters() {
        assert_eq!(
            key("main.settings.behavior", "field", "thermal.enabled"),
            "main.settings.behavior/field/thermal.enabled"
        );
        assert_ne!(
            key("editor.form", "field", "/path"),
            key("editor.form", "field", "%2Fpath")
        );
    }
    #[test]
    fn invalid_targets_report_a_failure_without_claiming_annotation_success() {
        let before = DIAGNOSTICS.with(|slot| slot.borrow().failures);
        annotate(null_mut(), "test.invalid", Some("Invalid"), None);
        assert_eq!(DIAGNOSTICS.with(|slot| slot.borrow().failures), before + 1);
        assert!(diagnostic().unwrap().contains("HWND is unavailable"));
    }
    #[test]
    fn never_visible_standard_controls_expose_semantics_and_live_native_patterns() {
        use windows_sys::Win32::{
            System::LibraryLoader::GetModuleHandleW,
            UI::{
                Controls::{BST_CHECKED, BST_UNCHECKED},
                Input::KeyboardAndMouse::EnableWindow,
                WindowsAndMessaging::*,
            },
        };
        let _apartment = UiThread::enter().unwrap();
        struct Owned(HWND);
        impl Drop for Owned {
            fn drop(&mut self) {
                unsafe {
                    DestroyWindow(self.0);
                }
            }
        }
        let parent = Owned(unsafe {
            CreateWindowExW(
                0,
                utf16("STATIC").as_ptr(),
                utf16("Never-visible accessibility fixture").as_ptr(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                500,
                400,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null_mut()),
                std::ptr::null(),
            )
        });
        assert!(!parent.0.is_null());
        let child = |class: &str, title: &str, style, id| unsafe {
            CreateWindowExW(
                0,
                utf16(class).as_ptr(),
                utf16(title).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                10,
                10,
                160,
                30,
                parent.0,
                id as usize as _,
                GetModuleHandleW(null_mut()),
                std::ptr::null(),
            )
        };
        let edit = child("EDIT", "70", ES_READONLY as u32, 1000);
        let checkbox = child("BUTTON", "", BS_AUTOCHECKBOX as u32, 1001);
        let button = child("BUTTON", "保存", BS_OWNERDRAW as u32, 1002);
        for hwnd in [edit, checkbox, button] {
            assert!(!hwnd.is_null());
        }
        annotate(parent.0, "test.hidden", None, None);
        annotate(
            edit,
            "editor.preset/field/cpu.pl1",
            Some("CPU 功耗"),
            Some("原生只读值"),
        );
        annotate(
            checkbox,
            "editor.sensors/field/thermal.enabled",
            Some("CPU 温控"),
            None,
        );
        annotate(button, "editor.preset/action/save", None, None);
        unsafe {
            EnableWindow(edit, 0);
            SendMessageW(checkbox, BM_SETCHECK, BST_CHECKED as usize, 0);
        }
        assert_eq!(unsafe { IsWindowVisible(parent.0) }, 0);
        let before = read_for_test(&[edit, checkbox, button]).unwrap();
        assert_eq!(before[0].id, "editor.preset/field/cpu.pl1");
        assert_eq!(before[0].name, "CPU 功耗");
        assert_eq!(before[0].help, "原生只读值");
        assert_eq!(before[0].control_type, 50004);
        assert_eq!(before[0].value.as_deref(), Some("70"));
        assert_eq!(before[0].read_only, Some(true));
        assert!(!before[0].enabled);
        assert_eq!(before[1].control_type, 50002);
        assert_eq!(before[1].toggle, Some(1));
        assert_eq!(before[2].name, "保存");
        assert_eq!(before[2].control_type, 50000);
        assert!(before[2].invoke);
        let property_calls = DIAGNOSTICS.with(|slot| slot.borrow().property_calls);
        assert!(annotate(
            edit,
            "editor.preset/field/cpu.pl1",
            Some("CPU 功耗"),
            Some("原生只读值")
        ));
        assert_eq!(
            DIAGNOSTICS.with(|slot| slot.borrow().property_calls),
            property_calls,
            "unchanged layout metadata must not rewrite COM properties"
        );
        assert!(annotate(
            button,
            "editor.preset/action/save",
            Some(""),
            None
        ));
        assert_eq!(read_for_test(&[button]).unwrap()[0].name, "");
        assert!(annotate(button, "editor.preset/action/save", None, None));
        assert_eq!(read_for_test(&[button]).unwrap()[0].name, "保存");
        unsafe {
            SetWindowTextW(edit, utf16("80").as_ptr());
            EnableWindow(edit, 1);
            SendMessageW(checkbox, BM_SETCHECK, BST_UNCHECKED as usize, 0);
        }
        // Locale metadata changes independently of native values and states.
        annotate(
            edit,
            "editor.preset/field/cpu.pl1",
            Some("CPU power"),
            Some("Native read-only value"),
        );
        let after = read_for_test(&[edit, checkbox]).unwrap();
        assert_eq!(after[0].id, before[0].id);
        assert_eq!(after[0].name, "CPU power");
        assert_eq!(after[0].value.as_deref(), Some("80"));
        assert!(after[0].enabled);
        // A hidden parent's default Win32 proxy need not report Offscreen=true.
        // Preserve its native answer rather than overriding it to fit a test.
        assert_eq!(before[0].offscreen, after[0].offscreen);
        assert_eq!(after[1].toggle, Some(0));
        let cleaned = DIAGNOSTICS.with(|slot| slot.borrow().cleaned);
        drop(parent);
        assert_eq!(DIAGNOSTICS.with(|slot| slot.borrow().cleaned), cleaned + 4);
        assert!(identity(edit).is_none());
        assert!(diagnostic().is_none());
    }
}
