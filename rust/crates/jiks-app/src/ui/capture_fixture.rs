//! Opt-in UI-layout fixture evidence, compiled only into debug builds.
//! Runs after the actual App/Editor RefMut is released; there is no broker,
//! device query, input injection or production environment-variable hook.
use super::native::*;
use crate::ui_model::UiBridge;
use anyhow::{Result, ensure};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    mem::{size_of, zeroed},
    path::{Path, PathBuf},
    ptr::null_mut,
    sync::atomic::{AtomicU64, Ordering},
};
use windows_sys::Win32::{Foundation::*, Graphics::Gdi::*, UI::WindowsAndMessaging::*};
const MESSAGE: u32 = WM_APP + 95;
thread_local! {static QUEUED:RefCell<BTreeMap<usize,(PathBuf,String)>>=const {RefCell::new(BTreeMap::new())};}
static SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(super) fn queue(hwnd: HWND, bridge: &UiBridge, key: &str) {
    let Some(directory) = bridge.layout_capture_dir.as_ref() else {
        return;
    };
    if !directory.is_absolute() {
        return;
    }
    let key = key
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || "._-".contains(ch) {
                ch
            } else {
                '_'
            }
        })
        .collect();
    QUEUED.with(|queued| {
        queued
            .borrow_mut()
            .insert(hwnd as usize, (directory.clone(), key));
    });
    unsafe {
        PostMessageW(hwnd, MESSAGE, 0, 0);
    }
}
pub(super) fn consume(message: &MSG) -> bool {
    if message.message != MESSAGE {
        return false;
    }
    let target = QUEUED.with(|queued| queued.borrow_mut().remove(&(message.hwnd as usize)));
    if let Some((directory, key)) = target {
        if let Err(error) = unsafe { capture(message.hwnd, &directory, &key) } {
            eprintln!("UI_LAYOUT_CAPTURE_ERROR {error:#}");
        }
    }
    true
}
struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    pixels: *mut u8,
    width: u32,
    height: u32,
}
impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}
impl Dib {
    unsafe fn new(width: u32, height: u32) -> Result<Self> {
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            ensure!(!dc.is_null(), "CreateCompatibleDC failed");
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..zeroed()
                },
                ..zeroed()
            };
            let mut pixels = null_mut();
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut pixels, null_mut(), 0);
            if bitmap.is_null() {
                DeleteDC(dc);
                anyhow::bail!("CreateDIBSection failed");
            }
            let old = SelectObject(dc, bitmap);
            if old.is_null() || old as isize == -1 {
                DeleteObject(bitmap);
                DeleteDC(dc);
                anyhow::bail!("Fixture bitmap could not be selected into its memory DC");
            }
            Ok(Self {
                dc,
                bitmap,
                old,
                pixels: pixels.cast(),
                width,
                height,
            })
        }
    }
    unsafe fn save(&self, path: &Path) -> Result<()> {
        ensure!(
            unsafe { GdiFlush() } != 0,
            "Fixture GDI commands did not finish"
        );
        let mut rgba = unsafe {
            std::slice::from_raw_parts(self.pixels, (self.width * self.height * 4) as usize)
        }
        .to_vec();
        for pixel in rgba.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        image::save_buffer_with_format(
            path,
            &rgba,
            self.width,
            self.height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )?;
        Ok(())
    }
}
unsafe fn capture(hwnd: HWND, directory: &Path, key: &str) -> Result<()> {
    unsafe {
        ensure!(IsWindow(hwnd) != 0, "Fixture capture target retired");
        let mut rect: RECT = zeroed();
        ensure!(
            GetWindowRect(hwnd, &mut rect) != 0,
            "Fixture rect unavailable"
        );
        let width = (rect.right - rect.left) as u32;
        let height = (rect.bottom - rect.top) as u32;
        ensure!(
            (20..=8192).contains(&width) && (20..=8192).contains(&height),
            "Fixture capture bounds invalid"
        );
        std::fs::create_dir_all(directory)?;
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let stem = format!("{sequence:04}-{key}");
        let dib = Dib::new(width, height)?;
        fill(
            dib.dc,
            &RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            },
            0x00ff00ff,
        );
        SendMessageW(
            hwnd,
            WM_PRINT,
            dib.dc as usize,
            (PRF_CLIENT | PRF_NONCLIENT | PRF_CHILDREN | PRF_ERASEBKGND) as isize,
        );
        let wmprint = directory.join(format!("{stem}.wmprint.png"));
        ensure!(!wmprint.exists(), "Fixture output exists");
        dib.save(&wmprint)?;
        let receipt = directory.join(format!("{stem}.json"));
        ensure!(!receipt.exists(), "Fixture output exists");
        std::fs::write(
            receipt,
            serde_json::to_vec_pretty(
                &serde_json::json!({"fixture_only":true,"in_process":true,"key":key,"hwnd":hwnd as usize,"width":width,"height":height,"method":"WM_PRINT on owning UI thread","wmprint":wmprint,"palette_colorref":{"paper":paper(),"surface":surface(),"ink":ink(),"muted":muted()}}),
            )?,
        )?;
        Ok(())
    }
}
