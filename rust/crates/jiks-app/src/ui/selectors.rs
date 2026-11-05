//! Explicit user-invoked native selectors. Constructing kinds/plans performs no IO.
//! Calls may pump a nested UI loop: callers MUST release RefCell/Mutex borrows before
//! `pick`, then verify the original owner generation before changing any draft.
use super::i18n::tr_static;
use anyhow::{Result, bail, ensure};
use windows::Win32::{
    Foundation::HWND as ComHwnd,
    System::Com::*,
    UI::Shell::{Common::COMDLG_FILTERSPEC, *},
};
use windows::core::{Interface, PCWSTR};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::{Controls::Dialogs::*, WindowsAndMessaging::IsWindow},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileFilter {
    Any,
    Executable,
    Audio,
    Spectrum,
    Backup,
    Image,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PickerKind {
    Open(FileFilter),
    Save(FileFilter),
    Folder,
    Color,
}
impl FileFilter {
    fn spec(self) -> (&'static str, &'static str, Option<&'static str>) {
        match self {
            Self::Any => ("所有文件", "*.*", None),
            Self::Executable => ("可执行文件", "*.exe;*.com", Some("exe")),
            Self::Audio => ("WAV 音频文件", "*.wav", Some("wav")),
            Self::Spectrum => ("Spectrum 灯效文件", "*.json", Some("json")),
            Self::Backup => ("拯救者工具箱设置备份", "*.jiksbackup", Some("jiksbackup")),
            Self::Image => ("图像文件", "*.png;*.jpg;*.jpeg;*.bmp;*.gif", None),
        }
    }
}
fn file_options(kind: PickerKind) -> FILEOPENDIALOGOPTIONS {
    let base = FOS_FORCEFILESYSTEM | FOS_NOCHANGEDIR | FOS_DONTADDTORECENT | FOS_PATHMUSTEXIST;
    match kind {
        PickerKind::Folder => base | FOS_PICKFOLDERS,
        PickerKind::Open(_) => base | FOS_FILEMUSTEXIST,
        PickerKind::Save(_) => base | FOS_OVERWRITEPROMPT | FOS_NOTESTFILECREATE,
        PickerKind::Color => base,
    }
}
fn wide_checked(text: &str) -> Result<Vec<u16>> {
    ensure!(
        !text.contains('\0'),
        "Selector text contains an embedded NUL"
    );
    let mut value = text.encode_utf16().collect::<Vec<_>>();
    ensure!(
        value.len() < 32767,
        "Selector text exceeds the Windows path bound"
    );
    value.push(0);
    Ok(value)
}
fn parse_color(text: &str) -> Option<u32> {
    let value = text.strip_prefix('#')?;
    if value.len() != 6 {
        return None;
    }
    let rgb = u32::from_str_radix(value, 16).ok()?;
    Some((rgb >> 16) | (rgb & 0x00ff00) | ((rgb & 0xff) << 16))
}
fn color_text(color: u32) -> String {
    format!(
        "#{:02X}{:02X}{:02X}",
        color & 255,
        (color >> 8) & 255,
        (color >> 16) & 255
    )
}
struct Apartment;
impl Apartment {
    unsafe fn enter() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok()?;
            Ok(Self)
        }
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}
/// No settings or cwd changes. `None` means a normal user cancellation.
/// Only this function opens native dialogs, and only when explicitly invoked.
pub(super) fn pick(owner: HWND, kind: PickerKind, initial: &str) -> Result<Option<String>> {
    ensure!(
        !owner.is_null() && unsafe { IsWindow(owner) } != 0,
        "Selector owner is no longer available"
    );
    wide_checked(initial)?;
    unsafe {
        if kind == PickerKind::Color {
            let mut custom = [0x00ff_ffffu32; 16];
            let mut dialog = CHOOSECOLORW {
                lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
                hwndOwner: owner,
                rgbResult: parse_color(initial).unwrap_or(0x00ff_ffff),
                lpCustColors: custom.as_mut_ptr(),
                Flags: CC_RGBINIT | CC_FULLOPEN | CC_ANYCOLOR,
                ..Default::default()
            };
            if ChooseColorW(&mut dialog) != 0 {
                return Ok(Some(color_text(dialog.rgbResult)));
            }
            let code = CommDlgExtendedError();
            if code == 0 {
                return Ok(None);
            }
            bail!("Native color selector failed: 0x{code:08X}");
        }
        let _apartment = Apartment::enter()?;
        let dialog: IFileDialog = match kind {
            PickerKind::Save(_) => {
                let save: IFileSaveDialog =
                    CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)?;
                save.cast()?
            }
            _ => {
                let open: IFileOpenDialog =
                    CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
                open.cast()?
            }
        };
        dialog.SetOptions(dialog.GetOptions()? | file_options(kind))?;
        let title = wide_checked(tr_static(match kind {
            PickerKind::Folder => "选择目录",
            PickerKind::Save(_) => "选择保存文件",
            _ => "选择文件",
        }))?;
        dialog.SetTitle(PCWSTR(title.as_ptr()))?;
        if let PickerKind::Open(filter) | PickerKind::Save(filter) = kind {
            let (label, pattern, extension) = filter.spec();
            let label = wide_checked(tr_static(label))?;
            let pattern = wide_checked(pattern)?;
            let (all_name, all_pattern, _) = FileFilter::Any.spec();
            let all = wide_checked(tr_static(all_name))?;
            let star = wide_checked(all_pattern)?;
            let filters = [
                COMDLG_FILTERSPEC {
                    pszName: PCWSTR(label.as_ptr()),
                    pszSpec: PCWSTR(pattern.as_ptr()),
                },
                COMDLG_FILTERSPEC {
                    pszName: PCWSTR(all.as_ptr()),
                    pszSpec: PCWSTR(star.as_ptr()),
                },
            ];
            dialog.SetFileTypes(&filters)?;
            dialog.SetFileTypeIndex(1)?;
            if let Some(extension) = extension {
                let extension = wide_checked(extension)?;
                dialog.SetDefaultExtension(PCWSTR(extension.as_ptr()))?;
            }
        }
        // An unavailable initial location must not prevent the user choosing another.
        // Parsing happens only after the explicit click; no startup filesystem probe.
        if !initial.is_empty() {
            let path = std::path::Path::new(initial);
            let folder = if kind == PickerKind::Folder {
                Some(path)
            } else {
                path.parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
            };
            if let Some(folder) = folder.and_then(|folder| folder.to_str()) {
                let folder = wide_checked(folder)?;
                if let Ok(item) =
                    SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(folder.as_ptr()), None)
                {
                    let _ = dialog.SetFolder(&item);
                }
            }
            if kind != PickerKind::Folder {
                if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                    let name = wide_checked(name)?;
                    dialog.SetFileName(PCWSTR(name.as_ptr()))?;
                }
            }
        }
        if let Err(error) = dialog.Show(Some(ComHwnd(owner))) {
            if error.code().0 as u32 == 0x800704c7 {
                return Ok(None);
            }
            return Err(error.into());
        }
        let path = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
        let text = path.to_string();
        CoTaskMemFree(Some(path.0.cast()));
        let text = text?;
        wide_checked(&text)?;
        Ok(Some(text))
    }
}
/// Pure completion guard used after a nested picker loop. A dead/reused HWND
/// cannot accept the result: the caller supplies its current stable owner token.
pub(super) fn completion(
    expected_owner: usize,
    current_owner: Option<usize>,
    selected: Option<String>,
) -> Option<String> {
    if current_owner == Some(expected_owner) {
        selected
    } else {
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_file_mode_keeps_cwd_and_recent_settings_unchanged() {
        for kind in [
            PickerKind::Open(FileFilter::Any),
            PickerKind::Save(FileFilter::Spectrum),
            PickerKind::Folder,
        ] {
            let options = file_options(kind);
            assert!(
                options.contains(FOS_NOCHANGEDIR)
                    && options.contains(FOS_DONTADDTORECENT)
                    && options.contains(FOS_FORCEFILESYSTEM)
            );
            assert!(!options.contains(FOS_ALLOWMULTISELECT));
        }
        assert!(file_options(PickerKind::Save(FileFilter::Any)).contains(FOS_NOTESTFILECREATE));
    }
    #[test]
    fn file_filters_keep_patterns_separate_from_localized_product_labels() {
        for filter in [
            FileFilter::Any,
            FileFilter::Executable,
            FileFilter::Audio,
            FileFilter::Spectrum,
            FileFilter::Backup,
            FileFilter::Image,
        ] {
            let (_, pattern, extension) = filter.spec();
            assert!(!pattern.contains('\0') && !pattern.contains(' '));
            if let Some(extension) = extension {
                assert!(!extension.starts_with('.'));
            }
        }
    }
    #[test]
    fn unicode_and_long_input_are_bounded_without_touching_files() {
        assert_eq!(
            wide_checked("D:\\用户\\文件名.txt").unwrap().last(),
            Some(&0)
        );
        assert!(wide_checked("bad\0path").is_err());
        assert!(wide_checked(&"界".repeat(32767)).is_err());
    }
    #[test]
    fn colors_use_rgb_ui_text_and_win32_bgr_only_at_the_boundary() {
        assert_eq!(parse_color("#1234AB"), Some(0x00ab3412));
        assert_eq!(color_text(0x00ab3412), "#1234AB");
        assert_eq!(parse_color("#123"), None);
        assert_eq!(parse_color("#GG0000"), None);
    }
    #[test]
    fn cancel_and_reused_owner_never_change_a_draft() {
        assert_eq!(completion(7, Some(7), None), None);
        assert_eq!(completion(7, None, Some("new".into())), None);
        assert_eq!(completion(7, Some(8), Some("new".into())), None);
        assert_eq!(
            completion(7, Some(7), Some("用户路径".into())),
            Some("用户路径".into())
        );
    }
}
