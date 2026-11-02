//! Actual native device adapters. Discovery never sends a HID feature report.
use crate::{
    domain::ExplicitAction,
    ownership::{OwnerToken, wide},
    wmi_native::WmiFacade,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    mem::{size_of, zeroed},
    ptr,
};
use windows::{
    Win32::{
        Media::Audio::{
            DEVICE_STATE_ACTIVE, Endpoints::IAudioEndpointVolume, IMMDeviceEnumerator,
            MMDeviceEnumerator, eCapture, eRender,
        },
        System::Com::{CLSCTX_ALL, CoCreateInstance},
        UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
        },
    },
    core::GUID,
};
use windows_sys::Win32::{
    Devices::HumanInterfaceDevice::*,
    Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE},
    Graphics::Gdi::*,
    Storage::FileSystem::*,
};

#[derive(Debug, Clone, Serialize)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub hertz: u32,
}
#[derive(Debug, Clone, Serialize)]
pub struct DisplayDevice {
    pub name: String,
    pub description: String,
    pub current: Option<DisplayMode>,
    pub modes: Vec<DisplayMode>,
}
fn utf16(value: &[u16]) -> String {
    String::from_utf16_lossy(&value[..value.iter().position(|c| *c == 0).unwrap_or(value.len())])
}
pub fn displays() -> Result<Vec<DisplayDevice>> {
    let mut result = Vec::new();
    let mut index = 0;
    loop {
        let mut device: DISPLAY_DEVICEW = unsafe { zeroed() };
        device.cb = size_of::<DISPLAY_DEVICEW>() as u32;
        if unsafe { EnumDisplayDevicesW(ptr::null(), index, &mut device, 0) } == 0 {
            break;
        }
        index += 1;
        if device.StateFlags & DISPLAY_DEVICE_ACTIVE == 0 {
            continue;
        }
        let name = utf16(&device.DeviceName);
        let wide = wide(&name)?;
        let mut mode: DEVMODEW = unsafe { zeroed() };
        mode.dmSize = size_of::<DEVMODEW>() as u16;
        let current = if unsafe {
            EnumDisplaySettingsW(wide.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode)
        } != 0
        {
            Some(DisplayMode {
                width: mode.dmPelsWidth,
                height: mode.dmPelsHeight,
                hertz: mode.dmDisplayFrequency,
            })
        } else {
            None
        };
        let mut modes = Vec::new();
        let mut mode_index = 0;
        loop {
            let mut mode: DEVMODEW = unsafe { zeroed() };
            mode.dmSize = size_of::<DEVMODEW>() as u16;
            if unsafe { EnumDisplaySettingsW(wide.as_ptr(), mode_index, &mut mode) } == 0 {
                break;
            }
            mode_index += 1;
            let next = DisplayMode {
                width: mode.dmPelsWidth,
                height: mode.dmPelsHeight,
                hertz: mode.dmDisplayFrequency,
            };
            if !modes.iter().any(|m: &DisplayMode| {
                m.width == next.width && m.height == next.height && m.hertz == next.hertz
            }) {
                modes.push(next)
            }
        }
        result.push(DisplayDevice {
            name,
            description: utf16(&device.DeviceString),
            current,
            modes,
        });
    }
    Ok(result)
}
pub(crate) fn display_write(
    _owner: &OwnerToken,
    _action: &ExplicitAction,
    name: &str,
    width: u32,
    height: u32,
    hertz: u32,
) -> Result<DisplayMode> {
    let devices = displays()?;
    let device = devices
        .iter()
        .find(|d| d.name == name)
        .context("Display is no longer present")?;
    ensure!(
        device
            .modes
            .iter()
            .any(|m| m.width == width && m.height == height && m.hertz == hertz),
        "Requested mode is not advertised by the display"
    );
    let name = wide(name)?;
    let mut mode: DEVMODEW = unsafe { zeroed() };
    mode.dmSize = size_of::<DEVMODEW>() as u16;
    ensure!(
        unsafe { EnumDisplaySettingsW(name.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) } != 0,
        "Cannot read current display settings"
    );
    mode.dmPelsWidth = width;
    mode.dmPelsHeight = height;
    mode.dmDisplayFrequency = hertz;
    mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    _action.check_write_boundary()?;
    ensure!(
        unsafe { ChangeDisplaySettingsExW(name.as_ptr(), &mode, ptr::null_mut(), 0, ptr::null()) }
            == DISP_CHANGE_SUCCESSFUL,
        "Display mode change rejected"
    );
    let mut observed: DEVMODEW = unsafe { zeroed() };
    observed.dmSize = size_of::<DEVMODEW>() as u16;
    ensure!(
        unsafe { EnumDisplaySettingsW(name.as_ptr(), ENUM_CURRENT_SETTINGS, &mut observed) } != 0
            && observed.dmPelsWidth == width
            && observed.dmPelsHeight == height
            && observed.dmDisplayFrequency == hertz,
        "Display mode readback mismatch"
    );
    Ok(DisplayMode {
        width: observed.dmPelsWidth,
        height: observed.dmPelsHeight,
        hertz: observed.dmDisplayFrequency,
    })
}

pub use crate::audio_control::EndpointState as AudioEndpoint;
fn endpoints(capture: bool) -> Result<Vec<IAudioEndpointVolume>> {
    let _connection = WmiFacade::connect("ROOT\\CIMV2")?; // Initializes this COM apartment, no query/device write.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let collection = unsafe {
        enumerator.EnumAudioEndpoints(
            if capture { eCapture } else { eRender },
            DEVICE_STATE_ACTIVE,
        )?
    };
    let mut result = Vec::new();
    for i in 0..unsafe { collection.GetCount()? } {
        let device = unsafe { collection.Item(i)? };
        result.push(unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)? });
    }
    Ok(result)
}
pub fn audio_endpoints() -> Result<Vec<AudioEndpoint>> {
    let mut result = Vec::new();
    for capture in [false, true] {
        for (index, endpoint) in endpoints(capture)?.into_iter().enumerate() {
            result.push(AudioEndpoint {
                capture,
                index: index as u32,
                muted: unsafe { endpoint.GetMute()? }.as_bool(),
                volume_percent: unsafe { endpoint.GetMasterVolumeLevelScalar()? } * 100.0,
            });
        }
    }
    Ok(result)
}
pub(crate) fn audio_write(
    _owner: &OwnerToken,
    _action: &ExplicitAction,
    capture: bool,
    mute: Option<bool>,
    volume: Option<u32>,
) -> Result<serde_json::Value> {
    struct NativeAudio<'a> {
        endpoints: Vec<IAudioEndpointVolume>,
        capture: bool,
        owner: &'a OwnerToken,
        action: &'a ExplicitAction,
    }
    impl crate::audio_control::AudioTransport for NativeAudio<'_> {
        fn count(&self) -> usize {
            self.endpoints.len()
        }
        fn guard(&mut self) -> Result<()> {
            self.action.check_write_boundary()
        }
        fn read(&mut self, index: usize) -> Result<AudioEndpoint> {
            let endpoint = &self.endpoints[index];
            Ok(AudioEndpoint {
                capture: self.capture,
                index: index as u32,
                muted: unsafe { endpoint.GetMute()? }.as_bool(),
                volume_percent: unsafe { endpoint.GetMasterVolumeLevelScalar()? } * 100.,
            })
        }
        fn set_mute(&mut self, index: usize, value: bool) -> Result<()> {
            self.action.check_write_boundary()?;
            unsafe { self.endpoints[index].SetMute(value, ptr::null::<GUID>())? };
            Ok(())
        }
        fn set_volume(&mut self, index: usize, value: u32) -> Result<()> {
            self.action.check_write_boundary()?;
            unsafe {
                self.endpoints[index]
                    .SetMasterVolumeLevelScalar(value as f32 / 100., ptr::null::<GUID>())?
            };
            Ok(())
        }
        fn set_led(&mut self, code: u32) -> Result<()> {
            ensure!([1, 2, 4, 5].contains(&code), "Unknown special-key LED code");
            WmiFacade::connect("ROOT\\WMI")?.method_write(
                self.owner,
                self.action,
                "LENOVO_UTILITY_DATA",
                "SetFeature",
                std::collections::BTreeMap::from([(
                    "featuretype".into(),
                    crate::wmi_native::MethodInput::U32(code),
                )]),
            )?;
            Ok(())
        }
    }
    let mut native = NativeAudio {
        endpoints: endpoints(capture)?,
        capture,
        owner: _owner,
        action: _action,
    };
    // NativeState.owner has already approved this exact 83NN Legion. Upstream
    // SpecialKeyLedHelper only invokes Utility.SetFeature for four OTHER series;
    // keeping the family gate avoids introducing a new Legion firmware write.
    Ok(serde_json::to_value(crate::audio_control::execute(
        &mut native,
        capture,
        mute,
        volume,
        crate::audio_control::LedFamily::Legion,
    )?)?)
}

pub(crate) fn send_key(
    _owner: &OwnerToken,
    _action: &ExplicitAction,
    key: u16,
    pressed: bool,
) -> Result<()> {
    ensure!(key > 0 && key <= 0xff, "Virtual key outside Windows range");
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(key),
                dwFlags: if pressed {
                    Default::default()
                } else {
                    KEYEVENTF_KEYUP
                },
                ..Default::default()
            },
        },
    };
    if pressed {
        _action.check_write_boundary()?;
    } else {
        crate::ensure_no_other_controllers()?;
    }
    ensure!(
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) } == 1,
        "SendInput rejected (possibly integrity/UIPI restriction)"
    );
    Ok(())
}

struct DeviceHandle(HANDLE);
impl Drop for DeviceHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn hid_open(path: &str, access: u32) -> Result<DeviceHandle> {
    ensure!(
        path.to_ascii_lowercase().starts_with("\\\\?\\hid#"),
        "Only an explicit HID interface path is accepted"
    );
    let path = wide(path)?;
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    ensure!(
        handle != INVALID_HANDLE_VALUE,
        "Cannot open HID interface: {}",
        std::io::Error::last_os_error()
    );
    Ok(DeviceHandle(handle))
}
#[derive(Debug, Clone, Serialize)]
pub struct HidMetadata {
    pub vendor_id: u16,
    pub product_id: u16,
    pub feature_length: u16,
    pub usage_page: u16,
    pub usage: u16,
}
fn metadata(handle: HANDLE) -> Result<HidMetadata> {
    let mut attributes: HIDD_ATTRIBUTES = unsafe { zeroed() };
    attributes.Size = size_of::<HIDD_ATTRIBUTES>() as u32;
    ensure!(
        unsafe { HidD_GetAttributes(handle, &mut attributes) },
        "HID attributes unavailable"
    );
    let mut preparsed = 0isize;
    ensure!(
        unsafe { HidD_GetPreparsedData(handle, &mut preparsed) },
        "HID preparsed descriptor unavailable"
    );
    ensure!(preparsed != 0, "HID returned a null preparsed descriptor");
    let mut caps: HIDP_CAPS = unsafe { zeroed() };
    let status = unsafe { HidP_GetCaps(preparsed, &mut caps) };
    unsafe {
        HidD_FreePreparsedData(preparsed);
    }
    ensure!(
        status == HIDP_STATUS_SUCCESS,
        "HID capabilities unavailable"
    );
    Ok(HidMetadata {
        vendor_id: attributes.VendorID,
        product_id: attributes.ProductID,
        feature_length: caps.FeatureReportByteLength,
        usage_page: caps.UsagePage,
        usage: caps.Usage,
    })
}
fn supported_hid(caps: &HidMetadata) -> bool {
    caps.vendor_id == 0x048d
        && match caps.feature_length {
            33 => caps.product_id & 0xff00 == 0xc900,
            960 => [0xc100, 0xc900, 0xc600].contains(&(caps.product_id & 0xff00)),
            _ => false,
        }
}
pub fn hid_metadata(path: &str) -> Result<HidMetadata> {
    let handle = hid_open(path, 0)?;
    metadata(handle.0)
}
#[derive(Debug, Clone, Serialize)]
pub struct HidInterface {
    pub path: String,
    pub metadata: Option<HidMetadata>,
    pub supported_protocol: bool,
    pub error: Option<String>,
}
/// SetupAPI + access-zero metadata only. Never GetFeature/SetFeature as a probe.
pub fn hid_interfaces() -> Result<Vec<HidInterface>> {
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
    let mut guid = unsafe { zeroed() };
    unsafe { HidD_GetHidGuid(&mut guid) };
    let info = unsafe {
        SetupDiGetClassDevsW(
            &guid,
            ptr::null(),
            ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    ensure!(info != -1, "Cannot enumerate HID interfaces");
    struct Info(HDEVINFO);
    impl Drop for Info {
        fn drop(&mut self) {
            unsafe { SetupDiDestroyDeviceInfoList(self.0) };
        }
    }
    let _info = Info(info);
    let mut result = vec![];
    for index in 0..4096 {
        let mut interface: SP_DEVICE_INTERFACE_DATA = unsafe { zeroed() };
        interface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        if unsafe { SetupDiEnumDeviceInterfaces(info, ptr::null(), &guid, index, &mut interface) }
            == 0
        {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(259) {
                return Ok(result);
            }
            return Err(error.into());
        }
        let mut required = 0u32;
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                info,
                &interface,
                ptr::null_mut(),
                0,
                &mut required,
                ptr::null_mut(),
            )
        };
        ensure!(
            (8..=65536).contains(&required),
            "Invalid HID interface detail length"
        );
        // DWORD-aligned allocation. Native x64 detail.cbSize=8, DevicePath offset=4.
        let mut words = vec![0u32; (required as usize).div_ceil(4)];
        words[0] = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        let capacity = (words.len() * 4) as u32;
        ensure!(
            unsafe {
                SetupDiGetDeviceInterfaceDetailW(
                    info,
                    &interface,
                    words.as_mut_ptr().cast(),
                    capacity,
                    &mut required,
                    ptr::null_mut(),
                )
            } != 0
                && required <= capacity,
            "Cannot read HID interface detail"
        );
        let path = detail_path(&words, required as usize)?;
        let (metadata, supported_protocol, error) = match hid_metadata(&path) {
            Ok(value) => {
                let supported = supported_hid(&value);
                (Some(value), supported, None)
            }
            Err(e) => (None, false, Some(e.to_string())),
        };
        result.push(HidInterface {
            path,
            metadata,
            supported_protocol,
            error,
        });
    }
    anyhow::bail!("HID interface enumeration exceeded its bound")
}
fn detail_path(words: &[u32], bytes: usize) -> Result<String> {
    ensure!(
        bytes >= 6 && bytes <= words.len() * 4 && bytes % 2 == 0,
        "Invalid HID detail packet size"
    );
    let units =
        unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u16>().add(2), (bytes - 4) / 2) };
    let end = units
        .iter()
        .position(|v| *v == 0)
        .context("Unterminated HID interface path")?;
    let path = String::from_utf16(&units[..end])?;
    ensure!(
        path.to_ascii_lowercase().starts_with("\\\\?\\hid#"),
        "Unexpected device interface class path"
    );
    Ok(path)
}
pub fn hid_read_feature(path: &str, report_id: u8) -> Result<Vec<u8>> {
    let handle = hid_open(path, GENERIC_READ)?;
    let caps = metadata(handle.0)?;
    ensure!(
        supported_hid(&caps),
        "Not a supported Lenovo HID feature interface"
    );
    let mut bytes = vec![0; caps.feature_length as usize];
    bytes[0] = report_id;
    ensure!(
        unsafe { HidD_GetFeature(handle.0, bytes.as_mut_ptr().cast(), bytes.len() as u32) },
        "HID GetFeature failed"
    );
    Ok(bytes)
}
pub(crate) fn hid_write(
    _owner: &OwnerToken,
    _action: &ExplicitAction,
    path: &str,
    bytes: &[u8],
) -> Result<()> {
    let handle = hid_open(path, GENERIC_READ | GENERIC_WRITE)?;
    let caps = metadata(handle.0)?;
    ensure!(
        supported_hid(&caps) && bytes.len() == caps.feature_length as usize,
        "HID feature report length/vendor/product mismatch"
    );
    if bytes.len() == 33 {
        ensure!(bytes[..2] == [0xcc, 0x16], "Invalid RGB keyboard header");
    } else {
        if bytes[1] == 0xcb {
            let profile = crate::spectrum::Profile::decode(bytes)?;
            ensure!(
                profile.encode_24_zone()?.as_slice() == bytes,
                "Spectrum effect frame is not a verified 24-zone encoding"
            );
        } else {
            ensure!(
                bytes[0] == 7
                    && bytes[2] == 0xc0
                    && bytes[3] == 3
                    && [
                        0xd1, 0xc4, 0xc5, 0xc8, 0xc9, 0xca, 0xcb, 0xcc, 0xcd, 0xce, 0xd0, 0xa1,
                        0xa5, 0xa6
                    ]
                    .contains(&bytes[1]),
                "Unknown Spectrum report command/header"
            );
        }
    }
    let mut buffer = bytes.to_vec();
    _action.check_write_boundary()?;
    ensure!(
        unsafe { HidD_SetFeature(handle.0, buffer.as_mut_ptr().cast(), buffer.len() as u32) },
        "HID SetFeature failed"
    );
    Ok(())
}

pub(crate) struct SpectrumDevice<'a> {
    handle: DeviceHandle,
    _owner: &'a OwnerToken,
    _action: &'a ExplicitAction,
}
impl<'a> SpectrumDevice<'a> {
    pub(crate) fn open(
        owner: &'a OwnerToken,
        action: &'a ExplicitAction,
        path: &str,
    ) -> Result<Self> {
        let handle = hid_open(path, GENERIC_READ | GENERIC_WRITE)?;
        let caps = metadata(handle.0)?;
        ensure!(
            crate::spectrum::is_83nn_candidate(
                &crate::read_machine_identity()?,
                crate::spectrum::HidCandidate {
                    vendor_id: caps.vendor_id,
                    product_id: caps.product_id,
                    feature_report_bytes: caps.feature_length
                }
            ),
            "HID interface is not a verified 83NN Spectrum candidate"
        );
        Ok(Self {
            handle,
            _owner: owner,
            _action: action,
        })
    }
}
impl crate::spectrum::SpectrumTransport for SpectrumDevice<'_> {
    fn set_feature(&mut self, frame: &crate::spectrum::FeatureReport) -> Result<()> {
        crate::native::reject_other_controllers()?;
        if frame[1] == 0xcb {
            let profile = crate::spectrum::Profile::decode(frame)?;
            ensure!(
                profile.encode_24_zone()? == *frame,
                "Invalid 24-zone Spectrum effects frame"
            );
        } else {
            ensure!(
                frame[0] == 7
                    && frame[2..4] == [0xc0, 3]
                    && [0xc8, 0xc9, 0xca, 0xcc, 0xcd, 0xce].contains(&frame[1]),
                "Unknown Spectrum operation; compatibility probing is not implicit"
            );
        }
        let mut buffer = *frame;
        self._action.check_write_boundary()?;
        ensure!(
            unsafe {
                HidD_SetFeature(
                    self.handle.0,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                )
            },
            "Spectrum SetFeature failed"
        );
        Ok(())
    }
    fn get_feature(&mut self, frame: &mut crate::spectrum::FeatureReport) -> Result<()> {
        ensure!(frame[0] == 7, "Unexpected Spectrum report ID");
        ensure!(
            unsafe {
                HidD_GetFeature(self.handle.0, frame.as_mut_ptr().cast(), frame.len() as u32)
            },
            "Spectrum GetFeature failed"
        );
        Ok(())
    }
}

pub fn batteries() -> Result<Vec<serde_json::Value>> {
    WmiFacade::connect("ROOT\\CIMV2")?.query(
        "SELECT Name,BatteryStatus,EstimatedChargeRemaining,DesignVoltage FROM Win32_Battery",
    )
}
pub fn hid_inventory_read_only() -> Result<Vec<serde_json::Value>> {
    WmiFacade::connect("ROOT\\CIMV2")?
        .query("SELECT Name,PNPDeviceID,Status FROM Win32_PnPEntity WHERE PNPClass='HIDClass'")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hid_detail_packet_uses_path_offset_four_without_native_device_calls() {
        use windows_sys::Win32::Devices::DeviceAndDriverInstallation::SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        assert_eq!(size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>(), 8);
        let path = "\\\\?\\hid#fixture";
        let units: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let bytes = 4 + units.len() * 2;
        let mut words = vec![0u32; bytes.div_ceil(4)];
        words[0] = 8;
        unsafe {
            std::ptr::copy_nonoverlapping(
                units.as_ptr(),
                words.as_mut_ptr().cast::<u16>().add(2),
                units.len(),
            )
        };
        assert_eq!(detail_path(&words, bytes).unwrap(), path);
        assert!(detail_path(&words, 3).is_err());
    }
}
