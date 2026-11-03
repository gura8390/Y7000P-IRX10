//! Local same-user/same-elevation CLI, admitted through the running coordinator.
//! No second hardware owner, settings repository, or implicit controller startup.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    mem::zeroed,
    path::Path,
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    Storage::FileSystem::*,
    System::{Pipes::*, Threading::*},
};
const LIMIT: usize = 64 * 1024;
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn profile_path(root: &Path) -> Result<String> {
    ensure!(root.is_absolute(), "实例配置目录必须是完整绝对路径");
    let canonical = jiks_store::canonical_directory_path(root)?;
    let path = canonical
        .to_str()
        .context("实例配置目录不是有效Unicode")?
        .replace('/', "\\");
    Ok(path
        .strip_prefix(r"\\?\")
        .unwrap_or(&path)
        .trim_end_matches('\\')
        .to_lowercase())
}
fn name(root: &Path) -> Result<String> {
    Ok(format!(
        r"\\.\pipe\Jiks-{:x}",
        Sha256::digest(profile_path(root)?.as_bytes())
    ))
}
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    Status,
    Execute { command: String, payload: Value },
}
#[derive(Serialize, Deserialize, Debug)]
pub struct Response {
    pub ok: bool,
    pub message: String,
    pub value: Value,
}
struct Handle(HANDLE);
/// Held by the graphical parent. A duplicate never starts another backend or
/// hooks; its only permitted action is asking the existing peer to show itself.
pub struct InstanceLease(Handle);
impl InstanceLease {
    pub fn key(root: &Path) -> Result<String> {
        unsafe {
            let root = profile_path(root)?;
            let (sid, elevated) = token_identity(GetCurrentProcess())?;
            let mut hash = Sha256::new();
            hash.update(root.as_bytes());
            hash.update(sid);
            hash.update([u8::from(elevated)]);
            Ok(format!(r"Local\Jiks.GUI.{:x}", hash.finalize()))
        }
    }
    pub fn acquire(root: &Path) -> Result<Option<Self>> {
        unsafe {
            let key = wide(&Self::key(root)?);
            let handle = CreateMutexW(null(), 0, key.as_ptr());
            ensure!(
                !handle.is_null(),
                "无法创建机控实例锁：{}",
                std::io::Error::last_os_error()
            );
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                return Ok(None);
            }
            Ok(Some(Self(Handle(handle))))
        }
    }
    pub fn is_held(&self) -> bool {
        !self.0.0.is_null()
    }
}
/// A fixed show-only native message, scoped to the exact tagged GUI and verified
/// same-user/elevation peer. It works when general CLI integration is disabled.
pub fn show_existing(root: &Path) -> Result<()> {
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        struct Find {
            key: Vec<u16>,
            hwnd: HWND,
        }
        unsafe extern "system" fn find(hwnd: HWND, data: isize) -> i32 {
            unsafe {
                let state = &mut *(data as *mut Find);
                if !GetPropW(hwnd, state.key.as_ptr()).is_null() {
                    let mut pid = 0;
                    GetWindowThreadProcessId(hwnd, &mut pid);
                    if same_identity(pid).is_ok() {
                        state.hwnd = hwnd;
                        return 0;
                    }
                }
                1
            }
        }
        let mut state = Find {
            key: wide(&InstanceLease::key(root)?),
            hwnd: null_mut(),
        };
        let started = Instant::now();
        loop {
            EnumWindows(Some(find), &mut state as *mut Find as isize);
            if !state.hwnd.is_null() {
                break;
            }
            ensure!(
                started.elapsed() < Duration::from_secs(3),
                "现有机控尚未创建窗口，请稍候重试"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let message = RegisterWindowMessageW(wide("Jiks.ShowExisting.1").as_ptr());
        ensure!(message != 0, "无法注册显示机控消息");
        let mut acknowledged = 0usize;
        ensure!(
            SendMessageTimeoutW(
                state.hwnd,
                message,
                0,
                0,
                SMTO_ABORTIFHUNG,
                2000,
                &mut acknowledged
            ) != 0
                && acknowledged == 1,
            "现有机控未确认显示，请在托盘重试；未启动第二个后台"
        );
        Ok(())
    }
}
#[cfg(test)]
#[test]
fn instance_guard_survives_profile_creation_and_releases_without_starting_backend() {
    let root = std::env::temp_dir().join(format!("JiksInstanceFixture-{}", std::process::id()));
    let first = InstanceLease::acquire(&root).unwrap().unwrap();
    assert!(first.is_held());
    std::fs::create_dir_all(&root).unwrap();
    assert!(InstanceLease::acquire(&root).unwrap().is_none());
    drop(first);
    assert!(InstanceLease::acquire(&root).unwrap().is_some());
    std::fs::remove_dir_all(root).unwrap();
}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn token_identity(process: HANDLE) -> Result<(Vec<u8>, bool)> {
    unsafe {
        let mut token = null_mut();
        ensure!(
            OpenProcessToken(process, TOKEN_QUERY, &mut token) != 0,
            "CLI token: {}",
            std::io::Error::last_os_error()
        );
        let token = Handle(token);
        let mut len = 0;
        GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut len);
        ensure!(len > 0 && len <= 16384, "CLI user token size");
        let mut data = vec![0u8; len as usize];
        ensure!(
            GetTokenInformation(token.0, TokenUser, data.as_mut_ptr().cast(), len, &mut len) != 0,
            "CLI user token unavailable"
        );
        let sid = data.as_ptr().cast::<TOKEN_USER>().read_unaligned().User.Sid;
        let sid_len = GetLengthSid(sid);
        ensure!(sid_len > 0 && sid_len < 4096, "CLI SID size");
        let mut copy = vec![0u8; sid_len as usize];
        ensure!(
            CopySid(sid_len, copy.as_mut_ptr().cast(), sid) != 0,
            "CLI SID copy"
        );
        let mut elevation: TOKEN_ELEVATION = zeroed();
        let mut bytes = 0;
        ensure!(
            GetTokenInformation(
                token.0,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut bytes
            ) != 0,
            "CLI elevation token unavailable"
        );
        Ok((copy, elevation.TokenIsElevated != 0))
    }
}
fn same_identity(pid: u32) -> Result<()> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        ensure!(!process.is_null(), "CLI client process unavailable");
        let process = Handle(process);
        let (own, raised) = token_identity(GetCurrentProcess())?;
        let (other, other_raised) = token_identity(process.0)?;
        ensure!(
            EqualSid(own.as_ptr() as _, other.as_ptr() as _) != 0 && raised == other_raised,
            "CLI只允许相同Windows用户和相同提权级别"
        );
        Ok(())
    }
}
pub struct Endpoint {
    handle: Handle,
    connected: bool,
    received: Vec<u8>,
    admitted: bool,
    output: Vec<u8>,
    written: usize,
    since: Instant,
}
impl Endpoint {
    pub fn open(root: &Path) -> Result<Self> {
        unsafe {
            let path = wide(&name(root)?);
            let h = CreateNamedPipeW(
                path.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                128 * 1024,
                128 * 1024,
                0,
                null(),
            );
            ensure!(
                h != INVALID_HANDLE_VALUE,
                "CLI端点未创建：{}",
                std::io::Error::last_os_error()
            );
            Ok(Self {
                handle: Handle(h),
                connected: false,
                received: vec![],
                admitted: false,
                output: vec![],
                written: 0,
                since: Instant::now(),
            })
        }
    }
    fn reset(&mut self) {
        unsafe {
            DisconnectNamedPipe(self.handle.0);
        }
        self.connected = false;
        self.received.clear();
        self.admitted = false;
        self.output.clear();
        self.written = 0;
        self.since = Instant::now();
    }
    pub fn respond(&mut self, response: Response) -> Result<()> {
        let mut bytes = serde_json::to_vec(&response)?;
        ensure!(bytes.len() < LIMIT, "CLI response exceeds64KiB");
        bytes.push(b'\n');
        self.output = bytes;
        self.written = 0;
        self.since = Instant::now();
        Ok(())
    }
    pub fn poll(&mut self) -> Result<Option<Request>> {
        unsafe {
            if !self.connected {
                let connected = ConnectNamedPipe(self.handle.0, null_mut());
                let error = GetLastError();
                if connected == 0 && error != ERROR_PIPE_CONNECTED {
                    if error == ERROR_PIPE_LISTENING || error == ERROR_NO_DATA {
                        return Ok(None);
                    }
                    bail!("CLI连接失败：{error}");
                }
                self.connected = true;
                self.since = Instant::now();
                let mut pid = 0;
                if GetNamedPipeClientProcessId(self.handle.0, &mut pid) == 0
                    || same_identity(pid).is_err()
                {
                    self.reset();
                    return Ok(None);
                }
            }
            if !self.output.is_empty() {
                let mut wrote = 0;
                let ok = WriteFile(
                    self.handle.0,
                    self.output[self.written..].as_ptr(),
                    (self.output.len() - self.written) as u32,
                    &mut wrote,
                    null_mut(),
                );
                if self.written == self.output.len() {
                    let mut byte = 0u8;
                    let mut count = 0;
                    if ReadFile(self.handle.0, &mut byte, 1, &mut count, null_mut()) == 0
                        && GetLastError() != ERROR_NO_DATA
                    {
                        self.reset();
                    } else if self.since.elapsed() > Duration::from_secs(10) {
                        self.reset();
                    }
                } else if ok != 0 {
                    self.written += wrote as usize;
                } else if ![ERROR_NO_DATA, ERROR_PIPE_BUSY].contains(&GetLastError()) {
                    self.reset();
                }
                return Ok(None);
            }
            if self.admitted {
                return Ok(None);
            }
            if self.since.elapsed() > Duration::from_secs(10) {
                self.reset();
                return Ok(None);
            }
            let mut bytes = [0u8; 4096];
            let mut count = 0;
            if ReadFile(
                self.handle.0,
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                &mut count,
                null_mut(),
            ) == 0
            {
                if ![ERROR_NO_DATA, ERROR_PIPE_LISTENING].contains(&GetLastError()) {
                    self.reset();
                }
                return Ok(None);
            }
            self.received.extend_from_slice(&bytes[..count as usize]);
            if self.received.len() > LIMIT {
                self.reset();
                bail!("CLI request exceeds64KiB");
            }
            if self.received.last() == Some(&b'\n') {
                let request = serde_json::from_slice(&self.received);
                self.admitted = true;
                match request {
                    Ok(request) => return Ok(Some(request)),
                    Err(error) => self.respond(Response {
                        ok: false,
                        message: format!("CLI参数格式错误：{error}"),
                        value: Value::Null,
                    })?,
                }
            }
            Ok(None)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_pipe_requires_a_running_endpoint_and_returns_actual_response() {
        let root = std::env::temp_dir().join(format!(
            "Jiks-pipe-fixture-{}-{}",
            std::process::id(),
            jiks_platform::monotonic_ms()
        ));
        let mut server = Endpoint::open(&root).unwrap();
        // The endpoint can precede profile creation. A client using the now
        // canonical long spelling must still connect to that same endpoint.
        std::fs::create_dir_all(&root).unwrap();
        let client_root = root.canonicalize().unwrap();
        let client = std::thread::spawn(move || request(&client_root, Request::Status));
        let started = Instant::now();
        let mut got = false;
        while !client.is_finished() {
            if let Some(Request::Status) = server.poll().unwrap() {
                got = true;
                server
                    .respond(Response {
                        ok: true,
                        message: "fixture actual response".into(),
                        value: serde_json::json!({"hardware_actions":0}),
                    })
                    .unwrap();
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
        let response = client.join().unwrap().unwrap();
        assert!(got && response.ok);
        assert_eq!(response.value["hardware_actions"], 0);
        assert_eq!(response.message, "fixture actual response");
        drop(server);
        std::fs::remove_dir(root).unwrap();
    }
    #[test]
    fn instance_and_pipe_identity_are_stable_across_short_alias_and_creation() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let parent = std::env::temp_dir().join(format!(
            "JiksInstance-LongAncestor-{}-{}",
            std::process::id(),
            jiks_platform::monotonic_ms()
        ));
        std::fs::create_dir(&parent).unwrap();
        let wide: Vec<u16> = parent.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut buffer = vec![0u16; 32768];
        let length =
            unsafe { GetShortPathNameW(wide.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) };
        assert!(length > 0 && (length as usize) < buffer.len());
        let alias =
            std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length as usize]));
        let root = alias.join("uncreated-parent").join("profile");
        let lease = InstanceLease::acquire(&root).unwrap().unwrap();
        let key_before = InstanceLease::key(&root).unwrap();
        let pipe_before = name(&root).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let canonical = root.canonicalize().unwrap();
        assert_eq!(key_before, InstanceLease::key(&canonical).unwrap());
        assert_eq!(pipe_before, name(&canonical).unwrap());
        assert!(InstanceLease::acquire(&canonical).unwrap().is_none());
        drop(lease);
        assert!(parent.is_absolute() && parent.starts_with(std::env::temp_dir()));
        std::fs::remove_dir_all(parent).unwrap();
    }
}
// Only a live endpoint can execute commands. The client never initializes hardware.
pub fn request(root: &Path, request: Request) -> Result<Response> {
    request_after_write(root, request, || {})
}
fn request_after_write(
    root: &Path,
    request: Request,
    after_write: impl FnOnce(),
) -> Result<Response> {
    let mut bytes = serde_json::to_vec(&request)?;
    ensure!(bytes.len() < LIMIT, "CLI请求过大");
    bytes.push(b'\n');
    let path = name(root)?;
    let started = Instant::now();
    let mut pipe = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => break file,
            Err(error) => {
                ensure!(
                    started.elapsed() < Duration::from_secs(2),
                    "请先启动机控并在设置中启用CLI；当前没有可连接端点：{error}"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    };
    use std::os::windows::io::AsRawHandle;
    unsafe {
        let mut pid = 0;
        ensure!(
            GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) != 0,
            "CLI server identity unavailable"
        );
        same_identity(pid)?;
        let mode = PIPE_READMODE_BYTE | PIPE_NOWAIT;
        ensure!(
            SetNamedPipeHandleState(pipe.as_raw_handle(), &mode, null(), null()) != 0,
            "CLI pipe mode"
        );
    }
    pipe.write_all(&bytes)?;
    after_write();
    let mut result = vec![];
    loop {
        let mut buffer = [0u8; 4096];
        match pipe.read(&mut buffer) {
            Ok(0) => unsafe {
                let mut available = 0;
                ensure!(
                    PeekNamedPipe(
                        pipe.as_raw_handle(),
                        null_mut(),
                        0,
                        null_mut(),
                        &mut available,
                        null_mut()
                    ) != 0,
                    "CLI连接在回执前关闭"
                );
            },
            Ok(count) => {
                result.extend_from_slice(&buffer[..count]);
                ensure!(result.len() <= LIMIT, "CLI response exceeds64KiB");
                if result.last() == Some(&b'\n') {
                    return serde_json::from_slice(&result).context("CLI回执格式无效");
                }
            }
            Err(error) => {
                if error.raw_os_error() != Some(ERROR_NO_DATA as i32) {
                    return Err(error.into());
                }
            }
        }
        ensure!(
            started.elapsed() < Duration::from_secs(90),
            "CLI等待超时；操作可能仍在后台，请在机控中核对实际状态"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
#[cfg(test)]
pub(crate) fn request_with_write_receipt(
    root: &Path,
    request: Request,
    receipt: std::sync::mpsc::SyncSender<()>,
) -> Result<Response> {
    request_after_write(root, request, || {
        let _ = receipt.send(());
    })
}
