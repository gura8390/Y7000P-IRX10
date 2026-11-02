//! Windows-only metadata listeners. A separate metadata worker owns long-lived
//! read-only WMI subscriptions; the GUI/hook pump never calls COM or a setter.
//! Official contracts: WlanRegisterNotification (unregister waits for
//! callbacks), Power Setting GUIDs, GetLastInputInfo and WH_KEYBOARD_LL.
use super::*;
use jiks_platform::firmware_events::{FirmwareEventKind, FirmwareSubscription};
use std::{
    cell::RefCell,
    mem::{offset_of, size_of},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
};
use windows_sys::{
    Win32::{
        Foundation::{
            CloseHandle, GetLastError, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, WPARAM,
        },
        NetworkManagement::WiFi::*,
        System::{
            Diagnostics::ToolHelp::*,
            LibraryLoader::GetModuleHandleW,
            Power::{
                EFFECTIVE_POWER_MODE, EFFECTIVE_POWER_MODE_V2, EffectivePowerModeGameMode,
                HPOWERNOTIFY, POWERBROADCAST_SETTING,
                PowerRegisterForEffectivePowerModeNotifications,
                PowerUnregisterFromEffectivePowerModeNotifications,
                RegisterPowerSettingNotification, UnregisterPowerSettingNotification,
            },
            Registry::*,
            RemoteDesktop::{WTSRegisterSessionNotification, WTSUnRegisterSessionNotification},
            SystemInformation::{GetLocalTime, GetSystemTime, GetTickCount64},
            Threading::{
                GetCurrentThreadId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
                QueryFullProcessImageNameW,
            },
        },
        UI::{
            Input::KeyboardAndMouse::{GetKeyState, GetLastInputInfo, LASTINPUTINFO},
            WindowsAndMessaging::*,
        },
    },
    core::GUID,
};

const STOP: u32 = WM_APP + 0x319;
const CHECK_LOCKS: u32 = WM_APP + 0x31a;
const QUEUE_CAPACITY: usize = 256;
const GUID_AC: GUID = GUID::from_u128(0x5d3e9a59_e9d5_4b00_a6bd_ff34ff516548);
const GUID_BATTERY: GUID = GUID::from_u128(0xa7ad8041_b45a_4cae_87a3_eecbb468a9e1);
const GUID_LID: GUID = GUID::from_u128(0xba3e0f4d_b817_4094_a2d1_d56379e6a0f3);
const GUID_DISPLAY: GUID = GUID::from_u128(0x2b84c20e_ad23_4ddf_93db_05ffbd7efca5);

#[derive(Clone)]
struct Sink {
    sender: SyncSender<SourceMessage>,
    stop: Arc<AtomicBool>,
    overflow: Arc<AtomicBool>,
    admission_ready: Arc<AtomicBool>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}
impl Sink {
    fn emit(&self, event: SourceMessage) -> bool {
        if self.stop.load(Ordering::Acquire) {
            return false;
        }
        match self.sender.try_send(event) {
            Ok(()) => self.notify(),
            Err(_) => {
                self.overflow.store(true, Ordering::Release);
                self.stop.store(true, Ordering::Release);
                false
            }
        }
    }
    fn notify(&self) -> bool {
        if let Some(wake) = &self.wake {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wake())).is_err() {
                self.overflow.store(true, Ordering::Release);
                self.stop.store(true, Ordering::Release);
                return false;
            }
        }
        true
    }
    fn terminal(&self, event: SourceMessage) {
        let _ = self.sender.try_send(event);
        self.notify();
    }
}
struct LocalSink {
    sink: Sink,
    options: SourceOptions,
    filter: ShortcutFilter,
    suspended: Arc<AtomicBool>,
    window: HWND,
}
thread_local! {static LOCAL:RefCell<Option<LocalSink>>=const{RefCell::new(None)};}

/// Construction is inert. start only spawns; Started is emitted after all
/// requested registrations and the optional shortcut hook actually succeed.
/// stop is nonblocking. try_finish joins only an already-ended thread and is
/// the physical unregister/unhook completion acknowledgement.
#[derive(Default)]
pub struct NativeEventSource {
    receiver: Option<Receiver<SourceMessage>>,
    stop: Arc<AtomicBool>,
    thread_id: Arc<AtomicU32>,
    suspended: Arc<AtomicBool>,
    join: Option<JoinHandle<Result<()>>>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    cleanup_verified: Arc<AtomicBool>,
    metadata_done: Arc<AtomicBool>,
}
impl NativeEventSource {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn is_running(&self) -> bool {
        self.join.is_some()
    }
    pub fn cleanup_confirmed(&self) -> bool {
        self.cleanup_verified.load(Ordering::Acquire) && self.metadata_done.load(Ordering::Acquire)
    }
    /// Install before start. The callback must only wake the coordinator (e.g.
    /// request repaint or try_send), never block or run automation/native IO.
    pub fn set_waker(&mut self, wake: Option<Arc<dyn Fn() + Send + Sync>>) -> Result<()> {
        ensure!(
            self.join.is_none(),
            "Cannot replace a running source wake target"
        );
        self.wake = wake;
        Ok(())
    }
    pub fn start(&mut self, options: SourceOptions) -> Result<()> {
        ensure!(
            self.join.is_none(),
            "Previous event source has not acknowledged shutdown"
        );
        options.validate()?;
        if options.is_empty() {
            return Ok(());
        }
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let overflow = Arc::new(AtomicBool::new(false));
        let sink = Sink {
            sender,
            stop: stop.clone(),
            overflow,
            admission_ready: Arc::new(AtomicBool::new(false)),
            wake: self.wake.clone(),
        };
        let id = self.thread_id.clone();
        let suspended = self.suspended.clone();
        let cleanup_verified = Arc::new(AtomicBool::new(false));
        let cleanup = cleanup_verified.clone();
        let metadata_done = Arc::new(AtomicBool::new(true));
        let metadata = metadata_done.clone();
        let join = thread::Builder::new()
            .name("jiks-subscribed-events".into())
            .spawn(move || {
                let result = run_worker(options, &sink, &id, suspended, cleanup, metadata);
                if let Err(error) = &result {
                    sink.terminal(SourceMessage::Fault {
                        source: "native-events",
                        detail: format!("{error:#}"),
                    });
                }
                sink.terminal(SourceMessage::Stopped);
                id.store(0, Ordering::Release);
                result
            })?;
        self.receiver = Some(receiver);
        self.stop = stop;
        self.join = Some(join);
        self.cleanup_verified = cleanup_verified;
        self.metadata_done = metadata_done;
        Ok(())
    }
    pub fn try_recv(&self) -> Option<SourceMessage> {
        self.receiver.as_ref()?.try_recv().ok()
    }
    pub fn set_macro_suspended(&self, value: bool) {
        self.suspended.store(value, Ordering::Release);
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        let id = self.thread_id.load(Ordering::Acquire);
        if id != 0 {
            unsafe { PostThreadMessageW(id, STOP, 0, 0) };
        }
    }
    pub fn try_finish(&mut self) -> Option<Result<()>> {
        if !self.join.as_ref()?.is_finished() {
            return None;
        }
        Some(
            self.join
                .take()
                .expect("finished handle")
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Native event worker panicked"))),
        )
    }
}
impl Drop for NativeEventSource {
    fn drop(&mut self) {
        self.stop(); /* Worker owns every registration until actual cleanup. */
    }
}

unsafe extern "system" fn shortcut_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 || data == 0 {
        return unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) };
    }
    let key = unsafe { ptr::read_unaligned(data as *const KBDLLHOOKSTRUCT) };
    let down = matches!(message as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    let up = matches!(message as u32, WM_KEYUP | WM_SYSKEYUP);
    if !down && !up {
        return unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) };
    }
    let swallowed = LOCAL.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(local) = slot.as_mut() else {
            return false;
        };
        // Match the original NativeWindowsMessageListener: defer GetKeyState
        // until after physical key-up, rather than guessing inside the hook.
        if local.options.lock_notifications
            && local.sink.admission_ready.load(Ordering::Acquire)
            && is_lock_release(key.vkCode, up, key.flags)
            && !local.window.is_null()
        {
            if unsafe { PostMessageW(local.window, CHECK_LOCKS, 0, 0) } == 0 {
                local.sink.emit(SourceMessage::Fault {
                    source: "lock-notifications",
                    detail: "Lock-state metadata post failed".into(),
                });
            }
        }
        let decision = local.filter.inspect(
            key.vkCode,
            down,
            key.flags,
            local.options.macro_enabled && local.sink.admission_ready.load(Ordering::Acquire),
            &local.options.macro_keys,
            local.suspended.load(Ordering::Acquire),
        );
        match decision {
            ShortcutDecision::Pass => false,
            ShortcutDecision::Swallow => {
                local.filter.accepted(key.vkCode, down);
                true
            }
            ShortcutDecision::Fire => {
                if local.sink.emit(SourceMessage::MacroHotkey {
                    source: "Keyboard".into(),
                    key: u64::from(key.vkCode),
                }) {
                    local.filter.accepted(key.vkCode, true);
                    true
                } else {
                    false
                }
            }
        }
    });
    if swallowed {
        1
    } else {
        unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) }
    }
}
fn emit(event: SourceMessage) {
    LOCAL.with(|slot| {
        if let Some(local) = slot.borrow().as_ref() {
            local.sink.emit(event);
        }
    });
}
fn is_lock_release(key: u32, up: bool, flags: u32) -> bool {
    up && flags & 0x12 == 0 && matches!(key, 0x14 | 0x90)
}
fn emit_lock_state() {
    let enabled = LOCAL.with(|slot| {
        slot.borrow().as_ref().is_some_and(|local| {
            local.options.lock_notifications && local.sink.admission_ready.load(Ordering::Acquire)
        })
    });
    if enabled {
        emit(SourceMessage::LockState {
            caps: unsafe { GetKeyState(0x14) } & 1 != 0,
            num: unsafe { GetKeyState(0x90) } & 1 != 0,
        });
    }
}
fn same_guid(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let needs = LOCAL
        .with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|local| local.options.needs.clone())
        })
        .unwrap_or_default();
    match message {
        CHECK_LOCKS => emit_lock_state(),
        WM_WTSSESSION_CHANGE if needs.session => {
            if matches!(w as u32, 7 | 8) {
                emit(SourceMessage::SessionLocked(w as u32 == 7));
            }
        }
        WM_DISPLAYCHANGE if needs.display => emit(SourceMessage::Refresh(RefreshReason::Display)),
        WM_POWERBROADCAST if needs.power || needs.lid || needs.display => {
            if w as u32 == PBT_APMRESUMEAUTOMATIC && needs.power {
                emit(SourceMessage::Refresh(RefreshReason::Power {
                    resume: true,
                }));
            } else if w as u32 == PBT_APMPOWERSTATUSCHANGE && needs.power {
                emit(SourceMessage::Refresh(RefreshReason::Power {
                    resume: false,
                }));
            } else if w as u32 == PBT_POWERSETTINGCHANGE && l != 0 {
                let setting = unsafe { ptr::read_unaligned(l as *const POWERBROADCAST_SETTING) };
                if setting.DataLength == 4 {
                    let value = unsafe {
                        ptr::read_unaligned(
                            (l as *const u8).add(offset_of!(POWERBROADCAST_SETTING, Data))
                                as *const u32,
                        )
                    };
                    if same_guid(&setting.PowerSetting, &GUID_LID) && value <= 1 {
                        emit(SourceMessage::Lid(value == 1));
                    } else if same_guid(&setting.PowerSetting, &GUID_DISPLAY) && value <= 2 {
                        emit(SourceMessage::Display(value != 0));
                    } else if same_guid(&setting.PowerSetting, &GUID_AC) {
                        emit(SourceMessage::Refresh(RefreshReason::Power {
                            resume: false,
                        }));
                    } else if same_guid(&setting.PowerSetting, &GUID_BATTERY) && value <= 100 {
                        emit(SourceMessage::BatteryPercentage(value as i32));
                    }
                }
            }
            return 1;
        }
        WM_DEVICECHANGE if needs.devices || needs.display => {
            if matches!(w as u32, DBT_DEVICEARRIVAL | DBT_DEVICEREMOVECOMPLETE) && l != 0 {
                let header = unsafe { ptr::read_unaligned(l as *const DEV_BROADCAST_HDR) };
                let offset = offset_of!(DEV_BROADCAST_DEVICEINTERFACE_W, dbcc_name);
                if header.dbch_devicetype == DBT_DEVTYP_DEVICEINTERFACE
                    && (header.dbch_size as usize) >= offset + 2
                    && header.dbch_size <= 8192
                {
                    let count = (header.dbch_size as usize - offset) / 2;
                    let start = unsafe { (l as *const u8).add(offset) as *const u16 };
                    let units: Vec<u16> = (0..count)
                        .map(|i| unsafe { ptr::read_unaligned(start.add(i)) })
                        .take_while(|v| *v != 0)
                        .collect();
                    if let Some(instance_id) = device_instance_id(&String::from_utf16_lossy(&units))
                    {
                        emit(SourceMessage::Device {
                            connected: w as u32 == DBT_DEVICEARRIVAL,
                            instance_id,
                        });
                    }
                }
                emit(SourceMessage::Refresh(RefreshReason::Devices));
                emit(SourceMessage::Refresh(RefreshReason::Display));
            } else if w as u32 == DBT_DEVNODES_CHANGED {
                emit(SourceMessage::Refresh(RefreshReason::Devices));
                emit(SourceMessage::Refresh(RefreshReason::Display));
            }
        }
        _ => {}
    }
    unsafe { DefWindowProcW(window, message, w, l) }
}

pub fn decode_wifi_notification(code: u32, reason: u32, ssid: &[u8]) -> Option<SourceMessage> {
    match code as i32 {
        value
            if value == wlan_notification_acm_connection_complete
                && reason == 0
                && ssid.len() <= 32 =>
        {
            Some(SourceMessage::Wifi {
                connected: true,
                ssid: Some(String::from_utf8_lossy(ssid).into_owned()),
            })
        }
        value if value == wlan_notification_acm_disconnected => Some(SourceMessage::Wifi {
            connected: false,
            ssid: None,
        }),
        _ => None,
    }
}
unsafe extern "system" fn wifi_callback(
    data: *mut L2_NOTIFICATION_DATA,
    context: *mut std::ffi::c_void,
) {
    if data.is_null() || context.is_null() {
        return;
    }
    let data = unsafe { ptr::read_unaligned(data) };
    if data.NotificationSource != WLAN_NOTIFICATION_SOURCE_ACM {
        return;
    }
    let sink = unsafe { &*(context as *const Sink) };
    let event = if data.NotificationCode == wlan_notification_acm_disconnected as u32 {
        decode_wifi_notification(data.NotificationCode, 0, &[])
    } else if data.NotificationCode == wlan_notification_acm_connection_complete as u32
        && !data.pData.is_null()
        && data.dwDataSize as usize >= size_of::<WLAN_CONNECTION_NOTIFICATION_DATA>()
    {
        let connection =
            unsafe { ptr::read_unaligned(data.pData as *const WLAN_CONNECTION_NOTIFICATION_DATA) };
        let length = connection.dot11Ssid.uSSIDLength as usize;
        if length <= 32 {
            decode_wifi_notification(
                data.NotificationCode,
                connection.wlanReasonCode,
                &connection.dot11Ssid.ucSSID[..length],
            )
        } else {
            None
        }
    } else {
        None
    };
    if let Some(event) = event {
        sink.emit(event);
    }
}

unsafe extern "system" fn effective_mode_callback(
    mode: EFFECTIVE_POWER_MODE,
    context: *const std::ffi::c_void,
) {
    if context.is_null() {
        return;
    }
    let sink = unsafe { &*(context as *const Sink) };
    if sink.stop.load(Ordering::Acquire) {
        return;
    }
    let active = mode == EffectivePowerModeGameMode;
    // Only a real effective Game Mode notification can associate the foreground
    // process with this evidence. No title/content, keyboard or process memory
    // is captured, and no automation executes in this callback.
    let mut pid = 0;
    if active {
        let window = unsafe { GetForegroundWindow() };
        if !window.is_null() {
            unsafe { GetWindowThreadProcessId(window, &mut pid) };
        }
    }
    sink.emit(SourceMessage::EffectiveGameMode {
        active,
        foreground_pid: (pid != 0).then_some(pid),
    });
}

#[derive(Default)]
struct Registrations {
    window: HWND,
    device: HDEVNOTIFY,
    power: Vec<HPOWERNOTIFY>,
    session: bool,
    hook: HHOOK,
    wlan: HANDLE,
    wifi_sink: Option<Box<Sink>>,
    effective: *mut std::ffi::c_void,
    effective_sink: Option<Box<Sink>>,
    cleanup_verified: Arc<AtomicBool>,
    cleanup_failed: bool,
}
impl Registrations {
    fn cleanup(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        unsafe {
            if !self.hook.is_null() {
                if UnhookWindowsHookEx(self.hook) == 0 {
                    errors.push("unhook");
                }
                self.hook = ptr::null_mut();
            }
            if !self.wlan.is_null() {
                // Unregister outside callbacks and without sharing their lock.
                // It waits for active callbacks before their context is freed.
                let unregistered = WlanRegisterNotification(
                    self.wlan,
                    WLAN_NOTIFICATION_SOURCE_NONE,
                    1,
                    None,
                    ptr::null(),
                    ptr::null(),
                    ptr::null_mut(),
                ) == 0;
                let closed = WlanCloseHandle(self.wlan, ptr::null()) == 0;
                if !unregistered && !closed {
                    errors.push("WLAN unregister/close");
                    // A still-registered native callback must never see freed
                    // context. Latch cleanup failure and retain its small sink.
                    if let Some(sink) = self.wifi_sink.take() {
                        Box::leak(sink);
                    }
                }
                self.wlan = ptr::null_mut();
                self.wifi_sink = None;
            }
            if !self.effective.is_null() {
                // Official API waits for all callbacks before returning.
                if PowerUnregisterFromEffectivePowerModeNotifications(self.effective) != 0 {
                    errors.push("effective mode unregister");
                    if let Some(sink) = self.effective_sink.take() {
                        Box::leak(sink);
                    }
                }
                self.effective = ptr::null_mut();
                self.effective_sink = None;
            }
            if self.session {
                if WTSUnRegisterSessionNotification(self.window) == 0 {
                    errors.push("WTS unregister");
                }
                self.session = false;
            }
            for power in self.power.drain(..) {
                if UnregisterPowerSettingNotification(power) == 0 {
                    errors.push("power unregister");
                }
            }
            if !self.device.is_null() {
                if UnregisterDeviceNotification(self.device) == 0 {
                    errors.push("device unregister");
                }
                self.device = ptr::null_mut();
            }
            if !self.window.is_null() {
                KillTimer(self.window, 1);
                if DestroyWindow(self.window) == 0 {
                    errors.push("window destroy");
                }
                self.window = ptr::null_mut();
            }
        }
        LOCAL.with(|slot| *slot.borrow_mut() = None);
        self.cleanup_failed |= !errors.is_empty();
        self.cleanup_verified
            .store(!self.cleanup_failed, Ordering::Release);
        ensure!(
            errors.is_empty(),
            "Native listener cleanup failed: {}",
            errors.join(", ")
        );
        Ok(())
    }
}
impl Drop for Registrations {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn register(
    options: &SourceOptions,
    sink: &Sink,
    cleanup_verified: Arc<AtomicBool>,
) -> Result<Registrations> {
    let mut owned = Registrations::default();
    owned.cleanup_verified = cleanup_verified;
    let module = unsafe { GetModuleHandleW(ptr::null()) };
    ensure!(!module.is_null(), "Listener module absent");
    let class: Vec<u16> = "JiksSubscribedMetadataEvents\0".encode_utf16().collect();
    let wc = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: module,
        lpszClassName: class.as_ptr(),
        ..Default::default()
    };
    ensure!(
        unsafe { RegisterClassW(&wc) } != 0 || unsafe { GetLastError() } == 1410,
        "Listener window class registration failed"
    );
    // Hidden top-level tool window (never ShowWindow): message-only windows do
    // not receive broadcasts such as WM_DISPLAYCHANGE. Explicit registrations
    // still deliver lid/session/device notifications to this owned window.
    owned.window = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            class.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            module,
            ptr::null(),
        )
    };
    ensure!(!owned.window.is_null(), "Listener window creation failed");
    if options.needs.session {
        ensure!(
            unsafe { WTSRegisterSessionNotification(owned.window, 0) } != 0,
            "Session notifications failed"
        );
        owned.session = true;
    }
    let mut guids = Vec::new();
    if options.needs.power {
        guids.extend([GUID_AC, GUID_BATTERY]);
    }
    if options.needs.lid {
        guids.push(GUID_LID);
    }
    if options.needs.display {
        guids.push(GUID_DISPLAY);
    }
    for guid in guids {
        let handle = unsafe { RegisterPowerSettingNotification(owned.window, &guid, 0) };
        ensure!(handle != 0, "Power notification registration failed");
        owned.power.push(handle);
    }
    if options.needs.devices || options.needs.display {
        let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
            dbcc_size: size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
            dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE,
            ..Default::default()
        };
        owned.device = unsafe {
            RegisterDeviceNotificationW(
                owned.window,
                &filter as *const _ as *const _,
                DEVICE_NOTIFY_ALL_INTERFACE_CLASSES,
            )
        };
        ensure!(
            !owned.device.is_null(),
            "Device notification registration failed"
        );
    }
    if options.has_hook() {
        owned.hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(shortcut_hook), module, 0) };
        ensure!(
            !owned.hook.is_null(),
            "Configured keyboard metadata hook installation failed"
        );
    }
    if options.needs.wifi {
        let mut version = 0;
        ensure!(
            unsafe { WlanOpenHandle(2, ptr::null(), &mut version, &mut owned.wlan) } == 0,
            "WLAN service unavailable"
        );
        owned.wifi_sink = Some(Box::new(sink.clone()));
        let context =
            owned.wifi_sink.as_ref().unwrap().as_ref() as *const Sink as *const std::ffi::c_void;
        ensure!(
            unsafe {
                WlanRegisterNotification(
                    owned.wlan,
                    WLAN_NOTIFICATION_SOURCE_ACM,
                    1,
                    Some(wifi_callback),
                    context,
                    ptr::null(),
                    ptr::null_mut(),
                )
            } == 0,
            "WLAN notification registration failed"
        );
    }
    if options.needs.games && options.game_detection.effective_power_mode {
        owned.effective_sink = Some(Box::new(sink.clone()));
        let context = owned.effective_sink.as_ref().unwrap().as_ref() as *const Sink
            as *const std::ffi::c_void;
        ensure!(
            unsafe {
                PowerRegisterForEffectivePowerModeNotifications(
                    EFFECTIVE_POWER_MODE_V2,
                    Some(effective_mode_callback),
                    context,
                    &mut owned.effective,
                )
            } == 0,
            "Effective Game Mode notification registration failed"
        );
    }
    ensure!(
        unsafe { SetTimer(owned.window, 1, 1000, None) } != 0,
        "Listener timer registration failed"
    );
    Ok(owned)
}

fn process_snapshot() -> Result<Vec<ProcessMetadata>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    ensure!(snapshot != INVALID_HANDLE_VALUE, "Process snapshot failed");
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
    let _snapshot = Handle(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    ensure!(
        unsafe { Process32FirstW(snapshot, &mut entry) } != 0,
        "Process snapshot first entry failed"
    );
    let mut result = Vec::new();
    loop {
        ensure!(
            result.len() < 8192,
            "Process metadata snapshot capacity exceeded"
        );
        let length = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        let file = String::from_utf16_lossy(&entry.szExeFile[..length]);
        let name = file
            .strip_suffix(".exe")
            .or_else(|| file.strip_suffix(".EXE"))
            .unwrap_or(&file)
            .to_owned();
        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID) };
        let executable_path = if !handle.is_null() {
            let _process = Handle(handle);
            let mut path = vec![0u16; 32768];
            let mut length = path.len() as u32;
            (unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut length) } != 0)
                .then(|| String::from_utf16_lossy(&path[..length as usize]))
        } else {
            None
        };
        result.push(ProcessMetadata {
            pid: entry.th32ProcessID,
            name,
            executable_path,
        });
        if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
            ensure!(
                unsafe { GetLastError() } == 18,
                "Process snapshot enumeration incomplete"
            );
            break;
        }
    }
    Ok(result)
}

pub fn idle_elapsed_ms(now: u32, last_input: u32, subscribed_at: u32) -> u64 {
    let since_input = now.wrapping_sub(last_input);
    let since_start = now.wrapping_sub(subscribed_at);
    // A future/non-monotonic injected tick is not days of user inactivity.
    if since_input > i32::MAX as u32 {
        0
    } else {
        u64::from(since_input.min(since_start))
    }
}
fn poll_metadata(
    options: &SourceOptions,
    sink: &Sink,
    started: u64,
    last_firmware: &mut Option<u64>,
) -> Result<()> {
    if options.needs.clock {
        let mut utc = Default::default();
        let mut local = Default::default();
        unsafe {
            GetSystemTime(&mut utc);
            GetLocalTime(&mut local)
        };
        sink.emit(SourceMessage::Clock {
            utc_time: TimeOfDay {
                hour: utc.wHour as u8,
                minute: utc.wMinute as u8,
                second: utc.wSecond as u8,
            },
            utc_day: utc.wDayOfWeek as u8,
            local_day_seconds: f64::from(local.wHour) * 3600.
                + f64::from(local.wMinute) * 60.
                + f64::from(local.wSecond),
        });
    }
    if options.needs.idle {
        let mut info = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetLastInputInfo(&mut info) } != 0 {
            sink.emit(SourceMessage::Inactivity {
                milliseconds: idle_elapsed_ms(
                    unsafe { GetTickCount64() } as u32,
                    info.dwTime,
                    started as u32,
                ),
            });
        } else {
            sink.emit(SourceMessage::Fault {
                source: "idle",
                detail: "GetLastInputInfo failed".into(),
            });
        }
    }
    if options.needs.processes {
        match process_snapshot() {
            Ok(snapshot) => {
                sink.emit(SourceMessage::Processes(snapshot));
            }
            Err(error) => {
                sink.emit(SourceMessage::Fault {
                    source: "process",
                    detail: format!("{error:#}"),
                });
            }
        }
    }
    let now = unsafe { GetTickCount64() };
    if options.needs.firmware && last_firmware.is_none_or(|last| now.saturating_sub(last) >= 2000) {
        sink.emit(SourceMessage::Refresh(RefreshReason::Firmware));
        *last_firmware = Some(now);
    }
    Ok(())
}

pub fn normalize_game_path(
    raw: &str,
    mut environment: impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    let raw = raw.trim().trim_matches('"');
    if raw.is_empty() || raw.contains('\0') || raw.len() > 32768 {
        return None;
    }
    let mut result = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find('%') {
        result.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let Some(end) = tail.find('%') else {
            result.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let key = &tail[..end];
        if let Some(value) = environment(key) {
            result.push_str(&value);
        } else {
            result.push('%');
            result.push_str(key);
            result.push('%');
        }
        rest = &tail[end + 1..];
        if result.len() > 32768 {
            return None;
        }
    }
    result.push_str(rest);
    (!result.trim().is_empty() && !result.contains('\0') && result.len() <= 32768).then_some(result)
}

fn game_config_paths() -> Result<BTreeSet<String>> {
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }
    let mut root = ptr::null_mut();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide("System\\GameConfigStore\\Children").as_ptr(),
            0,
            KEY_READ,
            &mut root,
        )
    };
    if status == 2 {
        return Ok(BTreeSet::new());
    }
    ensure!(status == 0, "GameConfigStore read failed: {status}");
    let _root = Key(root);
    let mut paths = BTreeSet::new();
    for index in 0..=8192 {
        let mut name = [0u16; 256];
        let mut length = name.len() as u32;
        let status = unsafe {
            RegEnumKeyExW(
                root,
                index,
                name.as_mut_ptr(),
                &mut length,
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status == 259 {
            return Ok(paths);
        }
        ensure!(
            index < 8192 && status == 0,
            "GameConfigStore enumeration failed/capacity: {status}"
        );
        name[length as usize] = 0;
        let mut key = ptr::null_mut();
        let status = unsafe { RegOpenKeyExW(root, name.as_ptr(), 0, KEY_QUERY_VALUE, &mut key) };
        ensure!(status == 0, "GameConfigStore child read failed: {status}");
        let _key = Key(key);
        let mut bytes = 0;
        let value_name = wide("MatchedExeFullPath");
        let status = unsafe {
            RegGetValueW(
                key,
                ptr::null(),
                value_name.as_ptr(),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut bytes,
            )
        };
        if status == 2 {
            continue;
        }
        ensure!(
            status == 0 && bytes <= 65536 && bytes % 2 == 0,
            "GameConfigStore string size/type invalid: {status}"
        );
        let mut value = vec![0u16; bytes as usize / 2];
        let status = unsafe {
            RegGetValueW(
                key,
                ptr::null(),
                value_name.as_ptr(),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
                ptr::null_mut(),
                value.as_mut_ptr() as *mut _,
                &mut bytes,
            )
        };
        ensure!(status == 0, "GameConfigStore string read failed: {status}");
        let length = value.iter().position(|v| *v == 0).unwrap_or(value.len());
        if let Some(path) =
            normalize_game_path(&String::from_utf16_lossy(&value[..length]), |name| {
                std::env::var(name).ok()
            })
        {
            let path = std::path::absolute(&path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(path);
            paths.insert(path);
        }
    }
    unreachable!()
}

fn metadata_worker(options: SourceOptions, sink: Sink) -> Result<()> {
    let mut subscriptions = Vec::new();
    let mut kinds = Vec::new();
    if options.needs.firmware || options.normal_notifications {
        kinds.extend([
            FirmwareEventKind::SmartFanMode,
            FirmwareEventKind::ThermalMode,
        ]);
    }
    if options.needs.display {
        kinds.push(FirmwareEventKind::Brightness);
    }
    if options.normal_notifications {
        kinds.extend([
            FirmwareEventKind::Lighting,
            FirmwareEventKind::KeyboardProfile,
            FirmwareEventKind::KeyLock,
        ]);
    }
    for kind in kinds {
        if sink.stop.load(Ordering::Acquire) {
            break;
        }
        match FirmwareSubscription::open(kind) {
            Ok(subscription) => subscriptions.push(Some(subscription)),
            Err(error) => {
                sink.emit(SourceMessage::Fault {
                    source: "firmware-subscription",
                    detail: format!("{kind:?}: {error:#}"),
                });
            }
        }
    }
    let started = unsafe { GetTickCount64() };
    let mut last_firmware = None;
    let mut last_tick = None;
    let mut last_games = None;
    while !sink.stop.load(Ordering::Acquire) {
        let now = unsafe { GetTickCount64() };
        if last_tick.is_none_or(|last| now.saturating_sub(last) >= 1000) {
            poll_metadata(&options, &sink, started, &mut last_firmware)?;
            last_tick = Some(now);
        }
        if options.needs.games
            && options.game_detection.config_store
            && last_games.is_none_or(|last| now.saturating_sub(last) >= 15000)
        {
            match game_config_paths() {
                Ok(paths) => {
                    sink.emit(SourceMessage::GameConfigPaths(paths));
                }
                Err(error) => {
                    sink.emit(SourceMessage::Fault {
                        source: "game-config-store",
                        detail: format!("{error:#}"),
                    });
                }
            }
            last_games = Some(now);
        }
        for slot in &mut subscriptions {
            let Some(subscription) = slot.as_mut() else {
                continue;
            };
            let mut failed = false;
            for index in 0..=32 {
                match subscription.poll(0) {
                    Ok(Some(event)) => {
                        ensure!(
                            index < 32,
                            "Firmware event provider backlog exceeded bounded drain"
                        );
                        let reason = if event.kind == FirmwareEventKind::Brightness {
                            RefreshReason::Display
                        } else if options.normal_notifications {
                            RefreshReason::NotificationState
                        } else {
                            RefreshReason::Firmware
                        };
                        sink.emit(SourceMessage::Refresh(reason));
                    }
                    Ok(None) => break,
                    Err(error) => {
                        sink.emit(SourceMessage::Fault {
                            source: "firmware-subscription",
                            detail: format!("{error:#}"),
                        });
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                *slot = None;
            }
        }
        thread::park_timeout(std::time::Duration::from_millis(100));
    }
    Ok(())
}
fn run_worker(
    options: SourceOptions,
    sink: &Sink,
    id: &AtomicU32,
    suspended: Arc<AtomicBool>,
    cleanup_verified: Arc<AtomicBool>,
    metadata_done: Arc<AtomicBool>,
) -> Result<()> {
    let mut message = MSG::default();
    unsafe { PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_NOREMOVE) };
    id.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
    if sink.stop.load(Ordering::Acquire) {
        cleanup_verified.store(true, Ordering::Release);
        return Ok(());
    }
    LOCAL.with(|slot| {
        *slot.borrow_mut() = Some(LocalSink {
            sink: sink.clone(),
            options: options.clone(),
            filter: ShortcutFilter::default(),
            suspended,
            window: ptr::null_mut(),
        })
    });
    let mut registrations = register(&options, sink, cleanup_verified)?;
    LOCAL.with(|slot| {
        if let Some(local) = slot.borrow_mut().as_mut() {
            local.window = registrations.window;
        }
    });
    if sink.stop.load(Ordering::Acquire) {
        return registrations.cleanup();
    }
    ensure!(
        sink.emit(SourceMessage::Started),
        "Event listener startup queue unavailable"
    );
    sink.admission_ready.store(true, Ordering::Release);
    emit_lock_state();
    // Blocking provider/process/registry reads cannot starve WH_KEYBOARD_LL's
    // message pump. Its sibling metadata thread owns its COM subscriptions.
    let metadata = if !options.needs.is_empty() || options.normal_notifications {
        let options = options.clone();
        let sink = sink.clone();
        let parent_id = id.load(Ordering::Acquire);
        let done = metadata_done.clone();
        metadata_done.store(false, Ordering::Release);
        let spawned = thread::Builder::new()
            .name("jiks-event-metadata".into())
            .spawn(move || {
                struct Done(Arc<AtomicBool>);
                impl Drop for Done {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::Release);
                    }
                }
                let _done = Done(done);
                let result = metadata_worker(options, sink.clone());
                if let Err(error) = &result {
                    sink.emit(SourceMessage::Fault {
                        source: "event-metadata",
                        detail: format!("{error:#}"),
                    });
                    sink.stop.store(true, Ordering::Release);
                    unsafe { PostThreadMessageW(parent_id, STOP, 0, 0) };
                }
                result
            });
        match spawned {
            Ok(join) => Some(join),
            Err(error) => {
                metadata_done.store(true, Ordering::Release);
                return Err(error.into());
            }
        }
    } else {
        None
    };
    let result = (|| -> Result<()> {
        while !sink.stop.load(Ordering::Acquire) {
            let received = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
            ensure!(received >= 0, "Native event message pump failed");
            if received == 0 || message.message == STOP {
                break;
            }
            if message.message != WM_TIMER {
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message)
                };
            }
        }
        ensure!(
            !sink.overflow.load(Ordering::Acquire),
            "Native event channel overflowed/disconnected"
        );
        Ok(())
    })();
    let cleanup = registrations.cleanup();
    sink.stop.store(true, Ordering::Release);
    let metadata_result = metadata.map_or(Ok(()), |join| {
        join.thread().unpark();
        join.join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("Event metadata worker panicked")))
    });
    result.and(cleanup).and(metadata_result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lock_metadata_accepts_only_physical_lock_releases_and_defaults_install_no_hook() {
        assert!(is_lock_release(0x14, true, 0));
        assert!(is_lock_release(0x90, true, 0));
        for (key, up, flags) in [
            (0x14, false, 0),
            (0x90, true, 0x10),
            (0x14, true, 2),
            (0x41, true, 0),
        ] {
            assert!(!is_lock_release(key, up, flags));
        }
        let mut options = SourceOptions::default();
        assert!(!options.has_hook());
        assert!(options.is_empty());
        options.lock_notifications = true;
        assert!(options.has_hook());
        assert!(!options.is_empty());
    }
    #[test]
    fn wlan_codec_rejects_failed_connect_overlong_ssid_and_unrelated_messages() {
        assert!(decode_wifi_notification(10, 1, b"fixture").is_none());
        assert!(decode_wifi_notification(10, 0, &[0; 33]).is_none());
        assert!(decode_wifi_notification(1, 0, b"fixture").is_none());
        assert_eq!(
            decode_wifi_notification(10, 0, "测试".as_bytes()),
            Some(SourceMessage::Wifi {
                connected: true,
                ssid: Some("测试".into())
            })
        );
        assert_eq!(
            decode_wifi_notification(21, 1, &[]),
            Some(SourceMessage::Wifi {
                connected: false,
                ssid: None
            })
        );
    }
    #[test]
    fn idle_tick_wrap_future_ticks_and_subscription_baseline_are_safe() {
        assert_eq!(idle_elapsed_ms(100, 0, 100), 0);
        assert_eq!(idle_elapsed_ms(11000, 0, 100), 10900);
        assert_eq!(idle_elapsed_ms(20, u32::MAX - 10, u32::MAX - 100), 31);
        assert_eq!(idle_elapsed_ms(100, 110, 0), 0);
    }
    #[test]
    fn callback_queue_is_bounded_and_failure_is_latched_not_thrown() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let sink = Sink {
            sender,
            stop: Arc::new(AtomicBool::new(false)),
            overflow: Arc::new(AtomicBool::new(false)),
            admission_ready: Arc::new(AtomicBool::new(false)),
            wake: None,
        };
        assert!(sink.emit(SourceMessage::Started));
        assert!(!sink.emit(SourceMessage::Stopped));
        assert!(sink.overflow.load(Ordering::Acquire));
        assert!(sink.stop.load(Ordering::Acquire));
        assert_eq!(receiver.try_recv().unwrap(), SourceMessage::Started);
    }
    #[test]
    fn game_config_string_expansion_is_pure_and_unknown_tokens_are_preserved() {
        assert_eq!(
            normalize_game_path(" \"%FIXTURE%\\game.exe\" ", |name| (name == "FIXTURE")
                .then(|| "D:\\synthetic".into()))
            .as_deref(),
            Some("D:\\synthetic\\game.exe")
        );
        assert_eq!(
            normalize_game_path("%MISSING%\\game.exe", |_| None).as_deref(),
            Some("%MISSING%\\game.exe")
        );
        assert!(normalize_game_path("\"\"", |_| None).is_none());
        assert!(normalize_game_path("bad\0path", |_| None).is_none());
    }
}
