// SPDX-License-Identifier: MPL-2.0
//! Whole-adapter Intel GPU statistics. Ported from the fixed LHM
//! D3DDisplayDevice/IntelIntegratedGpu algorithm; no current-process DXGI usage.
//! DXGI enumeration creates no D3D device/context. Statistics use null hProcess.
use crate::{domain::monotonic_ms, gpu_sensors::GpuSample};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, mem::size_of, ptr};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows_sys::{Wdk::Graphics::Direct3D::*, Win32::Foundation::LUID};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntelGpuIdentity {
    pub index: u32,
    pub name: String,
    pub luid_low: u32,
    pub luid_high: i32,
    pub is_igpu: bool,
    pub vendor_id: u32,
    pub device_id: u32,
}
fn nt(status: i32) -> Result<()> {
    ensure!(status == 0, "D3DKMT getter failed NTSTATUS {status:#x}");
    Ok(())
}
struct Adapter(u32);
impl Drop for Adapter {
    fn drop(&mut self) {
        unsafe {
            D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER { hAdapter: self.0 });
        }
    }
}
impl Adapter {
    fn open(luid: LUID) -> Result<Self> {
        let mut request = D3DKMT_OPENADAPTERFROMLUID {
            AdapterLuid: luid,
            hAdapter: 0,
        };
        nt(unsafe { D3DKMTOpenAdapterFromLuid(&mut request) })?;
        ensure!(request.hAdapter != 0, "D3DKMT returned null adapter");
        Ok(Self(request.hAdapter))
    }
    fn info<T: Default>(&self, kind: KMTQUERYADAPTERINFOTYPE) -> Result<T> {
        let mut value = T::default();
        let mut request = D3DKMT_QUERYADAPTERINFO {
            hAdapter: self.0,
            Type: kind,
            pPrivateDriverData: (&mut value as *mut T).cast(),
            PrivateDriverDataSize: size_of::<T>() as u32,
        };
        nt(unsafe { D3DKMTQueryAdapterInfo(&mut request) })?;
        Ok(value)
    }
}
fn integrated_bits(bits: u32) -> bool {
    bits & (1 << 5) != 0 && bits & (1 << 2) == 0
}
pub fn inventory() -> Result<Vec<IntelGpuIdentity>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut out = Vec::new();
    let mut ended = false;
    for index in 0..64 {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(a) => a,
            Err(e) if e.code().0 == 0x887a0002u32 as i32 => {
                ended = true;
                break;
            }
            Err(e) => return Err(e.into()),
        };
        let d = unsafe { adapter.GetDesc1()? };
        if d.VendorId != 0x8086 {
            continue;
        }
        let luid = LUID {
            LowPart: d.AdapterLuid.LowPart,
            HighPart: d.AdapterLuid.HighPart,
        };
        let handle = Adapter::open(luid)?;
        let ty: D3DKMT_ADAPTERTYPE = handle.info(KMTQAITYPE_ADAPTERTYPE)?;
        let bits = unsafe { ty.Anonymous.Value };
        if !integrated_bits(bits) {
            continue;
        }
        let end = d
            .Description
            .iter()
            .position(|v| *v == 0)
            .context("Unterminated Intel DXGI name")?;
        out.push(IntelGpuIdentity {
            index,
            name: String::from_utf16(&d.Description[..end])?,
            luid_low: luid.LowPart,
            luid_high: luid.HighPart,
            is_igpu: true,
            vendor_id: d.VendorId,
            device_id: d.DeviceId,
        });
    }
    ensure!(ended, "Intel DXGI enumeration exceeds bound");
    Ok(out)
}
#[derive(Debug, Clone)]
pub struct Segment {
    pub aperture: bool,
    pub resident: u64,
    pub committed: u64,
}
#[derive(Debug, Clone)]
pub struct Statistics {
    pub tick_ms: u64,
    pub shared_limit: u64,
    pub dedicated_limit: u64,
    pub segments: Vec<Segment>,
    pub node_running_100ns: BTreeMap<u32, i64>,
}
pub trait StatisticsPort {
    fn snapshot(&mut self) -> Result<Statistics>;
}
#[derive(Default)]
pub struct History {
    last: Option<Statistics>,
}
pub fn project_statistics(
    identity: &IntelGpuIdentity,
    s: Statistics,
    h: &mut History,
) -> Result<GpuSample> {
    let mut sample = GpuSample::new(identity.index, &identity.name, s.tick_ms);
    sample.is_igpu = true;
    let sum = |aperture| {
        s.segments
            .iter()
            .filter(|v| v.aperture == aperture)
            .try_fold(0u64, |n, v| {
                n.checked_add(v.resident)
                    .context("GPU segment byte count overflow")
            })
    };
    let shared = sum(true)?;
    let dedicated = sum(false)?;
    let gib = 1_073_741_824.;
    sample.insert(
        "GpuSharedMemoryUsed",
        shared as f64 / gib,
        "GB",
        "D3DKMT global aperture-segment resident bytes",
    )?;
    sample.insert(
        "GpuDedicatedMemoryUsed",
        dedicated as f64 / gib,
        "GB",
        "D3DKMT global non-aperture resident bytes",
    )?;
    if s.shared_limit > 0 {
        ensure!(
            shared <= s.shared_limit,
            "Shared GPU resident bytes exceed adapter capacity"
        );
        sample.insert(
            "GpuSharedMemoryTotal",
            s.shared_limit as f64 / gib,
            "GB",
            "D3DKMT adapter shared-system-memory capacity",
        )?;
        crate::gpu_sensors::insert_memory(
            &mut sample,
            s.shared_limit,
            shared,
            "Intel shared GPU memory, whole-adapter D3DKMT bytes",
        )?;
    }
    if let Some(previous) = h.last.as_ref() {
        let elapsed = s
            .tick_ms
            .checked_sub(previous.tick_ms)
            .filter(|v| *v >= 10 && *v <= 3000);
        if let Some(ms) = elapsed {
            let mut maximum: Option<f64> = None;
            for (id, current) in &s.node_running_100ns {
                if let Some(prior) = previous.node_running_100ns.get(id) {
                    if let Some(delta) = current.checked_sub(*prior).filter(|v| *v >= 0) {
                        let load = delta as f64 / (ms as f64 * 10_000.) * 100.;
                        if load <= 100.01 {
                            let load = load.min(100.);
                            sample.insert(
                                &format!("GpuEngine{id}Utilization"),
                                load,
                                "%",
                                "D3DKMT global engine running-time delta, monotonic interval",
                            )?;
                            maximum = Some(maximum.map_or(load, |m| m.max(load)));
                        }
                    }
                }
            }
            if let Some(load) = maximum {
                sample.insert(
                    "GpuUtilization",
                    load,
                    "%",
                    "D3DKMT busiest global engine (not sum of concurrent engines)",
                )?;
            }
        }
    }
    h.last = Some(s);
    Ok(sample)
}
struct NativeStatistics {
    adapter: Adapter,
    luid: LUID,
}
impl NativeStatistics {
    fn query(
        &self,
        kind: D3DKMT_QUERYSTATISTICS_TYPE,
        index: u32,
    ) -> Result<D3DKMT_QUERYSTATISTICS_RESULT> {
        let mut q = D3DKMT_QUERYSTATISTICS {
            Type: kind,
            AdapterLuid: self.luid,
            hProcess: ptr::null_mut(),
            ..Default::default()
        };
        if kind == D3DKMT_QUERYSTATISTICS_SEGMENT {
            q.Anonymous.QuerySegment = D3DKMT_QUERYSTATISTICS_QUERY_SEGMENT { SegmentId: index };
        } else if kind == D3DKMT_QUERYSTATISTICS_NODE {
            q.Anonymous.QueryNode = D3DKMT_QUERYSTATISTICS_QUERY_NODE { NodeId: index };
        }
        nt(unsafe { D3DKMTQueryStatistics(&mut q) })?;
        Ok(q.QueryResult)
    }
}
impl StatisticsPort for NativeStatistics {
    fn snapshot(&mut self) -> Result<Statistics> {
        let tick_ms = monotonic_ms();
        let adapter = unsafe {
            self.query(D3DKMT_QUERYSTATISTICS_ADAPTER, 0)?
                .AdapterInformation
        };
        ensure!(
            adapter.NbSegments <= 256 && adapter.NodeCount <= 256,
            "D3DKMT adapter counts exceed bound"
        );
        let sizes: D3DKMT_SEGMENTSIZEINFO = self.adapter.info(KMTQAITYPE_GETSEGMENTSIZE)?;
        let mut s = Statistics {
            tick_ms,
            shared_limit: sizes.SharedSystemMemorySize,
            dedicated_limit: sizes.DedicatedVideoMemorySize,
            segments: Vec::new(),
            node_running_100ns: BTreeMap::new(),
        };
        for index in 0..adapter.NbSegments {
            let segment = unsafe {
                self.query(D3DKMT_QUERYSTATISTICS_SEGMENT, index)?
                    .SegmentInformation
            };
            ensure!(segment.Aperture <= 1, "Unknown GPU aperture enum");
            s.segments.push(Segment {
                aperture: segment.Aperture == 1,
                resident: segment.BytesResident,
                committed: segment.BytesCommitted,
            });
        }
        for index in 0..adapter.NodeCount {
            let node = unsafe {
                self.query(D3DKMT_QUERYSTATISTICS_NODE, index)?
                    .NodeInformation
            };
            ensure!(
                node.GlobalInformation.RunningTime >= 0,
                "Negative GPU running-time counter"
            );
            s.node_running_100ns
                .insert(index, node.GlobalInformation.RunningTime);
        }
        Ok(s)
    }
}
#[derive(Default)]
pub(crate) struct IntelGpuSampler {
    identity: Option<IntelGpuIdentity>,
    native: Option<NativeStatistics>,
    history: History,
    igcl: crate::igcl::IgclSampler,
}
impl IntelGpuSampler {
    pub(crate) fn sample(&mut self, low: u32, high: i32, name: &str) -> Result<GpuSample> {
        if self
            .identity
            .as_ref()
            .is_none_or(|i| i.luid_low != low || i.luid_high != high || i.name != name)
        {
            self.native = None;
            self.identity = None;
            self.history = Default::default();
            self.igcl = Default::default();
            let matching = inventory()?
                .into_iter()
                .filter(|i| i.luid_low == low && i.luid_high == high && i.name == name)
                .collect::<Vec<_>>();
            ensure!(
                matching.len() == 1,
                "Selected Intel integrated GPU identity is absent/ambiguous"
            );
            let identity = matching.into_iter().next().unwrap();
            let luid = LUID {
                LowPart: low,
                HighPart: high,
            };
            self.native = Some(NativeStatistics {
                adapter: Adapter::open(luid)?,
                luid,
            });
            self.identity = Some(identity);
        }
        let identity = self.identity.as_ref().unwrap();
        let stats = self.native.as_mut().unwrap().snapshot()?;
        let mut sample = project_statistics(identity, stats, &mut self.history)?;
        if let Err(e) = self.igcl.supplement(identity, &mut sample) {
            sample.errors.insert("igcl".into(), format!("{e:#}"));
        }
        sample.completed_at_tick_ms = monotonic_ms();
        Ok(sample)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> IntelGpuIdentity {
        IntelGpuIdentity {
            index: 0,
            name: "Intel fake".into(),
            luid_low: 7,
            luid_high: 0,
            is_igpu: true,
            vendor_id: 0x8086,
            device_id: 0,
        }
    }
    fn stats(tick: u64, n: i64) -> Statistics {
        Statistics {
            tick_ms: tick,
            shared_limit: 8 * 1_073_741_824,
            dedicated_limit: 0,
            segments: vec![
                Segment {
                    aperture: true,
                    resident: 2 * 1_073_741_824,
                    committed: 3 * 1_073_741_824,
                },
                Segment {
                    aperture: false,
                    resident: 64 * 1024 * 1024,
                    committed: 128 * 1024 * 1024,
                },
            ],
            node_running_100ns: BTreeMap::from([(0, n), (1, n * 2)]),
        }
    }
    #[test]
    fn whole_adapter_memory_and_busiest_engine_are_not_process_usage_or_sum() {
        let mut h = History::default();
        let a = project_statistics(&identity(), stats(100, 100_000), &mut h).unwrap();
        assert!(a.is_igpu);
        assert!(!a.values.contains_key("GpuUtilization"));
        let b = project_statistics(&identity(), stats(200, 200_000), &mut h).unwrap();
        assert_eq!(b.values["GpuUtilization"].value, Some(20.));
        assert_eq!(b.values["GpuVramUsed"].value, Some(2.));
        assert_eq!(b.values["GpuVramTotal"].value, Some(8.));
        assert_eq!(b.values["GpuVramUtilization"].value, Some(25.));
        assert_eq!(b.values["GpuDedicatedMemoryUsed"].value, Some(0.0625));
    }
    #[test]
    fn counter_reset_resume_and_impossible_segments_do_not_invent_measurements() {
        let mut h = History::default();
        project_statistics(&identity(), stats(100, 100_000), &mut h).unwrap();
        let s = project_statistics(&identity(), stats(200, 1), &mut h).unwrap();
        assert!(!s.values.contains_key("GpuUtilization"));
        let s = project_statistics(&identity(), stats(5000, 200_000), &mut h).unwrap();
        assert!(!s.values.contains_key("GpuUtilization"));
        let mut s = stats(6000, 300_000);
        s.shared_limit = 1;
        assert!(project_statistics(&identity(), s, &mut h).is_err());
        assert!(integrated_bits(1 << 5));
        assert!(!integrated_bits((1 << 5) | (1 << 2)));
    }
    #[test]
    fn statistics_abi_uses_null_process_and_generated_windows_layout() {
        assert_eq!(size_of::<D3DKMT_SEGMENTSIZEINFO>(), 24);
        assert_eq!(size_of::<D3DKMT_OPENADAPTERFROMLUID>(), 12);
        assert_eq!(std::mem::offset_of!(D3DKMT_QUERYSTATISTICS, hProcess), 16);
        assert!(D3DKMT_QUERYSTATISTICS::default().hProcess.is_null());
    }
}
