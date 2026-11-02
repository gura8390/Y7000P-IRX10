//! Optional CPU image preparation. No renderer/device or persistent GPU context.
use crate::ui_model::UiModel;
use anyhow::{Context, Result, ensure};
use image::{ImageReader, imageops::FilterType};
use std::{
    mem::zeroed,
    path::Path,
    thread::{self, JoinHandle},
};
use windows_sys::Win32::{Foundation::RECT, Graphics::Gdi::*};
#[derive(Clone, PartialEq)]
struct Request {
    path: String,
    width: u32,
    height: u32,
    opacity: u32,
    blur: u32,
    paper: u32,
}
struct Pixels {
    width: u32,
    height: u32,
    bytes: Vec<u8>,
}
#[derive(Default)]
pub(super) struct Background {
    wanted: Option<Request>,
    attempted: Option<Request>,
    worker: Option<JoinHandle<Result<Pixels>>>,
    pixels: Option<Pixels>,
    dirty: bool,
}
impl Background {
    pub(super) fn sync(
        &mut self,
        model: &UiModel,
        width: i32,
        height: i32,
        paper: u32,
    ) -> Option<String> {
        let path = model
            .value("appearance.background")
            .as_str()
            .unwrap_or("")
            .to_owned();
        self.wanted = (!path.is_empty() && width > 0 && height > 0).then(|| Request {
            path,
            width: width as u32,
            height: height as u32,
            opacity: model
                .value("appearance.opacity")
                .as_f64()
                .unwrap_or(70.)
                .clamp(0., 100.) as u32,
            blur: model
                .value("appearance.blur")
                .as_u64()
                .unwrap_or(0)
                .min(100) as u32,
            paper,
        });
        let mut error = None;
        if self.worker.as_ref().is_some_and(|w| w.is_finished()) {
            self.dirty = true;
            match self.worker.take().unwrap().join() {
                Ok(Ok(pixels)) => self.pixels = Some(pixels),
                Ok(Err(e)) => {
                    self.pixels = None;
                    error = Some(format!("背景图片未显示：{e:#}"));
                }
                Err(_) => {
                    self.pixels = None;
                    error = Some("背景图片处理线程未完成".into());
                }
            }
        }
        if self.wanted.is_none() {
            self.pixels = None;
        }
        if self.worker.is_none() && self.attempted != self.wanted {
            self.dirty = true;
            self.attempted = self.wanted.clone();
            if let Some(request) = self.wanted.clone() {
                match thread::Builder::new()
                    .name("Jiks CPU background".into())
                    .spawn(move || prepare(request))
                {
                    Ok(worker) => self.worker = Some(worker),
                    Err(e) => error = Some(format!("背景线程未创建：{e}")),
                }
            }
        }
        error
    }
    pub(super) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }
    pub(super) fn visible(&self) -> bool {
        self.wanted.is_some() && self.pixels.is_some()
    }
    pub(super) unsafe fn paint(&self, dc: HDC, rect: &RECT) -> bool {
        unsafe {
            let Some(pixels) = self.pixels.as_ref().filter(|_| self.wanted.is_some()) else {
                return false;
            };
            let mut info: BITMAPINFO = zeroed();
            info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = pixels.width as i32;
            info.bmiHeader.biHeight = -(pixels.height as i32);
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB;
            StretchDIBits(
                dc,
                0,
                0,
                rect.right,
                rect.bottom,
                0,
                0,
                pixels.width as i32,
                pixels.height as i32,
                pixels.bytes.as_ptr().cast(),
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            ) != GDI_ERROR as i32
        }
    }
}
fn prepare(request: Request) -> Result<Pixels> {
    ensure!(
        request.width <= 8192 && request.height <= 8192,
        "窗口图片尺寸过大"
    );
    let path = Path::new(&request.path);
    ensure!(path.is_absolute(), "背景图片需要完整路径");
    ensure!(
        std::fs::metadata(path)?.len() <= 32 * 1024 * 1024,
        "背景图片文件超过32MiB"
    );
    let mut reader = ImageReader::open(path)?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let mut image = reader
        .decode()
        .context("不支持或损坏的PNG/JPEG/GIF/BMP图片")?
        .resize_to_fill(request.width, request.height, FilterType::Triangle)
        .to_rgba8();
    if request.blur > 0 {
        image = image::imageops::blur(&image, request.blur as f32 / 5.);
    }
    let paper = [
        (request.paper & 255) as u8,
        ((request.paper >> 8) & 255) as u8,
        ((request.paper >> 16) & 255) as u8,
    ];
    for pixel in image.pixels_mut() {
        let alpha = pixel[3] as u32 * request.opacity;
        let blend =
            |c: u8, base: u8| ((c as u32 * alpha + base as u32 * (25500 - alpha)) / 25500) as u8;
        let r = blend(pixel[0], paper[0]);
        let g = blend(pixel[1], paper[1]);
        let b = blend(pixel[2], paper[2]);
        pixel.0 = [b, g, r, 0];
    }
    Ok(Pixels {
        width: request.width,
        height: request.height,
        bytes: image.into_raw(),
    })
}
