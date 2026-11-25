use std::ffi::c_void;

use leonida_graphics::device;
use leonida_graphics::object::{HRESULT, Iid, Object};

use crate::{ServiceExport, ServiceModule};

extern "system" fn create_device(
    adapter: *const c_void,
    parameters: *const u32,
    iid: *const Iid,
    out: *mut *const Object,
) -> HRESULT {
    leonida_graphics::resource::MemoryHooks::use_allocator(
        super::xmem::XMemAlloc,
        super::xmem::XMemFree,
    );
    leonida_graphics::resource::MemoryHooks::use_page_mapper(super::xmem::map_alias);
    #[cfg(windows)]
    leonida_graphics::HostSetup::on_failure(host_gpu_failed);
    device::Device::create(adapter, parameters, iid, out)
}

#[cfg(windows)]
fn host_gpu_failed(what: &str) -> ! {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
    leonida_loader::log_crash(&format!("Leonida: host GPU failure: {what}"));
    const DXGI_ERROR_DEVICE_REMOVED: u32 = 0x887A_0005;
    unsafe { TerminateProcess(GetCurrentProcess(), DXGI_ERROR_DEVICE_REMOVED) };
    unreachable!("the process has ended")
}

pub fn use_shader_cache(dir: std::path::PathBuf) {
    #[cfg(windows)]
    {
        leonida_graphics::pipeline::PipelineCache::use_dir(dir.with_file_name("pipelines"));
        leonida_graphics::shader_cache::use_shader_cache(dir);
    }
    #[cfg(not(windows))]
    let _ = dir;
}

#[cfg(windows)]
pub use leonida_graphics::pipeline::{PipelineCache, ShaderStore};

pub(crate) fn module() -> ServiceModule {
    ServiceModule {
        name: "d3d12x",
        dlls: &["d3d12_xs.dll"],
        exports: vec![
            ServiceExport {
                name: "D3D12ConfigureVirtualMemoryX",
                address: device::VirtualMemory::configure as *const () as usize,
            },
            ServiceExport {
                name: "D3D12XboxCreateDevice",
                address: create_device as *const () as usize,
            },
            ServiceExport {
                name: "D3D12SerializeRootSignature",
                address: leonida_graphics::root::RootSerializer::serialize as *const () as usize,
            },
            ServiceExport {
                name: "D3D12SerializeVersionedRootSignature",
                address: leonida_graphics::root::RootSerializer::serialize_versioned as *const ()
                    as usize,
            },
            ServiceExport {
                name: "D3D12CreateRootSignatureDeserializer",
                address: leonida_graphics::root::RootSerializer::create_deserializer as *const ()
                    as usize,
            },
            ServiceExport {
                name: "IID_ID3D12Device",
                address: (&raw const device::IID) as usize,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostPolicy, ServiceResolver};
    use leonida_image::ImportSymbol;
    use leonida_loader::ImportResolver;
    use leonida_loader::Resolution;

    #[test]
    fn the_interface_id_import_is_bound_to_data() {
        let symbol = ImportSymbol::Name {
            hint: 0,
            name: "IID_ID3D12Device".into(),
        };
        let r = ServiceResolver::new(HostPolicy::none());
        let Resolution::Found { address, .. } = r.resolve("d3d12_xs.dll", &symbol) else {
            panic!("not bound");
        };
        assert_eq!(unsafe { *(address as *const [u8; 16]) }, device::IID);
    }
}
