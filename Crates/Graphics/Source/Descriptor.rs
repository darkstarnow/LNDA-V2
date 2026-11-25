#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{
    self, ComObject, DESCRIPTOR_HEAP, E_INVALIDARG, HRESULT, Iid, Object, Table, This, guid,
};
use crate::resource::{Memory, PAGEABLE_IIDS};

#[cfg(windows)]
#[path = "Descriptor/Host.rs"]
pub(crate) mod host;

const HEAP_IIDS: [Iid; 4] = [
    guid(
        0x8efb471d,
        0x616c,
        0x4f49,
        [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapDesc {
    kind: u32,
    count: u32,
    flags: u32,
    nodes: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Placement {
    address: u64,
    size: u64,
}

#[repr(C)]
pub(crate) struct DescriptorHeap {
    object: Object,
    desc: HeapDesc,
    pub(crate) start: u64,
    memory: Option<Memory>,
    #[cfg(windows)]
    host: std::sync::OnceLock<Option<host::HostHeap>>,
    #[cfg(windows)]
    pending: std::sync::Mutex<Vec<u32>>,
}

unsafe impl ComObject for DescriptorHeap {
    const KIND: usize = DESCRIPTOR_HEAP;
    const INTERFACES: &'static [Iid] = &HEAP_IIDS;
}

pub(crate) struct Descriptors;

pub(crate) struct ViewWaits;

impl DescriptorHeap {
    pub(crate) fn of<'a>(this: This) -> &'a Self {
        <Self as ComObject>::from_this(this)
    }

    #[cfg(windows)]
    fn shader_slot<'a>(handle: u64) -> Option<(&'a DescriptorHeap, u32)> {
        let heaps = SHADER_HEAPS.lock().unwrap_or_else(|e| e.into_inner());
        let &(_, _, at) = heaps
            .iter()
            .find(|&&(start, end, _)| (start..end).contains(&handle))?;
        let heap = unsafe { &*(at as *const DescriptorHeap) };
        Some((heap, heap.index(handle)?))
    }

    #[cfg(windows)]
    fn mirror(handle: u64) {
        let Some((heap, index)) = DescriptorHeap::shader_slot(handle) else {
            return;
        };
        let mut pending = heap.pending.lock().unwrap_or_else(|e| e.into_inner());
        match heap.host.get() {
            None => pending.push(index),
            Some(host) => {
                drop(pending);
                if let Some(host) = host {
                    host.mirror(index, handle, false);
                }
            }
        }
    }

    #[cfg(windows)]
    pub(crate) fn release_dynamic(heap: usize, first: u32, n: u32, discarded: bool) {
        let heaps = SHADER_HEAPS.lock().unwrap_or_else(|e| e.into_inner());
        if heaps.iter().any(|&(_, _, h)| h == heap)
            && let Some(Some(host)) =
                unsafe { &*(DescriptorHeap::from_this as *const DescriptorHeap) }
                    .host
                    .get()
        {
            host.release(first, n, discarded);
        }
    }

    fn increment(kind: u32) -> Option<u32> {
        [32, 16, 128, 128].get(kind as usize).copied()
    }

    pub(crate) extern "system" fn increment_size(_: This, kind: u32) -> u32 {
        DescriptorHeap::increment(kind).unwrap_or(0)
    }

    pub(crate) extern "system" fn create_heap(
        _: This,
        desc_ptr: *const HeapDesc,
        iid: *const Iid,
        out: *mut This,
    ) -> HRESULT {
        let Some(&desc) = (unsafe { desc_ptr.as_ref() }) else {
            return E_INVALIDARG;
        };
        let Some(size) = DescriptorHeap::increment(desc.kind)
            .map(|i| u64::from(i) * u64::from(desc.count.max(1)))
        else {
            return E_INVALIDARG;
        };
        let placement = unsafe { desc_ptr.add(1).cast::<Placement>().read_unaligned() };
        let placed = placement.address != 0 && placement.size == size;
        tracing::debug!(?desc, placed, "descriptor heap");
        let (start, memory) = if placed {
            (placement.address, None)
        } else {
            match Memory::zeroed(size, 64) {
                Ok(memory) => (memory.address(), Some(memory)),
                Err(hr) => return hr,
            }
        };
        let visible =
            desc.flags & SHADER_VISIBLE != 0 && matches!(desc.kind, VIEWS_KIND | SAMPLERS);
        let heap = Box::new(DescriptorHeap {
            object: Object::new(&FUNCTIONS),
            desc,
            start,
            memory,
            #[cfg(windows)]
            host: std::sync::OnceLock::new(),
            #[cfg(windows)]
            pending: Default::default(),
        });
        #[cfg(windows)]
        if visible {
            let at = std::ptr::from_ref::<DescriptorHeap>(&heap) as usize;
            let mut heaps = SHADER_HEAPS.lock().unwrap_or_else(|e| e.into_inner());
            heaps.push((start, start + size, at));
        }
        #[cfg(not(windows))]
        let _ = visible;
        object::hand_out(DESCRIPTOR_HEAP, heap, iid, &HEAP_IIDS, out)
    }

    extern "system" fn get_desc(this: This, out: *mut HeapDesc) {
        if !out.is_null() {
            unsafe { out.write(DescriptorHeap::from_this(this).desc) };
        }
    }

    extern "system" fn gpu_start(this: This) -> u64 {
        DescriptorHeap::from_this(this).start
    }
}

impl RtvRecord {
    pub(crate) extern "system" fn create_rtv(
        _: This,
        resource: This,
        desc: *const [u32; 6],
        handle: u64,
    ) {
        if handle == 0 {
            return;
        }
        let described = unsafe { desc.as_ref() };
        let record = RtvRecord {
            tag: RTV_TAG,
            described: u32::from(described.is_some()),
            resource,
            desc: described.copied().unwrap_or_default(),
        };
        unsafe { (handle as *mut RtvRecord).write_unaligned(record) };
    }

    pub(crate) extern "system" fn create_dsv(
        this: This,
        resource: This,
        desc: *const [u32; 6],
        handle: u64,
    ) {
        RtvRecord::create_rtv(this, resource, desc, handle);
        if handle != 0 {
            unsafe { (handle as *mut u32).write_unaligned(DSV_TAG) };
        }
    }

    pub(crate) fn rtv(handle: u64) -> Option<RtvRecord> {
        RtvRecord::tagged(handle, RTV_TAG)
    }

    pub(crate) fn dsv(handle: u64) -> Option<RtvRecord> {
        RtvRecord::tagged(handle, DSV_TAG)
    }

    fn tagged(handle: u64, tag: u32) -> Option<RtvRecord> {
        if handle == 0 {
            return None;
        }
        let record = unsafe { (handle as *const RtvRecord).read_unaligned() };
        (record.tag == tag).then_some(record)
    }
}

impl Views {
    fn inline() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_VIEW_INLINE").is_none_or(|v| v != "0"))
    }

    fn kind_words(kind: u32, payload: u64) -> [u32; 2] {
        [payload as u32, kind << KIND_SHIFT | (payload >> 32) as u32]
    }

    fn payload(i: u32, generation: u32) -> u64 {
        u64::from(generation & GENERATIONS) << 32 | u64::from(i)
    }

    fn constant_words(address: u64, size: u32) -> Option<[u32; 2]> {
        if address >> CONSTANT_ADDRESS_BITS != 0 {
            return None;
        }
        let code = if size < 1 << 14 {
            u64::from(size)
        } else if size.is_multiple_of(256) && size >> 8 < 1 << 14 {
            1 << 14 | u64::from(size >> 8)
        } else {
            return None;
        };
        Some(Views::kind_words(
            CONSTANT,
            code << CONSTANT_ADDRESS_BITS | address,
        ))
    }

    fn constant_of(i: u32, generation: u32) -> View {
        let payload = Views::payload(i, generation);
        let code = (payload >> CONSTANT_ADDRESS_BITS) as u32;
        let size = if code >> 14 == 0 {
            code
        } else {
            (code & 0x3fff) << 8
        };
        View::Constant(payload & ((1 << CONSTANT_ADDRESS_BITS) - 1), size)
    }

    fn shape_of(unordered: bool, resource: &[u32; 14], desc: &[u32; 16]) -> (Shape, u64) {
        let mut rest = [0u32; 14];
        rest[..10].copy_from_slice(&desc[..10]);
        rest[10..].copy_from_slice(&desc[12..]);
        (
            (unordered, *resource, rest),
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
        )
    }

    fn placed_words(shape: u32, address: u64) -> [u32; 2] {
        Views::kind_words(PLACED, u64::from(shape) << PLACED_ADDRESS_BITS | address)
    }

    fn words_for(view: View) -> [u32; 2] {
        use std::sync::atomic::Ordering::Relaxed;
        if Views::inline() {
            match &view {
                View::Constant(address, size) => {
                    if let Some(words) = Views::constant_words(*address, *size) {
                        NAMED_WHOLE[0].fetch_add(1, Relaxed);
                        return words;
                    }
                }
                View::Placed(unordered, resource, desc) => {
                    let (shape, address) = Views::shape_of(*unordered, resource, desc);
                    if address >> PLACED_ADDRESS_BITS == 0 {
                        let mut views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(shape) = views.shape(shape) {
                            NAMED_WHOLE[1].fetch_add(1, Relaxed);
                            return Views::placed_words(shape, address);
                        }
                    }
                }
                _ => {}
            }
        }
        VIEWS.lock().unwrap_or_else(|e| e.into_inner()).entry(view)
    }

    #[cfg(all(windows, feature = "diagnostics"))]
    pub(crate) fn kept() -> [u64; 8] {
        use std::sync::atomic::Ordering::Relaxed;
        let views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
        let each = size_of::<(View, u32)>() as u64;
        let shape = size_of::<Shape>() as u64;
        let (mut constants, mut placed) = (0, 0);
        for (view, _) in &views.list {
            match view {
                View::Constant(..) => constants += 1,
                View::Placed(..) => placed += 1,
                _ => {}
            }
        }
        [
            views.list.len() as u64,
            views.free.len() as u64,
            views.list.capacity() as u64 * each
                + views.index.len() as u64 * each * 3 / 2
                + views.shapes.capacity() as u64 * shape
                + views.shape_index.len() as u64 * shape * 3 / 2,
            constants,
            placed,
            views.shapes.len() as u64,
            NAMED_WHOLE[0].load(Relaxed),
            NAMED_WHOLE[1].load(Relaxed),
        ]
    }
}

impl ViewWaits {
    #[cfg(windows)]
    pub(crate) fn made(resource: usize) {
        MADE_LATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(resource);
    }

    #[cfg(windows)]
    fn wait_for(resource: usize, handle: u64) {
        const MOST: usize = 64;
        let many = {
            let mut waiting = WAITING.lock().unwrap_or_else(|e| e.into_inner());
            let slots = waiting.entry(resource).or_default();
            if !slots.contains(&handle) {
                slots.push(handle);
            }
            slots.len() > MOST
        };
        if many {
            let _ = crate::resource::HostTextures::host_copy(resource as This);
        } else if !crate::resource::GpuResource::host_waits(resource as This) {
            ViewWaits::made(resource);
        }
    }

    #[cfg(windows)]
    pub(crate) fn make_all() {
        let made = std::mem::take(&mut *MADE_LATE.lock().unwrap_or_else(|e| e.into_inner()));
        for resource in made {
            let handles = WAITING
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&resource);
            for handle in handles.into_iter().flatten() {
                let Some((heap, index)) = DescriptorHeap::shader_slot(handle) else {
                    continue;
                };
                let same = match Descriptors::view(handle) {
                    Some(View::Shader(r, _) | View::Unordered(r, _, _)) => r == resource,
                    _ => false,
                };
                if same && let Some(Some(host)) = heap.host.get() {
                    host.mirror(index, handle, true);
                }
            }
        }
    }
}

impl Descriptors {
    fn record(handle: u64, view: View) {
        if handle == 0 {
            return;
        }
        let [i, generation] = Views::words_for(view);
        unsafe { (handle as *mut [u32; 3]).write_unaligned([VIEW_TAG, i, generation]) };
        #[cfg(windows)]
        DescriptorHeap::mirror(handle);
    }

    pub(crate) fn forget(resource: usize) {
        #[cfg(windows)]
        WAITING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&resource);
        let mut views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entries) = views.by_resource.remove(&resource) else {
            return;
        };
        for i in entries {
            let (view, generation) = views.list[i as usize].clone();
            if views.index.get(&view) != Some(&i) || !view.resources().contains(&resource) {
                continue;
            }
            views.index.remove(&view);
            views.list[i as usize].1 = ((generation + 1) & GENERATIONS).max(1);
            views.free.push(i);
        }
    }

    fn view_at(i: u32, generation: u32) -> Option<View> {
        match generation >> KIND_SHIFT {
            0 => {
                let views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
                let (view, current) = views.list.get(i as usize)?;
                (*current == generation).then(|| view.clone())
            }
            CONSTANT => Some(Views::constant_of(i, generation)),
            PLACED => VIEWS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .placed_of(i, generation),
            _ => None,
        }
    }

    pub(crate) fn view(handle: u64) -> Option<View> {
        if handle == 0 {
            return None;
        }
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        if tag != VIEW_TAG {
            return None;
        }
        Descriptors::view_at(i, generation)
    }

    #[cfg(windows)]
    pub(crate) fn checked_view(handle: u64) -> Option<View> {
        let mut bytes = [0u8; 12];
        if handle == 0 || !crate::host::TitleCopy::read_into(handle as *const u8, &mut bytes) {
            return None;
        }
        let word = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        (word(0) == VIEW_TAG)
            .then(|| Descriptors::view_at(word(4), word(8)))
            .flatten()
    }

    fn words<const N: usize>(desc: *const [u32; N]) -> Option<[u32; N]> {
        (!desc.is_null()).then(|| unsafe { desc.read_unaligned() })
    }

    pub(crate) extern "system" fn create_cbv(_: This, desc: *const [u32; 4], handle: u64) {
        if let Some([lo, hi, size, _]) = Descriptors::words(desc) {
            let address = u64::from(lo) | u64::from(hi) << 32;
            Descriptors::record(handle, View::Constant(address, size));
        }
    }

    pub(crate) extern "system" fn create_srv(
        _: This,
        resource: This,
        desc: *const [u32; 10],
        handle: u64,
    ) {
        Descriptors::record(
            handle,
            View::Shader(resource as usize, Descriptors::words(desc)),
        );
    }

    pub(crate) extern "system" fn create_uav(
        _: This,
        resource: This,
        counter: This,
        desc: *const [u32; 10],
        handle: u64,
    ) {
        let view = View::Unordered(
            resource as usize,
            counter as usize,
            Descriptors::words(desc),
        );
        Descriptors::record(handle, view);
    }

    pub(crate) extern "system" fn create_placed_srv(
        _: This,
        resource: *const [u32; 14],
        desc: *const [u32; 16],
        handle: u64,
    ) {
        if let (Some(resource), Some(desc)) =
            (Descriptors::words(resource), Descriptors::words(desc))
        {
            Descriptors::record(handle, View::Placed(false, resource, desc));
        }
    }

    pub(crate) extern "system" fn create_placed_uav(
        _: This,
        resource: *const [u32; 14],
        desc: *const [u32; 16],
        handle: u64,
    ) {
        if let (Some(resource), Some(desc)) =
            (Descriptors::words(resource), Descriptors::words(desc))
        {
            Descriptors::record(handle, View::Placed(true, resource, desc));
        }
    }

    pub(crate) extern "system" fn create_sampler(_: This, desc: *const [u32; 13], handle: u64) {
        if let Some(desc) = Descriptors::words(desc) {
            Descriptors::record(handle, View::Sampler(desc));
        }
    }

    pub(crate) extern "system" fn create_sampler_x(_: This, desc: *const [u32; 27], handle: u64) {
        if let Some(desc) = Descriptors::words(desc) {
            Descriptors::record(handle, View::ConsoleSampler(desc));
        }
    }

    pub(crate) extern "system" fn copy_simple(_: This, count: u32, dst: u64, src: u64, kind: u32) {
        let step = u64::from(DescriptorHeap::increment(kind).unwrap_or(0));
        let len = u64::from(count) * step;
        if dst != 0 && src != 0 {
            unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, len as usize) };
            #[cfg(windows)]
            for i in 0..u64::from(count) {
                DescriptorHeap::mirror(dst + i * step);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) extern "system" fn copy(
        _: This,
        dst_ranges: u32,
        dst_starts: *const u64,
        dst_sizes: *const u32,
        src_ranges: u32,
        src_starts: *const u64,
        src_sizes: *const u32,
        kind: u32,
    ) {
        let Some(step) = DescriptorHeap::increment(kind) else {
            return;
        };
        let ranges = |n: u32, starts: *const u64, sizes: *const u32| -> Vec<(u64, u32)> {
            (0..n as usize)
                .map(|i| unsafe {
                    (
                        *starts.add(i),
                        if sizes.is_null() { 1 } else { *sizes.add(i) },
                    )
                })
                .collect()
        };
        if dst_starts.is_null() || src_starts.is_null() {
            return;
        }
        let src = ranges(src_ranges, src_starts, src_sizes);
        let mut dst = ranges(dst_ranges, dst_starts, dst_sizes).into_iter();
        let (mut to, mut room) = dst.next().unwrap_or((0, 0));
        for (mut from, mut left) in src {
            while left > 0 {
                while room == 0 {
                    let Some(next) = dst.next() else { return };
                    (to, room) = next;
                }
                let n = left.min(room);
                Descriptors::copy_simple(std::ptr::null(), n, to, from, kind);
                let bytes = u64::from(n) * u64::from(step);
                (to, from, room, left) = (to + bytes, from + bytes, room - n, left - n);
            }
        }
    }
}

impl DescriptorHeap {
    pub(crate) fn samplers(&self) -> bool {
        self.desc.kind == SAMPLERS
    }

    #[cfg(windows)]
    pub(crate) fn host(&self) -> Option<&host::HostHeap> {
        if let Some(h) = self.host.get() {
            return h.as_ref();
        }
        if self.desc.flags & SHADER_VISIBLE == 0 || !matches!(self.desc.kind, VIEWS_KIND | SAMPLERS)
        {
            return None;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        self.host
            .get_or_init(|| {
                let step = u64::from(DescriptorHeap::increment(self.desc.kind)?);
                let h = host::HostHeap::new(self.samplers(), self.desc.count)?;
                for i in pending.drain(..) {
                    h.mirror(i, self.start + u64::from(i) * step, false);
                }
                Some(h)
            })
            .as_ref()
    }

    #[cfg(windows)]
    pub(crate) fn handle(&self, index: u32) -> Option<u64> {
        let step = u64::from(DescriptorHeap::increment(self.desc.kind)?);
        (index < self.desc.count).then(|| self.start + u64::from(index) * step)
    }

    pub(crate) fn index(&self, handle: u64) -> Option<u32> {
        let step = u64::from(DescriptorHeap::increment(self.desc.kind)?);
        let at = handle.checked_sub(self.start)?;
        let index = at / step;
        (index < u64::from(self.desc.count)).then_some(index as u32)
    }
}

const VIEWS_KIND: u32 = 0;
const SAMPLERS: u32 = 1;
const SHADER_VISIBLE: u32 = 1;

#[cfg(windows)]
static SHADER_HEAPS: std::sync::Mutex<Vec<(u64, u64, usize)>> = std::sync::Mutex::new(Vec::new());

#[cfg(windows)]
impl Drop for DescriptorHeap {
    fn drop(&mut self) {
        let at = std::ptr::from_ref::<DescriptorHeap>(self) as usize;
        let mut heaps = SHADER_HEAPS.lock().unwrap_or_else(|e| e.into_inner());
        heaps.retain(|&(_, _, h)| h != at);
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct RtvRecord {
    tag: u32,
    described: u32,
    pub(crate) resource: This,
    desc: [u32; 6],
}
const RTV_TAG: u32 = u32::from_le_bytes(*b"vRTV");
const _: () = assert!(size_of::<RtvRecord>() <= 112);

impl RtvRecord {
    #[cfg(windows)]
    pub(crate) fn desc(
        &self,
    ) -> Option<windows::Win32::Graphics::Direct3D12::D3D12_RENDER_TARGET_VIEW_DESC> {
        const TEXTURE_2D: u32 = 4;
        self.host_desc(TEXTURE_2D)
    }

    #[cfg(windows)]
    pub(crate) fn depth_desc(
        &self,
    ) -> Option<windows::Win32::Graphics::Direct3D12::D3D12_DEPTH_STENCIL_VIEW_DESC> {
        const TEXTURE_2D: u32 = 3;
        self.host_desc(TEXTURE_2D)
    }

    #[cfg(windows)]
    fn host_desc<T>(&self, dimension: u32) -> Option<T> {
        assert!(size_of::<T>() <= size_of::<[u32; 6]>());
        let mut words = if self.described != 0 {
            self.desc
        } else {
            [
                crate::resource::GpuResource::view_format(self.resource)?,
                dimension,
                0,
                0,
                0,
                0,
            ]
        };
        words[0] = crate::resource::ResourceFormat::target_format(
            crate::resource::ResourceFormat::pc_format(words[0]),
        );
        Some(unsafe { (&raw const words).cast::<T>().read_unaligned() })
    }
}

const DSV_TAG: u32 = u32::from_le_bytes(*b"vDSV");

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum View {
    Constant(u64, u32),
    Shader(usize, Option<[u32; 10]>),
    Unordered(usize, usize, Option<[u32; 10]>),
    Sampler([u32; 13]),
    ConsoleSampler([u32; 27]),
    Placed(bool, [u32; 14], [u32; 16]),
}

impl View {
    #[cfg(windows)]
    fn is_buffer(&self) -> bool {
        match self {
            View::Constant(..) | View::Placed(..) => true,
            View::Shader(r, _) | View::Unordered(r, _, _) => {
                crate::resource::GpuResource::buffer_bytes(*r as This).is_some()
            }
            View::Sampler(_) | View::ConsoleSampler(_) => false,
        }
    }
}

pub(crate) struct Views {
    list: Vec<(View, u32)>,
    index: std::collections::BTreeMap<View, u32>,
    by_resource: std::collections::BTreeMap<usize, Vec<u32>>,
    free: Vec<u32>,
    shapes: Vec<Shape>,
    shape_index: std::collections::BTreeMap<Shape, u32>,
}

static VIEWS: std::sync::Mutex<Views> = std::sync::Mutex::new(Views {
    list: Vec::new(),
    index: std::collections::BTreeMap::new(),
    by_resource: std::collections::BTreeMap::new(),
    free: Vec::new(),
    shapes: Vec::new(),
    shape_index: std::collections::BTreeMap::new(),
});
const VIEW_TAG: u32 = u32::from_le_bytes(*b"vVIE");

const KIND_SHIFT: u32 = 30;
const GENERATIONS: u32 = (1 << KIND_SHIFT) - 1;
const CONSTANT: u32 = 1;
const PLACED: u32 = 2;
const CONSTANT_ADDRESS_BITS: u32 = 47;
const PLACED_ADDRESS_BITS: u32 = 40;
const SHAPES: usize = 1 << (62 - PLACED_ADDRESS_BITS);

type Shape = (bool, [u32; 14], [u32; 14]);

static NAMED_WHOLE: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

impl Views {
    fn shape(&mut self, shape: Shape) -> Option<u32> {
        if let Some(&at) = self.shape_index.get(&shape) {
            return Some(at);
        }
        if self.shapes.len() >= SHAPES {
            return None;
        }
        let at = self.shapes.len() as u32;
        self.shapes.push(shape);
        self.shape_index.insert(shape, at);
        Some(at)
    }

    fn placed_of(&self, i: u32, generation: u32) -> Option<View> {
        let payload = Views::payload(i, generation);
        let address = payload & ((1 << PLACED_ADDRESS_BITS) - 1);
        let &(unordered, resource, rest) =
            self.shapes.get((payload >> PLACED_ADDRESS_BITS) as usize)?;
        let mut desc = [0u32; 16];
        desc[..10].copy_from_slice(&rest[..10]);
        (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
        desc[12..].copy_from_slice(&rest[10..]);
        Some(View::Placed(unordered, resource, desc))
    }

    fn entry(&mut self, view: View) -> [u32; 2] {
        let i = match self.index.get(&view) {
            Some(&i) => i,
            None => {
                let i = match self.free.pop() {
                    Some(i) => {
                        self.list[i as usize].0 = view.clone();
                        i
                    }
                    None => {
                        self.list.push((view.clone(), 1));
                        self.list.len() as u32 - 1
                    }
                };
                for r in view.resources().into_iter().filter(|&r| r != 0) {
                    self.by_resource.entry(r).or_default().push(i);
                }
                self.index.insert(view, i);
                i
            }
        };
        [i, self.list[i as usize].1]
    }
}

impl View {
    fn resources(&self) -> [usize; 2] {
        match *self {
            View::Shader(r, _) => [r, 0],
            View::Unordered(r, counter, _) => [r, counter],
            _ => [0, 0],
        }
    }

    #[cfg(windows)]
    fn waiting_texture(&self) -> Option<usize> {
        match *self {
            View::Shader(r, _) | View::Unordered(r, _, _)
                if crate::resource::GpuResource::host_waits(r as This) =>
            {
                Some(r)
            }
            _ => None,
        }
    }
}

#[cfg(windows)]
static WAITING: std::sync::Mutex<std::collections::BTreeMap<usize, Vec<u64>>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

#[cfg(windows)]
static MADE_LATE: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<DESCRIPTOR_HEAP, 24>());
    f[0] = DescriptorHeap::query_interface as *const ();
    f[2] = DescriptorHeap::release as *const ();
    f[7] = object::parent_device as *const ();
    f[8] = DescriptorHeap::get_desc as *const ();
    f[9] = DescriptorHeap::gpu_start as *const ();
    f[10] = DescriptorHeap::gpu_start as *const ();
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{S_OK, slot};

    #[repr(C)]
    struct Extended(HeapDesc, Placement);

    fn create(desc: HeapDesc, placement: Placement, out: &mut This) -> HRESULT {
        let extended = Extended(desc, placement);
        DescriptorHeap::create_heap(std::ptr::null(), &extended.0, &HEAP_IIDS[0], out)
    }

    #[test]
    fn heaps_hold_count_descriptors_from_their_start() {
        let null = std::ptr::null();
        let desc = HeapDesc {
            kind: 0,
            count: 1000,
            flags: 1,
            nodes: 0,
        };
        let none = Placement {
            address: 0,
            size: 0,
        };
        let mut h = null;
        assert_eq!(create(desc, none, &mut h), S_OK);
        let cpu = slot::<extern "system" fn(This) -> u64>(h, 0x48)(h);
        assert_eq!(cpu % 64, 0);
        assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x50)(h), cpu);
        assert_eq!(DescriptorHeap::increment_size(null, 0), 32);
        unsafe { (cpu as *mut u8).add(999 * 32 + 31).write(1) };
        assert_eq!(DescriptorHeap::increment_size(null, 9), 0);
        let bad = HeapDesc { kind: 9, ..desc };
        assert_eq!(create(bad, none, &mut h), E_INVALIDARG);
    }

    #[cfg(windows)]
    #[test]
    fn scalar_heap_addresses_follow_title_descriptor_creation_copies_and_reuse() {
        use windows::Win32::Graphics::Direct3D12::D3D12_RANGE;

        let Some(_) = crate::host::Host::instance() else {
            return;
        };
        let null = std::ptr::null();
        let mut object = null;
        let desc = HeapDesc {
            kind: 0,
            count: 6,
            flags: 1,
            nodes: 0,
        };
        assert_eq!(
            create(
                desc,
                Placement {
                    address: 0,
                    size: 0
                },
                &mut object
            ),
            S_OK
        );
        let title = DescriptorHeap::from_this(object);
        let gpu = title.host().unwrap();
        let slot = |index| title.handle(index).unwrap();

        let mut acceleration = [0, 11, 5768, 0, 1608386816, 13, 0, 0, 0, 0];
        let tlas = u64::from(acceleration[4]) | u64::from(acceleration[5]) << 32;
        Descriptors::create_srv(null, null, &acceleration, slot(1));
        Descriptors::copy_simple(null, 1, slot(2), slot(1), 0);

        let backing = [0u64; 8];
        let base = backing.as_ptr() as u64;
        let mut resource = [0; 14];
        resource[0] = 1;
        resource[4] = 64;
        let mut placed = [0; 16];
        placed[0] = 41;
        placed[1] = 1;
        placed[4] = 2;
        placed[6] = 4;
        placed[10] = base as u32;
        placed[11] = (base >> 32) as u32;
        Descriptors::create_placed_srv(null, &resource, &placed, slot(3));

        let table = gpu.address_buffer(title).unwrap();
        let contents = || unsafe {
            let mut data = std::ptr::null_mut();
            table
                .Map(0, Some(&D3D12_RANGE { Begin: 0, End: 64 }), Some(&mut data))
                .unwrap();
            let words = data.cast::<[u64; 8]>().read_volatile();
            table.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            words
        };
        assert_eq!(contents(), [6, 0, 0, tlas, tlas, base + 8, 0, 0]);
        assert_eq!(&*gpu.acceleration_addresses(), &[tlas]);
        acceleration[4] += 256;
        Descriptors::create_srv(null, null, &acceleration, slot(1));
        Descriptors::copy_simple(null, 1, slot(4), slot(3), 0);
        Descriptors::copy_simple(null, 1, slot(3), slot(0), 0);
        Descriptors::create_srv(null, null, null.cast(), slot(2));
        assert_eq!(contents(), [6, 0, 0, tlas + 256, 0, 0, base + 8, 0]);
        assert_eq!(&*gpu.acceleration_addresses(), &[tlas + 256]);
        assert_eq!(DescriptorHeap::release(object), 0);
    }

    #[cfg(windows)]
    #[test]
    fn material_demand_revives_only_current_free_mips_and_protects_aliases() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
        };
        let Some(_) = crate::host::Host::instance() else {
            return;
        };
        let null = std::ptr::null();
        let mut object = null;
        assert_eq!(
            create(
                HeapDesc {
                    kind: 0,
                    count: 8,
                    flags: 1,
                    nodes: 0
                },
                Placement {
                    address: 0,
                    size: 0
                },
                &mut object
            ),
            S_OK
        );
        let heap = DescriptorHeap::from_this(object);
        let host = heap.host().unwrap();
        let color = [3, 0, 0, 0, 1024, 0, 512, 0x8_0001, 72, 1, 0, 0x105, 0, 0];
        let small = crate::resource::PlacedTextures::placed_bytes_from(&color, 4).unwrap();
        let mut writer = color;
        writer[8] = 28;
        writer[12] = 1;
        let writer_bytes = crate::resource::PlacedTextures::placed_bytes(&writer).unwrap();
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                writer_bytes.try_into().unwrap(),
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        } as u64;
        assert_ne!(base, 0);
        let addr = base + small;
        let put = |slot, at: u64, first: u32| {
            let mut srv = [0u32; 16];
            srv[0] = 72;
            srv[1] = 4;
            srv[2] = 5768;
            srv[4] = first;
            srv[5] = 8 - first;
            srv[10] = at as u32;
            srv[11] = (at >> 32) as u32;
            let mut shape = color;
            if slot == 2 {
                shape[4] = 2048;
            }
            let bytes = crate::resource::PlacedTextures::placed_bytes_from(&shape, first).unwrap();
            assert!(base <= at && at + bytes <= base + writer_bytes);
            Descriptors::create_placed_srv(null, &shape, &srv, heap.handle(slot).unwrap());
        };
        let _frame_guard = crate::frame::TEST_FRAME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let frame = crate::frame::FramePacer::frame();
        let clear = |s| Descriptors::create_cbv(null, &[0x400, 6, 256, 0], heap.handle(s).unwrap());
        put(1, addr, 4);
        let stamp = host.material_write_stamp();
        put(2, addr, 1);
        assert!(
            host.demand_material_texture(heap, 1, stamp, frame)
                .is_none(),
            "a different current owner"
        );
        clear(2);
        assert!(small < crate::resource::PlacedTextures::placed_bytes(&color).unwrap());
        put(3, addr + small, 0);
        let revived = host.demand_material_texture(heap, 1, stamp, frame).unwrap();
        assert!(host.texture_slots_due(frame, 8).0.contains(&revived));
        assert!(host.material_upload_allowed(1, revived.1, addr, small, small));
        assert!(!host.material_upload_allowed(1, revived.1, addr, small, small - 1));
        let srv = host.mirror(1, heap.handle(1).unwrap(), true).unwrap();
        let live = host::PlacedTexture::of(&srv).unwrap();
        let View::Shader(texture, _) = live.view else {
            panic!("no sampled texture")
        };
        let gpu = crate::resource::HostTextures::host_resource(texture as This).unwrap();
        assert_eq!(host.shows(1, gpu), Some(true));

        clear(1);
        put(1, addr, 4);
        assert!(
            host.demand_material_texture(heap, 1, stamp, frame)
                .is_none()
        );
        let new_stamp = host.material_write_stamp();
        put(2, addr, 1);
        clear(2);
        let _writer = crate::resource::PlacedTextures::texture_at(base, &writer).unwrap();
        assert!(
            host.demand_material_texture(heap, 1, new_stamp, frame)
                .is_none(),
            "GPU-writable owner"
        );
        assert_eq!(DescriptorHeap::release(object), 0);
    }

    #[cfg(windows)]
    #[test]
    fn material_demand_keeps_overlapping_gpu_owners_until_each_is_released() {
        use windows::core::{IUnknown, Interface};
        use windows_sys::Win32::System::Memory::*;
        let memory = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 << 20,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        assert!(!memory.is_null());
        let base = memory as u64;
        let owner = |side: u32| {
            let words: [u32; 14] = [3, 0, 0, 0, side, 0, side, 0x1_0001, 28, 1, 0, 0x105, 1, 0];
            let desc =
                unsafe { std::mem::transmute::<[u32; 14], crate::resource::ResourceDesc>(words) };
            let mut raw = std::ptr::null();
            assert_eq!(
                crate::resource::PlacedTextures::create_at_address(
                    std::ptr::null(),
                    base,
                    &desc,
                    0,
                    std::ptr::null(),
                    &crate::resource::PAGEABLE_IIDS[0],
                    &mut raw
                ),
                S_OK
            );
            crate::texture::HostWrites::note(raw);
            unsafe { IUnknown::from_raw(raw.cast_mut().cast()) }
        };
        let large = owner(512);
        let small = owner(32);
        let range = (base + 512 * 1024, base + 516 * 1024);
        let check = |expected| {
            assert_eq!(
                crate::resource::PlacedTextures::writable_texture_overlaps(range.0, range.1),
                expected
            );
            assert_eq!(
                crate::texture::HostWrites::overlaps(range.0, range.1),
                expected
            );
        };
        check(true);
        drop(small);
        check(true);
        drop(large);
        check(false);
        assert_ne!(unsafe { VirtualFree(memory, 0, MEM_RELEASE) }, 0);
    }

    #[cfg(windows)]
    #[test]
    fn material_demand_follows_gpu_record_and_rejects_reused_texture_memory() {
        use windows::Win32::Graphics::Direct3D12::*;
        let Some(device) = crate::host::Host::instance() else {
            return;
        };
        let null = std::ptr::null();
        let mut object = null;
        assert_eq!(
            create(
                HeapDesc {
                    kind: 0,
                    count: 128,
                    flags: 1,
                    nodes: 0
                },
                Placement {
                    address: 0,
                    size: 0
                },
                &mut object
            ),
            S_OK
        );
        let heap = DescriptorHeap::from_this(object);
        let host = heap.host().unwrap();
        let source_slot = 688 * 4 / 32;
        let record_bytes = [0u64; 8];
        let base = record_bytes.as_ptr() as u64;
        let mut resource = [0; 14];
        resource[0] = 1;
        resource[4] = 64;
        let mut view = [0; 16];
        view[0] = 41;
        view[1] = 1;
        view[6] = 16;
        view[10] = base as u32;
        view[11] = (base >> 32) as u32;
        let handle = heap.handle(source_slot).unwrap();
        Descriptors::create_placed_srv(null, &resource, &view, handle);
        let material = host.mirror(source_slot, handle, true).unwrap();
        let (shadow, address, _) = host::BufferViews::buffer_copy(&material).unwrap();
        let backing = vec![0u8; 1 << 20];
        let addr = backing.as_ptr() as u64;
        let tex = [3, 0, 0, 0, 64, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0];
        let mut tex_view = [0; 16];
        tex_view[10] = addr as u32;
        tex_view[11] = (addr >> 32) as u32;
        Descriptors::create_placed_srv(null, &tex, &tex_view, heap.handle(3).unwrap());
        let _frame_guard = crate::frame::TEST_FRAME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let frame = crate::frame::FramePacer::frame();
        let stamp = host.material_write_stamp();
        let write = host.demand_material_texture(heap, 3, stamp, frame).unwrap();
        let list = device.new_list().unwrap();
        for (slot, at, width, height, levels, first) in [
            (5, addr + 0x40000, 512, 256, 7, 1),
            (6, addr + 0x60000, 1024, 512, 8, 3),
        ] {
            let mut shape = [
                3,
                0,
                0,
                0,
                width,
                0,
                height,
                (levels << 16) | 1,
                71,
                1,
                0,
                0x105,
                0,
                0,
            ];
            let mut srv = [0u32; 16];
            srv[0] = 71;
            srv[1] = 4;
            srv[2] = 5768;
            srv[4] = first;
            srv[5] = levels - first;
            srv[10] = at as u32;
            srv[11] = (at >> 32) as u32;
            Descriptors::create_placed_srv(null, &shape, &srv, heap.handle(slot).unwrap());
            shape[4] *= 2;
            Descriptors::create_placed_srv(null, &shape, &srv, heap.handle(8).unwrap());
            Descriptors::create_cbv(null, &[0x400, 6, 256, 0], heap.handle(8).unwrap());
        }
        let words = [0xf800_0003u32, 0, 7, 0, 5, 6];
        let bytes = std::mem::size_of_val(&words) as u64;
        let (upload, mapped) = device.upload_buffer(bytes).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping(words.as_ptr().cast::<u8>(), mapped, bytes as usize);
            list.list
                .CopyBufferRegion(&shadow.buffer, shadow.offset(address), &upload, 0, bytes);
            list.list
                .ResourceBarrier(&[crate::host::StateBits::transition(
                    &shadow.buffer,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
        }
        crate::material_demand::MaterialDemand::record(&list, heap, heap.start, 0, &[0, 16, 20]);
        list.close();
        device.execute(&[&list]);
        let mut due: Vec<_> = (32..48).map(|n| (n, 1, false)).collect();
        due.push(write);
        let wanted = host.material_demand.take(heap, host, frame);
        assert_eq!(
            wanted
                .iter()
                .map(|&(slot, _, recovered)| (slot, recovered))
                .collect::<Vec<_>>(),
            [(3, false), (5, true), (6, true)]
        );
        assert_eq!(record_bytes, [0; 8]);
        crate::material_demand::MaterialDemand::promote(&mut due, &wanted, frame);
        assert_eq!(due[(frame % 4) as usize], write);
        assert_eq!(record_bytes[0], 0);
        let mut newer = tex;
        newer[4] = 128;
        Descriptors::create_placed_srv(null, &newer, &tex_view, heap.handle(4).unwrap());
        assert!(
            host.demand_material_texture(heap, 3, stamp, frame)
                .is_none()
        );
        assert!(
            host.demand_material_texture(heap, 4, stamp, frame)
                .is_none()
        );
        assert!(
            host.demand_material_texture(heap, 4, host.material_write_stamp(), frame)
                .is_some()
        );
        assert_eq!(DescriptorHeap::release(object), 0);
    }

    #[test]
    fn views_survive_copies() {
        let null = std::ptr::null();
        let mut heap = vec![0u64; 4 * 8];
        let at = |i: usize| heap.as_ptr() as u64 + 32 * i as u64;
        let (s0, s1, s2, s3) = (at(0), at(1), at(2), at(3));
        Descriptors::create_cbv(null, &[0x1000, 1, 256, 0], s0);
        assert_eq!(
            Descriptors::view(s0),
            Some(View::Constant(0x1_0000_1000, 256))
        );
        Descriptors::create_srv(null, null, std::ptr::null(), s1);
        assert_eq!(Descriptors::view(s1), Some(View::Shader(0, None)));
        Descriptors::create_srv(null, null, std::ptr::null(), s2);
        assert_eq!(unsafe { *(s1 as *const [u32; 3]) }, unsafe {
            *(s2 as *const [u32; 3])
        });
        let (dst, dst_n, src, src_n) = ([s2], [2u32], [s0, s1], [1u32, 1]);
        Descriptors::copy(
            null,
            1,
            dst.as_ptr(),
            dst_n.as_ptr(),
            2,
            src.as_ptr(),
            src_n.as_ptr(),
            0,
        );
        assert_eq!(Descriptors::view(s2), Descriptors::view(s0));
        assert_eq!(Descriptors::view(s3), Descriptors::view(s1));
        assert_eq!(Descriptors::view(at(0) + 8), None);
        heap[0] = 0;
    }

    #[test]
    fn constant_views_fit_their_records() {
        let top = (1u64 << CONSTANT_ADDRESS_BITS) - 1;
        for address in [
            0u64,
            0x100,
            0x2602_0000,
            0xd_6b60_0000,
            0x7ffd_a34f_0000,
            top,
        ] {
            for size in [
                0u32,
                1,
                255,
                256,
                4000,
                (1 << 14) - 1,
                1 << 14,
                65536,
                (1 << 22) - 256,
            ] {
                let [i, generation] = Views::constant_words(address, size).expect("fits");
                assert_eq!(generation >> KIND_SHIFT, CONSTANT);
                assert_eq!(
                    Descriptors::view_at(i, generation),
                    Some(View::Constant(address, size)),
                    "{address:#x} {size}"
                );
            }
        }
        assert_eq!(
            Views::constant_words(top + 1, 256),
            None,
            "an address past 47 bits"
        );
        assert_eq!(
            Views::constant_words(0x1000, (1 << 14) + 1),
            None,
            "16 KiB or more, no multiple of 256"
        );
        assert_eq!(Views::constant_words(0x1000, 1 << 22), None, "4 MiB");
        assert_ne!(
            Views::constant_words(0x1000, 256),
            Views::constant_words(0x1000, 512)
        );
        assert_ne!(
            Views::constant_words(0x1000, 256),
            Views::constant_words(0x1100, 256)
        );
        assert_ne!(
            Views::constant_words(0x1000, 1 << 14),
            Views::constant_words(0x1000, 64)
        );
    }

    #[test]
    fn views_at_new_addresses_add_no_entries() {
        if !Views::inline() {
            return;
        }
        let null = std::ptr::null();
        let slots = [0u64; 4];
        let at = slots.as_ptr() as u64;
        let resource: [u32; 14] = std::array::from_fn(|k| 0x5ee0_0000 + k as u32);
        let mut desc: [u32; 16] = std::array::from_fn(|k| 0x7a70_0000 + k as u32);
        let mine = |v: &View| match v {
            View::Constant(address, _) => (0x6_0000_0000..0x6_1000_0000).contains(address),
            View::Placed(_, r, _) => *r == resource,
            _ => false,
        };
        let entries = || {
            VIEWS
                .lock()
                .unwrap()
                .list
                .iter()
                .filter(|(v, _)| mine(v))
                .count()
        };
        let shapes = || {
            VIEWS
                .lock()
                .unwrap()
                .shapes
                .iter()
                .filter(|s| s.1 == resource)
                .count()
        };
        for n in 0..20_000u64 {
            let address = 0x6_0000_0000 + n * 0x100;
            Descriptors::create_cbv(null, &[address as u32, (address >> 32) as u32, 512, 0], at);
            assert_eq!(Descriptors::view(at), Some(View::Constant(address, 512)));
            let placed = 0xd_4940_0000 + n * 0x1_0000;
            (desc[10], desc[11]) = (placed as u32, (placed >> 32) as u32);
            Descriptors::create_placed_srv(null, &resource, &desc, at);
            assert_eq!(
                Descriptors::view(at),
                Some(View::Placed(false, resource, desc))
            );
        }
        assert_eq!(entries(), 0, "no table entries");
        assert_eq!(shapes(), 1, "one shape for 20,000 placed views");
        Descriptors::create_placed_uav(null, &resource, &desc, at);
        assert_eq!(
            Descriptors::view(at),
            Some(View::Placed(true, resource, desc))
        );
        assert_eq!(shapes(), 2);
        (desc[10], desc[11]) = (0, 0x100);
        Descriptors::create_placed_srv(null, &resource, &desc, at);
        assert_eq!(
            Descriptors::view(at),
            Some(View::Placed(false, resource, desc))
        );
        assert_eq!(entries(), 1);
        let words = |handle: u64| unsafe { *(handle as *const [u32; 3]) };
        let other = [0u64; 4];
        Descriptors::create_cbv(null, &[0x40, 6, 512, 0], at);
        Descriptors::create_cbv(null, &[0x40, 6, 512, 0], other.as_ptr() as u64);
        assert_eq!(words(at), words(other.as_ptr() as u64));
        Descriptors::create_cbv(null, &[0x140, 6, 512, 0], other.as_ptr() as u64);
        assert_ne!(words(at), words(other.as_ptr() as u64));
    }

    #[test]
    fn views_die_with_their_resource() {
        let null = std::ptr::null();
        let slots = [0u64; 2 * 4];
        let (a, b) = (slots.as_ptr() as u64, slots.as_ptr() as u64 + 32);
        let resource = 0x7777_0000usize as This;
        Descriptors::create_srv(null, resource, std::ptr::null(), a);
        assert_eq!(Descriptors::view(a), Some(View::Shader(0x7777_0000, None)));
        Descriptors::forget(0x7777_0000);
        assert_eq!(Descriptors::view(a), None, "stale after release");
        Descriptors::create_srv(null, resource, std::ptr::null(), b);
        assert_eq!(Descriptors::view(b), Some(View::Shader(0x7777_0000, None)));
        let (old, new) = unsafe { (*(a as *const [u32; 3]), *(b as *const [u32; 3])) };
        assert_ne!(old, new, "a new generation");
    }

    #[test]
    fn placed_heaps_start_at_the_title_memory() {
        let mut memory = vec![0u8; 64 * 32];
        let address = memory.as_mut_ptr() as u64;
        let desc = HeapDesc {
            kind: 0,
            count: 64,
            flags: 1,
            nodes: 1,
        };
        let mut h = std::ptr::null();
        assert_eq!(
            create(
                desc,
                Placement {
                    address,
                    size: 64 * 32
                },
                &mut h
            ),
            S_OK
        );
        assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x48)(h), address);
        assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x50)(h), address);
        assert_eq!(create(desc, Placement { address, size: 7 }, &mut h), S_OK);
        assert_ne!(slot::<extern "system" fn(This) -> u64>(h, 0x48)(h), address);
    }
}
