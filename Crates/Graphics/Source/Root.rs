#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, c_void};
use std::sync::OnceLock;

use crate::object::{
    self, ComObject, E_INVALIDARG, E_NOTIMPL, HRESULT, Iid, Object, ROOT_SIGNATURE, Table, This,
    guid,
};
use crate::resource::PAGEABLE_IIDS;

const ROOT_SIGNATURE_IIDS: [Iid; 3] = [
    guid(
        0xc54a6b66,
        0x72df,
        0x4ee8,
        [0x8b, 0xe5, 0xa9, 0x46, 0xa1, 0x42, 0x92, 0x14],
    ),
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

const CONSOLE_FLAGS: u32 = 0xff00_0000;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Desc {
    parameter_count: u32,
    parameters: *const c_void,
    sampler_count: u32,
    samplers: *const c_void,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VersionedDesc {
    version: u32,
    desc: Desc,
}

type Serialize =
    unsafe extern "system" fn(*const Desc, u32, *mut *mut c_void, *mut *mut c_void) -> HRESULT;
type SerializeVersioned =
    unsafe extern "system" fn(*const VersionedDesc, *mut *mut c_void, *mut *mut c_void) -> HRESULT;
type Deserialize =
    unsafe extern "system" fn(*const u8, usize, *const Iid, *mut *mut c_void) -> HRESULT;

impl Desc {
    fn without_console_flags(self) -> Self {
        if self.flags & CONSOLE_FLAGS != 0 {
            tracing::debug!(
                flags = format_args!("{:#x}", self.flags),
                "console root signature flags dropped"
            );
        }
        Self {
            flags: self.flags & !CONSOLE_FLAGS,
            ..self
        }
    }
}

struct HostExport {
    name: &'static CStr,
    address: OnceLock<Option<usize>>,
}

impl HostExport {
    const fn new(name: &'static CStr) -> Self {
        Self {
            name,
            address: OnceLock::new(),
        }
    }

    fn address(&self) -> Option<usize> {
        let address = *self.address.get_or_init(|| self.resolve());
        if address.is_none() {
            tracing::warn!(export = ?self.name, "no host d3d12.dll for root signatures");
        }
        address
    }

    #[cfg(windows)]
    fn resolve(&self) -> Option<usize> {
        use windows_sys::Win32::System::LibraryLoader::{
            GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
        };
        let dll: Vec<u16> = "d3d12.dll".encode_utf16().chain([0]).collect();
        let module = unsafe {
            LoadLibraryExW(
                dll.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if module.is_null() {
            return None;
        }
        unsafe { GetProcAddress(module, self.name.as_ptr().cast()) }.map(|f| f as usize)
    }

    #[cfg(not(windows))]
    fn resolve(&self) -> Option<usize> {
        None
    }
}

static SERIALIZE: HostExport = HostExport::new(c"D3D12SerializeRootSignature");
static SERIALIZE_VERSIONED: HostExport = HostExport::new(c"D3D12SerializeVersionedRootSignature");
static DESERIALIZE: HostExport = HostExport::new(c"D3D12CreateRootSignatureDeserializer");

pub struct RootSerializer;

impl RootSerializer {
    pub extern "system" fn serialize(
        desc: *const Desc,
        version: u32,
        blob: *mut *mut c_void,
        error: *mut *mut c_void,
    ) -> HRESULT {
        let Some(&desc) = (unsafe { desc.as_ref() }) else {
            return E_INVALIDARG;
        };
        let Some(address) = SERIALIZE.address() else {
            return E_NOTIMPL;
        };
        let host = unsafe { std::mem::transmute::<usize, Serialize>(address) };
        unsafe { host(&desc.without_console_flags(), version, blob, error) }
    }

    pub extern "system" fn serialize_versioned(
        desc: *const VersionedDesc,
        blob: *mut *mut c_void,
        error: *mut *mut c_void,
    ) -> HRESULT {
        let Some(&desc) = (unsafe { desc.as_ref() }) else {
            return E_INVALIDARG;
        };
        let Some(address) = SERIALIZE_VERSIONED.address() else {
            return E_NOTIMPL;
        };
        let desc = VersionedDesc {
            desc: desc.desc.without_console_flags(),
            ..desc
        };
        let host = unsafe { std::mem::transmute::<usize, SerializeVersioned>(address) };
        unsafe { host(&desc, blob, error) }
    }

    pub extern "system" fn create_deserializer(
        blob: *const u8,
        size: usize,
        iid: *const Iid,
        out: *mut *mut c_void,
    ) -> HRESULT {
        if blob.is_null() || out.is_null() {
            return E_INVALIDARG;
        }
        let Some(address) = DESERIALIZE.address() else {
            return E_NOTIMPL;
        };
        let host = unsafe { std::mem::transmute::<usize, Deserialize>(address) };
        unsafe { host(blob, size, iid, out) }
    }
}

#[repr(C)]
pub(crate) struct RootSignature {
    object: Object,
    blob: Vec<u8>,
}

unsafe impl ComObject for RootSignature {
    const KIND: usize = ROOT_SIGNATURE;
    const INTERFACES: &'static [Iid] = &ROOT_SIGNATURE_IIDS;
}

impl RootSignature {
    fn new(blob: Vec<u8>) -> Box<Self> {
        Box::new(Self {
            object: Object::new(&FUNCTIONS),
            blob,
        })
    }

    pub(crate) extern "system" fn create(
        _: This,
        _nodes: u32,
        blob: *const u8,
        size: usize,
        iid: *const Iid,
        out: *mut This,
    ) -> HRESULT {
        if blob.is_null() || size == 0 {
            return E_INVALIDARG;
        }
        tracing::debug!(size, "root signature");
        Self::new(unsafe { std::slice::from_raw_parts(blob, size) }.to_vec()).hand_out(iid, out)
    }
}

static FUNCTIONS: Table<24> = Table(object::pageable_table::<RootSignature, ROOT_SIGNATURE, 24>());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{S_OK, slot};

    use RootSerializer as Host;

    #[cfg(windows)]
    #[test]
    fn host_serialized_blobs_create_root_signatures() {
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_ROOT_CONSTANTS, D3D12_ROOT_PARAMETER, D3D12_ROOT_PARAMETER_0,
            D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS, D3D12_SHADER_VISIBILITY_ALL,
        };
        let parameter = D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Num32BitValues: 1,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        };
        let desc = Desc {
            parameter_count: 1,
            parameters: (&raw const parameter).cast(),
            sampler_count: 0,
            samplers: std::ptr::null(),
            flags: 0x0100_0001,
        };
        let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
        assert_eq!(Host::serialize(&desc, 1, &mut blob, &mut error), S_OK);
        let blob = blob.cast_const().cast::<Object>();
        let data = slot::<extern "system" fn(This) -> *const u8>(blob, 0x18)(blob);
        let size = slot::<extern "system" fn(This) -> usize>(blob, 0x20)(blob);
        assert!(size > 0);
        let null = std::ptr::null();
        let mut root = null;
        assert_eq!(
            RootSignature::create(null, 0, data, size, &ROOT_SIGNATURE_IIDS[0], &mut root),
            S_OK
        );
        assert_eq!(slot::<extern "system" fn(This) -> u32>(root, 0x10)(root), 0);
        slot::<extern "system" fn(This) -> u32>(blob, 0x10)(blob);
    }

    #[cfg(windows)]
    #[test]
    fn our_blobs_deserialize_on_the_host() {
        use windows::Win32::Graphics::Direct3D12::ID3D12RootSignatureDeserializer;
        use windows::core::Interface;
        let desc = VersionedDesc {
            version: 1,
            desc: Desc {
                parameter_count: 0,
                parameters: std::ptr::null(),
                sampler_count: 0,
                samplers: std::ptr::null(),
                flags: 1,
            },
        };
        let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
        assert_eq!(
            Host::serialize_versioned(&desc, &mut blob, &mut error),
            S_OK
        );
        let blob = blob.cast_const().cast::<Object>();
        let data = slot::<extern "system" fn(This) -> *const u8>(blob, 0x18)(blob);
        let size = slot::<extern "system" fn(This) -> usize>(blob, 0x20)(blob);
        let iid: Iid = ID3D12RootSignatureDeserializer::IID.to_u128().to_be_bytes();
        let iid = guid(
            u32::from_be_bytes(iid[0..4].try_into().unwrap()),
            u16::from_be_bytes(iid[4..6].try_into().unwrap()),
            u16::from_be_bytes(iid[6..8].try_into().unwrap()),
            iid[8..16].try_into().unwrap(),
        );
        let mut out = std::ptr::null_mut();
        assert_eq!(Host::create_deserializer(data, size, &iid, &mut out), S_OK);
        let deserializer = unsafe { ID3D12RootSignatureDeserializer::from_raw(out) };
        let read = unsafe { &*deserializer.GetRootSignatureDesc() };
        assert_eq!((read.NumParameters, read.Flags.0), (0, 1));
        slot::<extern "system" fn(This) -> u32>(blob, 0x10)(blob);
    }

    #[cfg(windows)]
    #[test]
    fn versioned_descriptions_serialize_on_the_host() {
        let desc = VersionedDesc {
            version: 1,
            desc: Desc {
                parameter_count: 0,
                parameters: std::ptr::null(),
                sampler_count: 0,
                samplers: std::ptr::null(),
                flags: 0x0200_0001,
            },
        };
        let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
        assert_eq!(
            Host::serialize_versioned(&desc, &mut blob, &mut error),
            S_OK
        );
        let blob = blob.cast_const().cast::<Object>();
        assert!(slot::<extern "system" fn(This) -> usize>(blob, 0x20)(blob) > 0);
        slot::<extern "system" fn(This) -> u32>(blob, 0x10)(blob);
        assert_eq!(
            Host::serialize_versioned(std::ptr::null(), &mut blob.cast_mut().cast(), &mut error),
            E_INVALIDARG
        );
    }
}
