use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::{Descriptors, VIEW_TAG, View};
use crate::host::Host;

pub(crate) const FIRST_RAW_PAGE_SLOTS: u32 = 1024;

struct RawPageRun {
    base: u32,
    slots: u32,
    populated: u32,
}

pub(crate) struct HostHeap {
    pub(crate) heap: ID3D12DescriptorHeap,
    null_views: Option<ID3D12DescriptorHeap>,
    texture_pressure: std::sync::atomic::AtomicBool,
    cpu: usize,
    step: u32,
    count: u32,
    dynamic: u32,
    sampler: bool,
    made: Mutex<Vec<u64>>,
    kept: Mutex<Vec<Option<ID3D12Resource>>>,
    next: Mutex<(u32, Vec<bool>)>,
    free: std::sync::atomic::AtomicU32,
    too_long: std::sync::atomic::AtomicU32,
    strides: Option<(ID3D12Resource, u64, usize)>,
    addresses: std::sync::OnceLock<(ID3D12Resource, usize)>,
    raw_pages: Mutex<Option<RawPageRun>>,
    buffers: Mutex<BufferSlots>,
    changed: Mutex<std::collections::HashSet<u32>>,
    pub(crate) buffer_pass: Mutex<BufferPass>,
    textures: Mutex<TextureQueue>,
    texture_sweep: Mutex<TextureSweep>,
    pub(crate) material_demand: crate::material_demand::Mailbox,
    material_epochs: std::sync::OnceLock<Box<[std::sync::atomic::AtomicU64]>>,
    material_clock: std::sync::atomic::AtomicU64,
    tabled: Vec<std::sync::atomic::AtomicU32>,
    placed: Mutex<Spans>,
}

#[derive(Default)]
struct BufferSlots {
    indices: std::collections::BTreeSet<u32>,
    acceleration: std::collections::BTreeMap<u32, u64>,
    acceleration_locations: Option<Arc<[u64]>>,
}

#[derive(Default)]
pub(crate) struct BufferPass {
    pub(crate) frame: Option<u64>,
    pub(crate) passes: u64,
    pub(crate) ranges:
        std::collections::HashMap<u32, (std::sync::Arc<crate::shadow::Shadow>, u64, u64)>,
}

#[derive(Default)]
pub(crate) struct Spans {
    spans: std::collections::BTreeMap<(u64, u64), ([u32; 14], std::collections::BTreeSet<u32>)>,
    by_slot: std::collections::HashMap<u32, (u64, u64)>,
    displaced: std::collections::BTreeMap<u32, u64>,
    displaced_next: u32,
    sizes: std::collections::HashMap<([u32; 14], u32), Option<u64>>,
}

impl Spans {
    fn displaced_batch(&mut self, max: usize) -> Vec<(u32, u64)> {
        let batch: Vec<_> = self
            .displaced
            .range(self.displaced_next..)
            .chain(self.displaced.range(..self.displaced_next))
            .take(max)
            .map(|(&slot, &key)| (slot, key))
            .collect();
        if let Some(&(slot, _)) = batch.last() {
            self.displaced_next = slot.wrapping_add(1);
        }
        batch
    }

    fn bytes(&mut self, desc: &[u32; 14], first: u32) -> Option<u64> {
        *self
            .sizes
            .entry((crate::resource::PlacedTextures::placed_key(*desc), first))
            .or_insert_with(|| crate::resource::PlacedTextures::placed_bytes_from(desc, first))
    }

    fn overlapping(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        let before = self
            .spans
            .range(..(start, 0))
            .next_back()
            .map(|(&(at, _), _)| at);
        let before = before
            .into_iter()
            .flat_map(|at| self.spans.range((at, 0)..=(at, u64::MAX)));
        before
            .map(|(&span, _)| span)
            .filter(|span| span.1 > start)
            .chain(
                self.spans
                    .range((start, 0)..(end, 0))
                    .map(|(&span, _)| span),
            )
            .collect()
    }

    fn against(&self, start: u64, end: u64, desc: &[u32; 14]) -> Vec<(u64, u64)> {
        let mut spans = self.overlapping(start, end);
        let key = crate::resource::PlacedTextures::placed_key(*desc);
        spans.retain(|span| {
            let same = span.0 == start
                && (span.1 == end || self.spans.get(span).is_some_and(|s| s.0 == key));
            !same
        });
        spans
    }

    fn note(
        &mut self,
        index: u32,
        start: u64,
        end: u64,
        desc: &[u32; 14],
        newer: bool,
    ) -> Vec<u32> {
        if self.by_slot.get(&index) == Some(&(start, end)) {
            return Vec::new();
        }
        self.forget(index);
        if end <= start {
            return Vec::new();
        }
        let mut stale = Vec::new();
        for span in self.against(start, end, desc) {
            if !newer {
                return stale;
            }
            if let Some((_, slots)) = self.spans.remove(&span) {
                for slot in slots {
                    self.by_slot.remove(&slot);
                    stale.push(slot);
                }
            }
        }
        self.spans
            .entry((start, end))
            .or_insert_with(|| {
                (
                    crate::resource::PlacedTextures::placed_key(*desc),
                    Default::default(),
                )
            })
            .1
            .insert(index);
        self.by_slot.insert(index, (start, end));
        stale
    }

    fn forget(&mut self, index: u32) {
        let Some(at) = self.by_slot.remove(&index) else {
            return;
        };
        if let Some(span) = self.spans.get_mut(&at) {
            span.1.remove(&index);
            if span.1.is_empty() {
                self.spans.remove(&at);
            }
        }
    }

    fn claim(
        &self,
        index: u32,
        start: u64,
        end: u64,
        desc: &[u32; 14],
    ) -> (Claim, Option<(u64, u64)>) {
        if self.by_slot.get(&index) == Some(&(start, end)) {
            return (Claim::Own, None);
        }
        let spans = self.against(start, end, desc);
        match spans.first() {
            None => (Claim::Free, None),
            Some(&span) if spans.iter().all(|s| s.0 == start) => (Claim::SameStart, Some(span)),
            Some(&span) => (Claim::Other, Some(span)),
        }
    }

    fn slots_in(&self, start: u64, end: u64) -> Vec<u32> {
        self.overlapping(start, end)
            .iter()
            .filter_map(|at| self.spans.get(at))
            .flat_map(|span| span.1.iter().copied())
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Claim {
    Own,
    Free,
    SameStart,
    Other,
}

pub(crate) static SWEPT: [std::sync::atomic::AtomicU64; 11] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 11];

#[derive(Default)]
struct TextureSweep {
    next: u32,
    counts: [u64; 10],
    last_ms: u64,
    resume: Vec<u32>,
}

static REMAPPED: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

static SPANNED: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(u64::MAX),
    std::sync::atomic::AtomicU64::new(0),
];

pub(crate) static STALE: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

const TABLED_FRAMES: u64 = 240;

const TABLED_GRACE: u64 = 8;

const TABLED_READ: u32 = 1 << 31;

pub(crate) static TABLED_WRITES: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Waiting {
    due: u64,
    written: u64,
    write: u64,
    looks: u8,
    recovered: bool,
    mapped: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Recheck {
    due: u64,
    pub(crate) at: (u64, u64),
    pub(crate) print: u64,
    pub(crate) looks: u8,
    pub(crate) uploaded: u64,
}

const RECHECK_FRAMES: [u64; 5] = [4, 12, 40, 120, 400];

#[derive(Default)]
pub(crate) struct TextureQueue {
    slots: std::collections::BTreeMap<u32, Waiting>,
    rechecks: std::collections::BTreeMap<u32, Recheck>,
    writes: u64,
}

impl TextureQueue {
    fn written(&mut self, index: u32, frame: u64, due: u64, seen: bool) {
        if seen && self.slots.contains_key(&index) {
            return;
        }
        self.writes += 1;
        let write = self.writes;
        self.rechecks.remove(&index);
        self.slots.insert(
            index,
            Waiting {
                due,
                written: frame,
                write,
                looks: 0,
                mapped: 0,
                recovered: false,
            },
        );
    }

    fn remove(&mut self, index: u32) {
        self.slots.remove(&index);
        self.rechecks.remove(&index);
    }

    fn recheck(&mut self, index: u32, frame: u64, at: (u64, u64), print: u64) {
        if !self.slots.contains_key(&index) {
            let due = frame + RECHECK_FRAMES[0];
            self.rechecks.insert(
                index,
                Recheck {
                    due,
                    at,
                    print,
                    looks: 0,
                    uploaded: frame,
                },
            );
        }
    }

    fn rechecks_due(&self, frame: u64, max: usize) -> Vec<(u32, Recheck)> {
        let mut due: Vec<(u32, Recheck)> = self
            .rechecks
            .iter()
            .filter(|(_, r)| r.due <= frame)
            .map(|(&index, &r)| (index, r))
            .collect();
        due.sort_unstable_by_key(|&(index, r)| (r.due, index));
        due.truncate(max);
        due
    }

    fn rechecked(&mut self, index: u32, seen: &Recheck, changed: bool, frame: u64) {
        if self.rechecks.get(&index) != Some(seen) {
            return;
        }
        if changed {
            self.changed(index, frame, frame);
            return;
        }
        match RECHECK_FRAMES.get(usize::from(seen.looks) + 1) {
            Some(wait) => {
                let next = Recheck {
                    due: frame + wait,
                    looks: seen.looks + 1,
                    ..*seen
                };
                self.rechecks.insert(index, next);
            }
            None => {
                self.rechecks.remove(&index);
            }
        }
    }

    fn due(&self, frame: u64, max: usize) -> Vec<(u32, u64, bool)> {
        let mut again = Vec::new();
        let mut fresh = Vec::new();
        for (&index, w) in self.slots.iter().filter(|(_, w)| w.due <= frame) {
            let class = if w.looks > 0 || w.mapped > 0 {
                &mut again
            } else {
                &mut fresh
            };
            class.push((index, w.write, w.recovered));
        }
        again.sort_unstable_by_key(|&(_, write, _)| write);
        fresh.sort_unstable_by_key(|&(_, write, _)| write);
        let count = max.min(again.len() + fresh.len());
        let mut fresh = fresh.into_iter();
        let mut retry = frame & 1 == 0;
        let mut newest = again.is_empty() || frame & 2 == 0;
        let mut order = Vec::with_capacity(count);
        while order.len() < count {
            if !again.is_empty() && (retry || fresh.len() == 0) {
                order.push(again.pop().unwrap());
            } else {
                let next = if newest {
                    fresh.next_back()
                } else {
                    fresh.next()
                };
                order.push(next.unwrap());
                newest = !newest;
            }
            retry = !retry;
        }
        order
    }

    fn same(&mut self, index: u32, write: u64) -> Option<&mut Waiting> {
        self.slots.get_mut(&index).filter(|w| w.write == write)
    }

    fn done(&mut self, index: u32, write: u64) {
        if self.same(index, write).is_some() {
            self.slots.remove(&index);
        }
    }

    fn later(&mut self, index: u32, write: u64, frame: u64, most: u64) {
        if let Some(w) = self.same(index, write) {
            w.due = frame + (1u64 << w.looks.min(16)).min(most.max(1));
            w.looks = w.looks.saturating_add(1);
        }
    }

    fn partly(&mut self, index: u32, write: u64, due: u64, mapped: u64) {
        if let Some(w) = self.same(index, write) {
            (w.due, w.looks, w.mapped) = (due, 0, mapped.max(1));
        }
    }

    fn changed(&mut self, index: u32, frame: u64, due: u64) {
        self.writes += 1;
        let write = self.writes;
        let recovered = self.slots.get(&index).is_some_and(|w| w.recovered);
        let due = self.slots.get(&index).map_or(due, |w| w.due.min(due));
        self.rechecks.remove(&index);
        self.slots.insert(
            index,
            Waiting {
                due,
                written: frame,
                write,
                looks: 0,
                mapped: 0,
                recovered,
            },
        );
    }

    fn mapped(&self, index: u32) -> u64 {
        self.slots.get(&index).map_or(0, |w| w.mapped)
    }

    fn census(&self, frame: u64) -> [u64; 5] {
        let mut c = [self.slots.len() as u64, 0, 0, 0, 0];
        for w in self.slots.values() {
            if w.mapped > 0 {
                c[3] += 1;
            } else if w.looks > 0 {
                c[2] += 1;
            }
            if w.due <= frame {
                c[1] += 1;
                c[4] = c[4].max(frame.saturating_sub(w.written));
            }
        }
        c
    }
}

pub(crate) static REFUSED_VIEWS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) static PLACED_TEXTURE_SLOTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static OBJECT_TEXTURE_SLOTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

static GPU_RANGES: Mutex<Vec<(u64, u64, u64, bool)>> = Mutex::new(Vec::new());

const PORTABLE_VIEWS: u32 = 1_000_000;

const UNCAPPED_VIEWS: u32 = 1 << 21;

pub(crate) const DYNAMIC: u32 = 1 << 18;
const NULL_BLOCK: u32 = 1 << 16;

impl HostHeap {
    pub(crate) fn new(sampler: bool, count: u32) -> Option<Self> {
        let host = Host::instance()?;
        let kind = if sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        let make = |count: u32| -> windows::core::Result<ID3D12DescriptorHeap> {
            unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            }
        };
        let extra = if sampler { 0 } else { DYNAMIC };
        if !sampler {
            HeapDiagnostics::probe_view_limit();
        }
        let null = if sampler {
            0
        } else {
            leonida_shader::emit::view_heap_limit()
        };
        let (heap, count, size) = if null == 0 {
            match make(count + extra) {
                Ok(heap) => (heap, count, count + extra),
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        } else {
            let size = null + 1;
            let fitted = if count <= null - extra {
                count
            } else {
                tracing::warn!(
                    count,
                    "host view heap capped at {size}; views past it read a null slot"
                );
                null - extra
            };
            match make(size) {
                Ok(heap) => (heap, fitted, size),
                Err(e) => {
                    tracing::warn!(error = %e, count, size, "no host view heap");
                    return None;
                }
            }
        };
        let (cpu, step) = unsafe {
            (
                heap.GetCPUDescriptorHandleForHeapStart().ptr,
                host.device.GetDescriptorHandleIncrementSize(kind),
            )
        };
        tracing::debug!(count, sampler, "host descriptor heap");
        let dynamic = if null == 0 { extra } else { null - count };
        let taken = vec![false; extra as usize];
        let gpu = unsafe { heap.GetGPUDescriptorHandleForHeapStart().ptr };
        GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner()).push((
            gpu,
            u64::from(size) * u64::from(step),
            u64::from(step),
            sampler,
        ));
        let strides = (!sampler)
            .then(|| {
                let bytes = u64::from(leonida_shader::emit::SLOT_STRIDES_COUNT) * 4;
                let (buffer, data) = host.upload_buffer(bytes)?;
                let gpu = unsafe {
                    std::ptr::write_bytes(data, 0, bytes as usize);
                    buffer.GetGPUVirtualAddress()
                };
                crate::host::HostSetup::name(&buffer, || {
                    "element strides by view heap slot".to_owned()
                });
                Some((buffer, gpu, data as usize))
            })
            .flatten();
        let mut h = HostHeap {
            heap,
            null_views: None,
            texture_pressure: std::sync::atomic::AtomicBool::new(false),
            cpu,
            step,
            count,
            dynamic,
            sampler,
            made: Mutex::new(vec![0; count as usize]),
            kept: Mutex::new(vec![None; (count + extra) as usize]),
            next: Mutex::new((0, taken)),
            free: std::sync::atomic::AtomicU32::new(extra),
            too_long: std::sync::atomic::AtomicU32::new(u32::MAX),
            strides,
            addresses: std::sync::OnceLock::new(),
            raw_pages: Mutex::new(None),
            buffers: Mutex::default(),
            changed: Mutex::default(),
            buffer_pass: Mutex::default(),
            textures: Mutex::default(),
            texture_sweep: Mutex::default(),
            material_demand: Default::default(),
            material_epochs: Default::default(),
            material_clock: Default::default(),
            tabled: (0..if sampler { 0 } else { count })
                .map(|_| std::sync::atomic::AtomicU32::new(0))
                .collect(),
            placed: Mutex::default(),
        };
        let started = std::time::Instant::now();
        if let Err(e) = h.fill_null(kind, size) {
            tracing::warn!(error = %e, "host descriptor heap not made null");
        }
        tracing::debug!(
            ms = started.elapsed().as_millis(),
            "host descriptor heap made null"
        );
        Some(h)
    }

    fn fill_null(
        &mut self,
        kind: D3D12_DESCRIPTOR_HEAP_TYPE,
        total: u32,
    ) -> windows::core::Result<()> {
        let Some(host) = Host::instance() else {
            return Ok(());
        };
        unsafe {
            let block: ID3D12DescriptorHeap =
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: NULL_BLOCK,
                        ..Default::default()
                    })?;
            let start = block.GetCPUDescriptorHandleForHeapStart();
            let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + (i * self.step) as usize,
            };
            if self.sampler {
                ViewMaker::null_sampler(start);
            } else {
                ViewMaker::null_view(start);
            }
            let mut filled = 1;
            while filled < NULL_BLOCK {
                let n = filled.min(NULL_BLOCK - filled);
                host.device
                    .CopyDescriptorsSimple(n, at(filled), start, kind);
                filled += n;
            }
            self.copy_nulls(host, start, kind, 0, total);
            if !self.sampler {
                self.null_views = Some(block);
            }
        }
        Ok(())
    }

    fn copy_nulls(
        &self,
        host: &crate::host::Host,
        source: D3D12_CPU_DESCRIPTOR_HANDLE,
        kind: D3D12_DESCRIPTOR_HEAP_TYPE,
        first: u32,
        count: u32,
    ) {
        let mut done = 0;
        while done < count {
            let n = NULL_BLOCK.min(count - done);
            unsafe {
                host.device
                    .CopyDescriptorsSimple(n, self.cpu(first + done), source, kind)
            };
            done += n;
        }
    }

    fn null_sampler(&self, index: u32) {
        ViewMaker::null_sampler(self.cpu(index));
    }
}

impl HostHeap {
    pub(crate) fn dynamic(&self, n: u32) -> Option<u32> {
        if self.sampler || n == 0 || n > self.dynamic {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::C::add(crate::copystat::C::DynamicAsked, 1);
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let (next, taken) = &mut *guard;
        let enough = self.free.load(std::sync::atomic::Ordering::Relaxed) >= n
            && n < self.too_long.load(std::sync::atomic::Ordering::Relaxed);
        let n = n as usize;
        let cursor = *next as usize;
        let end = taken.len();
        let second_end = cursor.saturating_add(n.saturating_sub(1)).min(end);
        let starts = (cursor..end).chain(0..second_end);
        let mut run = 0;
        let mut found = None;
        let mut last = usize::MAX;
        for at in starts.take(if enough { usize::MAX } else { 0 }) {
            if at != last.wrapping_add(1) {
                run = 0;
            }
            last = at;
            run = if taken[at] { 0 } else { run + 1 };
            if run == n {
                found = Some(at + 1 - n);
                break;
            }
        }
        if found.is_none() && end < self.dynamic as usize {
            let tail = taken
                .iter()
                .rev()
                .take(n - 1)
                .take_while(|slot| !**slot)
                .count();
            let enlarged = (end + n - tail).max(end * 2).min(self.dynamic as usize);
            taken.resize(enlarged, false);
            self.kept
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .resize(self.count as usize + enlarged, None);
            self.free.fetch_add(
                (enlarged - end) as u32,
                std::sync::atomic::Ordering::Relaxed,
            );
            self.too_long
                .store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
            if enlarged - end + tail >= n {
                found = Some(end - tail);
            }
        }
        let Some(first) = found else {
            if enough {
                self.too_long
                    .fetch_min(n as u32, std::sync::atomic::Ordering::Relaxed);
            }
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(
                    n,
                    capacity = self.dynamic,
                    free = self.free.load(std::sync::atomic::Ordering::Relaxed),
                    "no free dynamic descriptor slots: a table is not made"
                )
            });
            #[cfg(feature = "diagnostics")]
            {
                use crate::copystat::C;
                C::add(C::DynamicRefused, 1);
                C::add(C::DynamicNs, started.elapsed().as_nanos() as u64);
            }
            return None;
        };
        #[cfg(feature = "diagnostics")]
        crate::copystat::C::add(
            crate::copystat::C::DynamicNs,
            started.elapsed().as_nanos() as u64,
        );
        taken[first..first + n].fill(true);
        self.free
            .fetch_sub(n as u32, std::sync::atomic::Ordering::Relaxed);
        *next = ((first + n) % taken.len()) as u32;
        Some(self.count + first as u32)
    }

    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    pub(crate) fn strides_gpu(&self) -> Option<u64> {
        self.strides.as_ref().map(|s| s.1)
    }

    pub(crate) fn strides_buffer(&self) -> Option<&ID3D12Resource> {
        self.strides.as_ref().map(|s| &s.0)
    }

    pub(crate) fn address_buffer(&self, heap: &super::DescriptorHeap) -> Option<&ID3D12Resource> {
        if let Some((buffer, _)) = self.addresses.get() {
            return Some(buffer);
        }
        let _made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if self.addresses.get().is_none() {
            let bytes = (u64::from(self.count) + 2) * 8;
            let (buffer, data) = Host::instance()?.upload_buffer(bytes)?;
            unsafe {
                std::ptr::write_bytes(data, 0, bytes as usize);
                data.cast::<u64>().write(u64::from(self.count));
            }
            let slots = self.buffers.lock().unwrap_or_else(|e| e.into_inner());
            for &slot in slots.indices.iter().chain(slots.acceleration.keys()) {
                let address = heap
                    .handle(slot)
                    .and_then(super::Descriptors::view)
                    .as_ref()
                    .and_then(BufferViews::guest_srv_address)
                    .unwrap_or(0);
                unsafe { data.cast::<u64>().add(slot as usize + 2).write(address) };
            }
            let _ = self.addresses.set((buffer, data as usize));
        }
        self.addresses.get().map(|v| &v.0)
    }

    pub(crate) fn acceleration_addresses(&self) -> Arc<[u64]> {
        let mut views = self.buffers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(locations) = &views.acceleration_locations {
            return locations.clone();
        }
        let mut locations: Vec<u64> = views.acceleration.values().copied().collect();
        locations.sort_unstable();
        locations.dedup();
        let locations: Arc<[u64]> = locations.into();
        views.acceleration_locations = Some(locations.clone());
        locations
    }

    pub(crate) fn texture_slots_due(
        &self,
        frame: u64,
        max: usize,
    ) -> (Vec<(u32, u64, bool)>, [u64; 5]) {
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        (
            if max == 0 {
                Vec::new()
            } else {
                textures.due(frame, max)
            },
            textures.census(frame),
        )
    }

    pub(crate) fn material_write_stamp(&self) -> crate::material_demand::Mark {
        self.material_epochs
            .get_or_init(|| (0..self.count).map(|_| Default::default()).collect());
        crate::material_demand::Mark {
            write: self
                .textures
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .writes,
            descriptor: self
                .material_clock
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    pub(crate) fn demand_material_texture(
        &self,
        heap: &super::DescriptorHeap,
        index: u32,
        stamp: crate::material_demand::Mark,
        frame: u64,
    ) -> Option<(u32, u64, bool)> {
        let handle = heap.handle(index)?;
        let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        let key = *made.get(index as usize)?;
        if self
            .material_epochs
            .get()?
            .get(index as usize)?
            .load(std::sync::atomic::Ordering::Relaxed)
            > stamp.descriptor
        {
            return None;
        }
        let View::Placed(false, resource, desc) = super::Descriptors::checked_view(handle)? else {
            return None;
        };
        if resource[0] == 1 {
            return None;
        }
        let start = u64::from(desc[10]) | u64::from(desc[11]) << 32;
        let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
        let len = placed.bytes(&resource, crate::resource::PlacedTextures::first_mip(&desc))?;
        let end = start.checked_add(len)?;
        if len == 0 || start == 0 {
            return None;
        }
        let claim = placed.claim(index, start, end, &resource).0;
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(wait) = textures.slots.get(&index) {
            return (claim == Claim::Own && wait.due <= frame && wait.write <= stamp.write)
                .then_some((index, wait.write, wait.recovered));
        }
        let typed_bc = matches!(
            resource[8],
            71 | 72 | 74 | 75 | 77 | 78 | 80 | 81 | 83 | 84 | 95 | 96 | 98 | 99
        );
        if key != 0
            || !matches!(claim, Claim::Own | Claim::Free)
            || resource[0] != 3
            || resource[7] as u16 != 1
            || !typed_bc
            || resource[9] != 1
            || resource[12] & 15 != 0
            || desc[0] != resource[8]
            || desc[1] != 4
            || desc[4] >= resource[7] >> 16
            || textures.rechecks.contains_key(&index)
            || crate::texture::TitleMemoryMap::mapped_bytes(
                start as *const u8,
                usize::try_from(len).ok()?,
            ) != len
            || crate::resource::PlacedTextures::writable_texture_overlaps(start, end)
            || crate::texture::HostWrites::overlaps(start, end)
        {
            return None;
        }
        placed.note(index, start, end, &resource, false);
        textures.written(index, frame, frame, false);
        textures.slots.get_mut(&index)?.recovered = true;
        static REVIVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = REVIVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n <= 16 || n.is_power_of_two() {
            tracing::info!(
                frame,
                slot = index,
                first_mip = desc[4],
                bytes = len,
                revived = n,
                format = desc[0],
                "queued a mapped null material texture referenced by the GPU"
            );
        }
        Some((index, textures.slots[&index].write, true))
    }

    pub(crate) fn material_upload_allowed(
        &self,
        index: u32,
        write: u64,
        start: u64,
        len: u64,
        mapped: u64,
    ) -> bool {
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        let Some(waiting) = textures.slots.get(&index).filter(|w| w.write == write) else {
            return false;
        };
        if !waiting.recovered {
            return false;
        }
        drop(textures);
        start.checked_add(len).is_some_and(|end| {
            mapped == len
                && !crate::resource::PlacedTextures::writable_texture_overlaps(start, end)
                && !crate::texture::HostWrites::overlaps(start, end)
        })
    }

    pub(crate) fn texture_slots_all_due(&self, frame: u64) -> Vec<(u32, bool)> {
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures
            .slots
            .iter()
            .filter(|(_, w)| w.due <= frame)
            .map(|(&index, w)| {
                (
                    index,
                    w.mapped > 0 || textures.rechecks.contains_key(&index),
                )
            })
            .collect()
    }

    pub(crate) fn texture_slot_done(&self, index: u32, write: u64, again: Option<(u64, u64)>) {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        match again {
            Some((frame, mapped)) => textures.partly(index, write, frame, mapped),
            None => textures.done(index, write),
        }
    }

    pub(crate) fn texture_slot_recheck(&self, index: u32, frame: u64, at: (u64, u64), print: u64) {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures.recheck(index, frame, at, print);
    }

    pub(crate) fn texture_slots_rechecked(&self, frame: u64) -> [u64; 3] {
        const MOST: usize = 256;
        const SPAN: std::time::Duration = std::time::Duration::from_millis(15);
        let (due, waiting) = {
            let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
            (
                textures.rechecks_due(frame, MOST),
                textures.rechecks.len() as u64,
            )
        };
        let began = std::time::Instant::now();
        let mut found = [0, 0, waiting];
        for (index, seen) in due {
            if began.elapsed() > SPAN {
                break;
            }
            let changed =
                crate::texture::TitleMemoryMap::fingerprint(seen.at.0, seen.at.1) != seen.print;
            found[usize::from(changed)] += 1;
            if changed {
                static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let n = SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 40 || n.is_multiple_of(1000) {
                    tracing::info!(
                        slot = index,
                        address = format_args!("{:#x}", seen.at.0),
                        bytes = seen.at.1,
                        frames_after_upload = frame.saturating_sub(seen.uploaded),
                        looks = seen.looks,
                        changed = n + 1,
                        "a texture picked by index changed after its upload with nothing to tell of it: uploaded again"
                    );
                }
            }
            let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
            textures.rechecked(index, &seen, changed, frame);
        }
        found
    }

    pub(crate) fn texture_slot_later(&self, index: u32, write: u64, frame: u64, most: u64) {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures.later(index, write, frame, most);
    }

    pub(crate) fn texture_slot_mapped(&self, index: u32) -> u64 {
        self.textures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mapped(index)
    }

    pub(crate) fn texture_slots_remapped(&self, frame: u64) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let mut ranges = std::mem::take(&mut *REMAPPED.lock().unwrap_or_else(|e| e.into_inner()));
        if ranges.is_empty() {
            return 0;
        }
        RemapLog::merge(&mut ranges);
        let mut slots: Vec<u32> = {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            ranges
                .iter()
                .flat_map(|&(start, end)| placed.slots_in(start, end))
                .collect()
        };
        slots.sort_unstable();
        slots.dedup();
        let lately: Vec<bool> = slots
            .iter()
            .map(|&slot| self.tabled_lately(slot, frame))
            .collect();
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        for (&slot, lately) in slots.iter().zip(lately) {
            textures.changed(slot, frame, frame + if lately { TABLED_GRACE } else { 1 });
        }
        STALE[3].fetch_add(slots.len() as u64, Relaxed);
        slots.len()
    }

    pub(crate) fn texture_sweep(&self, heap: &super::DescriptorHeap, frame: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        const SLOTS: u32 = 1 << 15;
        const EVERY_MS: u64 = 100;
        static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
        let mode = *MODE.get_or_init(|| match std::env::var("LEONIDA_TEXTURE_SWEEP").as_deref() {
            Ok("0") => 0,
            Ok("count") => 1,
            _ => 2,
        });
        if mode == 0 || self.sampler {
            return;
        }
        let now = crate::host::HostClock::now_ms();
        let Ok(mut sweep) = self.texture_sweep.try_lock() else {
            return;
        };
        if now.saturating_sub(sweep.last_ms) < EVERY_MS {
            return;
        }
        sweep.last_ms = now;
        if mode == 2 {
            self.recover_displaced(|index| heap.handle(index), frame);
        }
        let from = sweep.next;
        let to = from.saturating_add(SLOTS).min(self.count);
        let chunk = (from / SLOTS) as usize;
        let resume = sweep.resume.get(chunk).copied().unwrap_or(from);
        let next = self.sweep_part(
            from..to,
            resume,
            |index| heap.handle(index),
            frame,
            mode == 2,
            &mut sweep.counts,
        );
        if chunk == sweep.resume.len() {
            sweep.resume.push(next);
        } else {
            sweep.resume[chunk] = next;
        }
        sweep.next = to;
        if to >= self.count {
            for (to, counted) in SWEPT.iter().zip(sweep.counts) {
                to.store(counted, Relaxed);
            }
            sweep.next = 0;
            sweep.counts = [0; 10];
        }
    }

    fn recover_displaced(&self, handle: impl Fn(u32) -> Option<u64>, frame: u64) -> u64 {
        let batch = self
            .placed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .displaced_batch(32);
        let mut restored = 0;
        for (index, key) in batch {
            let extent = self.mapped_displaced_view(index, key);
            let handle = handle(index)
                .filter(|&at| crate::texture::TitleMemoryMap::committed(at as *const u8, 12));
            let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            if placed.displaced.get(&index) != Some(&key) {
                continue;
            }
            let words = handle.map(|at| unsafe { (at as *const [u32; 3]).read_unaligned() });
            if made[index as usize] != 0
                || words != Some([VIEW_TAG, key as u32, (key >> 32) as u32])
            {
                placed.displaced.remove(&index);
                continue;
            }
            let Some((resource, start, end)) = extent else {
                continue;
            };
            if self.tabled_lately(index, frame) {
                continue;
            }
            if !matches!(
                placed.claim(index, start, end, &resource).0,
                Claim::Free | Claim::Own
            ) {
                continue;
            }
            placed.note(index, start, end, &resource, false);
            let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
            if textures.slots.contains_key(&index) {
                continue;
            }
            textures.written(index, frame, frame + 1, false);
            restored += 1;
        }
        if restored != 0 {
            static TOTAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let before = TOTAL.fetch_add(restored, std::sync::atomic::Ordering::Relaxed);
            let total = before + restored;
            if before == 0 || total / 256 != before / 256 {
                tracing::info!(
                    frame,
                    restored,
                    total,
                    "requeued displaced texture views after their aliases expired"
                );
            }
        }
        restored
    }

    fn mapped_displaced_view(&self, index: u32, key: u64) -> Option<([u32; 14], u64, u64)> {
        let View::Placed(false, resource, desc) =
            Descriptors::view_at(key as u32, (key >> 32) as u32)?
        else {
            return None;
        };
        let start = u64::from(desc[10]) | u64::from(desc[11]) << 32;
        let end = {
            let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            let end = start.checked_add(
                placed.bytes(&resource, crate::resource::PlacedTextures::first_mip(&desc))?,
            )?;
            if !matches!(
                placed.claim(index, start, end, &resource).0,
                Claim::Free | Claim::Own
            ) {
                return None;
            }
            end
        };
        crate::texture::TitleMemoryMap::any_mapped(start as usize, (end - start) as usize)
            .then_some((resource, start, end))
    }

    fn sweep_part(
        &self,
        slots: std::ops::Range<u32>,
        resume: u32,
        handle: impl Fn(u32) -> Option<u64>,
        frame: u64,
        put_back: bool,
        counts: &mut [u64; 10],
    ) -> u32 {
        use std::sync::atomic::Ordering::Relaxed;
        const MOST: u32 = 128;
        const SHORT: usize = 4_000;
        let mut back = 0;
        let mut unmade = Vec::new();
        {
            let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            let mut page = (u64::MAX, false);
            for index in slots {
                let Some(handle) = handle(index) else {
                    break;
                };
                if page.0 != handle >> 12 {
                    page = (
                        handle >> 12,
                        crate::texture::TitleMemoryMap::committed(handle as *const u8, 12),
                    );
                }
                if !page.1 {
                    continue;
                }
                let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
                if tag != VIEW_TAG || generation >> super::KIND_SHIFT == super::CONSTANT {
                    continue;
                }
                let key = u64::from(generation) << 32 | u64::from(i);
                let shown = made.get(index as usize).copied().unwrap_or(0);
                if shown == key {
                    counts[0] += 1;
                    counts[1] += 1;
                } else {
                    unmade.push((index, i, generation, shown != 0));
                }
            }
        }
        let pivot = unmade.partition_point(|&(index, ..)| index < resume);
        let mut next = resume;
        for &(index, i, generation, other) in unmade[pivot..].iter().chain(&unmade[..pivot]) {
            let Some(View::Placed(false, resource, desc)) = Descriptors::view_at(i, generation)
            else {
                continue;
            };
            if resource[0] == 1 {
                continue;
            }
            counts[0] += 1;
            let queued = {
                let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
                textures.slots.contains_key(&index)
            };
            if queued {
                counts[2] += 1;
                continue;
            }
            if self.tabled_lately(index, frame) {
                counts[3] += 1;
                continue;
            }
            let start = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let (bytes, claim, against) = {
                let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
                let Some(bytes) =
                    placed.bytes(&resource, crate::resource::PlacedTextures::first_mip(&desc))
                else {
                    continue;
                };
                let (claim, against) =
                    placed.claim(index, start, start.saturating_add(bytes), &resource);
                (bytes, claim, against)
            };
            if !crate::texture::TitleMemoryMap::any_mapped(start as usize, bytes as usize) {
                counts[4] += 1;
                continue;
            }
            counts[5 + claim as usize] += 1;
            counts[9] += u64::from(other);
            static SAID: [std::sync::atomic::AtomicU32; 4] =
                [const { std::sync::atomic::AtomicU32::new(0) }; 4];
            if SAID[claim as usize].fetch_add(1, Relaxed) < 12 {
                let (width, height, format) = (resource[4], resource[6], resource[8]);
                tracing::info!(
                    "a texture view nothing brings up to date: slot {index}, {claim:?}, {width}x{height} f{format} \
                     mips {} at {start:#x}, shows {}; against {against:x?}; its view {:x?}",
                    resource[7] >> 16,
                    if other { "another view" } else { "nothing" },
                    &desc[..10]
                );
            }
            if put_back && back < MOST && matches!(claim, Claim::Own | Claim::Free) {
                let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
                if textures.slots.len() < SHORT {
                    textures.written(index, frame, frame + 1, false);
                    back += 1;
                    next = index.saturating_add(1);
                    SWEPT[10].fetch_add(1, Relaxed);
                }
            }
        }
        next
    }

    pub(crate) fn texture_slot_gone(&self, index: u32, write: u64) -> bool {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if made.get(index as usize).is_none_or(|&key| key == 0)
            || !self.queued_write_current(index, write)
        {
            return false;
        }
        self.unmake(&mut made, index);
        self.textures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rechecks
            .remove(&index);
        STALE[4].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }

    fn unmake(&self, made: &mut [u64], index: u32) {
        let Some(key) = made.get_mut(index as usize).map(std::mem::take) else {
            return;
        };
        if key == 0 {
            return;
        }
        match Descriptors::view_at(key as u32, (key >> 32) as u32) {
            Some(view) => ViewMaker::null_for(&view, self.cpu(index)),
            None => ViewMaker::null_view(self.cpu(index)),
        }
        self.keep(index, None);
    }

    pub(crate) fn texture_slot_tabled(&self, index: u32, read: bool) {
        self.tabled_in(index, read, crate::frame::FramePacer::frame());
    }

    fn tabled_in(&self, index: u32, read: bool, frame: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(stamp) = self.tabled.get(index as usize) else {
            return;
        };
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        let old = stamp.load(Relaxed);
        if !read {
            if old & !TABLED_READ != now {
                stamp.store(now, Relaxed);
            }
            return;
        }
        if old != now | TABLED_READ {
            stamp.store(now | TABLED_READ, Relaxed);
            if RemapLog::tabled_out() {
                let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
                if textures.slots.remove(&index).is_some() {
                    TABLED_WRITES[1].fetch_add(1, Relaxed);
                }
            }
        }
    }

    fn tabled_lately(&self, index: u32, frame: u64) -> bool {
        let stamp = self.tabled_stamp(index) & !TABLED_READ;
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        RemapLog::tabled_out() && stamp != 0 && u64::from(now.wrapping_sub(stamp)) <= TABLED_FRAMES
    }

    fn tabled_stamp(&self, index: u32) -> u32 {
        self.tabled
            .get(index as usize)
            .map_or(0, |s| s.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn queue_texture(&self, index: u32, texture: bool, seen: bool, frame: u64) {
        let stamp = if RemapLog::tabled_out() {
            self.tabled_stamp(index)
        } else {
            0
        };
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        let lately =
            stamp != 0 && u64::from(now.wrapping_sub(stamp & !TABLED_READ)) <= TABLED_FRAMES;
        let read_now = seen && stamp == now | TABLED_READ;
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        if texture && !read_now {
            let due = if lately && !seen {
                TABLED_WRITES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                frame + TABLED_GRACE
            } else {
                0
            };
            textures.written(index, frame, due, seen);
        } else {
            textures.remove(index);
        }
    }

    pub(crate) fn buffer_slots(&self) -> Vec<u32> {
        let buffers = self.buffers.lock().unwrap_or_else(|e| e.into_inner());
        buffers.indices.iter().copied().collect()
    }

    pub(crate) fn buffer_changes(&self) -> Vec<u32> {
        let mut changed = self.changed.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *changed).into_iter().collect()
    }

    fn note_buffer(&self, index: u32, view: Option<&View>, seen: bool) -> Vec<u32> {
        if self.sampler {
            return Vec::new();
        }
        let texture = match view {
            Some(View::Placed(false, resource, _)) if resource[0] != 1 => {
                PLACED_TEXTURE_SLOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            Some(View::Shader(r, _))
                if crate::resource::GpuResource::buffer_bytes(*r as crate::object::This)
                    .is_none() =>
            {
                OBJECT_TEXTURE_SLOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            _ => false,
        };
        self.queue_texture(index, texture, seen, crate::frame::FramePacer::frame());
        let placed_view = match view {
            Some(View::Placed(false, resource, desc))
                if resource[0] != 1
                    && !self.tabled_lately(index, crate::frame::FramePacer::frame()) =>
            {
                let first = crate::resource::PlacedTextures::first_mip(desc);
                Some((
                    resource,
                    u64::from(desc[10]) | u64::from(desc[11]) << 32,
                    first,
                ))
            }
            _ => None,
        };
        let stale = {
            use std::sync::atomic::Ordering::Relaxed;
            let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            let span = placed_view.and_then(|(resource, start, first)| {
                Some((
                    resource,
                    start,
                    start.saturating_add(placed.bytes(resource, first)?),
                ))
            });
            match span {
                Some((resource, start, end)) => {
                    SPANNED[0].fetch_min(start, Relaxed);
                    SPANNED[1].fetch_max(end, Relaxed);
                    placed.note(index, start, end, resource, !seen && RemapLog::stale_out())
                }
                None => {
                    placed.forget(index);
                    Vec::new()
                }
            }
        };
        if !stale.is_empty() {
            STALE[0].fetch_add(stale.len() as u64, std::sync::atomic::Ordering::Relaxed);
            let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
            for &slot in &stale {
                textures.remove(slot);
            }
        }
        let stride = view.and_then(BufferViews::view_stride);
        if let Some((_, mapping)) = self.addresses.get() {
            let address = view.and_then(BufferViews::guest_srv_address).unwrap_or(0);
            unsafe {
                (*mapping as *mut u64)
                    .add(index as usize + 2)
                    .write_volatile(address)
            };
        }
        if let Some((_, _, data)) = &self.strides
            && index < leonida_shader::emit::SLOT_STRIDES_COUNT
        {
            unsafe {
                (*data as *mut u32)
                    .add(index as usize)
                    .write_volatile(stride.unwrap_or(0))
            };
        }
        let mut buffers = self.buffers.lock().unwrap_or_else(|e| e.into_inner());
        let location = view.and_then(BufferViews::acceleration_location);
        let previous = if let Some(location) = location {
            buffers.acceleration.insert(index, location)
        } else {
            buffers.acceleration.remove(&index)
        };
        if previous != location {
            buffers.acceleration_locations = None;
        }
        let was = if stride.is_some() {
            buffers.indices.insert(index);
            true
        } else {
            buffers.indices.remove(&index)
        };
        if was {
            self.changed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(index);
        }
        stale
    }

    pub(crate) fn set_texture_pressure(&self, full: bool) {
        self.texture_pressure
            .store(full, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn release(&self, first: u32, n: u32, discarded: bool) {
        let Some(at) = first.checked_sub(self.count) else {
            return;
        };
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let end = (at as usize + n as usize).min(guard.1.len());
        if let Some(slots) = guard.1.get_mut(at as usize..end) {
            let given = slots.iter().filter(|taken| **taken).count() as u32;
            if discarded
                && given != 0
                && self
                    .texture_pressure
                    .load(std::sync::atomic::Ordering::Relaxed)
            {
                let count = slots.len() as u32;
                if let (Some(block), Some(host)) = (&self.null_views, Host::instance()) {
                    let source = unsafe { block.GetCPUDescriptorHandleForHeapStart() };
                    self.copy_nulls(
                        host,
                        source,
                        D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        first,
                        count,
                    );
                } else {
                    for index in first..first + count {
                        ViewMaker::null_view(self.cpu(index));
                    }
                }
                let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
                let resources: Vec<_> = kept[first as usize..first as usize + count as usize]
                    .iter_mut()
                    .filter_map(Option::take)
                    .collect();
                drop(kept);
                if !resources.is_empty() {
                    crate::host::Retirement::retire(resources);
                }
            }
            self.free
                .fetch_add(given, std::sync::atomic::Ordering::Relaxed);
            self.too_long
                .store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
            slots.fill(false);
        }
    }

    pub(crate) fn make_at(&self, index: u32, view: &View) {
        let kept = ViewMaker::make_view(view, self.cpu(index));
        if kept.is_none() {
            ViewMaker::null_for(view, self.cpu(index));
        }
        self.keep(index, kept.flatten());
    }

    pub(crate) fn make_null(&self, index: u32) {
        if self.sampler {
            self.null_sampler(index);
        } else {
            ViewMaker::null_view(self.cpu(index));
        }
        self.keep(index, None);
    }

    fn keep(&self, index: u32, resource: Option<ID3D12Resource>) {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = kept.get_mut(index as usize)
            && let Some(old) = std::mem::replace(slot, resource)
        {
            crate::host::Retirement::retire(old);
        }
    }

    pub(crate) fn raw_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = Host::instance() else { return };
        let elements = (len / 4).clamp(1, u64::from(u32::MAX)) as u32;
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: offset / 4,
                    NumElements: elements,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        unsafe {
            host.device
                .CreateShaderResourceView(buffer, Some(&desc), self.cpu(index))
        };
        self.keep(index, Some(buffer.clone()));
    }

    pub(crate) fn null_raw_at(&self, index: u32) {
        ViewMaker::null_buffer_view(self.cpu(index));
        self.keep(index, None);
    }

    pub(crate) fn raw_page_views(
        &self,
        copies: &[Arc<crate::shadow::Shadow>],
    ) -> Option<(u32, u32)> {
        let mut run = self.raw_pages.lock().unwrap_or_else(|e| e.into_inner());
        let requested = u32::try_from(copies.len()).ok()?;
        let needed = requested.saturating_add(1);
        if run
            .as_ref()
            .is_none_or(|r| needed > r.slots && r.slots < self.dynamic)
        {
            let slots = needed
                .checked_next_power_of_two()
                .unwrap_or(u32::MAX)
                .max(FIRST_RAW_PAGE_SLOTS)
                .min(self.dynamic);
            let reservation = self.dynamic(slots).map(|base| (base, slots)).or_else(|| {
                let small = needed.min(FIRST_RAW_PAGE_SLOTS);
                if run.is_some() || small >= slots {
                    return None;
                }
                self.dynamic(small).map(|base| (base, small))
            });
            if let Some((base, slots)) = reservation {
                self.null_raw_uav_at(base);
                if let Some(old) = run.as_ref() {
                    tracing::info!(
                        previous = old.slots,
                        slots,
                        "grew raw memory descriptor run"
                    );
                }
                *run = Some(RawPageRun {
                    base,
                    slots,
                    populated: 0,
                });
            }
        }
        let run = run.as_mut()?;
        let count = requested.min(run.slots - 1);
        if count < requested {
            static WARN: std::sync::Once = std::sync::Once::new();
            WARN.call_once(|| {
                tracing::warn!(
                    requested,
                    count,
                    "raw memory bindings are limited by the available descriptor heap"
                )
            });
        }
        for (n, shadow) in copies[..count as usize]
            .iter()
            .enumerate()
            .skip(run.populated as usize)
        {
            let width = unsafe { shadow.buffer.GetDesc() }.Width;
            self.raw_uav_at(run.base + 1 + n as u32, &shadow.buffer, 0, width);
        }
        run.populated = run.populated.max(count);
        Some((run.base, count))
    }

    pub(crate) fn raw_uav_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = Host::instance() else { return };
        let elements = (len / 4).clamp(1, u64::from(u32::MAX)) as u32;
        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    FirstElement: offset / 4,
                    NumElements: elements,
                    StructureByteStride: 0,
                    CounterOffsetInBytes: 0,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                },
            },
        };
        unsafe {
            host.device.CreateUnorderedAccessView(
                buffer,
                None::<&ID3D12Resource>,
                Some(&desc),
                self.cpu(index),
            )
        };
        self.keep(index, Some(buffer.clone()));
    }

    pub(crate) fn null_raw_uav_at(&self, index: u32) {
        self.keep(index, None);
        let Some(host) = Host::instance() else { return };
        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    NumElements: 1,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    ..Default::default()
                },
            },
        };
        unsafe {
            host.device.CreateUnorderedAccessView(
                None::<&ID3D12Resource>,
                None::<&ID3D12Resource>,
                Some(&desc),
                self.cpu(index),
            )
        };
    }

    pub(crate) fn gpu_handle(&self, index: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        let start = unsafe { self.heap.GetGPUDescriptorHandleForHeapStart().ptr };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: start + u64::from(index) * u64::from(self.step),
        }
    }

    pub(crate) fn constant_at(&self, index: u32, gpu: u64, size: u32) {
        self.keep(index, None);
        let Some(host) = Host::instance() else { return };
        let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
            BufferLocation: gpu,
            SizeInBytes: BufferViews::constant_size(size),
        };
        unsafe {
            host.device
                .CreateConstantBufferView(Some(&desc), self.cpu(index))
        };
    }

    fn cpu(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + index as usize * self.step as usize,
        }
    }

    pub(crate) fn mirror(&self, index: u32, handle: u64, for_draw: bool) -> Option<View> {
        self.mirror_if(index, handle, for_draw, None, |_, _| true)
            .ok()
            .flatten()
    }

    fn queued_write_current(&self, index: u32, write: u64) -> bool {
        self.textures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slots
            .get(&index)
            .is_some_and(|slot| slot.write == write)
    }

    pub(crate) fn mirror_queued(
        &self,
        index: u32,
        handle: u64,
        write: u64,
        resident_only: bool,
    ) -> Result<(Option<View>, Option<crate::resource::ResidentTexture>), ()> {
        let mut pin = None;
        let view = self.mirror_if(index, handle, true, Some(write), |view, already_made| {
            if !resident_only {
                return true;
            }
            pin = ViewMaker::resident_sampled_texture(view);
            pin.is_some()
                || (already_made
                    && matches!(view, View::Placed(false, resource, _) if resource[12] & 7 != 0)
                    && self.kept.lock().unwrap_or_else(|e| e.into_inner())[index as usize]
                        .is_some())
        })?;
        Ok((view, pin))
    }

    fn mirror_if(
        &self,
        index: u32,
        handle: u64,
        for_draw: bool,
        queued_write: Option<u64>,
        allow: impl FnOnce(&View, bool) -> bool,
    ) -> Result<Option<View>, ()> {
        if index >= self.count {
            static HIGHEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if HIGHEST.fetch_max(index, std::sync::atomic::Ordering::Relaxed) < index {
                tracing::warn!(
                    index,
                    count = self.count,
                    for_draw,
                    "title slot past the host heap"
                );
            }
            return Ok(None);
        }
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if queued_write.is_some_and(|write| !self.queued_write_current(index, write)) {
            return Ok(None);
        }
        if !for_draw && let Some(epochs) = self.material_epochs.get() {
            let sequence = self
                .material_clock
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            epochs[index as usize].store(sequence, std::sync::atomic::Ordering::Relaxed);
        }
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        let key = u64::from(generation) << 32 | u64::from(i);
        let view = (tag == VIEW_TAG)
            .then(|| Descriptors::view_at(i, generation))
            .flatten();
        let Some(view) = view else {
            if made[index as usize] != 0 {
                made[index as usize] = 0;
                self.make_null(index);
                let _ = self.note_buffer(index, None, false);
            } else if for_draw
                || self.addresses.get().is_some()
                || (!self.sampler
                    && self
                        .placed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .by_slot
                        .contains_key(&index))
            {
                let _ = self.note_buffer(index, None, false);
            }
            return Ok(None);
        };
        if queued_write.is_some()
            && let View::Placed(false, resource, desc) = &view
        {
            let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            if placed.displaced.get(&index) == Some(&key) {
                let start = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                let span = placed
                    .bytes(resource, crate::resource::PlacedTextures::first_mip(desc))
                    .map(|bytes| (start, start.saturating_add(bytes)));
                if span.is_none() || placed.by_slot.get(&index) != span.as_ref() {
                    return Ok(None);
                }
            }
        }
        let changed_owner = made[index as usize] == key
            && for_draw
            && match &view {
                View::Placed(_, r, _) if r[0] != 1 && r[12] & 4 != 0 => {
                    !self.shows_placed_owner(index, &view)
                }
                _ => !self.buffer_owner_current(index, &view),
            };
        let current = made[index as usize] == key && !changed_owner;
        if !allow(&view, current) {
            return Err(());
        }
        if !current {
            let stale = self.note_buffer(index, Some(&view), for_draw);
            if !stale.is_empty() {
                let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
                for &slot in &stale {
                    let key = made[slot as usize];
                    if slot != index && key != 0 {
                        placed.displaced.insert(slot, key);
                    }
                }
                drop(placed);
                for slot in stale {
                    if slot != index {
                        self.unmake(&mut made, slot);
                    }
                }
            }
            let waiting = if for_draw {
                None
            } else {
                view.waiting_texture()
            };
            if let Some(texture) = waiting {
                super::ViewWaits::wait_for(texture, handle);
            }
            let later = !for_draw && (view.is_buffer() || waiting.is_some());
            if later
                && ViewMaker::keep_views()
                && Descriptors::view_at(
                    made[index as usize] as u32,
                    (made[index as usize] >> 32) as u32,
                )
                .is_some_and(|old| ViewMaker::same_placed_texture(&old, &view))
            {
                KEPT_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok(Some(view));
            }
            made[index as usize] = if later { 0 } else { key };
            let kept = if later {
                None
            } else {
                ViewMaker::make_view(&view, self.cpu(index))
            };
            if kept.is_none() && !self.sampler {
                ViewMaker::null_for(&view, self.cpu(index));
            }
            self.keep(index, kept.flatten());
        }
        Ok(Some(view))
    }

    pub(crate) fn shows(&self, index: u32, buffer: &ID3D12Resource) -> Option<bool> {
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        let shown = kept.get(index as usize)?.as_ref()?;
        Some(windows::core::Interface::as_raw(shown) == windows::core::Interface::as_raw(buffer))
    }

    pub(crate) fn buffer_owner_current(&self, index: u32, view: &View) -> bool {
        let buffer = match view {
            View::Placed(_, resource, _) => resource[0] == 1,
            View::Shader(_, Some(desc)) | View::Unordered(_, _, Some(desc)) => desc[1] == 1,
            _ => false,
        };
        if !buffer {
            return true;
        }
        let Some((address, width)) = BufferViews::buffer_extent(view) else {
            return true;
        };
        let Some(shadow) = crate::shadow::ShadowRegistry::find(address, width) else {
            return false;
        };
        if let Some(current) = self.shows(index, &shadow.buffer) {
            return current;
        }
        let words = match view {
            View::Shader(_, Some(desc)) => desc,
            View::Placed(false, _, desc) => desc[..10].try_into().unwrap(),
            _ => return true,
        };
        let mut desc = BufferViews::srv_desc(words);
        let host_width = unsafe { shadow.buffer.GetDesc() }.Width;
        BufferViews::rebase_srv(&mut desc, shadow.offset(address), host_width).is_err()
    }

    fn shows_placed_owner(&self, index: u32, view: &View) -> bool {
        let owner = PlacedTexture::of(view);
        let candidates = match owner.as_ref().map(|p| &p.view) {
            Some(View::Shader(resource, _)) => [
                crate::resource::HostTextures::host_resource(*resource as _),
                None,
                None,
            ],
            Some(View::Unordered(resource, _, _)) => {
                let resource = *resource as crate::object::This;
                [
                    crate::resource::HostTextures::host_resource(resource),
                    crate::resource::Twin::resource_of(resource),
                    crate::resource::Twin::plane_resource_of(resource),
                ]
            }
            _ => [None; 3],
        };
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        match kept.get(index as usize).and_then(Option::as_ref) {
            Some(shown) => candidates
                .into_iter()
                .flatten()
                .any(|candidate| candidate == shown),
            None => owner.is_none(),
        }
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn texture_slot_state(&self, index: u32, frame: u64, view: Option<&View>) -> String {
        let residency = if let Some(View::Placed(false, resource, desc)) = view
            && resource[0] != 1
        {
            let start = u64::from(desc[10]) | (u64::from(desc[11]) << 32);
            let first = crate::resource::PlacedTextures::first_mip(desc);
            let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            let bytes = placed.bytes(resource, first).unwrap_or(0);
            let (claim, overlap) =
                placed.claim(index, start, start.saturating_add(bytes), resource);
            let mapped =
                crate::texture::TitleMemoryMap::mapped_bytes(start as *const u8, bytes as usize);
            format!(" mip={first} visible={mapped}/{bytes} claim={claim:?} overlap={overlap:x?}")
        } else {
            String::new()
        };
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        let state = match textures.slots.get(&index) {
            None if textures.rechecks.contains_key(&index) => {
                "uploaded whole, to be looked at again".into()
            }
            None => "not queued".into(),
            Some(w) => format!(
                "queued due={} written@{} looks={} part_mapped={} recheck={}",
                if w.due <= frame {
                    "now".to_string()
                } else {
                    format!("@{}", w.due)
                },
                w.written,
                w.looks,
                w.mapped,
                textures.rechecks.contains_key(&index)
            ),
        };
        format!("{state}{residency}")
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn describe_slot(&self, index: u32) -> Option<String> {
        let key = *self
            .made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index as usize)?;
        if key == 0 {
            return Some("null".into());
        }
        let view = Descriptors::view_at(key as u32, (key >> 32) as u32)?;
        let mut text: String = format!("{view:?}").chars().take(120).collect();
        if let Some((address, len)) = BufferViews::buffer_range(&view) {
            let host =
                crate::shadow::ShadowRegistry::find(address, len.max(1)).map(|s| s.gpu(address));
            text.push_str(&format!(" title {address:#x}+{len:#x} host {host:#x?}"));
        }
        Some(text)
    }
}

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

pub(crate) struct RemapLog;

pub(crate) struct HeapDiagnostics;

pub(crate) struct BufferViews;

pub(crate) struct ViewMaker;

impl RemapLog {
    fn record(ranges: &mut Vec<(u64, u64)>, start: u64, end: u64, limit: usize) -> bool {
        if start >= end {
            return false;
        }
        if let Some(last) = ranges.last_mut()
            && last.0 <= end
            && start <= last.1
        {
            last.0 = last.0.min(start);
            last.1 = last.1.max(end);
        } else if ranges.len() >= limit {
            ranges.clear();
            ranges.push((0, u64::MAX));
            return true;
        } else {
            ranges.push((start, end));
        }
        false
    }

    fn merge(ranges: &mut Vec<(u64, u64)>) {
        ranges.sort_unstable();
        ranges.dedup_by(|next, previous| {
            if next.0 > previous.1 {
                return false;
            }
            previous.1 = previous.1.max(next.1);
            true
        });
    }

    fn stale_out() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_STALE_TEXTURES").is_none_or(|v| v != "0"))
    }

    pub(crate) fn remapped(at: usize, len: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        const MOST: usize = 1 << 18;
        let (start, end) = (at as u64, at.saturating_add(len) as u64);
        if end <= SPANNED[0].load(Relaxed) || start >= SPANNED[1].load(Relaxed) {
            return;
        }
        let mut ranges = REMAPPED.lock().unwrap_or_else(|e| e.into_inner());
        STALE[1].fetch_add(1, Relaxed);
        if RemapLog::record(&mut ranges, start, end, MOST) {
            STALE[2].fetch_add(1, Relaxed);
        }
    }

    fn tabled_out() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_TABLED_TEXTURES").is_none_or(|v| v != "1"))
    }
}

impl BufferViews {
    pub(crate) fn is_texture_view(view: &View) -> bool {
        match view {
            View::Placed(false, resource, _) => resource[0] != 1,
            View::Shader(r, _) => {
                crate::resource::GpuResource::buffer_bytes(*r as crate::object::This).is_none()
            }
            _ => false,
        }
    }

    fn view_stride(view: &View) -> Option<u32> {
        const BUFFER: u32 = 1;
        let (words, unordered): (&[u32; 10], bool) = match view {
            View::Shader(_, Some(d)) if d[1] == BUFFER => (d, false),
            View::Unordered(_, _, Some(d)) if d[1] == BUFFER => (d, true),
            View::Placed(unordered, resource, d) if resource[0] == BUFFER => {
                (d[..10].try_into().ok()?, *unordered)
            }
            _ => return None,
        };
        let (stride, raw, format) = unsafe {
            if unordered {
                let d = BufferViews::uav_desc(words);
                let b = d.Anonymous.Buffer;
                (
                    b.StructureByteStride,
                    b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0,
                    d.Format,
                )
            } else {
                let d = BufferViews::srv_desc(words);
                let b = d.Anonymous.Buffer;
                (
                    b.StructureByteStride,
                    b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0,
                    d.Format,
                )
            }
        };
        Some(if stride > 0 {
            stride
        } else if raw {
            1
        } else {
            crate::resource::ResourceFormat::element(format.0 as u32)
                .map_or(0, |(bytes, _)| bytes as u32)
        })
    }

    fn srv_desc(words: &[u32; 10]) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
        let mut words = *words;
        words[0] = crate::resource::ResourceFormat::pc_format(words[0]);
        unsafe {
            std::ptr::from_ref(&words)
                .cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()
                .read_unaligned()
        }
    }

    fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
        let mut words = *words;
        words[0] = crate::resource::ResourceFormat::pc_format(words[0]);
        unsafe {
            std::ptr::from_ref(&words)
                .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
                .read_unaligned()
        }
    }

    fn element_size(desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> u64 {
        let buffer = unsafe { desc.Anonymous.Buffer };
        if buffer.StructureByteStride > 0 {
            u64::from(buffer.StructureByteStride)
        } else if buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0 {
            4
        } else {
            crate::resource::ResourceFormat::element(desc.Format.0 as u32)
                .map_or(0, |(bytes, _)| bytes)
        }
    }

    pub(crate) fn structured_srv_records(view: &View) -> Option<u32> {
        let words = match view {
            View::Shader(resource, Some(desc)) if *resource != 0 => desc,
            View::Placed(false, resource, desc)
                if resource[0] == 1 && (desc[10] | desc[11]) != 0 =>
            {
                desc[..10].try_into().ok()?
            }
            _ => return None,
        };
        let desc = BufferViews::srv_desc(words);
        if desc.ViewDimension != D3D12_SRV_DIMENSION_BUFFER || desc.Format != DXGI_FORMAT_UNKNOWN {
            return None;
        }
        let buffer = unsafe { desc.Anonymous.Buffer };
        (buffer.StructureByteStride != 0).then_some(buffer.NumElements)
    }

    fn acceleration_location(view: &View) -> Option<u64> {
        let View::Shader(0, Some(words)) = view else {
            return None;
        };
        if words[1] != D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE.0 as u32 {
            return None;
        }
        let desc = BufferViews::srv_desc(words);
        let address = unsafe { desc.Anonymous.RaytracingAccelerationStructure.Location };
        (address != 0).then_some(address)
    }

    pub(crate) fn guest_srv_address(view: &View) -> Option<u64> {
        BufferViews::acceleration_location(view).or_else(|| {
            BufferViews::buffer_range(view)
                .and_then(|(address, bytes)| (address != 0 && bytes != 0).then_some(address))
        })
    }

    pub(crate) fn buffer_range(view: &View) -> Option<(u64, u64)> {
        let (base, words): (u64, &[u32; 10]) = match view {
            View::Shader(r, Some(desc)) => (
                crate::resource::GpuResource::buffer_bytes(*r as crate::object::This)?.0 as u64,
                desc,
            ),
            View::Placed(false, resource, desc) if resource[0] == 1 => (
                u64::from(desc[10]) | u64::from(desc[11]) << 32,
                desc[..10].try_into().ok()?,
            ),
            View::Unordered(r, _, Some(desc)) => {
                let base =
                    crate::resource::GpuResource::buffer_bytes(*r as crate::object::This)?.0 as u64;
                return BufferViews::unordered_range(base, desc);
            }
            View::Placed(true, resource, desc) if resource[0] == 1 => {
                let base = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                return BufferViews::unordered_range(base, desc[..10].try_into().ok()?);
            }
            _ => return None,
        };
        let desc = BufferViews::srv_desc(words);
        let element = BufferViews::element_size(&desc);
        let buffer = unsafe { desc.Anonymous.Buffer };
        Some((
            base.checked_add(buffer.FirstElement.checked_mul(element)?)?,
            u64::from(buffer.NumElements) * element,
        ))
    }

    pub(crate) fn buffer_extent(view: &View) -> Option<(u64, u64)> {
        match view {
            View::Shader(r, Some(_)) | View::Unordered(r, _, Some(_)) => {
                let (data, len) =
                    crate::resource::GpuResource::buffer_bytes(*r as crate::object::This)?;
                Some((data as u64, len))
            }
            View::Placed(_, resource, desc) if resource[0] == 1 => Some((
                u64::from(desc[10]) | u64::from(desc[11]) << 32,
                u64::from(resource[4]) | u64::from(resource[5]) << 32,
            )),
            _ => None,
        }
    }

    fn view_extent() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_VIEW_EXTENT").is_none_or(|v| v != "0"))
    }

    pub(crate) fn buffer_copy(
        view: &View,
    ) -> Option<(std::sync::Arc<crate::shadow::Shadow>, u64, u64)> {
        let (address, len) = BufferViews::buffer_range(view)?;
        if len == 0 {
            return None;
        }
        if BufferViews::view_extent()
            && let Some((base, width)) = BufferViews::buffer_extent(view)
            && address >= base
            && address - base < width.max(1)
            && let Some(shadow) = crate::shadow::ShadowRegistry::find(base, width)
        {
            return Some((shadow, address, len));
        }
        crate::shadow::ShadowRegistry::find(address, len).map(|shadow| (shadow, address, len))
    }

    fn unordered_range(base: u64, words: &[u32; 10]) -> Option<(u64, u64)> {
        let desc = BufferViews::uav_desc(words);
        let buffer = unsafe { desc.Anonymous.Buffer };
        let element = if buffer.StructureByteStride > 0 {
            u64::from(buffer.StructureByteStride)
        } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
            4
        } else {
            crate::resource::ResourceFormat::element(desc.Format.0 as u32)?.0
        };
        Some((
            base.checked_add(buffer.FirstElement.checked_mul(element)?)?,
            u64::from(buffer.NumElements) * element,
        ))
    }

    fn raw_window(offset: u64, first: u64, count: u32, stride: u32) -> Option<(u64, u32)> {
        if stride == 0 {
            return None;
        }
        let start = first.checked_mul(u64::from(stride))?.checked_add(offset)?;
        let bytes = u64::from(count) * u64::from(stride);
        (start % 16 == 0 && bytes % 4 == 0)
            .then(|| {
                u32::try_from(bytes / 4)
                    .ok()
                    .map(|dwords| (start / 4, dwords))
            })
            .flatten()
    }

    fn buffer_view_ok(
        format: DXGI_FORMAT,
        first: u64,
        count: u32,
        stride: u32,
        raw: bool,
        width: u64,
    ) -> bool {
        use crate::resource::ResourceFormat;
        let f = format.0 as u32;
        let element = if raw {
            if format != DXGI_FORMAT_R32_TYPELESS || stride != 0 || !first.is_multiple_of(4) {
                return false;
            }
            4
        } else if stride > 0 {
            if format != DXGI_FORMAT_UNKNOWN || stride > 2048 || !stride.is_multiple_of(4) {
                return false;
            }
            u64::from(stride)
        } else {
            let typeless = ResourceFormat::family(f) == f && ResourceFormat::family(f + 1) == f;
            match crate::resource::ResourceFormat::element(f) {
                Some((bytes, 1)) if f != 0 && !typeless && bytes > 0 => bytes,
                _ => return false,
            }
        };
        count > 0
            && first
                .checked_add(u64::from(count))
                .and_then(|end| end.checked_mul(element))
                .is_some_and(|end| end <= width)
    }

    fn constant_size(size: u32) -> u32 {
        size.min(64 << 10).next_multiple_of(256)
    }

    fn elements_inside(first: u64, count: u32, element: u64, width: u64) -> u32 {
        let room = (width / element.max(1)).saturating_sub(first);
        match u32::try_from(room) {
            Ok(0) | Err(_) => count,
            Ok(room) => count.min(room),
        }
    }

    fn left_null(
        why: &'static str,
        unordered: bool,
        address: u64,
        len: u64,
        element: u64,
        first: u64,
    ) -> bool {
        if !cfg!(feature = "diagnostics") {
            return false;
        }
        type Seen = std::collections::BTreeMap<
            (&'static str, bool, u64),
            std::collections::HashSet<(u64, u64)>,
        >;
        static SEEN: Mutex<Seen> = Mutex::new(Seen::new());
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        let views = seen.entry((why, unordered, element)).or_default();
        if views.insert((address, len)) && views.len().is_power_of_two() {
            tracing::warn!(
                views = views.len(),
                unordered,
                element,
                first,
                len,
                address = format_args!("{address:#x}"),
                "buffer views left null: {why}"
            );
            return true;
        }
        false
    }

    fn rebase_srv(
        desc: &mut D3D12_SHADER_RESOURCE_VIEW_DESC,
        offset: u64,
        width: u64,
    ) -> Result<(), SrvError> {
        let structured = unsafe { desc.Anonymous.Buffer };
        let window = (desc.Format == DXGI_FORMAT_UNKNOWN)
            .then(|| {
                BufferViews::raw_window(
                    offset,
                    structured.FirstElement,
                    structured.NumElements,
                    structured.StructureByteStride,
                )
            })
            .flatten();
        if let Some((first, count)) = window {
            desc.Format = DXGI_FORMAT_R32_TYPELESS;
            desc.Anonymous.Buffer = D3D12_BUFFER_SRV {
                FirstElement: first,
                NumElements: count,
                StructureByteStride: 0,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
            };
        }
        let element = BufferViews::element_size(desc);
        let buffer = unsafe { &mut desc.Anonymous.Buffer };
        if window.is_none() {
            if element == 0 || !offset.is_multiple_of(element) {
                return Err(SrvError::Alignment);
            }
            buffer.FirstElement = buffer
                .FirstElement
                .checked_add(offset / element)
                .ok_or(SrvError::Layout)?;
        }
        let unit = if window.is_some() { 4 } else { element };
        buffer.NumElements =
            BufferViews::elements_inside(buffer.FirstElement, buffer.NumElements, unit, width);
        let raw = buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0;
        if BufferViews::buffer_view_ok(
            desc.Format,
            buffer.FirstElement,
            buffer.NumElements,
            buffer.StructureByteStride,
            raw,
            width,
        ) {
            Ok(())
        } else {
            Err(SrvError::Layout)
        }
    }

    fn buffer_srv(
        address: u64,
        len: u64,
        mut desc: D3D12_SHADER_RESOURCE_VIEW_DESC,
        at: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Option<Kept> {
        let shadow = crate::shadow::ShadowRegistry::find(address, len)?;
        let offset = shadow.offset(address);
        let width = unsafe { shadow.buffer.GetDesc() }.Width;
        if let Err(reason) = BufferViews::rebase_srv(&mut desc, offset, width) {
            let element = BufferViews::element_size(&desc);
            let buffer = unsafe { desc.Anonymous.Buffer };
            if matches!(reason, SrvError::Alignment) {
                let sampled = BufferViews::left_null(
                    "not on an element of their host copy",
                    false,
                    address,
                    len,
                    element,
                    buffer.FirstElement,
                );
                if sampled && element == 12 {
                    let (shadow_start, shadow_bytes) = shadow.span();
                    let bytes = u64::from(buffer.NumElements) * element;
                    let view_end = buffer
                        .FirstElement
                        .checked_mul(element)
                        .and_then(|first| first.checked_add(bytes));
                    tracing::warn!(
                        format = desc.Format.0,
                        dimension = desc.ViewDimension.0,
                        stride = buffer.StructureByteStride,
                        count = buffer.NumElements,
                        flags = buffer.Flags.0,
                        offset,
                        address = format_args!("{address:#x}"),
                        shadow_start = format_args!("{shadow_start:#x}"),
                        shadow_bytes,
                        within_guest_buffer = view_end.is_some_and(|end| end <= len),
                        "12-byte SRV could not address the first whole element in its host buffer"
                    );
                }
                return None;
            }
            let (first, count, stride) = (
                buffer.FirstElement,
                buffer.NumElements,
                buffer.StructureByteStride,
            );
            let raw = buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0;
            REFUSED_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let why = if raw {
                "raw, refused by the host"
            } else {
                "refused by the host"
            };
            BufferViews::left_null(why, false, address, len, element, first);
            once!(
                format = desc.Format.0,
                first,
                count,
                stride,
                raw,
                width,
                "buffer view the host would refuse left null"
            );
            return None;
        }
        if desc.Shader4ComponentMapping != D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING {
            desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        }
        let host = Host::instance()?;
        unsafe {
            host.device
                .CreateShaderResourceView(&shadow.buffer, Some(&desc), at)
        };
        Some(Some(shadow.buffer.clone()))
    }

    fn buffer_uav(
        address: u64,
        len: u64,
        mut desc: D3D12_UNORDERED_ACCESS_VIEW_DESC,
        at: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Option<Kept> {
        let shadow = crate::shadow::ShadowRegistry::find(address, len)?;
        let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
        if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            once!("unordered access to a buffer whose host copy shaders cannot write");
            return None;
        }
        let buffer = unsafe { &mut desc.Anonymous.Buffer };
        let element = if buffer.StructureByteStride > 0 {
            u64::from(buffer.StructureByteStride)
        } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
            4
        } else {
            crate::resource::ResourceFormat::element(desc.Format.0 as u32)
                .map_or(0, |(bytes, _)| bytes)
        };
        let offset = shadow.offset(address);
        let window = (desc.Format == DXGI_FORMAT_UNKNOWN)
            .then(|| {
                BufferViews::raw_window(
                    offset,
                    buffer.FirstElement,
                    buffer.NumElements,
                    buffer.StructureByteStride,
                )
            })
            .flatten();
        if let Some((first, count)) = window {
            buffer.FirstElement = first;
            buffer.NumElements = count;
            buffer.StructureByteStride = 0;
            buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
        } else {
            if element == 0 || !offset.is_multiple_of(element) {
                BufferViews::left_null(
                    "not on an element of their host copy",
                    true,
                    address,
                    len,
                    element,
                    buffer.FirstElement,
                );
                return None;
            }
            buffer.FirstElement += offset / element;
        }
        buffer.CounterOffsetInBytes = 0;
        let width = unsafe { shadow.buffer.GetDesc() }.Width;
        let unit = if window.is_some() { 4 } else { element };
        buffer.NumElements =
            BufferViews::elements_inside(buffer.FirstElement, buffer.NumElements, unit, width);
        let (first, count, stride) = (
            buffer.FirstElement,
            buffer.NumElements,
            buffer.StructureByteStride,
        );
        let raw = buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0;
        if window.is_some() {
            desc.Format = DXGI_FORMAT_R32_TYPELESS;
        }
        if !BufferViews::buffer_view_ok(desc.Format, first, count, stride, raw, width) {
            REFUSED_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let why = if raw {
                "raw, refused by the host"
            } else {
                "refused by the host"
            };
            BufferViews::left_null(why, true, address, len, element, first);
            once!(
                format = desc.Format.0,
                first,
                count,
                stride,
                raw,
                width,
                "unordered buffer view the host would refuse left null"
            );
            return None;
        }
        let host = Host::instance()?;
        unsafe {
            host.device.CreateUnorderedAccessView(
                &shadow.buffer,
                None::<&ID3D12Resource>,
                Some(&desc),
                at,
            )
        };
        Some(Some(shadow.buffer.clone()))
    }
}

impl HeapDiagnostics {
    pub(crate) fn device_removed() -> Option<String> {
        let host = Host::instance()?;
        let reason = unsafe { host.device.GetDeviceRemovedReason() };
        reason.err().map(|e| e.to_string())
    }

    pub(crate) fn near_heaps(va: u64) -> String {
        let ranges = GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner());
        let mut text = String::new();
        for &(start, bytes, step, sampler) in ranges.iter() {
            let kind = if sampler { "sampler" } else { "view" };
            let place = if va < start {
                format!("{:#x} below", start - va)
            } else if va < start + bytes {
                format!("inside, slot {}", (va - start) / step)
            } else {
                format!(
                    "{:#x} past its end (slot {})",
                    va - start - bytes,
                    (va - start) / step
                )
            };
            text.push_str(&format!("; {kind} heap {start:#x}+{bytes:#x}: {place}"));
        }
        text
    }

    pub(crate) fn probe_view_limit() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let Some(host) = Host::instance() else { return };
            let fits = |count: u32| {
                let made: windows::core::Result<ID3D12DescriptorHeap> = unsafe {
                    host.device
                        .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                            NumDescriptors: count,
                            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                            NodeMask: 0,
                        })
                };
                made.is_ok()
            };
            let views = if fits(UNCAPPED_VIEWS) {
                UNCAPPED_VIEWS
            } else {
                PORTABLE_VIEWS
            };
            tracing::info!(
                views,
                "host view heaps hold {views} views; view indices are clamped to the null slot at the end from the first shader on"
            );
            leonida_shader::emit::set_view_heap_limit(views - 1);
        });
    }
}

impl ViewMaker {
    fn null_sampler(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
        let Some(host) = Host::instance() else { return };
        let desc = D3D12_SAMPLER_DESC {
            Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
            ..Default::default()
        };
        unsafe { host.device.CreateSampler(&desc, at) };
    }

    fn fits(
        texture: &ID3D12Resource,
        mut format: Option<&mut DXGI_FORMAT>,
        unordered: bool,
    ) -> bool {
        use crate::resource::{DEPTH_FORMATS, ResourceFormat};
        let d = unsafe { texture.GetDesc() };
        let allowed = if unordered {
            d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
        } else {
            d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 == 0
        };
        let own = d.Format.0 as u32;
        if !allowed || DEPTH_FORMATS.iter().any(|&(typed, _)| typed == own) {
            if unordered {
                crate::resource::DEPTH_UNORDERED[3]
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            once!(
                unordered,
                format = own,
                flags = d.Flags.0,
                "view the host does not allow"
            );
            return false;
        }
        let typeless = ResourceFormat::family(own) == own && ResourceFormat::family(own + 1) == own;
        if let Some(format) = format.as_deref_mut()
            && unordered
            && let Some(&(_, linear)) = crate::resource::SRGB
                .iter()
                .find(|&&(s, _)| s == format.0 as u32)
        {
            format.0 = linear as i32;
        }
        let view = format.as_ref().map_or(0, |f| f.0 as u32);
        if view == 0 {
            return !typeless;
        }
        if view != own && crate::resource::ResourceFormat::target_format(view) == own {
            if let Some(format) = format {
                *format = d.Format;
            }
            return true;
        }
        if ResourceFormat::family(view) != ResourceFormat::family(own) {
            once!(view, own, "view format of another family");
            return false;
        }
        if let Some(format) = format
            && !typeless
        {
            *format = d.Format;
        }
        true
    }

    fn twin_uav(
        twin: &ID3D12Resource,
        desc: Option<&D3D12_UNORDERED_ACCESS_VIEW_DESC>,
    ) -> Option<D3D12_UNORDERED_ACCESS_VIEW_DESC> {
        let d = unsafe { twin.GetDesc() };
        let mut desc = desc.cloned().unwrap_or_else(|| {
            let mut all = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            if d.DepthOrArraySize > 1 {
                all.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
                all.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                    ArraySize: u32::from(d.DepthOrArraySize),
                    ..Default::default()
                };
            }
            all
        });
        if d.Format.0 == 60 {
            let mut plane = 0;
            match desc.ViewDimension {
                D3D12_UAV_DIMENSION_TEXTURE2D => {
                    let mut view = unsafe { desc.Anonymous.Texture2D };
                    plane = std::mem::take(&mut view.PlaneSlice);
                    desc.Anonymous.Texture2D = view;
                }
                D3D12_UAV_DIMENSION_TEXTURE2DARRAY => {
                    let mut view = unsafe { desc.Anonymous.Texture2DArray };
                    plane = std::mem::take(&mut view.PlaneSlice);
                    desc.Anonymous.Texture2DArray = view;
                }
                _ => {}
            }
            if plane != 1 && !matches!(desc.Format.0, 22 | 47) {
                return None;
            }
            desc.Format.0 = 62;
            return Some(desc);
        }
        desc.Format.0 = match (desc.Format.0, d.Format.0) {
            (0 | 53 | 55 | 19..=21 | 44..=46, 53) => 56,
            (0 | 39 | 40 | 19..=21 | 44..=46, 39) => 41,
            (other, _) => other,
        };
        Some(desc)
    }

    fn make_view(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<Kept> {
        let host = Host::instance()?;
        let device = &host.device;
        match view {
            &View::Constant(address, size) => {
                let shadow = crate::shadow::ShadowRegistry::find(address, u64::from(size))?;
                let gpu = shadow.gpu(address);
                if gpu % 256 != 0 {
                    once!(
                        address = format_args!("{address:#x}"),
                        "constant buffer not 256-byte aligned on the host"
                    );
                    return None;
                }
                let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                    BufferLocation: gpu,
                    SizeInBytes: BufferViews::constant_size(size),
                };
                unsafe { device.CreateConstantBufferView(Some(&desc), at) };
                Some(Some(shadow.buffer.clone()))
            }
            View::Shader(resource, desc) => {
                let resource = *resource as crate::object::This;
                if let Some(texture) = crate::resource::HostTextures::host_resource(resource) {
                    #[cfg(feature = "diagnostics")]
                    {
                        let own = unsafe { texture.GetDesc() }.Format.0;
                        if matches!(own, 19..=22 | 44..=47) {
                            static SEEN: Mutex<Vec<(i32, Option<[u32; 10]>)>> =
                                Mutex::new(Vec::new());
                            let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
                            if seen.len() < 64 && !seen.contains(&(own, *desc)) {
                                seen.push((own, *desc));
                                tracing::info!(
                                    ?resource,
                                    texture_format = own,
                                    view = ?desc,
                                    "shader view of a depth-stencil texture"
                                );
                            }
                        }
                    }
                    let mut desc = desc.as_ref().map(BufferViews::srv_desc);
                    if !ViewMaker::fits(texture, desc.as_mut().map(|d| &mut d.Format), false) {
                        return None;
                    }
                    unsafe {
                        device.CreateShaderResourceView(
                            texture,
                            desc.as_ref().map(std::ptr::from_ref),
                            at,
                        )
                    };
                    Some(Some(texture.clone()))
                } else if let (Some((data, len)), Some(desc)) =
                    (crate::resource::GpuResource::buffer_bytes(resource), desc)
                {
                    BufferViews::buffer_srv(data as u64, len, BufferViews::srv_desc(desc), at)
                } else {
                    None
                }
            }
            View::Unordered(resource, _counter, desc) => {
                let resource = *resource as crate::object::This;
                let Some(texture) = crate::resource::HostTextures::host_resource(resource) else {
                    return match (crate::resource::GpuResource::buffer_bytes(resource), desc) {
                        (Some((data, len)), Some(desc)) => BufferViews::buffer_uav(
                            data as u64,
                            len,
                            BufferViews::uav_desc(desc),
                            at,
                        ),
                        _ => None,
                    };
                };
                if crate::resource::GpuResource::depth_and_unordered(resource) {
                    static LOGGED: std::sync::atomic::AtomicU32 =
                        std::sync::atomic::AtomicU32::new(0);
                    if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 512 {
                        tracing::info!(
                            ?resource,
                            view = ?desc,
                            "unordered access view of a depth texture"
                        );
                    }
                }
                let mut desc = desc.as_ref().map(BufferViews::uav_desc);
                let twin = crate::resource::Twin::resource_of(resource)
                    .and_then(|twin| Some((twin, ViewMaker::twin_uav(twin, desc.as_ref())?)))
                    .or_else(|| {
                        let twin = crate::resource::Twin::plane_resource_of(resource)?;
                        Some((twin, ViewMaker::twin_uav(twin, desc.as_ref())?))
                    });
                let texture = match twin {
                    Some((twin, view)) => {
                        desc = Some(view);
                        twin
                    }
                    None => texture,
                };
                if !ViewMaker::fits(texture, desc.as_mut().map(|d| &mut d.Format), true) {
                    return None;
                }
                unsafe {
                    device.CreateUnorderedAccessView(
                        texture,
                        None::<&ID3D12Resource>,
                        desc.as_ref().map(std::ptr::from_ref),
                        at,
                    )
                };
                Some(Some(texture.clone()))
            }
            View::Sampler(words) => {
                let desc = unsafe {
                    std::ptr::from_ref(words)
                        .cast::<D3D12_SAMPLER_DESC>()
                        .read_unaligned()
                };
                unsafe { device.CreateSampler(&desc, at) };
                Some(None)
            }
            View::ConsoleSampler(words) => {
                let desc = crate::sampler::ConsoleSampler::from_words(words).host_desc();
                unsafe { device.CreateSampler(&desc, at) };
                Some(None)
            }
            View::Placed(unordered, resource, desc) => {
                if let Some(view) = PlacedTexture::of(view) {
                    let made = ViewMaker::make_view(&view.view, at);
                    #[cfg(feature = "diagnostics")]
                    if *unordered && made.as_ref().is_some_and(Option::is_some) {
                        static FIRST: std::sync::Once = std::sync::Once::new();
                        FIRST.call_once(|| {
                            tracing::info!(
                                dimension = resource[0],
                                resource_format = resource[8],
                                view_format = desc[0],
                                flags = resource[12],
                                address = format_args!(
                                    "{:#x}",
                                    u64::from(desc[10]) | u64::from(desc[11]) << 32
                                ),
                                "mirrored a placed texture UAV"
                            )
                        });
                    }
                    return made;
                }
                const BUFFER: u32 = 1;
                if resource[0] != BUFFER {
                    static FIRST: [std::sync::Once; 2] =
                        [std::sync::Once::new(), std::sync::Once::new()];
                    FIRST[usize::from(*unordered)].call_once(|| {
                        tracing::warn!(
                            unordered,
                            dimension = resource[0],
                            format = resource[8],
                            flags = resource[12],
                            address = format_args!(
                                "{:#x}",
                                u64::from(desc[10]) | u64::from(desc[11]) << 32
                            ),
                            "could not resolve the placed texture view"
                        )
                    });
                    return None;
                }
                let len = u64::from(resource[4]) | u64::from(resource[5]) << 32;
                let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
                if *unordered {
                    BufferViews::buffer_uav(address, len, BufferViews::uav_desc(words), at)
                } else {
                    BufferViews::buffer_srv(address, len, BufferViews::srv_desc(words), at)
                }
            }
        }
    }

    fn keep_views() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_KEEP_VIEWS").is_none_or(|v| v != "0"))
    }

    fn same_placed_texture(a: &View, b: &View) -> bool {
        match (a, b) {
            (View::Placed(false, resource, desc), View::Placed(false, other, other_desc)) => {
                resource[0] != 1
                    && desc[10..12] == other_desc[10..12]
                    && crate::resource::PlacedTextures::placed_key(*resource)
                        == crate::resource::PlacedTextures::placed_key(*other)
            }
            _ => false,
        }
    }

    fn resident_sampled_texture(view: &View) -> Option<crate::resource::ResidentTexture> {
        let texture = match view {
            View::Shader(object, _) => *object as crate::object::This,
            View::Placed(false, resource, desc) if resource[0] != 1 && resource[12] & 7 == 0 => {
                let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                crate::resource::PlacedTextures::placed_known(address, resource)?
            }
            _ => return None,
        };
        crate::resource::GpuResource::resident_copy(texture)
    }

    #[cfg(feature = "diagnostics")]
    fn magenta_for(desc: &[u32; 16], at: D3D12_CPU_DESCRIPTOR_HANDLE) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("LEONIDA_NULL_MAGENTA").is_some_and(|v| v == "1")) {
            return false;
        }
        let colour = matches!(desc[0], 27..=29 | 70..=78 | 87..=91 | 97..=99);
        if desc[1] != D3D12_SRV_DIMENSION_TEXTURE2D.0 as u32 || !colour {
            return false;
        }
        let (Some(host), Some(texture)) = (Host::instance(), ViewMaker::magenta()) else {
            return false;
        };
        unsafe { host.device.CreateShaderResourceView(texture, None, at) };
        true
    }

    #[cfg(feature = "diagnostics")]
    fn magenta() -> Option<&'static ID3D12Resource> {
        static TEXTURE: std::sync::OnceLock<Option<ID3D12Resource>> = std::sync::OnceLock::new();
        TEXTURE
            .get_or_init(|| {
                let heap = D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_CUSTOM,
                    CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                    MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                    ..Default::default()
                };
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: 1,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    ..Default::default()
                };
                let mut texture: Option<ID3D12Resource> = None;
                unsafe {
                    Host::instance()?
                        .device
                        .CreateCommittedResource(
                            &heap,
                            D3D12_HEAP_FLAG_NONE,
                            &desc,
                            D3D12_RESOURCE_STATE_COMMON,
                            None,
                            &mut texture,
                        )
                        .ok()?;
                    let texture = texture?;
                    texture.Map(0, None, None).ok()?;
                    let texel = [255u8, 0, 255, 255];
                    texture
                        .WriteToSubresource(0, None, texel.as_ptr().cast(), 4, 4)
                        .ok()?;
                    texture.Unmap(0, None);
                    Some(texture)
                }
            })
            .as_ref()
    }

    fn null_for(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
        let Some(host) = Host::instance() else { return };
        let is_buffer = |r: &usize| {
            crate::resource::GpuResource::buffer_bytes(*r as crate::object::This).is_some()
        };
        let (buffer, dimension) = match view {
            View::Unordered(r, _, desc) => (
                is_buffer(r) || desc.is_some_and(|d| d[1] == 1),
                desc.map(|d| d[1]),
            ),
            View::Placed(true, resource, desc) => (resource[0] == 1, Some(desc[1])),
            View::Shader(r, desc) if is_buffer(r) || desc.is_some_and(|d| d[1] == 1) => {
                return ViewMaker::null_buffer_view(at);
            }
            View::Placed(false, resource, _) if resource[0] == 1 => {
                return ViewMaker::null_buffer_view(at);
            }
            View::Shader(_, Some(desc)) => return ViewMaker::null_like(desc, at),
            View::Placed(false, _, desc) => {
                #[cfg(feature = "diagnostics")]
                if ViewMaker::magenta_for(desc, at) {
                    return;
                }
                return ViewMaker::null_like(desc[..10].try_into().unwrap_or(&[0; 10]), at);
            }
            View::Constant(..) => {
                let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC::default();
                return unsafe { host.device.CreateConstantBufferView(Some(&desc), at) };
            }
            _ => return ViewMaker::null_view(at),
        };
        let desc = if buffer {
            D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_UINT,
                ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                ..Default::default()
            }
        } else {
            ViewMaker::null_uav_desc(dimension)
        };
        unsafe {
            host.device.CreateUnorderedAccessView(
                None::<&ID3D12Resource>,
                None::<&ID3D12Resource>,
                Some(&desc),
                at,
            )
        };
    }

    fn null_uav_desc(dimension: Option<u32>) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
        let mut desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        match dimension.map(|d| D3D12_UAV_DIMENSION(d as i32)) {
            Some(D3D12_UAV_DIMENSION_TEXTURE1D) => {
                desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1D;
            }
            Some(D3D12_UAV_DIMENSION_TEXTURE1DARRAY) => {
                desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY;
                desc.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV {
                    ArraySize: 1,
                    ..Default::default()
                };
            }
            Some(D3D12_UAV_DIMENSION_TEXTURE2DARRAY) => {
                desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
                desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                    ArraySize: 1,
                    ..Default::default()
                };
            }
            Some(D3D12_UAV_DIMENSION_TEXTURE3D) => {
                desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D;
                desc.Anonymous.Texture3D = D3D12_TEX3D_UAV {
                    WSize: u32::MAX,
                    ..Default::default()
                };
            }
            _ => {}
        }
        desc
    }

    fn null_buffer_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
        let Some(host) = Host::instance() else { return };
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    NumElements: 1,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                    ..Default::default()
                },
            },
        };
        unsafe {
            host.device
                .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
        };
    }

    fn null_like(words: &[u32; 10], at: D3D12_CPU_DESCRIPTOR_HANDLE) {
        const TEXTURES: std::ops::RangeInclusive<u32> = 2..=10;
        if !TEXTURES.contains(&words[1]) {
            return ViewMaker::null_view(at);
        }
        let Some(host) = Host::instance() else { return };
        let mut w = [0u32; 10];
        w[1] = words[1];
        match words[1] {
            6 => {}
            7 => w[5] = 1,
            3 | 5 | 10 => (w[5], w[7]) = (1, 1),
            _ => w[5] = 1,
        }
        let mut desc = BufferViews::srv_desc(&w);
        desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
        desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        unsafe {
            host.device
                .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
        };
    }

    fn null_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
        let Some(host) = Host::instance() else { return };
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV {
                    MipLevels: 1,
                    ..Default::default()
                },
            },
        };
        unsafe {
            host.device
                .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
        };
    }
}

impl PlacedTexture {
    pub(crate) fn of(view: &View) -> Option<PlacedTexture> {
        const BUFFER: u32 = 1;
        let View::Placed(unordered, resource, desc) = view else {
            return None;
        };
        if resource[0] == BUFFER || (*unordered && resource[12] & 4 == 0) {
            return None;
        }
        let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
        let owner = if resource[12] & 4 != 0 {
            crate::resource::PlacedTextures::writable_texture_at(address, resource)
        } else {
            None
        };
        let texture = match &owner {
            Some(owner) => owner.as_ptr(),
            None if *unordered => return None,
            None => crate::resource::PlacedTextures::texture_at(address, resource)?,
        };
        let words = desc[..10].try_into().ok();
        let view = if *unordered {
            View::Unordered(texture as usize, 0, words)
        } else {
            View::Shader(texture as usize, words)
        };
        Some(PlacedTexture {
            view,
            _owner: owner,
        })
    }
}

type Kept = Option<ID3D12Resource>;

enum SrvError {
    Alignment,
    Layout,
}

pub(crate) static KEPT_VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) struct PlacedTexture {
    pub(crate) view: View,
    _owner: Option<crate::resource::ResourceRef>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texture_remaps_visit_each_changed_region_once() {
        let mut ranges = Vec::new();
        for (from, to) in [
            (40, 50),
            (30, 40),
            (35, 40),
            (10, 20),
            (45, 60),
            (60, 61),
            (1, 1),
        ] {
            assert!(!RemapLog::record(&mut ranges, from, to, 3));
        }
        RemapLog::merge(&mut ranges);
        assert_eq!(ranges, [(10, 20), (30, 61)]);
        assert!(RemapLog::record(&mut ranges, 90, 99, 2));
        assert!(!RemapLog::record(&mut ranges, 120, 160, 2));
        assert_eq!(ranges, [(0, u64::MAX)]);
    }

    #[test]
    fn placed_buffer_slots_follow_new_and_released_owners() {
        let Some(_) = Host::instance() else { return };
        #[repr(align(256))]
        struct Aligned([u8; 512]);
        static DATA: Aligned = Aligned([0; 512]);
        let start = DATA.0.as_ptr() as u64;
        let address = start + 256;
        let heap = HostHeap::new(false, 2).unwrap();
        let mut records = [0u64; 8];
        let handle = records.as_mut_ptr() as u64;
        let mut resource = [0; 14];
        resource[0] = 1;
        resource[4] = 64;
        let mut desc = [0; 16];
        desc[0] = DXGI_FORMAT_R32_UINT.0 as u32;
        desc[1] = 1;
        desc[6] = 16;
        desc[10] = address as u32;
        desc[11] = (address >> 32) as u32;
        super::super::Descriptors::create_placed_srv(std::ptr::null(), &resource, &desc, handle);
        let mut unordered = desc;
        unordered[4] = 16;
        unordered[6] = 0;
        super::super::Descriptors::create_placed_uav(
            std::ptr::null(),
            &resource,
            &unordered,
            handle + 32,
        );
        let verify = |shadow: &crate::shadow::Shadow| {
            for slot in 0..2 {
                heap.mirror(slot, handle + 32 * u64::from(slot), true)
                    .unwrap();
                assert_eq!(heap.shows(slot, &shadow.buffer), Some(true));
            }
        };
        crate::shadow::ShadowRegistry::register(1, start, 512);
        let outer = crate::shadow::ShadowRegistry::find(address, 64).unwrap();
        verify(&outer);
        crate::shadow::ShadowRegistry::register(2, address, 64);
        let inner = crate::shadow::ShadowRegistry::find(address, 64).unwrap();
        assert_ne!(outer.id(), inner.id());
        verify(&inner);
        crate::shadow::ShadowRegistry::unregister(2, address);
        verify(&outer);
        crate::shadow::ShadowRegistry::unregister(1, start);
    }

    #[test]
    fn null_typed_srv_rebinds_when_a_new_owner_places_it_on_an_element() {
        Host::instance().expect("D3D12 test device");
        #[repr(align(256))]
        struct Aligned([u8; 1024]);
        static DATA: Aligned = Aligned([0; 1024]);
        let start = DATA.0.as_ptr() as u64;
        let address = start + 256;
        let heap = HostHeap::new(false, 1).unwrap();
        let mut record = [0u64; 4];
        let handle = record.as_mut_ptr() as u64;
        let mut resource = [0; 14];
        resource[0] = 1;
        resource[4] = 720;
        let mut desc = [0; 16];
        desc[0] = DXGI_FORMAT_R32G32B32_FLOAT.0 as u32;
        desc[1] = 1;
        desc[6] = 60;
        desc[10] = address as u32;
        desc[11] = (address >> 32) as u32;
        super::super::Descriptors::create_placed_srv(std::ptr::null(), &resource, &desc, handle);
        let view = super::super::Descriptors::view(handle).unwrap();
        crate::shadow::ShadowRegistry::register(3, start, 1024);
        let outer = crate::shadow::ShadowRegistry::find(address, 720).unwrap();
        heap.mirror(0, handle, true).unwrap();
        assert_eq!(heap.shows(0, &outer.buffer), None);
        assert!(heap.buffer_owner_current(0, &view));
        crate::shadow::ShadowRegistry::register(4, address, 720);
        let inner = crate::shadow::ShadowRegistry::find(address, 720).unwrap();
        assert!(!heap.buffer_owner_current(0, &view));
        heap.mirror(0, handle, true).unwrap();
        assert_eq!(heap.shows(0, &inner.buffer), Some(true));
        crate::shadow::ShadowRegistry::unregister(4, address);
        heap.mirror(0, handle, true).unwrap();
        assert_eq!(heap.shows(0, &outer.buffer), None);
        assert!(heap.buffer_owner_current(0, &view));
        crate::shadow::ShadowRegistry::unregister(3, start);
    }

    #[test]
    fn null_uavs_keep_the_titles_dimension() {
        let d = ViewMaker::null_uav_desc(Some(8));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE3D);
        assert_eq!(unsafe { d.Anonymous.Texture3D.WSize }, u32::MAX);
        let d = ViewMaker::null_uav_desc(Some(5));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY);
        assert_eq!(unsafe { d.Anonymous.Texture2DArray.ArraySize }, 1);
        let dimension = |d| ViewMaker::null_uav_desc(d).ViewDimension;
        assert_eq!(dimension(Some(2)), D3D12_UAV_DIMENSION_TEXTURE1D);
        assert_eq!(dimension(Some(4)), D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(dimension(None), D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(dimension(Some(6)), D3D12_UAV_DIMENSION_TEXTURE2D);
    }

    #[test]
    fn structured_record_count_uses_the_original_subview() {
        let mut resource = [0; 14];
        resource[0] = 1;
        let mut desc = [0; 16];
        desc[1] = 1;
        desc[4] = 11;
        desc[6] = 7;
        desc[7] = 16;
        desc[10] = 0x1000;
        assert_eq!(
            BufferViews::structured_srv_records(&View::Placed(false, resource, desc)),
            Some(7)
        );
        let object_desc = desc[..10].try_into().unwrap();
        assert_eq!(
            BufferViews::structured_srv_records(&View::Shader(1, Some(object_desc))),
            Some(7)
        );
        assert_eq!(
            BufferViews::structured_srv_records(&View::Shader(0, Some(object_desc))),
            None
        );
        assert_eq!(
            BufferViews::structured_srv_records(&View::Placed(true, resource, desc)),
            None
        );
        desc[10] = 0;
        assert_eq!(
            BufferViews::structured_srv_records(&View::Placed(false, resource, desc)),
            None
        );
        desc[10] = 0x1000;
        desc[6] = 0;
        assert_eq!(
            BufferViews::structured_srv_records(&View::Placed(false, resource, desc)),
            Some(0)
        );
        desc[7] = 0;
        assert_eq!(
            BufferViews::structured_srv_records(&View::Placed(false, resource, desc)),
            None
        );
        desc[7] = 16;
        desc[1] = 4;
        assert_eq!(
            BufferViews::structured_srv_records(&View::Placed(false, resource, desc)),
            None
        );
    }

    #[test]
    fn views_of_one_placed_texture_are_told_by_address_and_description() {
        let texture = [3, 0, 0, 0, 512, 0, 512, 0x8_0001, 98, 1, 0, 0x105, 0, 0];
        let view = |resource: [u32; 14], first_mip: u32, address: u32| {
            let mut desc = [0u32; 16];
            (desc[0], desc[1], desc[4], desc[5], desc[10], desc[11]) =
                (98, 4, first_mip, 8 - first_mip, address, 0xb);
            View::Placed(false, resource, desc)
        };
        let low = view(texture, 3, 0x1000_0000);
        assert!(ViewMaker::same_placed_texture(
            &low,
            &view(texture, 0, 0x1000_0000)
        ));
        let mut padded = texture;
        padded[1] = 0x1234;
        padded[13] = 0x7ffa;
        assert!(ViewMaker::same_placed_texture(
            &low,
            &view(padded, 0, 0x1000_0000)
        ));
        let mut spans = Spans::default();
        let start = 0xb_1000_0000;
        let small = start + spans.bytes(&texture, 3).unwrap();
        let full = start + spans.bytes(&padded, 0).unwrap();
        assert!(full > small);
        assert!(spans.note(1, start, small, &texture, true).is_empty());
        assert!(spans.note(2, start, full, &padded, true).is_empty());
        assert_eq!(spans.claim(1, start, small, &texture), (Claim::Own, None));
        assert!(!ViewMaker::same_placed_texture(
            &low,
            &view(texture, 3, 0x2000_0000)
        ));
        let mut wider = texture;
        wider[4] = 1024;
        assert!(!ViewMaker::same_placed_texture(
            &low,
            &view(wider, 3, 0x1000_0000)
        ));
        let mut buffer = texture;
        buffer[0] = 1;
        assert!(!ViewMaker::same_placed_texture(
            &view(buffer, 3, 0x1000_0000),
            &view(buffer, 3, 0x1000_0000)
        ));
        let View::Placed(_, resource, desc) = low.clone() else {
            unreachable!()
        };
        assert!(!ViewMaker::same_placed_texture(
            &low,
            &View::Placed(true, resource, desc)
        ));
        assert!(!ViewMaker::same_placed_texture(
            &low,
            &View::Shader(0, None)
        ));
    }

    #[cfg(feature = "diagnostics")]
    #[test]
    fn a_magenta_texture_is_made() {
        let Some(host) = Host::instance() else {
            return;
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let texture = ViewMaker::magenta().expect("the texture");
        unsafe {
            host.device
                .CreateShaderResourceView(texture, None, heap.cpu(0))
        };
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    #[test]
    fn null_3d_uav_is_accepted_by_the_device() {
        let Some(host) = Host::instance() else {
            return;
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let view = View::Unordered(0, 0, Some([28, 8, 0, 0, 0, 0, 0, 0, 0, 0]));
        ViewMaker::null_for(&view, heap.cpu(0));
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    #[test]
    fn a_structured_view_made_raw_passes_the_host_check() {
        let (first, count) = BufferViews::raw_window(32, 8, 100, 20).unwrap();
        assert_eq!((first, count), (48, 500));
        assert!(BufferViews::buffer_view_ok(
            DXGI_FORMAT_R32_TYPELESS,
            first,
            count,
            0,
            true,
            4096
        ));
        assert!(!BufferViews::buffer_view_ok(
            DXGI_FORMAT_UNKNOWN,
            first,
            count,
            0,
            true,
            4096
        ));
        assert!(!BufferViews::buffer_view_ok(
            DXGI_FORMAT_R32_TYPELESS,
            first,
            count,
            0,
            true,
            2000
        ));
        assert!(BufferViews::raw_window(4, 0, 100, 20).is_none());
        assert!(!BufferViews::buffer_view_ok(
            DXGI_FORMAT_R32_TYPELESS,
            0,
            0,
            0,
            true,
            4096
        ));
        assert!(BufferViews::buffer_view_ok(
            DXGI_FORMAT_UNKNOWN,
            1,
            10,
            12,
            false,
            4096
        ));
    }

    #[test]
    fn constant_views_are_whole_and_at_most_64_kib() {
        assert_eq!(BufferViews::constant_size(1), 256);
        assert_eq!(BufferViews::constant_size(256), 256);
        assert_eq!(BufferViews::constant_size(65_536), 65_536);
        assert_eq!(BufferViews::constant_size(969_472), 65_536);
        assert_eq!(BufferViews::constant_size(u32::MAX), 65_536);
    }

    #[test]
    fn views_past_the_end_are_cut_to_the_elements_inside() {
        assert_eq!(BufferViews::elements_inside(0, 10, 16, 160), 10);
        assert_eq!(BufferViews::elements_inside(4, 10, 16, 160), 6);
        assert_eq!(BufferViews::elements_inside(4, 10, 16, 167), 6);
        assert_eq!(BufferViews::elements_inside(10, 10, 16, 160), 10);
        assert_eq!(BufferViews::elements_inside(0, 7, 4, 1 << 40), 7);
    }

    #[test]
    fn uploaded_textures_are_looked_at_again() {
        let mut q = TextureQueue::default();
        let at = (0x1000, 64);
        q.written(7, 1, 1, false);
        let write = q.due(1, 1)[0].1;
        q.recheck(7, 1, at, 5);
        assert!(q.rechecks_due(1000, 9).is_empty(), "it waits in the queue");
        q.done(7, write);
        q.recheck(7, 1, at, 5);
        assert_eq!(q.census(1000)[0], 0, "no slot of the queue");
        let mut frame = 1;
        for wait in RECHECK_FRAMES {
            assert!(q.rechecks_due(frame + wait - 1, 9).is_empty());
            frame += wait;
            let due = q.rechecks_due(frame, 9);
            assert_eq!(due.len(), 1);
            q.rechecked(7, &due[0].1, false, frame);
        }
        assert!(q.rechecks_due(u64::MAX, 9).is_empty(), "the looks are over");
        q.recheck(7, frame, at, 5);
        let due = q.rechecks_due(u64::MAX, 9);
        q.rechecked(7, &due[0].1, true, frame + 4);
        assert_eq!(q.due(frame + 4, 9)[0].0, 7);
        assert!(q.rechecks_due(u64::MAX, 9).is_empty());
        q.recheck(8, 1, at, 5);
        let seen = q.rechecks_due(u64::MAX, 9);
        q.written(8, 2, 2, true);
        assert!(q.due(2, 9).iter().any(|d| d.0 == 8));
        q.rechecked(8, &seen[0].1, false, 5);
        assert!(q.rechecks_due(u64::MAX, 9).is_empty());
    }

    #[test]
    fn a_fingerprint_sees_changes() {
        let mut memory = vec![3u8; 1 << 20];
        let print = |m: &[u8]| {
            crate::texture::TitleMemoryMap::fingerprint(m.as_ptr() as u64, m.len() as u64)
        };
        let before = print(&memory);
        assert_eq!(print(&memory), before);
        let last = memory.len() - 1;
        for at in [0, ((memory.len() - 4096) / 11 * 5) & !0xfff, last] {
            memory[at] ^= 1;
            assert_ne!(print(&memory), before, "byte {at}");
            memory[at] ^= 1;
        }
        assert_ne!(print(&memory[..100]), print(&memory[..200]));
    }

    #[test]
    fn texture_queue_keeps_first_looks_in_a_retry_backlog() {
        let mut q = TextureQueue::default();
        for index in 0..10 {
            q.written(index, 0, 0, false);
        }
        for (index, write, _) in q.due(0, 10) {
            q.later(index, write, 0, 4);
        }
        for index in 10..20 {
            q.written(index, 1, 1, false);
        }
        let slots = |frame, max| {
            q.due(frame, max)
                .into_iter()
                .map(|d| d.0)
                .collect::<Vec<_>>()
        };
        assert_eq!(slots(1, 4), [19, 9, 10, 8]);
        assert_eq!(slots(2, 4), [9, 10, 8, 19]);
        assert_eq!(slots(3, 1), [10]);
        assert_eq!(slots(4, 1), [9]);
        assert_eq!(slots(5, 1), [19]);
        assert!(slots(5, 0).is_empty());
    }

    #[test]
    fn the_texture_queue_serves_the_newest_writes_first() {
        let mut q = TextureQueue::default();
        for i in 0..8u32 {
            q.written(10 + i, u64::from(i) + 1, 0, false);
        }
        q.written(5, 9, 0, false);
        let order: Vec<u32> = q.due(9, 100).into_iter().map(|d| d.0).collect();
        assert_eq!(order, [5, 10, 17, 11, 16, 12, 15, 13, 14]);
        assert_eq!(q.due(9, 2).len(), 2);
        assert_eq!(q.census(9), [9, 9, 0, 0, 8]);
        let write = |q: &TextureQueue, index: u32| {
            q.due(u64::MAX, 100)
                .iter()
                .find(|d| d.0 == index)
                .unwrap()
                .1
        };
        let w17 = write(&q, 17);
        let mut frame = 9;
        for wait in [1, 2, 4, 4] {
            q.later(17, w17, frame, 4);
            assert!(!q.due(frame + wait - 1, 100).iter().any(|d| d.0 == 17));
            frame += wait;
            assert_eq!(q.due(frame, 100)[0].0, 17);
        }
        assert_eq!(q.census(9)[2], 1);
        q.done(17, w17);
        assert!(!q.due(u64::MAX, 100).iter().any(|d| d.0 == 17));
        let w16 = write(&q, 16);
        q.partly(16, w16, 12, 4096);
        assert_eq!(q.mapped(16), 4096);
        assert_eq!(q.due(12, 100)[0].0, 16);
        q.written(16, 13, 0, false);
        q.done(16, w16);
        q.later(16, w16, 13, 4);
        assert_eq!(q.mapped(16), 0);
        assert_eq!(q.due(13, 100)[0].0, 16);
        let w15 = write(&q, 15);
        q.partly(15, w15, 20, 8192);
        q.written(15, 14, 0, true);
        assert_eq!(q.mapped(15), 8192);
        q.written(99, 14, 0, true);
        assert!(q.due(14, 100).iter().any(|d| d.0 == 99));
        q.written(98, 14, 16, false);
        assert!(!q.due(15, 100).iter().any(|d| d.0 == 98));
        assert!(q.due(16, 100).iter().any(|d| d.0 == 98));
    }

    #[test]
    fn a_slot_rewritten_moves_to_its_new_span() {
        let mut spans = Spans::default();
        assert!(
            spans
                .note(1, 0x10000, 0x12000, &shape(0x10000, 0x12000), true)
                .is_empty()
        );
        assert!(
            spans
                .note(2, 0x12000, 0x14000, &shape(0x12000, 0x14000), true)
                .is_empty()
        );
        assert!(
            spans
                .note(1, 0x10000, 0x12000, &shape(0x10000, 0x12000), true)
                .is_empty()
        );
        assert_eq!(spans.slots_in(0x10000, 0x10001), [1]);
        assert!(
            spans
                .note(1, 0x90000, 0x91000, &shape(0x90000, 0x91000), true)
                .is_empty()
        );
        assert_eq!(spans.slots_in(0x10000, 0x10001), [] as [u32; 0]);
        assert_eq!(spans.slots_in(0x12000, 0x12001), [2]);
        assert_eq!(spans.slots_in(0x90000, 0x90001), [1]);
        spans.forget(1);
        spans.forget(2);
        assert!(spans.spans.is_empty() && spans.by_slot.is_empty());
    }

    #[test]
    fn a_newer_view_of_other_memory_makes_the_older_stale() {
        let mut spans = Spans::default();
        assert!(
            spans
                .note(1, 0x1000, 0x3000, &shape(0x1000, 0x3000), true)
                .is_empty()
        );
        assert!(
            spans
                .note(2, 0x1000, 0x3000, &shape(0x1000, 0x3000), true)
                .is_empty()
        );
        assert!(
            spans
                .note(3, 0x3000, 0x4000, &shape(0x3000, 0x4000), true)
                .is_empty()
        );
        assert!(
            spans
                .note(4, 0x8000, 0x9000, &shape(0x8000, 0x9000), true)
                .is_empty()
        );
        assert!(
            spans
                .note(2, 0x1000, 0x3000, &shape(0x1000, 0x3000), true)
                .is_empty()
        );
        assert_eq!(spans.slots_in(0x2000, 0x3800), [1, 2, 3]);
        assert_eq!(spans.slots_in(0x4000, 0x8000), [] as [u32; 0]);
        assert!(
            spans
                .note(9, 0x2000, 0x3800, &shape(0x2000, 0x3800), false)
                .is_empty()
        );
        assert_eq!(spans.slots_in(0x2000, 0x3800), [1, 2, 3]);
        let mut stale = spans.note(5, 0x2000, 0x4000, &shape(0x2000, 0x4000), true);
        stale.sort_unstable();
        assert_eq!(stale, [1, 2, 3]);
        assert_eq!(spans.slots_in(0, u64::MAX), [5, 4]);
        assert_eq!(
            spans.note(6, 0x2800, 0x3000, &shape(0x2800, 0x3000), true),
            [5]
        );
        assert_eq!(
            spans.note(7, 0x2800, 0x2c00, &shape(0x2800, 0x2c00), true),
            [6]
        );
        assert!(
            spans
                .note(7, 0x8000, 0x9000, &shape(0x8000, 0x9000), true)
                .is_empty()
        );
        assert_eq!(spans.slots_in(0x2800, 0x2c00), [] as [u32; 0]);
        assert_eq!(spans.slots_in(0x8000, 0x8001), [4, 7]);
        spans.forget(4);
        spans.forget(7);
        assert!(spans.spans.is_empty() && spans.by_slot.is_empty());
    }

    #[test]
    fn a_texture_of_which_the_smaller_mips_are_there_ends_behind_them() {
        let texture = [3, 0, 0, 0, 512, 0, 512, 0x8_0001, 99, 1, 0, 0x105, 0, 0];
        let next = [3, 0, 0, 0, 1024, 0, 512, 0x8_0001, 71, 1, 0, 0x105, 0, 0];
        let mut spans = Spans::default();
        let at = 0xb_5c04_0000u64;
        let end = |spans: &mut Spans, desc: &[u32; 14], at: u64, first| {
            at + spans.bytes(desc, first).unwrap()
        };
        let a = end(&mut spans, &texture, at, 3);
        assert_eq!(a, at + 0x2000);
        assert!(spans.note(1, at, a, &texture, true).is_empty());
        let b = end(&mut spans, &next, a, 3);
        assert!(b <= a + 0x4000);
        assert!(spans.note(2, a, b, &next, true).is_empty());
        assert_eq!(spans.slots_in(at, b), [1, 2]);
        let small = end(&mut spans, &texture, at, 5);
        assert!(small < a);
        assert!(spans.note(3, at, small, &texture, true).is_empty());
        assert_eq!(spans.slots_in(at, at + 1), [3, 1]);
        let more = end(&mut spans, &texture, at, 1);
        assert!(more > b);
        assert_eq!(spans.note(1, at, more, &texture, true), [2]);
        assert_eq!(spans.claim(3, at, small, &texture), (Claim::Own, None));
        assert_eq!(
            spans.claim(2, a, b, &next),
            (Claim::Other, Some((at, more)))
        );
    }

    fn shape(start: u64, end: u64) -> [u32; 14] {
        let mut desc = [0; 14];
        (desc[0], desc[4], desc[6]) = (3, start as u32, end as u32);
        desc
    }

    #[test]
    fn spans_say_whose_memory_a_view_is() {
        let mut spans = Spans::default();
        assert!(
            spans
                .note(1, 0x1000, 0x3000, &shape(0x1000, 0x3000), true)
                .is_empty()
        );
        assert_eq!(
            spans.claim(1, 0x1000, 0x3000, &shape(0x1000, 0x3000)),
            (Claim::Own, None)
        );
        assert_eq!(
            spans.claim(2, 0x1000, 0x3000, &shape(0x1000, 0x3000)),
            (Claim::Free, None)
        );
        assert_eq!(
            spans.claim(2, 0x3000, 0x4000, &shape(0x3000, 0x4000)),
            (Claim::Free, None)
        );
        assert_eq!(
            spans.claim(2, 0x1000, 0x2000, &shape(0x1000, 0x2000)),
            (Claim::SameStart, Some((0x1000, 0x3000)))
        );
        assert_eq!(
            spans.claim(2, 0x800, 0x2000, &shape(0x800, 0x2000)),
            (Claim::Other, Some((0x1000, 0x3000)))
        );
    }

    #[test]
    fn a_texture_sweep_finds_views_nothing_brings_up_to_date() {
        let Some(host) = HostHeap::new(false, 136) else {
            return;
        };
        let at = unsafe {
            use windows_sys::Win32::System::Memory::{
                MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
            };
            VirtualAlloc(
                0x7d_4000_0000usize as *const _,
                1 << 20,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            ) as u64
        };
        if at == 0 {
            return;
        }
        let mut slots = vec![[0u32; 8]; 136];
        let base = slots.as_mut_ptr() as u64;
        let handle = |index: u32| Some(base + u64::from(index) * 32);
        let resource = [3, 0, 0, 0, 64, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0];
        let view = |width: u32, address: u64| {
            let mut desc = [0u32; 16];
            (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
            let mut resource = resource;
            resource[4] = width;
            View::Placed(false, resource, desc)
        };
        super::super::Descriptors::record(handle(1).unwrap(), view(64, at));
        super::super::Descriptors::record(handle(2).unwrap(), view(64, at + 0x8_0000));
        host.note_buffer(5, Some(&view(128, at + 0x8_0000)), false);
        let queued = || -> Vec<u32> {
            host.texture_slots_due(u64::MAX, 8)
                .0
                .into_iter()
                .map(|d| d.0)
                .collect()
        };
        for slot in queued() {
            host.textures.lock().unwrap().remove(slot);
        }
        let mut counts = [0; 10];
        host.sweep_part(0..8, 0, handle, 50, false, &mut counts);
        assert_eq!(counts, [2, 0, 0, 0, 0, 0, 1, 1, 0, 0]);
        assert_eq!(queued(), []);
        host.sweep_part(0..8, 0, handle, 50, true, &mut counts);
        assert_eq!(queued(), [1]);

        for slot in 8..136 {
            super::super::Descriptors::record(handle(slot).unwrap(), view(64, at));
        }
        host.textures.lock().unwrap().remove(1);
        let next = host.sweep_part(0..136, 0, handle, 51, true, &mut counts);
        let all_due = || host.texture_slots_due(u64::MAX, 136).0;
        let first = all_due();
        assert_eq!(first.len(), 128);
        assert!(!first.iter().any(|d| d.0 == 135));
        for (slot, _, _) in first {
            host.textures.lock().unwrap().remove(slot);
        }
        host.sweep_part(0..136, next, handle, 52, true, &mut counts);
        let second = all_due();
        assert!(second.iter().any(|d| d.0 == 135));
        assert!(!second.iter().any(|d| d.0 == 2));
    }

    #[test]
    fn expired_alias_recovers_only_unchanged_mapped_texture_views() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return;
        };
        static PIXELS: [u8; 1 << 20] = [0; 1 << 20];
        let address = PIXELS.as_ptr() as u64;
        let resource = [3, 0, 0, 0, 256, 0, 256, 0x9_0001, 28, 1, 0, 0x105, 0, 0];
        let visible = crate::resource::PlacedTextures::placed_bytes_from(&resource, 2).unwrap();
        assert!(visible < crate::resource::PlacedTextures::placed_bytes(&resource).unwrap());
        let record = |address: u64, width: u32, first: u32| {
            let mut shape = resource;
            shape[4] = width;
            let mut view = [0; 16];
            view[..6].copy_from_slice(&[
                28,
                4,
                D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                0,
                first,
                1,
            ]);
            (view[10], view[11]) = (address as u32, (address >> 32) as u32);
            let [i, generation] = super::super::Views::words_for(View::Placed(false, shape, view));
            [VIEW_TAG, i, generation]
        };
        let words = [const { std::cell::Cell::new([0u32; 3]) }; 8];
        let handle = |i: u32| words[i as usize].as_ptr() as u64;
        let recover = |frame| heap.recover_displaced(|i| Some(handle(i)), frame);
        let old = record(address, 256, 2);
        for i in 1..=3 {
            words[i].set(old);
            heap.mirror(i as u32, handle(i as u32), true).unwrap();
        }
        assert!(heap.kept.lock().unwrap()[1..=3].iter().all(Option::is_some));
        let blocker = record(address, 512, 0);
        for i in 4..=5 {
            words[i].set(blocker);
            heap.mirror(i as u32, handle(i as u32), false).unwrap();
        }
        assert!(heap.kept.lock().unwrap()[1..=3].iter().all(Option::is_none));
        assert_eq!(recover(40), 0);
        words[3].set(record(address + visible, 128, 0));
        words[4].set([0; 3]);
        assert!(heap.mirror(4, handle(4), false).is_none());
        assert_eq!(recover(41), 0);
        words[6].set(words[3].get());
        heap.mirror(6, handle(6), false).unwrap();
        words[5].set([0; 3]);
        assert!(heap.mirror(5, handle(5), false).is_none());
        assert_eq!(recover(42), 2);
        let queued = |i| heap.textures.lock().unwrap().slots.contains_key(&i);
        assert!(queued(1) && queued(2) && !queued(3));
        let stale_write = heap.textures.lock().unwrap().slots[&1].write;
        words[4].set(blocker);
        heap.mirror(4, handle(4), false).unwrap();
        assert!(!queued(1) && !queued(2));
        assert_eq!(recover(43), 0);
        for resident_only in [false, true] {
            assert!(
                heap.mirror_queued(1, handle(1), stale_write, resident_only)
                    .unwrap()
                    .0
                    .is_none()
            );
        }
        assert!(heap.kept.lock().unwrap()[1].is_none());
        heap.queue_texture(1, true, false, 43);
        let premature = heap.textures.lock().unwrap().slots[&1].write;
        assert!(
            heap.mirror_queued(1, handle(1), premature, false)
                .unwrap()
                .0
                .is_none()
        );
        heap.texture_slot_done(1, premature, None);
        words[4].set([0; 3]);
        assert!(heap.mirror(4, handle(4), false).is_none());
        assert_eq!(recover(44), 2);
        for i in 1..=2 {
            let write = heap.textures.lock().unwrap().slots[&i].write;
            assert!(
                heap.mirror_queued(i, handle(i), write, false)
                    .unwrap()
                    .0
                    .is_some()
            );
        }
        assert!(!heap.texture_slot_gone(1, stale_write));
        assert_eq!(recover(45), 0);
        assert!(heap.placed.lock().unwrap().displaced.is_empty());
        assert!(heap.kept.lock().unwrap()[1..=2].iter().all(Option::is_some));
        assert!(HeapDiagnostics::device_removed().is_none());
    }

    #[test]
    fn remapped_memory_makes_its_texture_slots_due() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return;
        };
        let slots = |at: u64| -> Vec<u32> {
            heap.texture_slots_due(at, 8)
                .0
                .into_iter()
                .map(|d| d.0)
                .collect()
        };
        let at = 0x7fee_0000_0000u64;
        let view = |address: u64, width: u32| {
            let mut desc = [0u32; 16];
            (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
            View::Placed(
                false,
                [3, 0, 0, 0, width, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0],
                desc,
            )
        };
        let len = crate::resource::PlacedTextures::placed_bytes(&[
            3, 0, 0, 0, 64, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0,
        ])
        .unwrap();
        assert!(heap.note_buffer(1, Some(&view(at, 64)), false).is_empty());
        let (queued, write, _) = heap.texture_slots_due(u64::MAX, 8).0[0];
        assert_eq!(queued, 1);
        heap.texture_slot_done(1, write, None);
        assert_eq!(slots(u64::MAX), []);
        RemapLog::remapped((at + 2 * len) as usize, 0x1000);
        RemapLog::remapped(at as usize, 0x1000);
        assert_eq!(heap.texture_slots_remapped(50), 1);
        assert_eq!(slots(50), []);
        assert_eq!(slots(51), [1]);
        assert_eq!(heap.note_buffer(2, Some(&view(at, 128)), false), [1]);
        assert_eq!(slots(u64::MAX), [2]);
    }

    #[test]
    fn slots_read_through_tables_leave_the_texture_queue() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return;
        };
        let frame = 100;
        let slots = |at: u64| -> Vec<u32> {
            heap.texture_slots_due(at, 8)
                .0
                .into_iter()
                .map(|d| d.0)
                .collect()
        };
        let texture = View::Placed(
            false,
            [3, 0, 0, 0, 4, 0, 4, 0x1_0001, 28, 1, 0, 0x105, 0, 0],
            [0; 16],
        );
        assert!(BufferViews::is_texture_view(&texture));
        heap.queue_texture(2, true, false, frame);
        heap.queue_texture(3, true, false, frame);
        assert_eq!(slots(frame), [3, 2]);
        heap.tabled_in(3, true, frame);
        assert_eq!(slots(frame), [2]);
        heap.queue_texture(3, true, true, frame);
        assert_eq!(slots(frame), [2]);
        heap.queue_texture(3, true, false, frame + 1);
        assert_eq!(slots(frame + TABLED_GRACE), [2]);
        assert_eq!(slots(frame + 1 + TABLED_GRACE), [3, 2]);
        heap.tabled_in(3, true, frame + 1);
        assert_eq!(slots(u64::MAX), [2]);
        heap.tabled_in(2, false, frame + 1);
        assert_eq!(slots(frame + 1), [2]);
        heap.queue_texture(2, true, false, frame + 2);
        assert_eq!(slots(frame + 2), []);
        heap.queue_texture(2, true, false, frame + 3 + TABLED_FRAMES);
        assert_eq!(slots(frame + 3 + TABLED_FRAMES), [2]);
    }

    #[test]
    fn recreated_heap_reserves_its_own_raw_page_views() {
        let Some(mut heap) = HostHeap::new(false, 4) else {
            return;
        };
        assert_eq!(heap.raw_page_views(&[]), Some((4, 0)));
        assert_eq!(heap.raw_page_views(&[]), Some((4, 0)));
        assert_eq!(heap.dynamic(1), Some(4 + super::FIRST_RAW_PAGE_SLOTS));
        heap = HostHeap::new(false, 4).unwrap();
        assert_eq!(heap.dynamic(7), Some(4));
        assert_eq!(heap.raw_page_views(&[]), Some((11, 0)));
        assert_eq!(heap.dynamic(1), Some(11 + super::FIRST_RAW_PAGE_SLOTS));
    }

    #[test]
    fn dynamic_slots_are_held_until_released() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let a = heap.dynamic(heap.dynamic - 100).unwrap();
        let b = heap.dynamic(60).unwrap();
        assert_eq!((a, b), (4, 4 + heap.dynamic - 100));
        assert_eq!(heap.dynamic(50), None);
        heap.release(a, 50, true);
        assert_eq!(heap.dynamic(50), Some(4));
        assert_eq!(heap.dynamic(40), Some(4 + heap.dynamic - 40));
    }

    #[test]
    fn discarded_dynamic_ranges_unpin_only_their_own_resources() {
        let Some(host) = Host::instance() else { return };
        let heap = HostHeap::new(false, 4).unwrap();
        let (buffer, _) = host.upload_buffer(256).unwrap();
        heap.set_texture_pressure(true);
        let first = heap.dynamic(3).unwrap();
        for index in [1, first, first + 1, first + 2] {
            heap.raw_at(index, &buffer, 0, 256);
        }
        heap.release(first, 2, true);
        heap.release(first + 2, 1, false);
        let kept = heap.kept.lock().unwrap();
        assert!(kept[1].is_some());
        assert!(kept[first as usize].is_none() && kept[first as usize + 1].is_none());
        assert!(kept[first as usize + 2].is_some());
    }

    #[test]
    fn changed_mip_views_can_use_resident_textures_under_pressure() {
        if crate::host::Host::instance().is_none() {
            return;
        }
        static GUEST_BYTES: [u8; 65536] = [0; 65536];
        let address = GUEST_BYTES.as_ptr() as u64;
        let resource = [3, 0, 0, 0, 32, 0, 32, 0x3_0001, 28, 1, 0, 0x105, 0, 0];
        let mut desc = [0; 16];
        desc[0] = 28;
        desc[1] = 4;
        desc[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        desc[5] = 1;
        desc[10] = address as u32;
        desc[11] = (address >> 32) as u32;
        assert!(
            ViewMaker::resident_sampled_texture(&View::Placed(false, resource, desc)).is_none()
        );
        assert!(crate::resource::PlacedTextures::placed_known(address, &resource).is_none());
        let texture = crate::resource::PlacedTextures::texture_at(address, &resource).unwrap();
        let heap = HostHeap::new(false, 4).unwrap();
        let record = |view: View| {
            let [index, generation] = super::super::Views::words_for(view);
            [VIEW_TAG, index, generation, 0]
        };
        let original = crate::resource::HostTextures::host_copy(texture).unwrap();
        desc[4] = 1;
        let placed =
            ViewMaker::resident_sampled_texture(&View::Placed(false, resource, desc)).unwrap();
        let object =
            ViewMaker::resident_sampled_texture(&View::Shader(texture as usize, None)).unwrap();
        assert!(std::sync::Arc::ptr_eq(&original, &placed._gpu));
        assert!(std::sync::Arc::ptr_eq(&original, &object._gpu));
        let view = View::Placed(false, resource, desc);
        let words = record(view.clone());
        heap.queue_texture(2, true, false, 1);
        let write = heap.textures.lock().unwrap().slots[&2].write;
        let (mirrored, held) = heap
            .mirror_queued(2, words.as_ptr() as u64, write, true)
            .unwrap();
        assert_eq!(mirrored, Some(view));
        assert!(std::sync::Arc::ptr_eq(&original, &held.unwrap()._gpu));
        let mut other = resource;
        other[4] *= 2;
        let unavailable = record(View::Placed(false, other, desc));
        assert!(
            heap.mirror_queued(2, unavailable.as_ptr() as u64, write, true)
                .is_err()
        );
        assert!(crate::resource::PlacedTextures::placed_known(address, &other).is_none());
        assert!(HeapDiagnostics::device_removed().is_none());
    }

    #[test]
    fn a_26_slot_table_can_grow_into_the_unused_host_heap() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        assert!(heap.dynamic > DYNAMIC + 26);
        assert_eq!(heap.dynamic(DYNAMIC - 12), Some(4));
        assert_eq!(heap.dynamic(26), Some(4 + DYNAMIC - 12));
        assert_eq!(heap.dynamic(1), Some(4 + DYNAMIC + 14));
        heap.release(4 + DYNAMIC - 12, 26, true);
        heap.release(4, DYNAMIC - 12, true);
        heap.release(4 + DYNAMIC + 14, 1, true);
        assert_eq!(heap.dynamic(heap.dynamic), Some(4));
        assert!(heap.dynamic(1).is_none());
    }

    #[test]
    fn dynamic_allocation_finds_contiguous_range_crossing_search_cursor() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        assert_eq!(heap.dynamic(heap.dynamic), Some(4));
        heap.release(4 + 10, 20, true);
        assert_eq!(heap.dynamic(10), Some(4 + 10));
        heap.release(4 + 10, 10, true);
        assert_eq!(heap.dynamic(15), Some(4 + 10));
    }
}
