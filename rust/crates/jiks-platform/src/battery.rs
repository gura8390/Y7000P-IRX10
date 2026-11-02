//! Battery class-driver queries and a separately opt-in Lenovo thermal query.
//! No battery SET_INFORMATION or charge policy write is exposed here.
use crate::{
    aux_io::{Device, u32_at},
    domain::monotonic_ms,
};
use anyhow::{Result, ensure};
use serde::Serialize;
use windows_sys::{
    Win32::{Storage::FileSystem::FILE_READ_DATA, System::Power::*},
    core::GUID,
};
#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub slot: u32,
    pub tag: u32,
    pub capacity_unit: String,
    pub capacity: Option<u32>,
    pub designed_capacity: Option<u32>,
    pub full_charged_capacity: Option<u32>,
    pub cycle_count: Option<u32>,
    pub health_percent: Option<f64>,
    pub rate: Option<i32>,
    pub rate_unit: String,
    pub voltage_millivolts: Option<u32>,
    pub power_state: u32,
}
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub requested_at_tick_ms: u64,
    pub completed_at_tick_ms: u64,
    pub batteries: Vec<Sample>,
    pub errors: Vec<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Thermal {
    pub requested_at_tick_ms: u64,
    pub completed_at_tick_ms: u64,
    pub celsius: Option<f64>,
    pub manufacture_date: Option<String>,
    pub first_use_date: Option<String>,
    pub energy_index: Option<u32>,
    pub errors: Vec<String>,
}
fn capacity(value: u32) -> Option<u32> {
    (value != u32::MAX).then_some(value)
}
pub fn decode(slot: u32, tag: u32, info: &[u8], status: &[u8]) -> Result<Sample> {
    ensure!(
        tag != 0 && info.len() == 36 && status.len() == 16,
        "Battery tag/INFORMATION/STATUS ABI mismatch"
    );
    let flags = u32_at(info, 0)?;
    ensure!(
        flags & BATTERY_SYSTEM_BATTERY != 0,
        "This battery-class interface is not a system battery"
    );
    let relative = flags & BATTERY_CAPACITY_RELATIVE != 0;
    let designed = capacity(u32_at(info, 12)?);
    let full = capacity(u32_at(info, 16)?);
    let cycle = capacity(u32_at(info, 32)?);
    let rate = u32_at(status, 12)? as i32;
    let health = designed
        .zip(full)
        .filter(|(d, _)| *d > 0)
        .map(|(d, f)| f as f64 * 100. / d as f64);
    Ok(Sample {
        slot,
        tag,
        capacity_unit: if relative { "relative" } else { "mWh" }.into(),
        capacity: capacity(u32_at(status, 4)?),
        designed_capacity: designed,
        full_charged_capacity: full,
        cycle_count: cycle,
        health_percent: health,
        rate: (rate != i32::MIN).then_some(rate),
        rate_unit: if relative { "relative/hour" } else { "mW" }.into(),
        voltage_millivolts: capacity(u32_at(status, 8)?),
        power_state: u32_at(status, 0)?,
    })
}
fn deadline(tick: u64) -> Result<()> {
    ensure!(
        monotonic_ms() < tick,
        "Auxiliary battery query deadline expired before a later IOCTL"
    );
    Ok(())
}
pub fn read_information() -> Result<Snapshot> {
    let requested = monotonic_ms();
    let end = requested.saturating_add(2000);
    let guid = GUID::from_u128(0x72631e54_78a4_11d0_bcf7_00aa00b7b32a);
    let paths = crate::aux_io::interfaces(&guid)?;
    let mut out = Snapshot {
        requested_at_tick_ms: requested,
        completed_at_tick_ms: requested,
        batteries: vec![],
        errors: vec![],
    };
    for (slot, path) in paths.into_iter().take(4).enumerate() {
        let result = (|| {
            deadline(end)?;
            let device = Device::open(&path, FILE_READ_DATA)?;
            let tag = device.query(IOCTL_BATTERY_QUERY_TAG, &0u32.to_le_bytes(), 4)?;
            let tag = u32_at(&tag, 0)?;
            ensure!(tag != 0, "No battery is present in this slot");
            let mut query = [0u8; 12];
            query[..4].copy_from_slice(&tag.to_le_bytes());
            deadline(end)?;
            let info = device.query(IOCTL_BATTERY_QUERY_INFORMATION, &query, 36)?;
            let mut wait = [0u8; 20];
            wait[..4].copy_from_slice(&tag.to_le_bytes());
            deadline(end)?;
            let status = device.query(IOCTL_BATTERY_QUERY_STATUS, &wait, 16)?;
            deadline(end)?;
            let after = device.query(IOCTL_BATTERY_QUERY_TAG, &0u32.to_le_bytes(), 4)?;
            deadline(end)?;
            ensure!(
                u32_at(&after, 0)? == tag,
                "Battery tag changed during the snapshot; cached capacity/health discarded"
            );
            decode(slot as u32, tag, &info, &status)
        })();
        match result {
            Ok(sample) => out.batteries.push(sample),
            Err(error) => out.errors.push(format!("slot{slot}: {error:#}")),
        }
    }
    out.completed_at_tick_ms = monotonic_ms();
    Ok(out)
}
pub fn decode_date(raw: u16) -> Option<String> {
    if raw == 0 {
        return None;
    }
    let year = (raw >> 9) + 1980;
    let month = (raw >> 5) & 15;
    let day = raw & 31;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    (year >= 2018 && year <= 2030 && day >= 1 && day <= max)
        .then(|| format!("{year:04}-{month:02}-{day:02}"))
}
pub fn decode_lenovo(bytes: &[u8]) -> Result<Option<(f64, Option<String>, Option<String>)>> {
    ensure!(
        bytes.len() == 84,
        "Lenovo battery information must have original sequential 84-byte layout"
    );
    let raw = u16::from_le_bytes(bytes[14..16].try_into()?);
    if raw == 0 || raw == u16::MAX {
        return Ok(None);
    }
    let c = (raw as f64 - 2731.6) / 10.;
    ensure!(
        (0.0..150.0).contains(&c),
        "Invalid Lenovo battery temperature"
    );
    Ok(Some((
        c,
        decode_date(u16::from_le_bytes(bytes[16..18].try_into()?)),
        decode_date(u16::from_le_bytes(bytes[18..20].try_into()?)),
    )))
}
pub fn read_temperature() -> Result<Thermal> {
    let requested = monotonic_ms();
    let end = requested.saturating_add(2000);
    let device = Device::open(
        "\\\\.\\EnergyDrv",
        FILE_READ_DATA | windows_sys::Win32::Storage::FileSystem::FILE_WRITE_DATA,
    )?;
    let mut out = Thermal {
        requested_at_tick_ms: requested,
        completed_at_tick_ms: requested,
        celsius: None,
        manufacture_date: None,
        first_use_date: None,
        energy_index: None,
        errors: vec![],
    };
    for index in 0..3u32 {
        let result = (|| {
            deadline(end)?;
            let bytes = device.query(0x83102138, &index.to_le_bytes(), 84)?;
            deadline(end)?;
            decode_lenovo(&bytes)
        })();
        match result {
            Ok(Some((temp, manufactured, first))) => {
                out.celsius = Some(temp);
                out.manufacture_date = manufactured;
                out.first_use_date = first;
                out.energy_index = Some(index);
                break;
            }
            Ok(None) => {}
            Err(e) => out.errors.push(format!("energy index{index}: {e:#}")),
        }
    }
    out.completed_at_tick_ms = monotonic_ms();
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn battery_layout_unknown_units_and_signed_discharge_are_preserved() {
        assert_eq!(std::mem::size_of::<BATTERY_INFORMATION>(), 36);
        assert_eq!(std::mem::size_of::<BATTERY_STATUS>(), 16);
        assert_eq!(std::mem::size_of::<BATTERY_QUERY_INFORMATION>(), 12);
        assert_eq!(std::mem::size_of::<BATTERY_WAIT_STATUS>(), 20);
        let mut info = [0u8; 36];
        info[..4].copy_from_slice(&BATTERY_SYSTEM_BATTERY.to_le_bytes());
        info[12..16].copy_from_slice(&80000u32.to_le_bytes());
        info[16..20].copy_from_slice(&76000u32.to_le_bytes());
        info[32..].copy_from_slice(&125u32.to_le_bytes());
        let mut status = [0u8; 16];
        status[4..8].copy_from_slice(&38000u32.to_le_bytes());
        status[8..12].copy_from_slice(&15000u32.to_le_bytes());
        status[12..].copy_from_slice(&(-20000i32).to_le_bytes());
        let sample = decode(0, 1, &info, &status).unwrap();
        assert_eq!(sample.health_percent, Some(95.));
        assert_eq!(sample.rate, Some(-20000));
        assert_eq!(sample.cycle_count, Some(125));
        info[..4]
            .copy_from_slice(&(BATTERY_SYSTEM_BATTERY | BATTERY_CAPACITY_RELATIVE).to_le_bytes());
        status[12..].copy_from_slice(&i32::MIN.to_le_bytes());
        assert_eq!(
            decode(0, 1, &info, &status).unwrap().capacity_unit,
            "relative"
        );
        assert_eq!(decode(0, 1, &info, &status).unwrap().rate, None);
        assert!(decode(0, 0, &info, &status).is_err());
    }
    #[test]
    fn lenovo_temperature_and_dates_use_alignment_not_pack1() {
        let mut bytes = [0u8; 84];
        bytes[14..16].copy_from_slice(&3032u16.to_le_bytes());
        bytes[16..18].copy_from_slice(&(((2026 - 1980) << 9) | (11 << 5) | 3u16).to_le_bytes());
        let (temp, date, _) = decode_lenovo(&bytes).unwrap().unwrap();
        assert!((temp - 30.04).abs() < 1e-8);
        assert_eq!(date.as_deref(), Some("2026-11-03"));
        assert!(decode_lenovo(&bytes[..83]).is_err());
        assert!(decode_date(((2026 - 1980) << 9) | (2 << 5) | 30).is_none());
        bytes[14..16].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(decode_lenovo(&bytes).unwrap().is_none());
    }
}
