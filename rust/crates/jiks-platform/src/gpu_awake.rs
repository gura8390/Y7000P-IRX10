//! Explicit D3D11 keep-awake lease. Default state owns no GPU resource. The
//! owner token outlives COM context/device release on the same physical worker.
use crate::{
    domain::ExplicitAction,
    hybrid::{BiosGraphics, HybridSnapshot},
    ownership::OwnerToken,
};
use anyhow::{Context, Result, ensure};
use std::rc::Rc;
use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
                ID3D11Device, ID3D11DeviceContext,
            },
            Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1},
        },
    },
    core::Interface,
};
pub fn validate_policy(snapshot: &HybridSnapshot, ac_connected: bool) -> Result<()> {
    ensure!(
        snapshot.bios_current != Some(BiosGraphics::Uma),
        "UMA selection prevents keeping NVIDIA awake"
    );
    if snapshot.gsync == Some(true) {
        return Ok(());
    }
    match snapshot.igpu_mode {
        Some(0) => Ok(()),
        Some(1) => anyhow::bail!("IGPU-only mode prevents keeping NVIDIA awake"),
        Some(2) => {
            ensure!(
                ac_connected,
                "Hybrid Auto on DC prevents keeping NVIDIA awake"
            );
            Ok(())
        }
        None if snapshot.igpu_supported == Some(false) && snapshot.gsync == Some(false) => Ok(()),
        _ => anyhow::bail!("GPU routing is unknown; keep-awake cannot assume a safe mode"),
    }
}
pub(crate) struct GpuLease {
    _resources: OrderedResources<ID3D11DeviceContext, ID3D11Device, Rc<OwnerToken>>,
    pub name: String,
}
struct OrderedResources<C, D, O> {
    context: Option<C>,
    device: Option<D>,
    owner: Option<O>,
}
impl<C, D, O> Drop for OrderedResources<C, D, O> {
    fn drop(&mut self) {
        self.context.take();
        self.device.take();
        self.owner.take();
    }
}
pub fn names_match(a: &str, b: &str) -> bool {
    let normalize = |s: &str| s.strip_prefix("NVIDIA ").unwrap_or(s).to_ascii_lowercase();
    normalize(a) == normalize(b)
}
impl GpuLease {
    pub(crate) fn open(
        owner: Rc<OwnerToken>,
        action: &ExplicitAction,
        expected_name: &str,
        mut guard: impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        ensure!(
            !expected_name.is_empty(),
            "Explicit NVIDIA name is required"
        );
        guard()?;
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let mut matching = Vec::new();
        let mut finished = false;
        for index in 0..64 {
            let adapter = match unsafe { factory.EnumAdapters1(index) } {
                Ok(v) => v,
                Err(e) if e.code().0 == 0x887a0002u32 as i32 => {
                    finished = true;
                    break;
                }
                Err(e) => return Err(e.into()),
            };
            let desc = unsafe { adapter.GetDesc1()? };
            if desc.VendorId != 0x10de {
                continue;
            }
            let end = desc
                .Description
                .iter()
                .position(|v| *v == 0)
                .context("Unterminated DXGI description")?;
            let name = String::from_utf16(&desc.Description[..end])?;
            if names_match(&name, expected_name) {
                matching.push((adapter, name));
            }
        }
        ensure!(
            finished && matching.len() == 1,
            "Selected NVIDIA DXGI identity is absent/ambiguous; no first-GPU fallback"
        );
        let (adapter, name) = matching.remove(0);
        let adapter: IDXGIAdapter = adapter.cast()?;
        guard()?;
        action.check_write_boundary()?;
        let (mut device, mut context) = (None, None);
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        ensure!(
            device.is_some() && context.is_some(),
            "NVIDIA D3D11 keep-awake context was not created"
        );
        Ok(Self {
            _resources: OrderedResources {
                context,
                device,
                owner: Some(owner),
            },
            name,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn context_and_device_release_before_owner_without_creating_any_gpu() {
        struct Fake(&'static str, Rc<std::cell::RefCell<Vec<&'static str>>>);
        impl Drop for Fake {
            fn drop(&mut self) {
                self.1.borrow_mut().push(self.0);
            }
        }
        let log = Rc::new(std::cell::RefCell::new(Vec::new()));
        let resources = OrderedResources {
            context: Some(Fake("context", log.clone())),
            device: Some(Fake("device", log.clone())),
            owner: Some(Fake("owner", log.clone())),
        };
        drop(resources);
        assert_eq!(*log.borrow(), vec!["context", "device", "owner"]);
        assert!(names_match("NVIDIA GeForce fixture", "GeForce fixture"));
    }
    use crate::hybrid::IgpuBackend;
    #[test]
    fn default_unknown_and_sleep_modes_never_authorize_gpu_context() {
        let mut s = HybridSnapshot::default();
        assert!(validate_policy(&s, true).is_err());
        s.igpu_mode = Some(1);
        s.igpu_backend = Some(IgpuBackend::GameZone);
        assert!(validate_policy(&s, true).is_err());
        s.igpu_mode = Some(2);
        assert!(validate_policy(&s, false).is_err());
        assert!(validate_policy(&s, true).is_ok());
        s.bios_current = Some(BiosGraphics::Uma);
        assert!(validate_policy(&s, true).is_err());
    }
    #[test]
    fn explicit_discrete_route_or_normal_hybrid_can_keep_awake() {
        let mut s = HybridSnapshot {
            gsync: Some(true),
            ..Default::default()
        };
        assert!(validate_policy(&s, false).is_ok());
        s.gsync = Some(false);
        s.igpu_mode = Some(0);
        assert!(validate_policy(&s, false).is_ok());
    }
}
