//! Fn metadata and modifier observation. Construction is inert; callbacks only
//! enqueue small events. No callback runs automation, WMI methods or setters.
use anyhow::{Result, bail, ensure};
use jiks_platform::firmware_events::{FirmwareEventKind, FirmwareSubscription};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::*, System::LibraryLoader::GetModuleHandleW, UI::WindowsAndMessaging::*,
};
pub(crate) fn modifier_flags(node:Option<&jiks_core::settings::JsonNode>)->Result<u32>{
    use jiks_core::settings::JsonNode;
    let Some(node)=node.filter(|node|!matches!(node,JsonNode::Null))else{return Ok(0);};
    if let Some(value)=node.as_i64(){ensure!((0..=7).contains(&value),"未知智能Fn修饰键标志");return Ok(value as u32);}
    let text=node.as_str().ok_or_else(||anyhow::anyhow!("智能Fn修饰键格式无效"))?;
    if let Ok(value)=text.parse::<u32>(){ensure!(value<=7,"未知智能Fn修饰键标志");return Ok(value);}
    let mut flags=0;ensure!(!text.trim().is_empty(),"智能Fn修饰键为空");
    for name in text.split(',').map(str::trim){flags|=if name.eq_ignore_ascii_case("None"){0}else if name.eq_ignore_ascii_case("Shift"){1}else if name.eq_ignore_ascii_case("Ctrl"){2}else if name.eq_ignore_ascii_case("Alt"){4}else{bail!("未知智能Fn修饰键名称：{name}")};}
    Ok(flags)
}
#[cfg(test)]mod flags_tests{use super::*;use jiks_core::settings::JsonNode;
#[test]fn legacy_jsonnet_flags_and_all_native_combinations_agree(){for(flags,text)in[(0,"None"),(1,"Shift"),(2,"Ctrl"),(4,"Alt"),(7,"Alt, Ctrl, Shift")]{assert_eq!(modifier_flags(Some(&JsonNode::String(text.into()))).unwrap(),flags);assert_eq!(modifier_flags(Some(&JsonNode::Number(flags.to_string()))).unwrap(),flags);}assert!(modifier_flags(Some(&JsonNode::String("FutureKey".into()))).is_err());assert!(modifier_flags(Some(&JsonNode::Number("8".into()))).is_err());}}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Options {
    pub utility: bool,
    pub modifiers: u32,
    pub escape: bool,
}
impl Options {
    pub fn empty(self) -> bool {
        !self.utility && self.modifiers == 0 && !self.escape
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Started,
    Utility { key: u32, at_ms: u64 },
    Modifiers { mask: u32, at_ms: u64 },
    Escape,
    CleanupPending(String),
}
#[derive(Default)]
pub struct NativeSource {
    join: Option<JoinHandle<Result<()>>>,
    stop: Arc<AtomicBool>,
    retry: Arc<AtomicU64>,
    events: Option<Receiver<Event>>,
}
impl NativeSource {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn start(&mut self, options: Options) -> Result<()> {
        ensure!(
            self.join.is_none(),
            "Previous Fn listener still owns its thread"
        );
        ensure!(
            options.modifiers & !7 == 0,
            "Unknown Smart Fn modifier flags"
        );
        if options.empty() {
            return Ok(());
        }
        let (send, receive) = mpsc::sync_channel(256);
        let stop = Arc::new(AtomicBool::new(false));
        let requested = stop.clone();
        self.retry = Arc::new(AtomicU64::new(0));
        let retry = self.retry.clone();
        self.join = Some(
            thread::Builder::new()
                .name("Jiks Fn metadata".into())
                .spawn(move || run(options, send, requested, retry))?,
        );
        self.stop = stop;
        self.events = Some(receive);
        Ok(())
    }
    pub fn next(&self) -> Option<Event> {
        self.events.as_ref()?.try_recv().ok()
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = &self.join {
            join.thread().unpark();
        }
    }
    pub fn retry_cleanup(&self) {
        self.retry.fetch_add(1, Ordering::AcqRel);
        self.stop();
    }
    pub fn active(&self) -> bool {
        self.join.is_some()
    }
    pub fn try_finish(&mut self) -> Option<Result<()>> {
        if !self.join.as_ref()?.is_finished() {
            return None;
        }
        Some(
            self.join
                .take()
                .unwrap()
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Fn listener thread failed"))),
        )
    }
}
impl Drop for NativeSource {
    fn drop(&mut self) {
        self.stop();
    }
}
#[derive(Default)]
pub struct ModifierState {
    down: BTreeSet<u32>,
    mask: u32,
}
impl ModifierState {
    pub fn update(&mut self, key: u32, down: bool, injected: bool) -> Option<u32> {
        if injected || !matches!(key, 0x10 | 0x11 | 0x12 | 0xa0..=0xa5) {
            return None;
        }
        if down {
            self.down.insert(key);
        } else {
            self.down.remove(&key);
        }
        let next = self.down.iter().fold(0, |mask, key| {
            mask | match key {
                0x10 | 0xa0 | 0xa1 => 1,
                0x11 | 0xa2 | 0xa3 => 2,
                0x12 | 0xa4 | 0xa5 => 4,
                _ => 0,
            }
        });
        if next == self.mask {
            None
        } else {
            self.mask = next;
            Some(next)
        }
    }
}
struct Sink {
    send: SyncSender<Event>,
    stop: Arc<AtomicBool>,
    options: Options,
    modifiers: ModifierState,
}
impl Sink {
    fn emit(&self, event: Event) {
        if self.send.try_send(event).is_err() {
            self.stop.store(true, Ordering::Release);
        }
    }
}
thread_local! {static LOCAL:RefCell<Option<Sink>>=const{RefCell::new(None)};}
unsafe extern "system" fn hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let handled = std::panic::catch_unwind(|| {
        if code >= 0 && lp != 0 {
            let key = unsafe { ptr::read_unaligned(lp as *const KBDLLHOOKSTRUCT) };
            let down = matches!(wp as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let up = matches!(wp as u32, WM_KEYUP | WM_SYSKEYUP);
            if down || up {
                LOCAL.with(|slot| {
                    if let Ok(mut slot) = slot.try_borrow_mut() {
                        if let Some(sink) = slot.as_mut() {
                            if sink.stop.load(Ordering::Acquire) {
                                return;
                            }
                            let injected = key.flags & LLKHF_INJECTED != 0;
                            if sink.options.escape && down && key.vkCode == 0x1b && !injected {
                                sink.emit(Event::Escape);
                            }
                            if let Some(mask) = sink.modifiers.update(key.vkCode, down, injected) {
                                sink.emit(Event::Modifiers {
                                    mask,
                                    at_ms: jiks_platform::monotonic_ms(),
                                });
                            }
                        }
                    }
                });
            }
        }
    });
    if handled.is_err() {
        LOCAL.with(|slot| {
            if let Ok(slot) = slot.try_borrow() {
                if let Some(sink) = slot.as_ref() {
                    sink.stop.store(true, Ordering::Release);
                }
            }
        });
    }
    unsafe { CallNextHookEx(ptr::null_mut(), code, wp, lp) }
}
fn run(
    options: Options,
    send: SyncSender<Event>,
    stop: Arc<AtomicBool>,
    retry: Arc<AtomicU64>,
) -> Result<()> {
    let mut utility = if options.utility {
        Some(FirmwareSubscription::open(FirmwareEventKind::Utility)?)
    } else {
        None
    };
    let mut msg = MSG::default();
    unsafe {
        PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_NOREMOVE);
    }
    LOCAL.with(|slot| {
        *slot.borrow_mut() = Some(Sink {
            send: send.clone(),
            stop: stop.clone(),
            options,
            modifiers: ModifierState::default(),
        })
    });
    let hook_handle = if options.modifiers != 0 || options.escape {
        let handle = unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), GetModuleHandleW(ptr::null()), 0)
        };
        if handle.is_null() {
            LOCAL.with(|slot| slot.borrow_mut().take());
            bail!(
                "Smart Fn hook was not registered: {}",
                std::io::Error::last_os_error()
            );
        }
        handle
    } else {
        ptr::null_mut()
    };
    let result = (|| -> Result<()> {
        send.try_send(Event::Started)
            .map_err(|_| anyhow::anyhow!("Fn startup acknowledgement unavailable"))?;
        while !stop.load(Ordering::Acquire) {
            unsafe {
                while PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    if msg.message == WM_QUIT {
                        stop.store(true, Ordering::Release);
                        break;
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if let Some(subscription) = &mut utility {
                for index in 0..=64 {
                    match subscription.poll(0)? {
                        Some(event) => {
                            ensure!(index < 64, "Fn WMI event backlog exceeded bounded drain");
                            send.try_send(Event::Utility {
                                key: event.value,
                                at_ms: event.received_at_tick_ms,
                            })
                            .map_err(|_| anyhow::anyhow!("Fn event queue overflow"))?;
                        }
                        None => break,
                    }
                }
            }
            thread::park_timeout(Duration::from_millis(20));
        }
        Ok(())
    })();
    // Subscription is dropped on its COM owner thread before the join can ack.
    drop(utility);
    if !hook_handle.is_null() {
        loop {
            if unsafe { UnhookWindowsHookEx(hook_handle) } != 0 {
                break;
            }
            let epoch = retry.load(Ordering::Acquire);
            let _ = send.try_send(Event::CleanupPending(format!(
                "Fn hook unregister未核对；保留hook线程，可再次停止重试：{}",
                std::io::Error::last_os_error()
            )));
            // Never free the callback state while the OS may still call it. A retry
            // is explicit; no hot retry loop and no fake thread-exit confirmation.
            while retry.load(Ordering::Acquire) == epoch {
                unsafe {
                    while PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                thread::park_timeout(Duration::from_millis(50));
            }
        }
    }
    LOCAL.with(|slot| slot.borrow_mut().take());
    result
}

/// Pure 300 ms single/double resolution. Cycling lists is committed by the host.
#[derive(Default)]
pub struct Presses {
    pending: BTreeMap<u32, u64>,
}
impl Presses {
    pub fn press(&mut self, key: u32, now: u64, has_double: bool) -> Vec<(u32, bool)> {
        if !has_double {
            return vec![(key, false)];
        }
        if let Some(deadline) = self.pending.remove(&key) {
            if now < deadline {
                return vec![(key, true)];
            }
            self.pending.insert(key, now.saturating_add(300));
            return vec![(key, false)];
        }
        self.pending.insert(key, now.saturating_add(300));
        vec![]
    }
    pub fn due(&mut self, now: u64) -> Vec<(u32, bool)> {
        let keys = self
            .pending
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(key, _)| *key)
            .collect::<Vec<_>>();
        for key in &keys {
            self.pending.remove(key);
        }
        keys.into_iter().map(|key| (key, false)).collect()
    }
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn construction_and_empty_options_have_no_thread_or_hook() {
        let mut source = NativeSource::new();
        assert!(!source.active());
        source.start(Options::default()).unwrap();
        assert!(!source.active());
        assert!(source.try_finish().is_none());
    }
    #[test]
    fn modifier_pairs_and_injected_keys_do_not_create_false_release() {
        let mut state = ModifierState::default();
        assert_eq!(state.update(0xa0, true, false), Some(1));
        assert_eq!(state.update(0xa1, true, false), None);
        assert_eq!(state.update(0xa0, false, false), None);
        assert_eq!(state.update(0xa2, true, false), Some(3));
        assert_eq!(state.update(0xa2, false, true), None);
        assert_eq!(state.update(0xa1, false, false), Some(2));
        assert_eq!(state.update(0xa2, false, false), Some(0));
    }
    #[test]
    fn single_double_and_different_keys_are_ordered_without_sleep() {
        let mut presses = Presses::default();
        assert!(presses.press(42, 100, true).is_empty());
        assert!(presses.due(399).is_empty());
        assert_eq!(presses.press(42, 399, true), vec![(42, true)]);
        assert!(presses.due(400).is_empty());
        assert!(presses.press(62, 500, true).is_empty());
        assert_eq!(presses.press(63, 550, false), vec![(63, false)]);
        assert_eq!(presses.due(800), vec![(62, false)]);
        assert!(presses.press(42, 1000, true).is_empty());
        assert_eq!(presses.press(42, 1300, true), vec![(42, false)]);
        assert_eq!(presses.due(1600), vec![(42, false)]);
    }
}
