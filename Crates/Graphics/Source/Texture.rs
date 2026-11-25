use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

pub(crate) struct Staging {
    buffer: ID3D12Resource,
    #[cfg(feature = "diagnostics")]
    total: u64,
    data: *mut u8,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: leonida_formats::texture::Layout,
    mips: u32,
    first: u32,
    size: usize,
    bytes: u64,
    memory: *const u8,
    last: Mutex<Last>,
    dirty: std::sync::atomic::AtomicBool,
    uploaded: std::sync::atomic::AtomicBool,
    used: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct Last {
    hash: Option<blake3::Hash>,
    asked: u64,
    partial: Option<u64>,
    unguarded: u64,
}

unsafe impl Send for Staging {}
unsafe impl Sync for Staging {}

static STAGING: Mutex<Option<HashMap<usize, Arc<Staging>>>> = Mutex::new(None);

struct Through {
    buffer: ID3D12Resource,
    parts: Vec<(
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    )>,
}

const ALIAS_PARTS: u32 = 54;

unsafe impl Send for Through {}
unsafe impl Sync for Through {}

static THROUGH: Mutex<Option<HashMap<usize, Arc<Through>>>> = Mutex::new(None);

pub(crate) struct TextureUploads;

pub(crate) struct HostWrites;

pub(crate) struct TitleMemoryMap;

impl TextureUploads {
    pub(crate) fn alias(list: &HostList, placed: This, source: This) -> bool {
        const TARGET_OR_UNORDERED: u32 = 0x4 | 0x8;
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let (Some(to), Some(from)) = (
            crate::resource::HostTextures::host_resource(placed),
            crate::resource::HostTextures::host_resource(source),
        ) else {
            return false;
        };
        let Some(host) = host() else { return false };
        let through = {
            let mut map = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
            let map = map.get_or_insert_with(HashMap::new);
            match map.get(&(placed as usize)) {
                Some(through) => through.clone(),
                None => {
                    let (of_from, of_to) = unsafe { (from.GetDesc(), to.GetDesc()) };
                    let count = |d: &D3D12_RESOURCE_DESC| {
                        let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
                            1
                        } else {
                            u32::from(d.DepthOrArraySize)
                        };
                        u32::from(d.MipLevels) * layers
                    };
                    let parts = count(&of_from);
                    if parts == 0 || parts > ALIAS_PARTS || parts != count(&of_to) {
                        return false;
                    }
                    let mut rows = [(); 2].map(|()| {
                        vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); parts as usize]
                    });
                    let mut bytes = [0u64; 2];
                    unsafe {
                        host.device.GetCopyableFootprints(
                            &of_from,
                            0,
                            parts,
                            0,
                            Some(rows[0].as_mut_ptr()),
                            None,
                            None,
                            Some(&mut bytes[0]),
                        );
                        host.device.GetCopyableFootprints(
                            &of_to,
                            0,
                            parts,
                            0,
                            Some(rows[1].as_mut_ptr()),
                            None,
                            None,
                            Some(&mut bytes[1]),
                        );
                    }
                    let alike = rows[0].iter().zip(&rows[1]).all(|(a, b)| {
                        let (fa, fb) = (a.Footprint, b.Footprint);
                        a.Offset == b.Offset
                            && (fa.Width, fa.Height, fa.Depth, fa.RowPitch)
                                == (fb.Width, fb.Height, fb.Depth, fb.RowPitch)
                    });
                    if bytes[0] == 0 || bytes[0] != bytes[1] || !alike {
                        return false;
                    }
                    let Some(buffer) = host.uav_buffer(bytes[0]) else {
                        return false;
                    };
                    crate::host::name(&buffer, || {
                        format!(
                            "alias copy {}x{} f{} to f{}",
                            of_from.Width, of_from.Height, of_from.Format.0, of_to.Format.0
                        )
                    });
                    tracing::info!(
                        view = ?placed,
                        texture = ?source,
                        width = of_from.Width,
                        height = of_from.Height,
                        parts,
                        written_as = of_from.Format.0,
                        read_as = of_to.Format.0,
                        "a view placed in the memory of a texture the host writes, in another format: read from that texture by a copy on the host"
                    );
                    let [of_written, of_read] = rows;
                    let through = Arc::new(Through {
                        buffer,
                        parts: of_written.into_iter().zip(of_read).collect(),
                    });
                    map.insert(placed as usize, through.clone());
                    through
                }
            }
        };
        let first = list.first_use(placed as usize);
        let before = list
            .transition(source, COPY_SOURCE)
            .map(|(texture, before)| {
                let state = crate::host::legal_state(texture, COPY_SOURCE);
                let barrier = crate::host::transition(
                    texture,
                    D3D12_RESOURCE_STATES(before as i32),
                    D3D12_RESOURCE_STATES(state as i32),
                );
                unsafe { list.list.ResourceBarrier(&[barrier]) };
                before
            });
        if !first && before.is_none() {
            return true;
        }
        crate::list::CommandList::ensure_state(list, placed, COPY_DEST);
        let part = |texture: &ID3D12Resource, index: usize| D3D12_TEXTURE_COPY_LOCATION {
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: index as u32,
            },
        };
        let rows = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
            pResource: unsafe { std::mem::transmute_copy(&through.buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *row,
            },
        };
        let (common, read, written) = (
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        #[cfg(feature = "diagnostics")]
        list.note_op(|| format!("alias copy of {source:?} into {placed:?}"));
        unsafe {
            list.list
                .ResourceBarrier(&[crate::host::transition(&through.buffer, common, written)]);
            for (index, (of_written, _)) in through.parts.iter().enumerate() {
                list.list
                    .CopyTextureRegion(&rows(of_written), 0, 0, 0, &part(from, index), None);
            }
            list.list
                .ResourceBarrier(&[crate::host::transition(&through.buffer, written, read)]);
            for (index, (_, of_read)) in through.parts.iter().enumerate() {
                list.list
                    .CopyTextureRegion(&part(to, index), 0, 0, 0, &rows(of_read), None);
            }
            list.list
                .ResourceBarrier(&[crate::host::transition(&through.buffer, read, common)]);
        }
        list.keep(&through.buffer);
        if let Some(before) = before
            && before & TARGET_OR_UNORDERED != 0
        {
            crate::list::CommandList::ensure_state(list, source, before);
        }
        true
    }

    fn no_wait() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_TEXTURE_NO_WAIT").is_some_and(|v| v == "1"))
    }

    fn waited<T>(slot: usize, f: impl FnOnce() -> T) -> T {
        let started = std::time::Instant::now();
        let out = f();
        REFRESH_WAITS[slot].fetch_add(
            started.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        out
    }

    fn with_read<R>(size: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
        READ.with(|read| {
            let mut read = read.borrow_mut();
            if read.len() < size {
                read.resize(size, 0);
            }
            f(&mut read[..size])
        })
    }

    fn through_copy() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_TEXTURE_COPY").is_some_and(|v| v == "1"))
    }

    fn with_title<R>(memory: *const u8, size: usize, f: impl FnOnce(&[u8]) -> R) -> R {
        TextureUploads::with_read(size, |read| {
            crate::host::copy_mapped(read.as_mut_ptr(), memory, size, false);
            f(read)
        })
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn memory() -> [u64; 3] {
        use std::sync::atomic::Ordering::Relaxed;
        let kept = STAGING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(0, |map| map.len() as u64);
        [STAGED[0].load(Relaxed), STAGED[1].load(Relaxed), kept]
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn let_go() -> [u64; 2] {
        use std::sync::atomic::Ordering::Relaxed;
        [LET_GO[0].load(Relaxed), LET_GO[1].load(Relaxed)]
    }

    pub(crate) fn let_go_idle() {
        use std::sync::atomic::Ordering::Relaxed;
        const IDLE_FRAMES: u64 = 4;
        let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        let Some(map) = map.as_mut() else {
            return;
        };
        if map.is_empty() {
            return;
        }
        let idle_ms = crate::host::ram_idle_ms(crate::host::available_physical_ram());
        let (mut went, mut bytes) = (0u64, 0u64);
        map.retain(|_, s| {
            let keep = Arc::strong_count(s) > 1
                || !crate::host::long_ago(s.used.load(Relaxed), idle_ms, IDLE_FRAMES);
            if !keep {
                LET_GO[0].fetch_add(1, Relaxed);
                LET_GO[1].fetch_add(s.bytes, Relaxed);
                went += 1;
                bytes += s.bytes;
            }
            keep
        });
        if went > 0 {
            crate::host::trail(|| {
                format!(
                    "sweep: {went} staging buffers of textures let go ({} KiB), held by no list",
                    bytes >> 10
                )
            });
        }
    }

    pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
        const TARGET_DEPTH_UNORDERED: i32 = 0x7;
        unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
    }

    pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) {
        TextureUploads::upload_from(list, resource, texture, 0);
    }

    pub(crate) fn upload_from(
        list: &HostList,
        resource: This,
        texture: &ID3D12Resource,
        first: u32,
    ) {
        let host_written = HOST_WRITTEN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|set| set.contains(&(resource as usize)));
        if host_written
            || !list.first_use(resource as usize) && TextureUploads::staged_from(resource) <= first
        {
            return;
        }
        let Some(staging) = TextureUploads::staging_for(resource, texture, first) else {
            return;
        };
        use std::sync::atomic::Ordering::Relaxed;
        staging.used.store(crate::host::moment(), Relaxed);
        if staging.uploaded.load(Relaxed) && !staging.dirty.load(Relaxed) {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexSkipped, 1);
            list.defer_texture(staging);
            return;
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexUploads, 1);
        const COPY_DEST: u32 = 0x400;
        crate::list::CommandList::ensure_state(list, resource, COPY_DEST);
        for (i, footprint) in staging.footprints.iter().enumerate() {
            #[cfg(feature = "diagnostics")]
            {
                let f = footprint.Footprint;
                let (gpu, d) =
                    unsafe { (staging.buffer.GetGPUVirtualAddress(), texture.GetDesc()) };
                let len = u64::from(f.RowPitch) * u64::from(staging.rows[i]) * u64::from(f.Depth);
                list.note_read(
                    || {
                        format!(
                            "texture upload {resource:?} ({}x{}x{} f{} mips {}) sub {i} of {} from staging {gpu:#x}+{:#x}: offset {:#x} {}x{}x{} pitch {}",
                            d.Width,
                            d.Height,
                            d.DepthOrArraySize,
                            d.Format.0,
                            d.MipLevels,
                            staging.footprints.len(),
                            staging.total,
                            footprint.Offset,
                            f.Width,
                            f.Height,
                            f.Depth,
                            f.RowPitch
                        )
                    },
                    (gpu + footprint.Offset, len),
                );
            }
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: staging.subresource(i),
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: unsafe { std::mem::transmute_copy(&staging.buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: *footprint,
                },
            };
            unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
        {
            let staging = staging.clone();
            list.defer_host(move || {
                staging.uploaded.store(true, Relaxed);
                staging.dirty.store(false, Relaxed);
            });
        }
        list.defer_texture(staging);
    }

    pub(crate) fn unstage(resource: This) {
        let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(map) = map.as_mut() {
            map.remove(&(resource as usize));
        }
    }

    pub(crate) fn upload_again(resource: This) {
        let staging = {
            let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
            map.as_ref()
                .and_then(|map| map.get(&(resource as usize)).cloned())
        };
        if let Some(staging) = staging {
            staging
                .uploaded
                .store(false, std::sync::atomic::Ordering::Relaxed);
            staging
                .last
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .partial = None;
        }
    }

    pub(crate) fn staged_memory(resource: This) -> Option<(u64, u64)> {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        let (at, len) = map.as_ref()?.get(&(resource as usize))?.memory();
        Some((at as u64, len as u64))
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn staged_state(resource: This) -> Option<(bool, bool)> {
        use std::sync::atomic::Ordering::Relaxed;
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        let staging = map.as_ref()?.get(&(resource as usize))?;
        Some((staging.uploaded.load(Relaxed), staging.dirty.load(Relaxed)))
    }

    pub(crate) fn is_staged(resource: This) -> bool {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()
            .is_some_and(|map| map.contains_key(&(resource as usize)))
    }

    pub(crate) fn staged(resource: This) -> Option<(u64, bool)> {
        let s = {
            let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
            map.as_ref()?.get(&(resource as usize))?.clone()
        };
        Some((s.size as u64, TitleMemoryMap::committed(s.memory, s.size)))
    }

    fn staged_from(resource: This) -> u32 {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()
            .and_then(|map| map.get(&(resource as usize)))
            .map_or(u32::MAX, |s| s.first)
    }

    fn staged_part(
        layout: &leonida_formats::texture::Layout,
        mips: u32,
        first: u32,
    ) -> (u32, usize) {
        let first = if first < mips && layout.bytes_from(first) < layout.size {
            first
        } else {
            0
        };
        (first, layout.bytes_from(first) as usize)
    }

    fn staging_for(resource: This, texture: &ID3D12Resource, first: u32) -> Option<Arc<Staging>> {
        let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        let map = map.get_or_insert_with(HashMap::new);
        if let Some(s) = map.get(&(resource as usize))
            && s.first <= first
        {
            return Some(s.clone());
        }
        let (memory, desc) = crate::resource::GpuResource::texture_memory(resource)?;
        let Some(layout) = crate::resource::ResourceLayout::texture_layout(&desc)
            .map(|t| t.planes[0].layout.clone())
        else {
            once!(
                ?desc,
                "texture contents without a console layout not uploaded"
            );
            return None;
        };
        if layout.desc.samples != 1 || !layout.addressable() {
            once!(
                ?desc,
                "texture contents without a supported staging layout not uploaded"
            );
            return None;
        }
        let host = host()?;
        let d = unsafe { texture.GetDesc() };
        let (first, size) = TextureUploads::staged_part(&layout, u32::from(d.MipLevels), first);
        let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
            u32::from(d.MipLevels)
        } else {
            u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize) - first
        };
        let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
        let mut rows = vec![0u32; count as usize];
        let mut total = 0;
        unsafe {
            host.device.GetCopyableFootprints(
                &d,
                first,
                count,
                0,
                Some(footprints.as_mut_ptr()),
                Some(rows.as_mut_ptr()),
                None,
                Some(&mut total),
            );
        }
        let (buffer, data) = host.upload_buffer(total)?;
        crate::host::name(&buffer, || {
            format!(
                "staging of {resource:?} {}x{}x{} f{}",
                d.Width, d.Height, d.DepthOrArraySize, d.Format.0
            )
        });
        tracing::debug!(?desc, total, "texture contents uploaded from title memory");
        STAGED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        STAGED[1].fetch_add(total, std::sync::atomic::Ordering::Relaxed);
        let s = Arc::new(Staging {
            buffer,
            #[cfg(feature = "diagnostics")]
            total,
            data,
            footprints,
            rows,
            layout,
            mips: u32::from(d.MipLevels),
            first,
            size,
            bytes: total,
            memory,
            last: Mutex::default(),
            dirty: std::sync::atomic::AtomicBool::new(true),
            uploaded: std::sync::atomic::AtomicBool::new(false),
            used: std::sync::atomic::AtomicU64::new(crate::host::moment()),
        });
        map.insert(resource as usize, s.clone());
        Some(s)
    }
}

impl HostWrites {
    pub(crate) fn note(resource: This) {
        let new = {
            let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
            set.get_or_insert_with(Default::default)
                .insert(resource as usize)
        };
        if new && let Some((at, desc)) = crate::resource::GpuResource::texture_memory(resource) {
            let words = unsafe {
                std::ptr::from_ref(&desc)
                    .cast::<[u32; 14]>()
                    .read_unaligned()
            };
            let bytes = crate::resource::PlacedTextures::placed_bytes(&words).unwrap_or(u64::MAX);
            if !at.is_null() && bytes > 0 {
                HOST_WRITTEN_AT
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        (at as u64, resource as usize),
                        (bytes, words[4], words[6], words[7], words[8]),
                    );
            }
        }
    }

    pub(crate) fn overlaps(start: u64, end: u64) -> bool {
        let writes = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
        writes
            .range(..(end, 0))
            .rev()
            .any(|(&(base, _), entry)| base.saturating_add(entry.0) > start)
    }

    pub(crate) fn note_placed(address: u64, desc: &[u32; 14]) {
        let hit = {
            let map = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
            map.range(..=(address, usize::MAX))
                .next_back()
                .and_then(|(&(at, _), _)| {
                    map.range((at, 0)..=(at, usize::MAX))
                        .find_map(|(_, &d)| (address < at.saturating_add(d.0)).then_some((at, d)))
                })
        };
        if let Some((at, (bytes, width, height, layers_levels, format))) = hit {
            static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
                tracing::warn!(
                    view_address = format_args!("{address:#x}"),
                    view_offset = address - at,
                    view_width = desc[4],
                    view_height = desc[6],
                    view_layers_levels = format_args!("{:#x}", desc[7]),
                    view_format = desc[8],
                    copied_address = format_args!("{at:#x}"),
                    copied_bytes = bytes,
                    copied_width = width,
                    copied_height = height,
                    copied_layers_levels = format_args!("{layers_levels:#x}"),
                    copied_format = format,
                    "HERO LEAD: a placed texture view lies in the memory of a texture the host copied into: it is filled from title memory, which the copy never reached"
                );
            }
        }
    }

    pub(crate) fn wrote(resource: usize) -> bool {
        HOST_WRITTEN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|set| set.contains(&resource))
    }

    pub(crate) fn forget(resource: usize, address: u64) {
        let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(map) = map.as_mut() {
            map.remove(&resource);
        }
        if HOST_WRITTEN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .is_some_and(|set| set.remove(&resource))
        {
            HOST_WRITTEN_AT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(address, resource));
        }
        let mut through = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(through) = through.as_mut() {
            through.remove(&resource);
        }
    }
}

impl TitleMemoryMap {
    pub(crate) fn fingerprint(at: u64, len: u64) -> u64 {
        const PIECE: usize = 4 << 10;
        const PIECES: usize = 12;
        thread_local! {
            static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let len = len as usize;
        READ.with(|read| {
            let mut read = read.borrow_mut();
            read.resize(PIECES * PIECE, 0);
            let mut hasher = blake3::Hasher::new();
            let mut take = |from: usize, bytes: usize| {
                read[..bytes].fill(0);
                crate::host::copy_mapped(
                    read.as_mut_ptr(),
                    (at as usize + from) as *const u8,
                    bytes,
                    false,
                );
                hasher.update(&read[..bytes]);
            };
            if len <= PIECES * PIECE {
                take(0, len);
            } else {
                for n in 0..PIECES - 1 {
                    take(((len - PIECE) / (PIECES - 1) * n) & !0xfff, PIECE);
                }
                take(len - PIECE, PIECE);
            }
            let hash = hasher.finalize();
            let mut first = [0u8; 8];
            first.copy_from_slice(&hash.as_bytes()[..8]);
            u64::from_le_bytes(first) | 1
        })
    }

    pub(crate) fn committed(at: *const u8, len: usize) -> bool {
        if TitleMemoryMap::by_query() {
            return TitleMemoryMap::committed_by_query(at, len);
        }
        if len == 0 {
            return true;
        }
        let (there, of) = TitleMemoryMap::readable(at as usize, len, true);
        there == of
    }

    fn by_query() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_COMMITTED_QUERY").is_some_and(|v| v == "1"))
    }

    fn readable(at: usize, len: usize, all: bool) -> (usize, usize) {
        use windows_sys::Win32::System::ProcessStatus::{
            K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        const KERNEL_PAGE: usize = 4096;
        let (first, last) = (at / KERNEL_PAGE, at.saturating_add(len - 1) / KERNEL_PAGE);
        let of = last - first + 1;
        let mut there = 0;
        let mut info: [PSAPI_WORKING_SET_EX_INFORMATION; 256] = unsafe { std::mem::zeroed() };
        let mut page = first;
        while page <= last {
            let n = (last - page + 1).min(info.len());
            for (i, entry) in info[..n].iter_mut().enumerate() {
                entry.VirtualAddress = ((page + i) * KERNEL_PAGE) as *mut _;
            }
            let asked = unsafe {
                K32QueryWorkingSetEx(
                    GetCurrentProcess(),
                    info.as_mut_ptr().cast(),
                    (n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
                )
            } != 0;
            for (i, entry) in info[..n].iter().enumerate() {
                let valid = asked && unsafe { entry.VirtualAttributes.Flags } & 1 != 0;
                let mut byte = [0u8];
                if valid
                    || crate::host::read_into(((page + i) * KERNEL_PAGE) as *const u8, &mut byte)
                {
                    there += 1;
                } else if all {
                    return (there, of);
                }
            }
            page += n;
        }
        (there, of)
    }

    fn committed_by_query(at: *const u8, len: usize) -> bool {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery,
        };
        let mut p = at as usize;
        let end = p + len;
        while p < end {
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
            if n == 0 || info.State != MEM_COMMIT {
                return false;
            }
            p = info.BaseAddress as usize + info.RegionSize;
        }
        true
    }

    pub(crate) fn mapped_bytes(at: *const u8, len: usize) -> u64 {
        let start = at as usize;
        let end = start.saturating_add(len);
        if start == end {
            return 0;
        }
        let query = TitleMemoryMap::by_query();
        let ends_missing = !query
            && !TitleMemoryMap::committed(at, 1)
            && !TitleMemoryMap::committed((end - 1) as *const u8, 1);
        if ends_missing && (end - 1) / 4096 - start / 4096 <= 1 {
            return 0;
        }
        if !query && !ends_missing {
            static BY_PAGE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *BY_PAGE.get_or_init(|| {
                std::env::var_os("LEONIDA_MAPPED_BY_PAGE").is_some_and(|v| v == "1")
            }) {
                let (there, _) = TitleMemoryMap::readable(start, len, false);
                return (there as u64 * 4096).min(len as u64);
            }
            const BLOCK: usize = 64 << 10;
            let mut there = 0u64;
            let mut block = start & !(BLOCK - 1);
            while block < end {
                let (lo, hi) = (block.max(start), block.saturating_add(BLOCK).min(end));
                if TitleMemoryMap::readable(lo, 1, true).0 == 1 {
                    there += (hi - lo) as u64;
                }
                block = block.saturating_add(BLOCK);
            }
            return there;
        }
        TitleMemoryMap::mapped_regions(start, end, false)
    }

    pub(crate) fn any_mapped(start: usize, len: usize) -> bool {
        let end = start.saturating_add(len);
        if start == end {
            return false;
        }
        if TitleMemoryMap::committed(start as *const u8, 1) {
            return true;
        }
        (end - 1) / 4096 > start / 4096 && TitleMemoryMap::mapped_regions(start, end, true) > 0
    }

    fn mapped_regions(start: usize, end: usize, first_only: bool) -> u64 {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
        };
        let (mut p, mut there) = (start, 0u64);
        while p < end {
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
            let next = (info.BaseAddress as usize)
                .saturating_add(info.RegionSize)
                .min(end);
            if n == 0 || next <= p {
                break;
            }
            if info.State == MEM_COMMIT && info.Protect & (PAGE_NOACCESS | PAGE_GUARD) == 0 {
                there += (next - p) as u64;
                if first_only {
                    break;
                }
            }
            p = next;
        }
        there
    }
}

pub(crate) static UNLOCKED: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

pub(crate) static REFRESH_WAITS: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

thread_local! {
    static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

static STAGED: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

impl Drop for Staging {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        STAGED[0].fetch_sub(1, Relaxed);
        STAGED[1].fetch_sub(self.bytes, Relaxed);
    }
}

static LET_GO: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

static HOST_WRITTEN_AT: Mutex<std::collections::BTreeMap<(u64, usize), (u64, u32, u32, u32, u32)>> =
    Mutex::new(std::collections::BTreeMap::new());

impl Staging {
    pub(crate) fn memory(&self) -> (usize, usize) {
        (self.memory as usize, self.size)
    }

    fn part(&self, i: usize) -> (u32, u32) {
        let mips = self.mips - self.first;
        (self.first + i as u32 % mips, i as u32 / mips)
    }

    fn subresource(&self, i: usize) -> u32 {
        let (mip, slice) = self.part(i);
        slice * self.mips + mip
    }

    pub(crate) fn refresh(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let read = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    fn refresh_now(&self) -> usize {
        let whole = std::time::Instant::now();
        let read = self.refresh_timed();
        REFRESH_WAITS[3].fetch_add(
            whole.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        read
    }

    fn refresh_timed(&self) -> usize {
        let size = self.size;
        let start = self.memory as usize;
        let Some(end) = start.checked_add(size) else {
            return 0;
        };
        let mut last =
            TextureUploads::waited(0, || self.last.lock().unwrap_or_else(|e| e.into_inner()));
        let last = &mut *last;
        if let Some((now, written)) =
            TextureUploads::waited(1, || crate::watch::WriteJournal::writes(start, end, false))
        {
            if last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked) {
                return 0;
            }
            last.asked = now;
            TextureUploads::with_read(size, |read| {
                crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
                self.detile(read);
            });
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
            return size;
        }
        static GUARD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let mut unseen = false;
        if *GUARD.get_or_init(|| std::env::var_os("LEONIDA_TEXTURE_GUARD").is_none_or(|v| v != "0"))
            && let Some((now, written)) =
                TextureUploads::waited(1, || crate::watch::WriteJournal::writes(start, end, true))
        {
            const HASH_EVERY: u64 = 8;
            let unwritten = last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked);
            last.asked = now;
            let frame = crate::frame::FramePacer::frame();
            let turn = frame
                .wrapping_add((start >> 16) as u64)
                .is_multiple_of(HASH_EVERY);
            let desc = &self.layout.desc;
            let film = desc.swizzle.0 == 0 && desc.mips == 1;
            if unwritten && !turn && !film && frame >= last.unguarded {
                return 0;
            }
            unseen = unwritten;
        }
        let _mapped = if TextureUploads::no_wait() {
            let Some(mapped) = crate::watch::TitleMemory::reading_now(start..end) else {
                return self.refresh_unlocked(last, size);
            };
            mapped
        } else {
            TextureUploads::waited(2, || crate::watch::TitleMemory::reading_range(start..end))
        };
        if !TitleMemoryMap::committed(self.memory, size) {
            let frame = crate::frame::FramePacer::frame();
            if last.partial.is_some_and(|at| frame < at + 30) {
                return 0;
            }
            last.partial = Some(frame);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
            let changed = TextureUploads::with_read(size, |read| {
                crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
                let hash = blake3::hash(read);
                if last.hash == Some(hash) {
                    return false;
                }
                last.hash = Some(hash);
                if read.iter().all(|&b| b == 0) {
                    return false;
                }
                self.detile(read);
                true
            });
            #[cfg(feature = "diagnostics")]
            if changed {
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
            }
            let _ = changed;
            return size;
        }
        if TextureUploads::through_copy() {
            return TextureUploads::with_title(self.memory, size, |tiled| {
                let hash = blake3::hash(tiled);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
                if last.hash == Some(hash) {
                    return size;
                }
                if unseen && last.hash.is_some() {
                    self.unseen_write(last);
                }
                last.hash = Some(hash);
                self.detile(tiled);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
                size
            });
        }
        let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
        let hash = blake3::hash(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        if last.hash == Some(hash) {
            return size;
        }
        if unseen && last.hash.is_some() {
            self.unseen_write(last);
        }
        last.hash = Some(hash);
        self.detile(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        size
    }

    fn unseen_write(&self, last: &mut Last) {
        let frame = crate::frame::FramePacer::frame();
        let more = if frame < last.unguarded { 120 } else { 16 };
        #[cfg(feature = "diagnostics")]
        {
            use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
            static SEEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 24 {
                let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
                unsafe { VirtualQuery(self.memory.cast(), &mut info, size_of_val(&info)) };
                tracing::info!(
                    frame,
                    memory = ?self.memory,
                    size = self.layout.size,
                    protection = format_args!("{:#x}", info.Protect),
                    kind = format_args!("{:#x}", info.Type),
                    run = info.RegionSize,
                    desc = ?self.layout.desc,
                    "a guarded texture changed and no write to it was seen"
                );
            }
        }
        last.unguarded = frame + more;
    }

    fn refresh_unlocked(&self, last: &mut Last, size: usize) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let read = TextureUploads::with_read(size, |read| {
            if !crate::host::read_into(self.memory, read) {
                return false;
            }
            let hash = blake3::hash(read);
            if last.hash != Some(hash) {
                last.hash = Some(hash);
                self.detile(read);
            }
            true
        });
        if read {
            UNLOCKED[0].fetch_add(1, Relaxed);
            size
        } else {
            UNLOCKED[1].fetch_add(1, Relaxed);
            last.asked = 0;
            0
        }
    }

    fn detile(&self, tiled: &[u8]) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        if tiled.iter().all(|&b| b == 0) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
            let (mip, slice) = self.part(i);
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            let len = pitch * rows as usize * f.Depth as usize;
            let linear = unsafe {
                std::slice::from_raw_parts_mut(self.data.add(footprint.Offset as usize), len)
            };
            if let Err(e) =
                self.layout
                    .detile(tiled, mip, slice, linear, pitch, pitch * rows as usize)
            {
                tracing::warn!(?e, mip, slice, "texture not detiled");
                return;
            }
        }
    }
}

#[cfg(test)]
mod first_mip_tests {
    use leonida_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};

    fn layout(depth_or_array: u32) -> leonida_formats::texture::Layout {
        let desc = TextureDesc {
            dimension: Dimension::Tex2d,
            width: 1024,
            height: 1024,
            depth_or_array,
            mips: 9,
            format: 71,
            samples: 1,
            swizzle: Swizzle::SW_4KB_S,
        };
        Texture::new(&desc).unwrap().planes[0].layout.clone()
    }

    #[test]
    fn a_view_of_the_smaller_mips_stages_only_their_memory() {
        let l = layout(1);
        assert_eq!(
            super::TextureUploads::staged_part(&l, 9, 4),
            (4, l.bytes_from(4) as usize)
        );
        assert!(l.bytes_from(4) < l.size);
        assert_eq!(
            super::TextureUploads::staged_part(&l, 9, 0),
            (0, l.size as usize)
        );
        assert_eq!(
            super::TextureUploads::staged_part(&l, 9, 9),
            (0, l.size as usize),
            "past the chain"
        );
        let array = layout(2);
        assert_eq!(
            super::TextureUploads::staged_part(&array, 9, 4),
            (0, array.size as usize),
            "slices each hold their chain"
        );
    }

    #[test]
    fn a_2d_view_names_its_first_mip() {
        assert_eq!(
            crate::resource::PlacedTextures::first_mip(&[71, 4, 0x1688, 0, 4, 5, 0, 0, 0, 0]),
            4
        );
        assert_eq!(
            crate::resource::PlacedTextures::first_mip(&[71, 5, 0x1688, 0, 4, 5, 0, 0, 0, 0]),
            0,
            "an array view"
        );
        assert_eq!(
            crate::resource::PlacedTextures::first_mip(&[71, 4, 0x1688, 0, 99, 5, 0, 0, 0, 0]),
            15
        );
    }
}

#[cfg(test)]
mod title_memory_tests {
    use super::TextureUploads;

    #[test]
    fn a_texture_refresh_waits_only_for_its_own_mapping() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        use std::time::{Duration, Instant};
        use windows::core::{IUnknown, Interface};
        use windows_sys::Win32::System::Memory::*;

        if crate::host::host().is_none() {
            return;
        }
        let words: [u32; 14] = [3, 0, 0, 0, 16, 0, 4, 65537, 10, 1, 0, 256, 3_145_728, 0];
        let desc =
            unsafe { std::mem::transmute::<[u32; 14], crate::resource::ResourceDesc>(words) };
        let page = 64 << 10;
        let memory = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 * page,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        assert!(!memory.is_null());
        let at = memory as usize;
        unsafe { memory.cast::<u8>().write_bytes(0x57, 2 * page) };
        let mut title = std::ptr::null();
        assert_eq!(
            crate::resource::PlacedTextures::create_at_address(
                std::ptr::null_mut(),
                at as u64,
                &desc,
                0,
                std::ptr::null(),
                &crate::resource::PAGEABLE_IIDS[0],
                &mut title
            ),
            0
        );
        let owner = unsafe { IUnknown::from_raw(title.cast_mut().cast()) };
        let host = crate::resource::HostTextures::host_copy(title).unwrap();
        let staging = super::TextureUploads::staging_for(title, &host.texture, 0).unwrap();
        assert!(staging.size <= page);
        for (range, overlapping) in [(at + page..at + 2 * page, false), (at..at + page, true)] {
            *staging.last.lock().unwrap() = super::Last::default();
            let finished = AtomicBool::new(false);
            std::thread::scope(|s| {
                let (started, ready) = mpsc::channel();
                let finished = &finished;
                s.spawn(move || {
                    let _writer = crate::watch::TitleMemory::remapping_range(range);
                    started.send(()).unwrap();
                    std::thread::sleep(Duration::from_secs(1));
                    finished.store(true, Ordering::Release);
                });
                ready.recv().unwrap();
                let start = Instant::now();
                assert_eq!(staging.refresh_timed(), staging.size);
                eprintln!(
                    "texture refresh, overlap={overlapping}: {:?}",
                    start.elapsed()
                );
                assert_eq!(
                    finished.load(Ordering::Acquire),
                    overlapping && !super::TextureUploads::no_wait()
                );
                assert_eq!(unsafe { *staging.data }, 0x57);
            });
        }
        drop(staging);
        drop(host);
        drop(owner);
        crate::watch::TitleMemory::remapped(at, 2 * page);
        assert_ne!(unsafe { VirtualFree(memory, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn a_texture_with_only_interior_pages_counts_its_resident_bytes() {
        use windows_sys::Win32::System::Memory::*;
        const BLOCK: usize = 64 << 10;
        unsafe {
            let base =
                VirtualAlloc(std::ptr::null(), 5 * BLOCK, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            let middle = base.add(2 * BLOCK);
            assert_eq!(
                VirtualAlloc(middle.cast(), BLOCK, MEM_COMMIT, PAGE_READWRITE),
                middle.cast()
            );
            assert_eq!(
                super::TitleMemoryMap::mapped_bytes(base, 5 * BLOCK),
                BLOCK as u64
            );
            assert!(super::TitleMemoryMap::any_mapped(base as usize, 5 * BLOCK));
            assert_eq!(
                super::TitleMemoryMap::mapped_bytes(base.add(BLOCK - 3), 2 * BLOCK + 9),
                BLOCK as u64
            );
            assert_eq!(super::TitleMemoryMap::mapped_bytes(base, 2 * BLOCK), 0);
            assert_ne!(VirtualFree(middle.cast(), BLOCK, MEM_DECOMMIT), 0);
            assert_eq!(super::TitleMemoryMap::mapped_bytes(base, 5 * BLOCK), 0);
            assert!(!super::TitleMemoryMap::any_mapped(base as usize, 5 * BLOCK));
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    #[test]
    fn an_uncommitted_tail_reads_as_zeros() {
        use windows_sys::Win32::System::Memory::*;
        const HEAD: usize = 0xb_0000;
        const SIZE: usize = 0x20_0000;
        unsafe {
            let other = vec![0x77u8; SIZE];
            TextureUploads::with_title(other.as_ptr(), SIZE, |bytes| {
                assert!(bytes.iter().all(|&b| b == 0x77))
            });
            let base =
                VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(
                VirtualAlloc(base.cast(), HEAD, MEM_COMMIT, PAGE_READWRITE),
                base.cast()
            );
            base.write_bytes(0x5a, HEAD);
            TextureUploads::with_title(base, SIZE, |bytes| {
                assert!(bytes[..HEAD].iter().all(|&b| b == 0x5a), "what is mapped");
                assert!(bytes[HEAD..].iter().all(|&b| b == 0), "what is not: zeros");
            });
            assert_ne!(
                VirtualFree(base.add(0x4_0000).cast(), 0x1_0000, MEM_DECOMMIT),
                0
            );
            TextureUploads::with_title(base, SIZE, |bytes| {
                assert!(bytes[..0x4_0000].iter().all(|&b| b == 0x5a));
                assert!(bytes[0x4_0000..0x5_0000].iter().all(|&b| b == 0));
                assert!(bytes[0x5_0000..HEAD].iter().all(|&b| b == 0x5a));
                assert!(bytes[HEAD..].iter().all(|&b| b == 0));
            });
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    #[test]
    fn the_small_texture_of_runs_72_and_73_detiles_the_same_both_ways() {
        use windows_sys::Win32::System::Memory::*;
        for (height, within) in [(4u32, 0x300usize), (8, 0x700), (4, 0xf00), (8, 0xf00)] {
            let words: [u32; 14] = [
                3, 0, 0, 0, 16, 0, height, 65537, 10, 1, 0, 256, 3_145_728, 0,
            ];
            let desc: crate::resource::ResourceDesc = unsafe { std::mem::transmute(words) };
            let layout = crate::resource::ResourceLayout::texture_layout(&desc)
                .expect("a console layout")
                .planes[0]
                .layout
                .clone();
            let size = layout.size as usize;
            let (row, pitch) = (16 * 8, 256usize);
            assert!(
                size >= row * height as usize,
                "the layout holds every texel"
            );
            unsafe {
                let pages = size.div_ceil(4096) + 1;
                let base = VirtualAlloc(
                    std::ptr::null(),
                    (pages + 1) * 4096,
                    MEM_RESERVE,
                    PAGE_NOACCESS,
                )
                .cast::<u8>();
                assert!(!base.is_null());
                assert_eq!(
                    VirtualAlloc(base.cast(), pages * 4096, MEM_COMMIT, PAGE_READWRITE),
                    base.cast()
                );
                for i in 0..pages * 4096 {
                    base.add(i).write((i * 7 + i / 251) as u8);
                }
                let memory = base.add(within);
                let in_place = std::slice::from_raw_parts(memory, size);
                TextureUploads::with_title(memory, size, |copied| assert_eq!(copied, in_place));
                let staged = (height as usize - 1) * pitch + row;
                assert_eq!(staged, if height == 4 { 896 } else { 1920 });
                let detile = |tiled: &[u8]| {
                    let mut linear = vec![0xeeu8; pitch * height as usize];
                    layout
                        .detile(tiled, 0, 0, &mut linear, pitch, pitch * height as usize)
                        .expect("detiled");
                    linear
                };
                let direct = detile(in_place);
                let through = TextureUploads::with_title(memory, size, |copied| detile(copied));
                assert_eq!(direct, through);
                for y in 0..height as usize {
                    assert!(
                        direct[y * pitch..][..row].iter().any(|&b| b != 0xee),
                        "row {y} written"
                    );
                    assert!(
                        direct[y * pitch + row..(y + 1) * pitch]
                            .iter()
                            .all(|&b| b == 0xee),
                        "nothing past row {y}"
                    );
                }
                let mut exact = vec![0u8; staged];
                layout
                    .detile(in_place, 0, 0, &mut exact, pitch, pitch * height as usize)
                    .expect("the staging buffer's bytes are enough");
                assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
            }
        }
    }
}
