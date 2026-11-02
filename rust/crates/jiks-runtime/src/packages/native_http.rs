//! Native HTTPS transport; no session, socket or file is opened by new().
use super::{HttpHead, HttpRequest, HttpTransport, check_cancel, parse_url};
use anyhow::{Context, Result, ensure};
use std::{ffi::c_void, path::Path, sync::atomic::AtomicBool};
use windows_sys::Win32::Networking::WinHttp::*;
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
struct Handle(usize);
impl Handle {
    fn new(value: *mut c_void) -> Result<Self> {
        ensure!(
            !value.is_null(),
            "WinHTTP: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(value as usize))
    }
    fn raw(&self) -> *mut c_void {
        self.0 as *mut c_void
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            WinHttpCloseHandle(self.raw());
        }
    }
}
pub struct NativeHttp {
    session: Option<Handle>,
}
impl Default for NativeHttp {
    fn default() -> Self {
        Self::new()
    }
}
impl NativeHttp {
    pub fn new() -> Self {
        Self { session: None }
    }
}
fn checked(result: i32, cancel: &AtomicBool) -> Result<()> {
    check_cancel(cancel)?;
    ensure!(result != 0, "WinHTTP: {}", std::io::Error::last_os_error());
    Ok(())
}
fn header(request: &Handle, kind: u32) -> Option<String> {
    let mut buffer = vec![0u16; 4096];
    let mut size = (buffer.len() * 2) as u32;
    if unsafe {
        WinHttpQueryHeaders(
            request.raw(),
            kind,
            std::ptr::null(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return None;
    }
    buffer.truncate(size as usize / 2);
    while buffer.last() == Some(&0) {
        buffer.pop();
    }
    String::from_utf16(&buffer).ok()
}
impl HttpTransport for NativeHttp {
    fn get(
        &mut self,
        input: &HttpRequest,
        cancel: &AtomicBool,
        body: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<HttpHead> {
        check_cancel(cancel)?;
        let url = parse_url(&input.url)?;
        if self.session.is_none() {
            let agent = wide("Jiks/0.1 (+https://github.com/gura8390/Y7000P-IRX10)");
            let session = Handle::new(unsafe {
                WinHttpOpen(
                    agent.as_ptr(),
                    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                )
            })?;
            checked(
                unsafe { WinHttpSetTimeouts(session.raw(), 5000, 5000, 10000, 5000) },
                cancel,
            )?;
            self.session = Some(session);
        }
        let host = wide(&url.host);
        let connection = Handle::new(unsafe {
            WinHttpConnect(self.session.as_ref().unwrap().raw(), host.as_ptr(), 443, 0)
        })?;
        let method = wide("GET");
        let path = wide(&url.path);
        let referer = input.referer.as_deref().map(wide);
        let request = Handle::new(unsafe {
            WinHttpOpenRequest(
                connection.raw(),
                method.as_ptr(),
                path.as_ptr(),
                std::ptr::null(),
                referer.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            )
        })?;
        // Redirects are validated in the parent module, before a second request.
        let policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
        checked(
            unsafe {
                WinHttpSetOption(
                    request.raw(),
                    WINHTTP_OPTION_REDIRECT_POLICY,
                    (&policy as *const u32).cast(),
                    4,
                )
            },
            cancel,
        )?;
        let headers = wide("Accept: application/json, application/xml, */*\r\n");
        checked(
            unsafe {
                WinHttpSendRequest(
                    request.raw(),
                    headers.as_ptr(),
                    u32::MAX,
                    std::ptr::null(),
                    0,
                    0,
                    0,
                )
            },
            cancel,
        )?;
        checked(
            unsafe { WinHttpReceiveResponse(request.raw(), std::ptr::null_mut()) },
            cancel,
        )?;
        let head = HttpHead {
            status: header(&request, WINHTTP_QUERY_STATUS_CODE)
                .context("HTTP status missing")?
                .parse()?,
            content_length: header(&request, WINHTTP_QUERY_CONTENT_LENGTH)
                .and_then(|v| v.parse().ok()),
            location: header(&request, WINHTTP_QUERY_LOCATION),
        };
        if head.status != 200 {
            return Ok(head);
        }
        let mut buffer = vec![0u8; 64 * 1024];
        let mut count = 0u64;
        loop {
            check_cancel(cancel)?;
            let mut read = 0;
            checked(
                unsafe {
                    WinHttpReadData(
                        request.raw(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len() as u32,
                        &mut read,
                    )
                },
                cancel,
            )?;
            if read == 0 {
                break;
            }
            count = count
                .checked_add(read as u64)
                .context("HTTP byte count overflow")?;
            if let Some(length) = head.content_length {
                ensure!(count <= length, "HTTP body exceeds advertised length");
            }
            body(&buffer[..read as usize])?;
        }
        if let Some(length) = head.content_length {
            ensure!(count == length, "HTTP body ended before advertised length");
        }
        Ok(head)
    }
}
pub(super) fn publish_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // Same-directory atomic rename, explicitly without REPLACE_EXISTING.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
