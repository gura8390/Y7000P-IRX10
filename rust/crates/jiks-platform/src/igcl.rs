//! Explicit read-only Intel Graphics Control Library adapter. ABI follows
//! Intel's igcl_api.h (v1.1) and official PROG_init LUID identity example:
//! https://intel.github.io/drivers.gpu.control-library/Control/PROG_init.html
//! No control, overclock, fan, voltage, power-limit or driver-install API exists here.
use crate::{gpu_sensors::GpuSample, intel_gpu::IntelGpuIdentity, ownership::wide};
use anyhow::{Context, Result, ensure};
use std::{ffi::c_void, marker::PhantomData, mem::size_of, ptr, rc::Rc};
use windows_sys::Win32::{
    Foundation::{FreeLibrary, HMODULE, LUID},
    System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
};
#[repr(C)]
#[derive(Default)]
struct InitArgs {
    size: u32,
    version: u8,
    app_version: u32,
    flags: u32,
    supported_version: u32,
    application_uid: [u32; 4],
}
#[repr(C)]
struct Properties {
    size: u32,
    version: u8,
    device_id: *mut c_void,
    device_id_size: u32,
    device_type: u32,
    subfunctions: u32,
    driver_version: u64,
    firmware: [u64; 3],
    vendor_id: u32,
    device: u32,
    revision: u32,
    eus: u32,
    subslices: u32,
    slices: u32,
    name: [u8; 100],
    flags: u32,
    frequency: u32,
    subsys: u16,
    subsys_vendor: u16,
    bdf: [u8; 3],
    xe_cores: u32,
    reserved: [u8; 112],
}
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Item {
    pub supported: u8,
    pub units: u32,
    pub kind: u32,
    pub bits: u64,
}
#[repr(C)]
#[derive(Default)]
struct Psu {
    supported: u8,
    kind: u32,
    energy: Item,
    voltage: Item,
}
#[repr(C)]
#[derive(Default)]
pub struct Telemetry {
    size: u32,
    version: u8,
    pub gpu: [Item; 8],
    gpu_limited: [u8; 5],
    pub vram: [Item; 7],
    vram_limited: [u8; 5],
    total_card_energy: Item,
    psu: [Psu; 5],
    fan_speed: [Item; 5],
    tail: [Item; 9],
}
impl Item {
    pub fn value(self, unit: u32) -> Result<Option<f64>> {
        if self.supported == 0 {
            return Ok(None);
        }
        ensure!(
            self.supported == 1 && self.units == unit,
            "Unsupported IGCL boolean/unit encoding"
        );
        let v = match self.kind {
            0 => self.bits as i8 as f64,
            1 => self.bits as u8 as f64,
            2 => self.bits as i16 as f64,
            3 => self.bits as u16 as f64,
            4 => self.bits as i32 as f64,
            5 => self.bits as u32 as f64,
            6 => self.bits as i64 as f64,
            7 => self.bits as f64,
            8 => f32::from_bits(self.bits as u32) as f64,
            9 => f64::from_bits(self.bits),
            _ => anyhow::bail!("IGCL telemetry value is not numeric"),
        };
        ensure!(v.is_finite() && v >= 0., "Invalid IGCL numeric value");
        Ok(Some(v))
    }
}
type Init = unsafe extern "C" fn(*mut InitArgs, *mut *mut c_void) -> u32;
type Close = unsafe extern "C" fn(*mut c_void) -> u32;
type Enumerate = unsafe extern "C" fn(*mut c_void, *mut u32, *mut *mut c_void) -> u32;
type GetProperties = unsafe extern "C" fn(*mut c_void, *mut Properties) -> u32;
type GetTelemetry = unsafe extern "C" fn(*mut c_void, *mut Telemetry) -> u32;
fn checked(code: u32) -> Result<()> {
    ensure!(code == 0, "IGCL native status {code:#x}");
    Ok(())
}
struct Session {
    module: HMODULE,
    api: *mut c_void,
    device: *mut c_void,
    close: Option<Close>,
    telemetry: GetTelemetry,
    _thread: PhantomData<Rc<()>>,
}
impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            if let Some(f) = self.close {
                f(self.api);
            }
            FreeLibrary(self.module);
        }
    }
}
impl Session {
    fn open(identity: &IntelGpuIdentity) -> Result<Self> {
        let path = wide("ControlLib.dll")?;
        let module =
            unsafe { LoadLibraryExW(path.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        ensure!(
            !module.is_null(),
            "System Intel ControlLib.dll unavailable; IGCL getter is implemented but not available in this driver install"
        );
        // Bootstrap resolution must also release the DLL on error.
        struct Library(HMODULE);
        impl Drop for Library {
            fn drop(&mut self) {
                unsafe {
                    FreeLibrary(self.0);
                }
            }
        }
        let library = Library(module);
        let resolve = |name: &[u8]| {
            unsafe { GetProcAddress(module, name.as_ptr()) }
                .context("Required Intel read-only API is absent")
        };
        let init: Init = unsafe { std::mem::transmute(resolve(b"ctlInit\0")?) };
        let close: Close = unsafe { std::mem::transmute(resolve(b"ctlClose\0")?) };
        let enumerate: Enumerate =
            unsafe { std::mem::transmute(resolve(b"ctlEnumerateDevices\0")?) };
        let properties: GetProperties =
            unsafe { std::mem::transmute(resolve(b"ctlGetDeviceProperties\0")?) };
        let telemetry: GetTelemetry =
            unsafe { std::mem::transmute(resolve(b"ctlPowerTelemetryGet\0")?) };
        let mut args = InitArgs {
            size: size_of::<InitArgs>() as u32,
            app_version: 0x0001_0001,
            flags: 1,
            ..Default::default()
        };
        let mut api = ptr::null_mut();
        checked(unsafe { init(&mut args, &mut api) })?;
        ensure!(!api.is_null(), "Null Intel API handle");
        std::mem::forget(library);
        let mut session = Self {
            module,
            api,
            device: ptr::null_mut(),
            close: Some(close),
            telemetry,
            _thread: PhantomData,
        };
        let mut count = 0;
        checked(unsafe { enumerate(api, &mut count, ptr::null_mut()) })?;
        ensure!(
            count > 0 && count <= 64,
            "Intel API adapter count out of bounds"
        );
        let mut handles = vec![ptr::null_mut(); count as usize];
        let capacity = count;
        checked(unsafe { enumerate(api, &mut count, handles.as_mut_ptr()) })?;
        ensure!(
            count <= capacity,
            "Intel device list grew beyond preallocated capacity"
        );
        handles.truncate(count as usize);
        let mut matches = Vec::new();
        for handle in handles {
            ensure!(!handle.is_null(), "Null Intel device handle");
            let mut luid = LUID::default();
            let mut p: Properties = unsafe { std::mem::zeroed() };
            p.size = size_of::<Properties>() as u32;
            p.version = 2;
            p.device_id = (&mut luid as *mut LUID).cast();
            p.device_id_size = size_of::<LUID>() as u32;
            checked(unsafe { properties(handle, &mut p) })?;
            if p.device_type == 1
                && p.vendor_id == 0x8086
                && p.flags & 1 != 0
                && luid.LowPart == identity.luid_low
                && luid.HighPart == identity.luid_high
            {
                matches.push(handle);
            }
        }
        ensure!(
            matches.len() == 1,
            "Intel IGCL LUID identity absent/ambiguous; no first-integrated-device fallback"
        );
        session.device = matches[0];
        Ok(session)
    }
    fn read(&self) -> Result<Telemetry> {
        let mut v = Telemetry {
            size: size_of::<Telemetry>() as u32,
            version: 1,
            ..Default::default()
        };
        checked(unsafe { (self.telemetry)(self.device, &mut v) })?;
        ensure!(
            v.size as usize == size_of::<Telemetry>() && v.version == 1,
            "Intel telemetry ABI changed"
        );
        Ok(v)
    }
}
#[derive(Default)]
pub(crate) struct IgclSampler {
    session: Option<Session>,
    attempted: bool,
    open_error: Option<String>,
    energy: Option<(u64, f64)>,
}
impl IgclSampler {
    pub(crate) fn supplement(
        &mut self,
        id: &IntelGpuIdentity,
        sample: &mut GpuSample,
    ) -> Result<()> {
        if !self.attempted {
            self.attempted = true;
            match Session::open(id) {
                Ok(s) => self.session = Some(s),
                Err(e) => self.open_error = Some(format!("{e:#}")),
            }
        }
        let Some(session) = self.session.as_ref() else {
            anyhow::bail!(
                "{}",
                self.open_error
                    .as_deref()
                    .unwrap_or("Intel sensor session missing")
            );
        };
        let data = session.read()?;
        project(&data, sample, &mut self.energy)
    }
}
fn project(
    data: &Telemetry,
    sample: &mut GpuSample,
    energy: &mut Option<(u64, f64)>,
) -> Result<()> {
    for (key, item, unit, label) in [
        ("GpuVoltage", data.gpu[2], 3, "V"),
        ("GpuFrequency", data.gpu[3], 0, "MHz"),
        ("GpuCoreTemperature", data.gpu[4], 5, "°C"),
        ("GpuMemoryFrequency", data.vram[2], 0, "MHz"),
        ("GpuVramTemperature", data.vram[6], 5, "°C"),
        ("GpuEffectiveFrequency", data.tail[3], 0, "MHz"),
    ] {
        match item.value(unit) {
            Ok(Some(value)) => {
                ensure!(
                    unit != 5 || (value > 0. && value < 150.),
                    "Invalid Intel GPU temperature"
                );
                sample.insert(
                    key,
                    value,
                    label,
                    "Intel IGCL native telemetry, exact adapter LUID",
                )?;
            }
            Ok(None) => sample.error(
                key,
                label,
                &anyhow::anyhow!("IGCL getter returned bSupported=false"),
            ),
            Err(e) => sample.error(key, label, &e),
        }
    }
    if let Some(joules) = data.gpu[1].value(6)? {
        if let Some((tick, prior)) = energy.replace((sample.requested_at_tick_ms, joules)) {
            if let Some(watts) = energy_watts(tick, prior, sample.requested_at_tick_ms, joules) {
                sample.insert(
                    "GpuPower",
                    watts,
                    "W",
                    "Intel IGCL monotonic GPU energy-counter delta",
                )?;
            }
        }
    }
    for (i, item) in data.fan_speed.iter().copied().enumerate() {
        if let Some(rpm) = item.value(9)? {
            sample.insert(
                &format!("GpuFan{i}Speed"),
                rpm,
                "RPM",
                "Intel IGCL fan telemetry",
            )?;
            if i == 0 {
                sample.insert("GpuFanSpeed", rpm, "RPM", "Intel IGCL fan telemetry")?;
            }
        }
    }
    Ok(())
}
pub fn energy_watts(start: u64, before: f64, end: u64, after: f64) -> Option<f64> {
    let ms = end.checked_sub(start)?;
    if !(10..=3000).contains(&ms) || !before.is_finite() || !after.is_finite() || after < before {
        return None;
    }
    let power = (after - before) / (ms as f64 / 1000.);
    (power.is_finite() && power >= 0. && power < 1000.).then_some(power)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn item(unit: u32, value: f64) -> Item {
        Item {
            supported: 1,
            units: unit,
            kind: 9,
            bits: value.to_bits(),
        }
    }
    #[test]
    fn official_c_bool_layout_and_versioned_sizes_are_not_dotnet_bool_marshalling() {
        assert_eq!(size_of::<InitArgs>(), 36);
        assert_eq!(size_of::<Properties>(), 320);
        assert_eq!(size_of::<Item>(), 24);
        assert_eq!(size_of::<Telemetry>(), 1024);
        assert_eq!(std::mem::offset_of!(Telemetry, vram), 208);
        assert_eq!(std::mem::offset_of!(Properties, flags), 188);
        assert_eq!(std::mem::offset_of!(Telemetry, fan_speed), 688);
    }
    #[test]
    fn intel_supported_flags_units_and_energy_delta_are_exact() {
        let mut t = Telemetry::default();
        t.gpu[2] = item(3, 0.8);
        t.gpu[3] = item(0, 1200.);
        t.gpu[4] = item(5, 45.);
        t.gpu[1] = item(6, 10.);
        let mut s = GpuSample::new(0, "Intel fake", 100);
        s.is_igpu = true;
        let mut energy = None;
        project(&t, &mut s, &mut energy).unwrap();
        assert_eq!(s.values["GpuCoreTemperature"].value, Some(45.));
        assert!(s.values["GpuVramTemperature"].value.is_none());
        assert!(!s.values.contains_key("GpuPower"));
        t.gpu[1] = item(6, 11.5);
        s.requested_at_tick_ms = 200;
        project(&t, &mut s, &mut energy).unwrap();
        assert_eq!(s.values["GpuPower"].value, Some(15.));
        assert!(item(3, 1.).value(5).is_err());
        assert!(item(3, f64::NAN).value(3).is_err());
        assert_eq!(energy_watts(0, 10., 100, 9.), None);
        assert_eq!(energy_watts(100, 10., 5000, 11.), None);
    }
}
