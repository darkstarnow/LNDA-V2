#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{
    self, ADAPTER, DXGI_DEVICE, E_INVALIDARG, FACTORY, HRESULT, Iid, MORE_DATA, NOT_FOUND, OUTPUT,
    Object, S_OK, Table, This, guid,
};

pub(crate) const DEVICE_IIDS: [Iid; 3] = [
    guid(
        0x54ec77fa,
        0x1377,
        0x44e6,
        [0x8c, 0x32, 0x88, 0xfd, 0x5f, 0x44, 0xc8, 0x4c],
    ),
    guid(
        0x77db970f,
        0x6276,
        0x48ba,
        [0xba, 0x28, 0x07, 0x01, 0x43, 0xb4, 0x39, 0x2c],
    ),
    guid(
        0x05008617,
        0xfbfd,
        0x4051,
        [0xa7, 0x90, 0x14, 0x48, 0x84, 0xb4, 0xf6, 0xa9],
    ),
];
const ADAPTER_IIDS: [Iid; 3] = [
    guid(
        0x2411e7e1,
        0x12ac,
        0x4ccf,
        [0xbd, 0x14, 0x97, 0x98, 0xe8, 0x53, 0x4d, 0xc0],
    ),
    guid(
        0x29038f61,
        0x3839,
        0x4626,
        [0x91, 0xfd, 0x08, 0x68, 0x79, 0x01, 0x1a, 0x05],
    ),
    guid(
        0x0aa1ae0a,
        0xfa0e,
        0x4b84,
        [0x86, 0x44, 0xe0, 0x5f, 0xf8, 0xe5, 0xac, 0xb5],
    ),
];
const FACTORY_IIDS: [Iid; 3] = [
    guid(
        0x7b7166ec,
        0x21c7,
        0x44ae,
        [0xb2, 0x1a, 0xc9, 0xae, 0x32, 0x1a, 0xe3, 0x69],
    ),
    guid(
        0x770aae78,
        0xf26f,
        0x4dba,
        [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87],
    ),
    guid(
        0x50c83a1c,
        0xe072,
        0x4c48,
        [0x87, 0xb0, 0x36, 0x30, 0xfa, 0x36, 0xa6, 0xd0],
    ),
];
const OUTPUT_IIDS: [Iid; 1] = [guid(
    0xae02eedb,
    0xc735,
    0x4690,
    [0x8d, 0x52, 0x5a, 0x8d, 0xc2, 0x02, 0x13, 0xaa],
)];

trait Singleton: 'static {
    const KIND: usize;
    const INTERFACES: &'static [Iid];

    fn instance() -> This;

    fn answer(iid: *const Iid, out: *mut This) -> HRESULT {
        Self::answer_as(Self::KIND, iid, out)
    }

    fn answer_as(by: usize, iid: *const Iid, out: *mut This) -> HRESULT {
        let found = object::Object::implements(iid, Self::INTERFACES).then_some(Self::instance());
        object::Object::answer(by, iid, found, out)
    }

    fn offer(by: usize, out: *mut This) -> HRESULT {
        object::Object::answer(by, &Self::INTERFACES[0], Some(Self::instance()), out)
    }

    extern "system" fn query_interface(_: This, iid: *const Iid, out: *mut This) -> HRESULT {
        Self::answer(iid, out)
    }
}

pub(crate) struct DxgiDevice;
struct DxgiAdapter;
struct DxgiFactory;
struct DxgiOutput;

static DEVICE_OBJECT: Object = Object::new(&DEVICE_FUNCTIONS);
static ADAPTER_OBJECT: Object = Object::new(&ADAPTER_FUNCTIONS);
static FACTORY_OBJECT: Object = Object::new(&FACTORY_FUNCTIONS);
static OUTPUT_OBJECT: Object = Object::new(&OUTPUT_FUNCTIONS);

impl Singleton for DxgiDevice {
    const KIND: usize = DXGI_DEVICE;
    const INTERFACES: &'static [Iid] = &DEVICE_IIDS;

    fn instance() -> This {
        &DEVICE_OBJECT
    }

    extern "system" fn query_interface(_: This, iid: *const Iid, out: *mut This) -> HRESULT {
        if crate::device::Device::implements(iid) {
            return object::Object::answer(
                DXGI_DEVICE,
                iid,
                Some(crate::device::Device::instance()),
                out,
            );
        }
        Self::answer(iid, out)
    }
}

impl Singleton for DxgiAdapter {
    const KIND: usize = ADAPTER;
    const INTERFACES: &'static [Iid] = &ADAPTER_IIDS;

    fn instance() -> This {
        &ADAPTER_OBJECT
    }
}

impl Singleton for DxgiFactory {
    const KIND: usize = FACTORY;
    const INTERFACES: &'static [Iid] = &FACTORY_IIDS;

    fn instance() -> This {
        &FACTORY_OBJECT
    }
}

impl Singleton for DxgiOutput {
    const KIND: usize = OUTPUT;
    const INTERFACES: &'static [Iid] = &OUTPUT_IIDS;

    fn instance() -> This {
        &OUTPUT_OBJECT
    }
}

impl DxgiDevice {
    pub(crate) fn object() -> This {
        <Self as Singleton>::instance()
    }

    pub(crate) fn implements(iid: *const Iid) -> bool {
        object::Object::implements(iid, &DEVICE_IIDS)
    }

    extern "system" fn get_parent(_: This, iid: *const Iid, out: *mut This) -> HRESULT {
        DxgiAdapter::answer_as(DXGI_DEVICE, iid, out)
    }

    extern "system" fn get_adapter(_: This, out: *mut This) -> HRESULT {
        DxgiAdapter::offer(DXGI_DEVICE, out)
    }
}

#[repr(C)]
struct AdapterDescription {
    name: [u16; 128],
    vendor: u32,
    device: u32,
    subsystem: u32,
    revision: u32,
    dedicated_video_memory: usize,
    dedicated_system_memory: usize,
    shared_system_memory: usize,
    luid: [u32; 2],
}
const _: () = assert!(size_of::<AdapterDescription>() == 0x130);

const AMD: u32 = 0x1002;

impl AdapterDescription {
    fn current() -> Self {
        let mut name = [0; 128];
        for (slot, unit) in name.iter_mut().zip("Leonida console GPU".encode_utf16()) {
            *slot = unit;
        }
        let (gpu_optimal, standard) = crate::console::Console::current().title_memory();
        Self {
            name,
            vendor: AMD,
            device: 0,
            subsystem: 0,
            revision: 0,
            dedicated_video_memory: gpu_optimal,
            dedicated_system_memory: 0,
            shared_system_memory: standard,
            luid: [1, 0],
        }
    }
}

impl DxgiAdapter {
    extern "system" fn get_parent(_: This, iid: *const Iid, out: *mut This) -> HRESULT {
        DxgiFactory::answer_as(ADAPTER, iid, out)
    }

    extern "system" fn enum_outputs(_: This, index: u32, out: *mut This) -> HRESULT {
        if index != 0 {
            return NOT_FOUND;
        }
        DxgiOutput::offer(ADAPTER, out)
    }

    extern "system" fn get_desc(_: This, out: *mut AdapterDescription) -> HRESULT {
        if out.is_null() {
            return E_INVALIDARG;
        }
        unsafe { out.write(AdapterDescription::current()) };
        S_OK
    }
}

impl DxgiFactory {
    extern "system" fn enum_adapters(_: This, index: u32, out: *mut This) -> HRESULT {
        if index != 0 {
            return NOT_FOUND;
        }
        DxgiAdapter::offer(FACTORY, out)
    }
}

#[repr(C)]
struct Mode {
    width: u32,
    height: u32,
    refresh: [u32; 2],
    format: u32,
    scanlines: u32,
    scaling: u32,
    flags: u32,
}
const _: () = assert!(size_of::<Mode>() == 32);

impl Mode {
    fn reported() -> Self {
        let (width, height) = Self::display_size();
        Self {
            width,
            height,
            refresh: [60, 1],
            format: 0,
            scanlines: 1,
            scaling: 0,
            flags: 0,
        }
    }

    fn display_size() -> (u32, u32) {
        static SIZE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
        *SIZE.get_or_init(|| {
            let size = match Self::requested_size() {
                Some((w, h)) if (640..=7680).contains(&w) && (360..=4320).contains(&h) => (w, h),
                _ => (3840, 2160),
            };
            tracing::info!(
                width = size.0,
                height = size.1,
                "display mode reported to the title"
            );
            size
        })
    }

    fn requested_size() -> Option<(u32, u32)> {
        let value = std::env::var("LEONIDA_DISPLAY_MODE").ok()?;
        let (w, h) = value.trim().split_once(['x', 'X'])?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    }
}

impl DxgiOutput {
    extern "system" fn get_parent(_: This, iid: *const Iid, out: *mut This) -> HRESULT {
        DxgiAdapter::answer_as(OUTPUT, iid, out)
    }

    extern "system" fn get_display_mode_list(
        _: This,
        _format: u32,
        _flags: u32,
        count: *mut u32,
        out: *mut Mode,
    ) -> HRESULT {
        let Some(count) = (unsafe { count.as_mut() }) else {
            return E_INVALIDARG;
        };
        if out.is_null() {
            *count = 1;
            return S_OK;
        }
        if *count < 1 {
            return MORE_DATA;
        }
        *count = 1;
        unsafe { out.write(Mode::reported()) };
        S_OK
    }
}

static DEVICE_FUNCTIONS: Table<32> = {
    let mut f = object::missing_table::<DXGI_DEVICE, 32>();
    f[0] = <DxgiDevice as Singleton>::query_interface as *const ();
    f[6] = DxgiDevice::get_parent as *const ();
    f[7] = DxgiDevice::get_adapter as *const ();
    Table(f)
};

static ADAPTER_FUNCTIONS: Table<24> = {
    let mut f = object::missing_table::<ADAPTER, 24>();
    f[0] = <DxgiAdapter as Singleton>::query_interface as *const ();
    f[6] = DxgiAdapter::get_parent as *const ();
    f[7] = DxgiAdapter::enum_outputs as *const ();
    f[8] = DxgiAdapter::get_desc as *const ();
    Table(f)
};

static FACTORY_FUNCTIONS: Table<32> = {
    let mut f = object::missing_table::<FACTORY, 32>();
    f[0] = <DxgiFactory as Singleton>::query_interface as *const ();
    f[7] = DxgiFactory::enum_adapters as *const ();
    f[12] = DxgiFactory::enum_adapters as *const ();
    Table(f)
};

static OUTPUT_FUNCTIONS: Table<32> = {
    let mut f = object::missing_table::<OUTPUT, 32>();
    f[0] = <DxgiOutput as Singleton>::query_interface as *const ();
    f[6] = DxgiOutput::get_parent as *const ();
    f[21] = DxgiOutput::get_display_mode_list as *const ();
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{E_NOINTERFACE, slot};

    type Query = extern "system" fn(This, *const Iid, *mut This) -> HRESULT;
    type Get = extern "system" fn(This, *mut This) -> HRESULT;
    type Indexed = extern "system" fn(This, u32, *mut This) -> HRESULT;
    type Describe = extern "system" fn(This, *mut AdapterDescription) -> HRESULT;

    #[test]
    fn startup_walks_from_device_to_output() {
        let d3d = crate::device::Device::instance();
        let newest_pc = guid(
            0x6007896c,
            0x3244,
            0x4afd,
            [0xbf, 0x18, 0xa6, 0xd3, 0xbe, 0xda, 0x50, 0x23],
        );
        let mut dxgi = std::ptr::null();
        let qi = slot::<Query>(d3d, 0);
        assert_eq!(qi(d3d, &newest_pc, &mut dxgi), E_NOINTERFACE);
        assert!(dxgi.is_null());
        assert_eq!(qi(d3d, &DEVICE_IIDS[2], &mut dxgi), S_OK);
        assert_eq!(dxgi, DxgiDevice::object());

        let mut adapter = std::ptr::null();
        assert_eq!(slot::<Get>(dxgi, 0x38)(dxgi, &mut adapter), S_OK);
        let mut description = std::mem::MaybeUninit::<AdapterDescription>::uninit();
        assert_eq!(
            slot::<Describe>(adapter, 0x40)(adapter, description.as_mut_ptr()),
            S_OK
        );
        assert_eq!(unsafe { description.assume_init() }.vendor, AMD);

        let mut factory = std::ptr::null();
        let parent = slot::<Query>(adapter, 0x30);
        assert_eq!(parent(adapter, &FACTORY_IIDS[2], &mut factory), S_OK);
        let mut again = std::ptr::null();
        assert_eq!(slot::<Indexed>(factory, 0x38)(factory, 0, &mut again), S_OK);
        assert_eq!(again, adapter);

        let mut output = std::ptr::null();
        let outputs = slot::<Indexed>(adapter, 0x38);
        assert_eq!(outputs(adapter, 0, &mut output), S_OK);
        assert!(!output.is_null());
        assert_eq!(outputs(adapter, 1, &mut output), NOT_FOUND);
    }
}
