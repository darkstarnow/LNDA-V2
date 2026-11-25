use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D12::*;

use crate::object::This;

fn kinds() -> &'static [(u64, u32, u32)] {
    static KINDS: OnceLock<Vec<(u64, u32, u32)>> = OnceLock::new();
    KINDS.get_or_init(|| {
        const DEFAULT: &str = "256x256f41,256x256f16,64x64f16,128x128f36";
        let list = std::env::var("LEONIDA_TEXTURE_BACK").unwrap_or_else(|_| DEFAULT.to_owned());
        let kinds: Vec<(u64, u32, u32)> = list
            .split(',')
            .filter_map(|k| {
                let (size, format) = k.trim().split_once('f')?;
                let (w, h) = size.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?, format.parse().ok()?))
            })
            .collect();
        if !kinds.is_empty() {
            tracing::info!(?kinds, "textures of these kinds that shaders write are copied back into title memory (LEONIDA_TEXTURE_BACK)");
        }
        kinds
    })
}

pub(crate) fn wanted(resource: This) -> bool {
    let kinds = kinds();
    !kinds.is_empty()
        && crate::resource::PlacedTextures::texture_kind(resource)
            .is_some_and(|k| kinds.contains(&k))
}

pub(crate) struct Back {
    _resource: crate::resource::ResourceRef,
    mapping: crate::inplace::ReadbackMapping,
    buffer: ID3D12Resource,
    buffer_bytes: usize,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    mips: u32,
    layout: leonida_formats::texture::Layout,
    at: usize,
    size: usize,
    kind: Option<(u64, u32, u32)>,
}

fn subresource_count(desc: &D3D12_RESOURCE_DESC) -> u32 {
    let slices = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        1
    } else {
        u32::from(desc.DepthOrArraySize)
    };
    u32::from(desc.MipLevels) * slices
}

impl Back {
    pub(crate) fn destination(&self) -> (usize, usize) {
        (self.at, self.at + self.size)
    }
}

pub(crate) fn add_destination_aliases(ranges: &mut Vec<(usize, usize)>) {
    const PAGE: usize = 64 << 10;
    for index in 0..ranges.len() {
        let (at, end) = ranges[index];
        let mut page = at & !(PAGE - 1);
        while page < end {
            let lo = at.saturating_sub(page);
            let hi = (end - page).min(PAGE);
            for alias in crate::inplace::SectionViews::pages_of(page) {
                if alias != page {
                    ranges.push((alias + lo, alias + hi));
                }
            }
            page = page.saturating_add(PAGE);
        }
    }
}

pub(crate) fn record(
    list: &ID3D12GraphicsCommandList,
    retained: crate::resource::ResourceRef,
    texture: &ID3D12Resource,
) -> Option<Back> {
    let host = crate::host::Host::instance()?;
    let resource = retained.as_ptr();
    let (layout, at, size) = crate::resource::PlacedTextures::title_texture(resource)?;
    at.checked_add(size)?;
    let desc = unsafe { texture.GetDesc() };
    let mips = u32::from(desc.MipLevels);
    let subresources = subresource_count(&desc);
    let n = subresources as usize;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n];
    let mut rows = vec![0u32; n];
    let mut total = 0u64;
    unsafe {
        host.device.GetCopyableFootprints(
            &desc,
            0,
            subresources,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        )
    };
    let buffer_bytes = usize::try_from(total).ok()?;
    if buffer_bytes == 0 || buffer_bytes > isize::MAX as usize || at == 0 || size == 0 {
        return None;
    }
    let mapping = crate::inplace::ReadbackMapping::record(at, size)?;
    let buffer = host.read_back_buffer(total)?;
    for (i, footprint) in footprints.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    COPIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let kind = crate::resource::PlacedTextures::texture_kind(resource);
    Some(Back {
        _resource: retained,
        mapping,
        buffer,
        buffer_bytes,
        footprints,
        rows,
        mips,
        layout,
        at,
        size,
        kind,
    })
}

pub(crate) static COPIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn land(back: &Back) {
    let Some(_mapping) = back.mapping.for_write() else {
        return;
    };
    let mut tiled = vec![0u8; back.size];
    crate::host::TitleCopy::copy_mapped(tiled.as_mut_ptr(), back.at as *const u8, back.size, false);
    let mut data = std::ptr::null_mut();
    unsafe {
        if back.buffer.Map(0, None, Some(&mut data)).is_err() || data.is_null() {
            return;
        }
    }
    let mapped = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), back.buffer_bytes) };
    for (i, (footprint, &rows)) in back.footprints.iter().zip(&back.rows).enumerate() {
        let (mip, slice) = (i as u32 % back.mips, i as u32 / back.mips);
        let f = footprint.Footprint;
        let pitch = f.RowPitch as usize;
        let end = back
            .footprints
            .get(i + 1)
            .map_or(mapped.len(), |next| next.Offset as usize);
        let linear = &mapped[footprint.Offset as usize..end];
        if let Err(e) =
            back.layout
                .tile(&mut tiled, mip, slice, linear, pitch, pitch * rows as usize)
        {
            static SEEN: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
            let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
            if !seen.contains(&back.at) {
                seen.push(back.at);
                tracing::warn!(
                    ?e,
                    mip,
                    slice,
                    kind = ?back.kind,
                    at = format_args!("{:#x}", back.at),
                    desc = ?back.layout.desc,
                    "a texture copied back could not be tiled (logged once per texture)"
                );
            }
            unsafe { back.buffer.Unmap(0, None) };
            return;
        }
    }
    unsafe { back.buffer.Unmap(0, None) };
    crate::host::TitleCopy::copy_mapped(back.at as *mut u8, tiled.as_ptr(), back.size, true);
    #[cfg(feature = "diagnostics")]
    verify_first_landing(back, &tiled);
}

#[cfg(feature = "diagnostics")]
fn verify_first_landing(back: &Back, tiled: &[u8]) {
    static SEEN: std::sync::Mutex<Vec<(u64, u32, u32)>> = std::sync::Mutex::new(Vec::new());
    let Some(kind) = back.kind else { return };
    {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        if seen.contains(&kind) {
            return;
        }
        seen.push(kind);
    }
    let mut guest = vec![0; tiled.len()];
    let complete =
        crate::host::TitleCopy::read(back.at as *const u8, guest.as_mut_ptr(), guest.len());
    let verified = complete && guest == tiled;
    let desc = back.layout.desc;
    let (mut finite, mut nonzero, mut lo, mut hi) =
        (0usize, 0usize, f32::INFINITY, f32::NEG_INFINITY);
    if verified && matches!(kind.2, 16 | 41) {
        for gy in 0..5 {
            for gx in 0..5 {
                let (x, y) = ((desc.width - 1) * gx / 4, (desc.height - 1) * gy / 4);
                let Ok(offset) = back.layout.offset(0, x, y, 0) else {
                    continue;
                };
                let at = offset as usize;
                let Some(texel) = guest.get(at..at + desc.element_bytes as usize) else {
                    continue;
                };
                for bytes in texel.chunks_exact(4) {
                    let value = f32::from_le_bytes(bytes.try_into().unwrap());
                    if value.is_finite() {
                        finite += 1;
                        nonzero += usize::from(value != 0.0);
                        lo = lo.min(value);
                        hi = hi.max(value);
                    }
                }
            }
        }
    }
    let sample_range = (finite != 0).then_some((lo, hi));
    tracing::info!(?kind, at = format_args!("{:#x}", back.at), resource = ?back._resource.as_ptr(),
        bytes = tiled.len(), verified, finite, nonzero, ?sample_range, "first texture copy-back checked in title memory");
}
