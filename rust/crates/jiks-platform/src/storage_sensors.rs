//! Existing Windows StorNVMe SMART/health log queries; no vendor pass-through,
//! reset, threshold, firmware download, SMART enable or disk writes.
//! Protocol follows Microsoft winioctl.h and the fixed original DiskInfoToolkit
//! 87ffc498301c92ef61d2b8c3066928bf4cf51409 standard NVMe getter.
use crate::{
    aux_io::{Device, u32_at},
    domain::monotonic_ms,
    wmi_native::WmiFacade,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use windows_sys::Win32::Storage::FileSystem::FILE_READ_DATA;
const QUERY: u32 = 0x002d1400;
const DEVICE_NUMBER: u32 = 0x002d1080;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Disk {
    #[serde(rename(deserialize = "Index"))]
    pub index: u32,
    #[serde(rename(deserialize = "Model"))]
    pub model: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct DiskSample {
    pub index: u32,
    pub model: String,
    pub celsius: Option<f64>,
    pub sensor_celsius: Vec<Option<f64>>,
    pub source: String,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub requested_at_tick_ms: u64,
    pub completed_at_tick_ms: u64,
    pub disks: Vec<DiskSample>,
}
pub fn request(property: u32, namespace: u32) -> Result<Vec<u8>> {
    ensure!(
        [49, 50].contains(&property) && [0, 1, u32::MAX].contains(&namespace),
        "Only reviewed standard NVMe health queries are allowed"
    );
    let mut bytes = vec![0u8; 48 + 512];
    for (offset, value) in [
        (0, property),
        (8, 3),
        (12, 2),
        (16, 2),
        (20, namespace),
        (24, 40),
        (28, 512),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    Ok(bytes)
}
pub fn decode_response(bytes: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        bytes.len() >= 48 && u32_at(bytes, 0)? == 48 && u32_at(bytes, 4)? == 48,
        "STORAGE_PROTOCOL_DATA_DESCRIPTOR version/size mismatch"
    );
    ensure!(
        u32_at(bytes, 8)? == 3 && u32_at(bytes, 12)? == 2 && u32_at(bytes, 16)? == 2,
        "Storage response is not NVMe LogPage data"
    );
    let offset = u32_at(bytes, 24)? as usize;
    let length = u32_at(bytes, 28)? as usize;
    ensure!(
        offset >= 40 && length >= 512 && length <= 4096,
        "Invalid NVMe payload offset/length"
    );
    let begin = 8usize
        .checked_add(offset)
        .ok_or_else(|| anyhow::anyhow!("NVMe payload overflow"))?;
    let end = begin
        .checked_add(length)
        .ok_or_else(|| anyhow::anyhow!("NVMe payload overflow"))?;
    ensure!(
        end <= bytes.len(),
        "NVMe payload exceeds actual bytesReturned"
    );
    Ok(bytes[begin..end].to_vec())
}
pub fn temperature(kelvin: u16) -> Option<f64> {
    let c = kelvin as f64 - 273.15;
    (kelvin != 0 && kelvin != u16::MAX && c > 0. && c < 150.).then_some(c)
}
pub fn decode_health(bytes: &[u8]) -> Result<(Option<f64>, Vec<Option<f64>>)> {
    ensure!(
        bytes.len() >= 512,
        "NVMe health log must include all 512 standard bytes"
    );
    let primary = temperature(u16::from_le_bytes(bytes[1..3].try_into()?));
    let sensors = (200..216)
        .step_by(2)
        .map(|offset| temperature(u16::from_le_bytes([bytes[offset], bytes[offset + 1]])))
        .collect();
    Ok((primary, sensors))
}
pub trait Port {
    fn tick(&self) -> u64;
    fn disks(&mut self) -> Result<Vec<Disk>>;
    fn health(&mut self, disk: &Disk, deadline: u64) -> Result<Vec<u8>>;
}
pub fn sample_with(port: &mut impl Port, budget_ms: u64) -> Result<Snapshot> {
    ensure!(
        (1..=10_000).contains(&budget_ms),
        "Auxiliary storage budget out of bounds"
    );
    let requested = port.tick();
    let deadline = requested.saturating_add(budget_ms);
    let mut disks = port.disks()?;
    ensure!(disks.len() <= 64, "Storage inventory exceeds bound");
    disks.sort_by_key(|d| d.index);
    ensure!(
        disks.windows(2).all(|p| p[0].index != p[1].index)
            && disks
                .iter()
                .all(|d| d.index < 256 && !d.model.trim().is_empty()),
        "Disk identity is missing/ambiguous"
    );
    let mut out = Snapshot {
        requested_at_tick_ms: requested,
        completed_at_tick_ms: requested,
        disks: vec![],
    };
    for disk in disks.into_iter().take(2) {
        let mut row = DiskSample {
            index: disk.index,
            model: disk.model.clone(),
            celsius: None,
            sensor_celsius: vec![],
            source: "Windows IOCTL_STORAGE_QUERY_PROPERTY NVMe SMART/Health log composite Kelvin"
                .into(),
            error: None,
        };
        let result = (|| {
            ensure!(
                port.tick() < deadline,
                "Storage sample deadline expired; no later disk query was issued"
            );
            let bytes = port.health(&disk, deadline)?;
            ensure!(
                port.tick() < deadline,
                "Storage physical query completed after the sample deadline; value is not fresh"
            );
            decode_health(&bytes)
        })();
        match result {
            Ok((value, sensors)) => {
                row.celsius = value;
                row.sensor_celsius = sensors;
                if value.is_none() {
                    row.error=Some("NVMe composite temperature is missing or outside the original provider's (0,150) C range".into());
                }
            }
            Err(e) => row.error = Some(format!("{e:#}")),
        };
        out.disks.push(row);
    }
    out.completed_at_tick_ms = port.tick();
    Ok(out)
}
struct Native;
impl Port for Native {
    fn tick(&self) -> u64 {
        monotonic_ms()
    }
    fn disks(&mut self) -> Result<Vec<Disk>> {
        WmiFacade::connect("ROOT\\CIMV2")?.query("SELECT Index,Model FROM Win32_DiskDrive")
    }
    fn health(&mut self, disk: &Disk, deadline: u64) -> Result<Vec<u8>> {
        let device = Device::open(
            &format!("\\\\.\\PhysicalDrive{}", disk.index),
            FILE_READ_DATA,
        )?;
        ensure!(
            monotonic_ms() < deadline,
            "NVMe deadline expired before device-number query"
        );
        let number = device.query(DEVICE_NUMBER, &[], 12)?;
        ensure!(
            u32_at(&number, 4)? == disk.index,
            "Physical disk number changed; refusing mismatched sample"
        );
        let mut descriptor_query = [0u8; 12];
        descriptor_query[..4].copy_from_slice(&0u32.to_le_bytes());
        ensure!(
            monotonic_ms() < deadline,
            "NVMe deadline expired before bus descriptor query"
        );
        let descriptor = device.query(QUERY, &descriptor_query, 4096)?;
        ensure!(
            descriptor.len() >= 36 && u32_at(&descriptor, 28)? == 17,
            "Disk is not reported as NVMe by the Windows storage descriptor"
        );
        let mut errors = Vec::new();
        for namespace in [u32::MAX, 1, 0] {
            for property in [49, 50] {
                ensure!(
                    monotonic_ms() < deadline,
                    "NVMe query deadline expired before a later protocol request"
                );
                match device
                    .query(QUERY, &request(property, namespace)?, 48 + 512)
                    .and_then(|bytes| decode_response(&bytes))
                {
                    Ok(log) => return Ok(log),
                    Err(error) => {
                        errors.push(format!("property{property}/ns{namespace}: {error:#}"))
                    }
                }
            }
        }
        anyhow::bail!(
            "No standard NVMe health log response: {}",
            errors.join("; ")
        )
    }
}
pub fn read() -> Result<Snapshot> {
    sample_with(&mut Native, 2000)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_is_only_health_read_and_validates_actual_returned_buffer() {
        let mut response = request(49, u32::MAX).unwrap();
        assert_eq!(u32_at(&response, 16).unwrap(), 2);
        response[..4].copy_from_slice(&48u32.to_le_bytes());
        response[4..8].copy_from_slice(&48u32.to_le_bytes());
        response[49..51].copy_from_slice(&313u16.to_le_bytes());
        assert_eq!(decode_response(&response).unwrap().len(), 512);
        let (temp, sub) = decode_health(&decode_response(&response).unwrap()).unwrap();
        assert!((temp.unwrap() - 39.85).abs() < 1e-9);
        assert!(sub.iter().all(Option::is_none));
        assert!(decode_response(&response[..559]).is_err());
        response[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_response(&response).is_err());
        assert!(request(50, 2).is_err());
        assert!(request(0, 0).is_err());
        assert_eq!(temperature(0), None);
        assert_eq!(temperature(u16::MAX), None);
    }
    #[test]
    fn deadline_equal_stops_later_physical_queries_and_preserves_slot_errors() {
        struct Fake {
            tick: std::cell::Cell<u64>,
            calls: Vec<u32>,
        }
        impl Port for Fake {
            fn tick(&self) -> u64 {
                self.tick.get()
            }
            fn disks(&mut self) -> Result<Vec<Disk>> {
                Ok(vec![
                    Disk {
                        index: 1,
                        model: "NVMe2".into(),
                    },
                    Disk {
                        index: 0,
                        model: "NVMe1".into(),
                    },
                ])
            }
            fn health(&mut self, disk: &Disk, _: u64) -> Result<Vec<u8>> {
                self.calls.push(disk.index);
                self.tick.set(101);
                let mut log = vec![0; 512];
                log[1..3].copy_from_slice(&300u16.to_le_bytes());
                Ok(log)
            }
        }
        let mut f = Fake {
            tick: std::cell::Cell::new(1),
            calls: vec![],
        };
        let sample = sample_with(&mut f, 100).unwrap();
        assert_eq!(f.calls, vec![0]);
        assert_eq!(sample.disks.len(), 2);
        assert!(
            sample.disks[0].celsius.is_none()
                && sample.disks[0].error.as_ref().unwrap().contains("deadline")
        );
        assert!(
            sample.disks[1].celsius.is_none()
                && sample.disks[1].error.as_ref().unwrap().contains("deadline")
        );
        assert_eq!(sample.completed_at_tick_ms, 101);
    }
}
