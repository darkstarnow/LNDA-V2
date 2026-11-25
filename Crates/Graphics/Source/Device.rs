#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::object::{
    self, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, HRESULT, Iid, Object, S_FALSE, S_OK, Table, This,
    guid, guid_text,
};
use crate::{
    descriptor, dxgi, fence, frame, list, pipeline, query, queue, resource, root, signature,
};

const UNSUPPORTED: HRESULT = 0x887A_0004_u32 as i32;

pub static IID: Iid = guid(
    0x189819f1,
    0x1db6,
    0x4b57,
    [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7],
);

const LEVEL_IIDS: [Iid; 9] = [
    guid(
        0x77acce80,
        0x638e,
        0x4e65,
        [0x88, 0x95, 0xc1, 0xf2, 0x33, 0x86, 0x86, 0x3e],
    ),
    guid(
        0x30baa41e,
        0xb15b,
        0x475c,
        [0xa0, 0xbb, 0x1a, 0xf5, 0xc5, 0xb6, 0x43, 0x28],
    ),
    guid(
        0x81dadc15,
        0x2bad,
        0x4392,
        [0x93, 0xc5, 0x10, 0x13, 0x45, 0xc4, 0xaa, 0x98],
    ),
    guid(
        0xe865df17,
        0xa9ee,
        0x46f9,
        [0xa4, 0x63, 0x30, 0x98, 0x31, 0x5a, 0xa2, 0xe5],
    ),
    guid(
        0x8b4f173b,
        0x2fea,
        0x4b80,
        [0x8f, 0x58, 0x43, 0x07, 0x19, 0x1a, 0xb9, 0x5d],
    ),
    guid(
        0xc70b221b,
        0x40e4,
        0x4a17,
        [0x89, 0xaf, 0x02, 0x5a, 0x07, 0x27, 0xa6, 0xdc],
    ),
    guid(
        0x5c014b53,
        0x68a1,
        0x4b9b,
        [0x8b, 0xd1, 0xdd, 0x60, 0x46, 0xb9, 0x35, 0x8b],
    ),
    guid(
        0x728f1fa2,
        0xea11,
        0x4a9f,
        [0x9b, 0x52, 0x9b, 0x42, 0xa4, 0x84, 0xd5, 0xc9],
    ),
    guid(
        0x5405c344,
        0xd457,
        0x444e,
        [0xb4, 0xdd, 0x23, 0x66, 0xe4, 0x5a, 0xee, 0x39],
    ),
];

pub const CHECKED_VERSION: u32 = 0x0002_0004;

const HIGHEST_FEATURE_LEVEL: u32 = 0xc200;

#[repr(C)]
pub struct Device {
    object: Object,
    null_constant_buffer: u64,
    out_of_line: [u32; 16],
}
const _: () = assert!(size_of::<Device>() == 0x58);

pub(crate) static DEVICE: Device = Device {
    object: Object::new(&FUNCTIONS),
    null_constant_buffer: 0,
    out_of_line: [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
};

static NEWER: AtomicBool = AtomicBool::new(false);

impl Device {
    pub(crate) fn instance() -> This {
        (&raw const DEVICE).cast()
    }

    pub(crate) fn implements(iid: *const Iid) -> bool {
        object::is(iid, &[IID]) || object::is(iid, &LEVEL_IIDS)
    }

    pub(crate) fn newer() -> bool {
        NEWER.load(Ordering::Relaxed)
    }

    pub extern "system" fn create(
        _adapter: *const c_void,
        parameters: *const u32,
        iid: *const Iid,
        out: *mut This,
    ) -> HRESULT {
        if parameters.is_null() || iid.is_null() {
            return E_INVALIDARG;
        }
        let (version, iid) = unsafe { (parameters.read(), &*iid) };
        tracing::info!(version = format_args!("{version:#x}"), "D3D12.X device");
        if version != CHECKED_VERSION {
            tracing::warn!(
                version = format_args!("{version:#x}"),
                "device layouts were checked against {CHECKED_VERSION:#x} only"
            );
        }
        if !Self::implements(iid) {
            tracing::warn!(iid = %guid_text(iid), "device created as an unknown interface");
            return E_NOINTERFACE;
        }
        if out.is_null() {
            return S_FALSE;
        }
        if *iid == LEVEL_IIDS[8] {
            NEWER.store(true, Ordering::Relaxed);
        }
        object::answer(object::DEVICE, iid, Some(Self::instance()), out)
    }

    extern "system" fn query_interface(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
        let found = if Self::implements(iid) {
            Some(this)
        } else if dxgi::DxgiDevice::implements(iid) {
            Some(dxgi::DxgiDevice::object())
        } else {
            None
        };
        object::answer(object::DEVICE, iid, found, out)
    }

    extern "system" fn get_node_count(_: This) -> u32 {
        1
    }

    extern "system" fn check_feature_support(
        _: This,
        feature: u32,
        data: *mut c_void,
        size: u32,
    ) -> HRESULT {
        let Some(feature) = Feature::from_id(feature) else {
            tracing::warn!(feature, size, "device feature query not implemented");
            return E_NOTIMPL;
        };
        if data.is_null() || size as usize != feature.size() {
            return E_INVALIDARG;
        }
        feature.answer(data)
    }

    extern "system" fn unknown_pipeline<const SLOT: usize>(
        _: This,
        desc: *const u8,
        _iid: *const Iid,
        out: *mut This,
    ) -> HRESULT {
        static CALLS: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];
        let calls = CALLS[usize::from(SLOT == 0x218)].fetch_add(1, Ordering::Relaxed) + 1;
        if !out.is_null() {
            unsafe { *out = std::ptr::null() };
        }
        if calls <= 4 || calls.is_power_of_two() {
            let description = Self::title_words(desc as usize);
            let first = description
                .as_ref()
                .and_then(|w| Self::title_words(w[0] as usize));
            tracing::warn!(
                slot = format_args!("{:#x}", SLOT),
                calls,
                description = format_args!("{description:x?}"),
                first_points_at = format_args!("{first:x?}"),
                "a pipeline creation method that is not implemented: the title goes without this pipeline"
            );
        }
        E_NOTIMPL
    }

    #[cfg(windows)]
    fn title_words(at: usize) -> Option<Vec<u64>> {
        let mut bytes = [0u8; 128];
        (at != 0 && crate::host::TitleCopy::read_into(at as *const u8, &mut bytes)).then(|| {
            bytes
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| c.iter().rev().fold(0u64, |v, &b| v << 8 | u64::from(b)))
                .collect()
        })
    }

    #[cfg(not(windows))]
    fn title_words(_: usize) -> Option<Vec<u64>> {
        None
    }
}

#[repr(C)]
pub struct VirtualMemory {
    flags: u32,
    page_table_pages: u32,
    preserved_bytes: u32,
    lowest: u64,
    highest: u64,
}
const _: () = assert!(size_of::<VirtualMemory>() == 0x20);

impl VirtualMemory {
    pub extern "system" fn configure(config: *mut VirtualMemory) -> HRESULT {
        let Some(config) = (unsafe { config.as_mut() }) else {
            return E_INVALIDARG;
        };
        tracing::info!(
            flags = config.flags,
            page_table_pages = config.page_table_pages,
            preserved_bytes = config.preserved_bytes,
            "graphics virtual memory"
        );
        (config.lowest, config.highest) = Self::user_space();
        S_OK
    }

    #[cfg(windows)]
    fn user_space() -> (u64, u64) {
        use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
        let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
        unsafe { GetSystemInfo(&mut info) };
        (
            info.lpMinimumApplicationAddress as u64,
            info.lpMaximumApplicationAddress as u64,
        )
    }

    #[cfg(not(windows))]
    fn user_space() -> (u64, u64) {
        (0x1_0000, 0x7fff_fffe_ffff)
    }
}

#[repr(C)]
struct FeatureLevels {
    count: u32,
    requested: *const u32,
    highest: u32,
}

const OPTIONS: [u32; 16] = [1, 1, 2, 3, 3, 1, 1, 1, 3, 40, 0, 0, 0, 0, 1, 2];

#[derive(Clone, Copy)]
enum Feature {
    Options,
    Architecture,
    Levels,
    FormatSupport,
    Multisample,
}

impl Feature {
    fn from_id(id: u32) -> Option<Self> {
        Some(match id {
            0 => Self::Options,
            1 => Self::Architecture,
            2 => Self::Levels,
            3 => Self::FormatSupport,
            4 => Self::Multisample,
            _ => return None,
        })
    }

    fn size(self) -> usize {
        match self {
            Self::Options => size_of_val(&OPTIONS),
            Self::Architecture | Self::Multisample => 16,
            Self::Levels => size_of::<FeatureLevels>(),
            Self::FormatSupport => 12,
        }
    }

    fn answer(self, data: *mut c_void) -> HRESULT {
        match self {
            Self::Options => {
                unsafe { data.cast::<[u32; 16]>().write_unaligned(OPTIONS) };
                S_OK
            }
            Self::Architecture => {
                let a = unsafe { &mut *data.cast::<[u32; 4]>() };
                if a[0] != 0 {
                    return E_INVALIDARG;
                }
                a[1..].copy_from_slice(&[0, 1, 1]);
                S_OK
            }
            Self::FormatSupport => {
                let q = unsafe { &mut *data.cast::<[u32; 3]>() };
                q[1] = 0x7fff_ffff;
                q[2] = 0xffff;
                S_OK
            }
            Self::Multisample => {
                let q = unsafe { &mut *data.cast::<[u32; 4]>() };
                q[3] = u32::from(matches!(q[1], 1 | 2 | 4 | 8));
                S_OK
            }
            Self::Levels => unsafe { &mut *data.cast::<FeatureLevels>() }.resolve(),
        }
    }
}

impl FeatureLevels {
    fn resolve(&mut self) -> HRESULT {
        if self.requested.is_null() {
            return E_INVALIDARG;
        }
        let requested = unsafe { std::slice::from_raw_parts(self.requested, self.count as usize) };
        match requested
            .iter()
            .copied()
            .filter(|&l| l <= HIGHEST_FEATURE_LEVEL)
            .max()
        {
            Some(level) => {
                self.highest = level;
                S_OK
            }
            None => UNSUPPORTED,
        }
    }
}

static FUNCTIONS: Table<160> = {
    let mut f = object::d3d12_object(object::missing_table::<{ object::DEVICE }, 160>());
    f[0] = Device::query_interface as *const ();
    f[7] = Device::get_node_count as *const ();
    f[8] = queue::CommandQueue::create as *const ();
    f[9] = list::CommandAllocator::create as *const ();
    f[12] = list::CommandList::create as *const ();
    f[10] = pipeline::PipelineState::create_graphics as *const ();
    f[11] = pipeline::PipelineState::create_compute as *const ();
    f[13] = Device::check_feature_support as *const ();
    f[14] = descriptor::DescriptorHeap::create_heap as *const ();
    f[15] = descriptor::DescriptorHeap::increment_size as *const ();
    f[16] = root::RootSignature::create as *const ();
    f[17] = descriptor::Descriptors::create_cbv as *const ();
    f[18] = descriptor::Descriptors::create_srv as *const ();
    f[19] = descriptor::Descriptors::create_uav as *const ();
    f[20] = descriptor::RtvRecord::create_rtv as *const ();
    f[21] = descriptor::RtvRecord::create_dsv as *const ();
    f[22] = descriptor::Descriptors::create_sampler as *const ();
    f[23] = descriptor::Descriptors::copy as *const ();
    f[24] = descriptor::Descriptors::copy_simple as *const ();
    f[25] = resource::ResourceLayout::allocation_info as *const ();
    f[26] = resource::GpuResource::custom_heap_properties as *const ();
    f[27] = resource::GpuResource::create_committed as *const ();
    f[28] = resource::ResourceHeap::create_heap as *const ();
    f[29] = resource::PlacedTextures::create_placed as *const ();
    f[36] = fence::Fence::create as *const ();
    f[38] = resource::ResourceLayout::copyable_footprints as *const ();
    f[39] = query::QueryHeap::create as *const ();
    f[41] = signature::CommandSignature::create as *const ();
    f[48] = queue::CommandQueue::create_x as *const ();
    f[57] = Device::unknown_pipeline::<0x1c8> as *const ();
    f[61] = descriptor::Descriptors::create_sampler_x as *const ();
    f[62] = pipeline::PipelineState::create_compute_x as *const ();
    f[67] = Device::unknown_pipeline::<0x218> as *const ();
    f[69] = object::ignore as *const ();
    f[70] = descriptor::Descriptors::create_placed_srv as *const ();
    f[71] = descriptor::Descriptors::create_placed_uav as *const ();
    f[73] = list::CommandList::create_x as *const ();
    f[74] = resource::PagePools::register as *const ();
    f[75] = resource::PagePools::unregister as *const ();
    f[76] = resource::GpuResource::create_committed as *const ();
    f[78] = frame::FramePacer::set_interval as *const ();
    f[79] = frame::FramePacer::schedule as *const ();
    f[80] = frame::FramePacer::wait as *const ();
    f[81] = frame::FramePacer::statistics as *const ();
    f[86] = pipeline::PipelineState::create_stream as *const ();
    f[108] = resource::ResourceLayout::allocation_info2 as *const ();
    f[127] = resource::GpuResource::create_committed3 as *const ();
    f[133] = resource::PlacedTextures::create_at_address as *const ();
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    fn create_as(iid: &Iid) -> (HRESULT, This) {
        let version = CHECKED_VERSION;
        let mut device = std::ptr::null();
        let hr = Device::create(std::ptr::null(), &version, iid, &mut device);
        (hr, device)
    }

    #[test]
    fn creation_hands_out_the_device() {
        let (hr, device) = create_as(&IID);
        assert_eq!(hr, S_OK);
        assert_eq!(device, Device::instance());
        assert_ne!(unsafe { device.cast::<u8>().add(0x18).read() }, 0);
        assert_eq!(create_as(&[0; 16]).0, E_NOINTERFACE);
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(device, 0x38)(device),
            1
        );
        let missing = slot::<extern "system" fn() -> HRESULT>(device, 0xf0);
        assert_eq!(missing(), E_NOTIMPL);
    }

    #[test]
    fn newer_device_levels_are_the_same_device() {
        let (hr, device) = create_as(&LEVEL_IIDS[8]);
        assert_eq!((hr, device), (S_OK, Device::instance()));
        let mut same = std::ptr::null();
        assert_eq!(
            Device::query_interface(device, &LEVEL_IIDS[4], &mut same),
            S_OK
        );
        assert_eq!(same, device);
        let missing = slot::<extern "system" fn() -> HRESULT>(device, 0x3a0);
        assert_eq!(missing(), E_NOTIMPL);
    }

    #[test]
    fn the_device_is_found_at_every_level_through_others() {
        type Get = extern "system" fn(This, *const Iid, *mut This) -> HRESULT;
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let mut fence = std::ptr::null();
        assert_eq!(
            crate::fence::Fence::create(std::ptr::null(), 0, 0, fence_iid, &mut fence),
            S_OK
        );
        let get_device = slot::<Get>(fence, 0x38);
        let dxgi = dxgi::DxgiDevice::object();
        let query = slot::<Get>(dxgi, 0);
        for iid in std::iter::once(&IID).chain(&LEVEL_IIDS) {
            let mut found = std::ptr::null();
            assert_eq!(get_device(fence, iid, &mut found), S_OK, "{iid:02x?}");
            assert_eq!(found, Device::instance());
            let mut found = std::ptr::null();
            assert_eq!(query(dxgi, iid, &mut found), S_OK, "{iid:02x?}");
            assert_eq!(found, Device::instance());
        }
        let mut found = Device::instance();
        assert_eq!(get_device(fence, fence_iid, &mut found), E_NOINTERFACE);
        assert!(found.is_null());
        slot::<extern "system" fn(This) -> u32>(fence, 0x10)(fence);
    }

    #[cfg(windows)]
    #[test]
    fn the_hosts_children_hand_out_newer_device_levels() {
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_FENCE_FLAG_NONE, ID3D12Device5, ID3D12Fence,
        };
        let Some(host) = crate::host::Host::instance() else {
            return;
        };
        unsafe {
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let mut device: Option<ID3D12Device5> = None;
            assert!(fence.GetDevice(&mut device).is_ok());
            assert!(device.is_some());
        }
    }

    #[test]
    fn virtual_memory_configuration_reports_user_space() {
        let mut config = VirtualMemory {
            flags: 0,
            page_table_pages: 7,
            preserved_bytes: 0,
            lowest: 0,
            highest: 0,
        };
        assert_eq!(VirtualMemory::configure(&mut config), S_OK);
        assert!(0 < config.lowest && config.lowest < config.highest);
        let heap = Box::new(0u8);
        assert!((config.lowest..=config.highest).contains(&(&raw const *heap as u64)));
        assert_eq!(VirtualMemory::configure(std::ptr::null_mut()), E_INVALIDARG);
    }

    #[test]
    fn feature_levels_pick_the_highest_supported() {
        type Check = extern "system" fn(This, u32, *mut c_void, u32) -> HRESULT;
        let device = Device::instance();
        let check = slot::<Check>(device, 0x68);
        let query = |levels: &[u32], size| {
            let mut query = FeatureLevels {
                count: levels.len() as u32,
                requested: levels.as_ptr(),
                highest: 0,
            };
            let hr = check(device, 2, (&raw mut query).cast(), size);
            (hr, query.highest)
        };
        let levels = [0xb000, 0xb100, 0xc000, 0xc100, 0xd000];
        assert_eq!(query(&levels, 0x18), (S_OK, 0xc100));
        assert_eq!(query(&levels, 0x10).0, E_INVALIDARG);
        assert_eq!(query(&levels[4..], 0x18).0, UNSUPPORTED);
    }
}
