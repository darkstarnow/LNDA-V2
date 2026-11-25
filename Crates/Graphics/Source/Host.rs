use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCWSTR, w};

const RTV_RING: u32 = 1024;

pub(crate) struct Host {
    pub(crate) device: ID3D12Device,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    submitted: Mutex<u64>,
    event: HANDLE,
    rtvs: ID3D12DescriptorHeap,
    rtv_size: u32,
    dsvs: ID3D12DescriptorHeap,
    dsv_size: u32,
    next_dsv: AtomicU32,
    messages: Option<ID3D12InfoQueue>,
    next_rtv: AtomicU32,
    presenter: Mutex<Option<Presenter>>,
    adapter: Option<IDXGIAdapter3>,
}

unsafe impl Send for Host {}
unsafe impl Sync for Host {}

static RETIRED: Mutex<Vec<(u64, Retired)>> = Mutex::new(Vec::new());
static STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FINISHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct Retired(#[allow(dead_code)] Box<dyn std::any::Any>);

pub(crate) struct StateBits;

pub(crate) struct HostClock;

pub(crate) struct Retirement;

pub(crate) struct HostTrail;

pub struct HostSetup;

pub(crate) struct Preludes;

pub(crate) struct ScratchPool;

pub(crate) struct LateRun;

pub(crate) struct FillWork;

pub(crate) struct TitleCopy;

pub(crate) struct HostWindow;

impl StateBits {
    pub(crate) fn pc_state(state: u32) -> u32 {
        state & 0x01ff_ffff
    }

    pub(crate) fn texture_state(state: u32) -> u32 {
        const BUFFER_ONLY: u32 = 0x1 | 0x2 | 0x100 | 0x200;
        StateBits::pc_state(state) & !BUFFER_ONLY
    }

    pub(crate) fn legal_state(texture: &ID3D12Resource, state: u32) -> u32 {
        StateBits::legal_for(unsafe { texture.GetDesc() }.Flags.0 as u32, state)
    }

    pub(crate) fn legal_for(flags: u32, state: u32) -> u32 {
        const FORBIDDEN: [(u32, bool, u32); 4] = [
            (0x1, false, 0x4),
            (0x4, false, 0x8),
            (0x2, false, 0x10 | 0x20),
            (0x8, true, 0x40 | 0x80),
        ];
        FORBIDDEN
            .iter()
            .filter(|&&(flag, deny, _)| (flags & flag != 0) == deny)
            .fold(state, |s, &(_, _, bits)| s & !bits)
    }

    pub(crate) fn transition(
        resource: &ID3D12Resource,
        before: D3D12_RESOURCE_STATES,
        after: D3D12_RESOURCE_STATES,
    ) -> D3D12_RESOURCE_BARRIER {
        D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: unsafe { std::mem::transmute_copy(resource) },
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: before,
                    StateAfter: after,
                }),
            },
        }
    }
}

impl HostClock {
    pub(crate) fn now_ms() -> u64 {
        static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
        EPOCH
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis() as u64
    }

    pub(crate) fn moment() -> u64 {
        (crate::frame::FramePacer::frame() << 32) | ((HostClock::now_ms() / 100) & 0xffff_ffff)
    }

    pub(crate) fn long_ago(moment: u64, ms: u64, frames: u64) -> bool {
        let now = HostClock::moment();
        let tenths = (now & 0xffff_ffff).saturating_sub(moment & 0xffff_ffff);
        (now >> 32).saturating_sub(moment >> 32) >= frames && tenths * 100 >= ms
    }

    pub(crate) fn available_physical_ram() -> Option<u64> {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
        ok.then_some(m.ullAvailPhys)
    }

    pub(crate) fn vram_idle_ms(video_memory: Option<[u64; 4]>) -> u64 {
        const NORMAL_MS: u64 = 5_000;
        const MODERATE_MS: u64 = 2_500;
        const HIGH_MS: u64 = 1_000;
        let Some([budget, used, ..]) = video_memory else {
            return NORMAL_MS;
        };
        if budget == 0 {
            return NORMAL_MS;
        }
        let used_pct = (u128::from(used) * 100) / u128::from(budget);
        if used_pct >= 85 {
            HIGH_MS
        } else if used_pct >= 75 {
            MODERATE_MS
        } else {
            NORMAL_MS
        }
    }

    pub(crate) fn ram_idle_ms(avail_phys: Option<u64>) -> u64 {
        const GIB: u64 = 1024 * 1024 * 1024;
        const NORMAL_MS: u64 = 10_000;
        const MODERATE_MS: u64 = 5_000;
        const HIGH_MS: u64 = 2_000;
        let Some(avail) = avail_phys else {
            return NORMAL_MS;
        };
        if avail < 2 * GIB {
            HIGH_MS
        } else if avail < 4 * GIB {
            MODERATE_MS
        } else {
            NORMAL_MS
        }
    }

    pub(crate) fn now_us() -> u64 {
        static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
        EPOCH
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_micros() as u64
    }

    pub(crate) fn thread_id() -> u32 {
        unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
    }

    #[cfg(feature = "diagnostics")]
    fn private_bytes() -> u64 {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut c = PROCESS_MEMORY_COUNTERS {
            cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        c.PagefileUsage as u64
    }

    #[cfg(windows)]
    fn process_cpu_us() -> Option<u64> {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let (mut creation, mut exit, mut kernel, mut user) = unsafe {
            (
                std::mem::zeroed::<FILETIME>(),
                std::mem::zeroed::<FILETIME>(),
                std::mem::zeroed::<FILETIME>(),
                std::mem::zeroed::<FILETIME>(),
            )
        };
        let ok = unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        };
        if ok == 0 {
            return None;
        }
        let ticks = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
        Some((ticks(kernel) + ticks(user)) / 10)
    }

    #[cfg(windows)]
    fn process_working_set() -> Option<u64> {
        use windows_sys::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut counters = unsafe { std::mem::zeroed::<PROCESS_MEMORY_COUNTERS>() };
        let ok = unsafe {
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut counters,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            )
        };
        (ok != 0).then_some(counters.WorkingSetSize as u64)
    }

    #[cfg(windows)]
    fn logical_processors() -> u32 {
        use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
        let mut info = unsafe { std::mem::zeroed::<SYSTEM_INFO>() };
        unsafe { GetSystemInfo(&mut info) };
        info.dwNumberOfProcessors.max(1)
    }

    #[cfg(windows)]
    fn process_age() -> Option<Duration> {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
        let mut times = unsafe { std::mem::zeroed::<[FILETIME; 4]>() };
        let [creation, exit, kernel, user] = &mut times;
        if unsafe { GetProcessTimes(GetCurrentProcess(), creation, exit, kernel, user) } == 0 {
            return None;
        }
        let ticks = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
        let created = Duration::from_nanos(ticks.checked_sub(UNIX_EPOCH_TICKS)?.checked_mul(100)?);
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .checked_sub(created)
    }

    fn process_uptime() -> Duration {
        static FIRST: std::sync::OnceLock<(Instant, Duration)> = std::sync::OnceLock::new();
        let (at, age) =
            *FIRST.get_or_init(|| (Instant::now(), HostClock::process_age().unwrap_or_default()));
        age + at.elapsed()
    }
}

impl Retirement {
    pub(crate) fn references(object: &impl Interface) -> u32 {
        type Method = unsafe extern "system" fn(*mut std::ffi::c_void) -> u32;
        let raw = object.as_raw();
        unsafe {
            let methods = *raw.cast::<*const [Method; 3]>();
            ((*methods)[1])(raw);
            ((*methods)[2])(raw)
        }
    }

    fn sweep() {
        crate::inplace::InPlaceLevel::tick();
        static ON: OnceLock<bool> = OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("LEONIDA_SWEEP").is_none_or(|v| v != "0")) {
            return;
        }
        static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = HostClock::now_ms();
        let last = LAST.load(Ordering::Relaxed);
        if now < last + 1000
            || LAST
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        crate::pipeline::PipelineCache::trim();
        crate::shadow::ShadowMemory::let_go_idle();
        crate::texture::TextureUploads::let_go_idle();
        crate::resource::HostTextures::let_go_idle();
    }

    pub(crate) fn retire<T: 'static>(object: T) {
        let started = STARTED.load(Ordering::Acquire);
        if started <= FINISHED.load(Ordering::Acquire) {
            RETIREMENTS[0].fetch_add(1, Ordering::Relaxed);
            drop(object);
            return;
        }
        RETIREMENTS[1].fetch_add(1, Ordering::Relaxed);
        RETIRED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((started, Retired(Box::new(object))));
    }

    fn bury(finished: u64) {
        let done: Vec<(u64, Retired)> = {
            let mut retired = RETIRED.lock().unwrap_or_else(|e| e.into_inner());
            if retired.is_empty() {
                return;
            }
            let (done, keep) = std::mem::take(&mut *retired)
                .into_iter()
                .partition(|(at, _)| *at <= finished);
            *retired = keep;
            done
        };
        drop(done);
    }
}

impl HostTrail {
    fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_TRAIL").is_none_or(|v| v != "0"))
    }

    pub(crate) fn trail(what: impl FnOnce() -> String) {
        if !HostTrail::enabled() {
            return;
        }
        let entry = (HostClock::now_us(), HostClock::thread_id(), what());
        let mut trail = TRAIL.lock().unwrap_or_else(|e| e.into_inner());
        if trail.len() >= TRAIL_MOST {
            trail.pop_front();
        }
        trail.push_back(entry);
    }

    fn submitting(phase: u64, lists: usize) {
        SUBMITTING[1].store(lists as u64, Ordering::Relaxed);
        SUBMITTING[2].store(HostClock::now_us(), Ordering::Relaxed);
        SUBMITTING[0].store(phase, Ordering::Relaxed);
    }

    pub(crate) fn removal_context() -> String {
        use std::fmt::Write;
        let now = HostClock::now_us();
        let ms = |at: u64| now.saturating_sub(at) as f64 / 1000.0;
        let phase = match SUBMITTING[0].load(Ordering::Relaxed) {
            0 => "between submissions (nothing of ours was running on the card)",
            1 => "preparing a submission (nothing of ours was running on the card)",
            2 => "waiting for the card to run a submission",
            _ => "copying back what a submission wrote (nothing of ours was running on the card)",
        };
        let mut text = format!(
            "the submitting thread was {phase}, of {} lists, for {:.1} ms; {}; objects let go at once {} and after a running submission {}; submissions started {} finished {}",
            SUBMITTING[1].load(Ordering::Relaxed),
            ms(SUBMITTING[2].load(Ordering::Relaxed)),
            crate::pipeline::PipelineCache::removal_note(),
            RETIREMENTS[0].load(Ordering::Relaxed),
            RETIREMENTS[1].load(Ordering::Relaxed),
            STARTED.load(Ordering::Relaxed),
            FINISHED.load(Ordering::Relaxed),
        );
        let trail = TRAIL.lock().unwrap_or_else(|e| e.into_inner());
        let _ = write!(text, "; the last {} things done, newest last:", trail.len());
        for (at, thread, what) in trail.iter() {
            let _ = write!(
                text,
                "\n  {:.1} ms before, thread {thread}: {what}",
                ms(*at)
            );
        }
        text
    }

    pub(crate) fn log_removal_context() {
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !HostTrail::enabled() || SAID.swap(true, Ordering::Relaxed) {
            return;
        }
        tracing::error!(
            "when the host device went: {}",
            HostTrail::removal_context()
        );
    }

    fn working(on: bool) -> bool {
        let now = HostClock::now_us() + 1;
        let since = WORKING[1].swap(if on { now } else { 0 }, Ordering::Relaxed);
        if since != 0 {
            WORKING[0].fetch_add(now.saturating_sub(since), Ordering::Relaxed);
        }
        since != 0
    }

    fn worked() -> u64 {
        let since = WORKING[1].load(Ordering::Relaxed);
        let now = if since != 0 {
            (HostClock::now_us() + 1).saturating_sub(since)
        } else {
            0
        };
        WORKING[0].load(Ordering::Relaxed) + now
    }

    fn stood(since: std::time::Instant) {
        CARD_CLOCK_US[2].fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
    }

    pub(crate) fn walls_trace() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            std::env::var_os("LEONIDA_WALLS_TRACE").is_some_and(|v| v == "1" || v == "2")
        })
    }

    fn last_notes(runs: &[&HostList]) -> String {
        #[cfg(feature = "diagnostics")]
        {
            let mut text = String::new();
            for (i, r) in runs.iter().enumerate() {
                let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
                for (n, _) in notes.iter().rev().take(6).rev() {
                    text.push_str(&format!(
                        "; list {i} of {}, of its {}: {n}",
                        runs.len(),
                        notes.len()
                    ));
                }
            }
            text
        }
        #[cfg(not(feature = "diagnostics"))]
        {
            let _ = runs;
            String::new()
        }
    }

    #[cfg(feature = "diagnostics")]
    fn fault_in(va: u64, (start, bytes): (u64, u64)) -> String {
        let end = start + bytes;
        if va < start {
            format!(" (fault {:#x} below its source)", start - va)
        } else if va < end {
            format!(" (fault at source +{:#x})", va - start)
        } else {
            format!(" (fault {:#x} past its source's end)", va - end)
        }
    }

    fn in_flight(
        history: &[D3D12_AUTO_BREADCRUMB_OP],
        markers: &[(u32, String)],
        done: usize,
    ) -> String {
        let barrier =
            |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER;
        let from = history[..done]
            .iter()
            .rposition(barrier)
            .map_or(0, |b| b + 1);
        let to = history[done..]
            .iter()
            .position(barrier)
            .map_or(history.len(), |b| (done + b).max(done + 1));
        let mut text = format!(", commands {from}..{to} between its barriers:");
        let mut shown = 0;
        for i in from.max(done.saturating_sub(4))..to {
            let op = history[i];
            if op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER || shown == 12 {
                continue;
            }
            shown += 1;
            let marker = i
                .checked_sub(1)
                .filter(|&m| history[m] == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
                .and_then(|m| markers.iter().find(|(at, _)| *at as usize == m))
                .map_or("no marker", |(_, s)| s.as_str());
            text.push_str(&format!("; command {i} (op {}): {marker}", op.0));
        }
        text
    }
}

impl CardClock {
    fn watch(clock: &'static CardClock) {
        let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
            return;
        };
        let mut seen = 0;
        loop {
            let target = {
                let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
                while state.0 == seen {
                    state = clock.sent.wait(state).unwrap_or_else(|e| e.into_inner());
                }
                state.0
            };
            unsafe {
                if clock.fence.SetEventOnCompletion(target, event).is_ok() {
                    WaitForSingleObject(event, INFINITE);
                }
            }
            seen = target;
            let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.0 == target && state.1.is_none() {
                state.1 = Some((std::time::Instant::now(), HostTrail::worked()));
            }
        }
    }

    fn line(frames: f64, ms: f64) {
        let [idle, behind, stood] =
            [0, 1, 2].map(|i| CARD_CLOCK_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
        tracing::info!(
            "the card's clock per frame: nothing of ours queued {idle:.1} ms (the card about {:.0}% busy by this count), \
             {behind:.1} ms of that while the submitting thread was HostTrail::working (the card idle behind us), the rest while the title \
             had not submitted; the submitting thread stood waiting for the card {stood:.1} ms",
            if ms > 0.0 {
                (100.0 * (1.0 - idle / ms)).clamp(0.0, 100.0)
            } else {
                0.0
            }
        );
    }
}

impl HostSetup {
    pub fn on_failure(report: fn(&str) -> !) {
        let _ = ON_FAILURE.set(report);
    }

    pub(crate) fn fail(what: std::fmt::Arguments) -> ! {
        let mut text = what.to_string();
        if let Some(host) = Host::instance() {
            if let Err(e) = unsafe { host.device.GetDeviceRemovedReason() } {
                text.push_str(&format!("; device removed: {e}"));
                HostTrail::log_removal_context();
            }
            match host.log_messages() {
                Some(first) => text.push_str(&format!("; debug layer: {first}")),
                None if host.messages.is_none() => text
                    .push_str("; a diagnostics build with LEONIDA_D3D12_DEBUG=1 names the command"),
                None => {}
            }
        }
        tracing::error!("{text}");
        #[cfg(test)]
        eprintln!("host GPU failure: {text}");
        match ON_FAILURE.get() {
            Some(report) => report(&text),
            None => std::process::abort(),
        }
    }

    fn create() -> windows::core::Result<Host> {
        unsafe {
            let debug =
                cfg!(feature = "diagnostics") && std::env::var_os("LEONIDA_D3D12_DEBUG").is_some();
            if debug {
                let mut layer: Option<ID3D12Debug> = None;
                match D3D12GetDebugInterface(&mut layer) {
                    Ok(()) => layer.iter().for_each(|l| l.EnableDebugLayer()),
                    Err(e) => tracing::warn!(error = %e, "no D3D12 debug layer"),
                }
                if std::env::var_os("LEONIDA_D3D12_GBV").is_some()
                    && let Some(l) = &layer
                {
                    if let Ok(d1) = l.cast::<ID3D12Debug1>() {
                        d1.SetEnableGPUBasedValidation(true);
                    }
                    if let Ok(d2) = l.cast::<ID3D12Debug2>() {
                        d2.SetGPUBasedValidationFlags(
                            D3D12_GPU_BASED_VALIDATION_FLAGS_DISABLE_STATE_TRACKING,
                        );
                    }
                    tracing::info!("GPU-based validation on");
                }
            }
            if HostSetup::dred_on() {
                let mut dred: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
                if D3D12GetDebugInterface(&mut dred).is_ok()
                    && let Some(dred) = dred
                {
                    dred.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                    dred.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                }
            }
            let mut device: Option<ID3D12Device> = None;
            if HostSetup::warp() {
                let adapter = HostSetup::warp_adapter()?;
                D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
                tracing::info!("host device on the software adapter (WARP)");
            } else {
                D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device)?;
            }
            let device = device.ok_or_else(windows::core::Error::empty)?;
            if std::env::var_os("LEONIDA_D3D12_BACKGROUND").is_some_and(|v| v == "0")
                && let Ok(device6) = device.cast::<ID3D12Device6>()
            {
                let set = device6.SetBackgroundProcessingMode(
                    D3D12_BACKGROUND_PROCESSING_MODE_DISABLE_BACKGROUND_WORK,
                    D3D12_MEASUREMENTS_ACTION_KEEP_ALL,
                    None,
                    None,
                );
                tracing::info!(
                    ok = set.is_ok(),
                    "the host's background shader work is switched off (LEONIDA_D3D12_BACKGROUND=0)"
                );
            }
            let mut o1 = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
            if device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS1,
                    (&raw mut o1).cast(),
                    size_of_val(&o1) as u32,
                )
                .is_ok()
            {
                tracing::info!(
                    min = o1.WaveLaneCountMin,
                    max = o1.WaveLaneCountMax,
                    "host wave lanes"
                );
                leonida_shader::emit::set_host_max_wave(o1.WaveLaneCountMax);
            }
            let messages = debug
                .then(|| device.cast::<ID3D12InfoQueue>().ok())
                .flatten();
            let queue: ID3D12CommandQueue =
                device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let event = CreateEventW(None, false, false, None)?;
            let rtvs: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: RTV_RING,
                    ..Default::default()
                })?;
            let rtv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
            let dsvs: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                    NumDescriptors: RTV_RING,
                    ..Default::default()
                })?;
            let dsv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV);
            let adapter = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
                .and_then(|factory| {
                    factory.EnumAdapterByLuid::<IDXGIAdapter3>(device.GetAdapterLuid())
                })
                .ok();
            tracing::info!("host D3D12 device ready");
            Ok(Host {
                device,
                queue,
                fence,
                submitted: Mutex::new(0),
                event,
                rtvs,
                rtv_size,
                dsvs,
                dsv_size,
                next_dsv: AtomicU32::new(0),
                messages,
                next_rtv: AtomicU32::new(0),
                presenter: Mutex::new(None),
                adapter,
            })
        }
    }

    pub(crate) fn warp() -> bool {
        cfg!(feature = "diagnostics") && std::env::var_os("LEONIDA_D3D12_WARP").is_some()
    }

    fn warp_adapter() -> windows::core::Result<IDXGIAdapter> {
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let adapter: IDXGIAdapter = factory.EnumWarpAdapter()?;
            let desc = adapter.cast::<IDXGIAdapter1>()?.GetDesc1()?;
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                tracing::warn!(
                    vendor = format_args!("{:#x}", desc.VendorId),
                    flags = format_args!("{:#x}", desc.Flags),
                    "DXGI did not identify the WARP adapter as software; refusing device"
                );
                return Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_FAIL,
                ));
            }
            Ok(adapter)
        }
    }

    pub(crate) fn dred_on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            cfg!(feature = "diagnostics") && std::env::var_os("LEONIDA_D3D12_DRED").is_some()
        })
    }

    #[cfg(feature = "diagnostics")]
    fn timing() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_GPU_TIMING").is_some())
    }

    pub(crate) fn name(object: &impl Interface, text: impl FnOnce() -> String) {
        if HostSetup::dred_on()
            && let Ok(object) = object.cast::<ID3D12Object>()
        {
            let _ = unsafe { object.SetName(&windows::core::HSTRING::from(text())) };
        }
    }
}

impl Host {
    pub(crate) fn instance() -> Option<&'static Host> {
        static HOST: OnceLock<Option<Host>> = OnceLock::new();
        HOST.get_or_init(|| match HostSetup::create() {
            Ok(host) => Some(host),
            Err(e) => {
                tracing::warn!(error = %e, "no host D3D12 device; command lists will not run");
                None
            }
        })
        .as_ref()
    }
}

impl Preludes {
    fn record_transitions(list: &ID3D12GraphicsCommandList, transitions: &[PreludeTransition]) {
        if transitions.is_empty() {
            return;
        }
        let barriers: Vec<_> = transitions
            .iter()
            .map(|PreludeTransition(texture, from, to)| {
                StateBits::transition(
                    texture,
                    D3D12_RESOURCE_STATES(*from as i32),
                    D3D12_RESOURCE_STATES(*to as i32),
                )
            })
            .collect();
        unsafe { list.ResourceBarrier(&barriers) };
    }

    fn record_preludes(plans: Vec<PlannedPrelude>) {
        let barrier_count: usize = plans
            .iter()
            .map(|p| p.to_copy.len() + p.barriers.len())
            .sum();
        let includes_copies = plans.iter().any(|p| !p.syncs.is_empty());
        if plans.len() < 4 || (barrier_count < 256 && !includes_copies) {
            for plan in plans {
                plan.record();
            }
            return;
        }
        #[cfg(feature = "diagnostics")]
        {
            static FIRST: std::sync::Once = std::sync::Once::new();
            FIRST.call_once(|| {
                tracing::info!(
                    lists = plans.len(),
                    "recording submission preludes on CPU workers"
                )
            });
        }
        let jobs = plans
            .into_iter()
            .map(|plan| Box::new(move || plan.record()) as Box<dyn FnOnce() + Send>)
            .collect();
        crate::shadow::WideCopy::run(jobs);
    }

    fn parallel_preludes(runs: &[&HostList]) -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        if runs.len() < 4
            || !*ON.get_or_init(|| {
                std::env::var_os("LEONIDA_PARALLEL_PRELUDES").is_some_and(|v| v == "1")
            })
        {
            return false;
        }
        let mut unique = std::collections::HashSet::with_capacity(runs.len());
        runs.iter()
            .all(|list| unique.insert(std::ptr::from_ref(*list)))
    }

    fn lazy_prelude() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_LAZY_PRELUDE").is_none_or(|v| v != "0"))
    }
}

impl PlanePass {
    fn get() -> Option<&'static PlanePass> {
        static PASS: OnceLock<Option<PlanePass>> = OnceLock::new();
        PASS.get_or_init(|| {
            let host = Host::instance()?;
            let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
            unsafe {
                let views: ID3D12DescriptorHeap = host
                    .device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: PLANE_VIEWS,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
                    .ok()?;
                Some(PlanePass {
                    cpu: views.GetCPUDescriptorHandleForHeapStart().ptr,
                    gpu: views.GetGPUDescriptorHandleForHeapStart().ptr,
                    step: host.device.GetDescriptorHandleIncrementSize(kind),
                    views,
                    next: AtomicU32::new(0),
                    made: Mutex::default(),
                })
            }
        })
        .as_ref()
    }

    pub(crate) fn compile(
        source: &str,
        entry: windows::core::PCSTR,
        target: windows::core::PCSTR,
    ) -> windows::core::Result<Vec<u8>> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        let compiled = unsafe {
            D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                windows::core::s!("leonida"),
                None,
                None,
                entry,
                target,
                0,
                0,
                &mut code,
                Some(&mut errors),
            )
        };
        if let Err(e) = compiled {
            if let Some(errors) = errors {
                let text = unsafe {
                    std::slice::from_raw_parts(
                        errors.GetBufferPointer().cast::<u8>(),
                        errors.GetBufferSize(),
                    )
                };
                tracing::warn!("a shader of ours: {}", String::from_utf8_lossy(text));
            }
            return Err(e);
        }
        let code = code.ok_or_else(windows::core::Error::empty)?;
        let bytes = unsafe {
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
        };
        Ok(bytes.to_vec())
    }

    fn record_plane_pass(
        list: &ID3D12GraphicsCommandList,
        depth: &ID3D12Resource,
        plane: &ID3D12Resource,
    ) {
        let (Some(host), Some(pass)) = (Host::instance(), PlanePass::get()) else {
            return;
        };
        let (of, twin) = unsafe { (depth.GetDesc(), plane.GetDesc()) };
        let steps = twin.Format.0 == 53;
        let format = DXGI_FORMAT(match of.Format.0 {
            44 => 45,
            19 => 20,
            typed => typed,
        });
        let Some((root, pipeline)) = pass.pipeline(steps, format) else {
            return;
        };
        for slice in 0..u32::from(of.DepthOrArraySize) {
            for mip in 0..u32::from(of.MipLevels) {
                let slot = pass.next.fetch_add(1, Ordering::Relaxed) % PLANE_VIEWS;
                let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT(if steps { 57 } else { 41 }),
                    ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                            MostDetailedMip: mip,
                            MipLevels: 1,
                            FirstArraySlice: slice,
                            ArraySize: 1,
                            ..Default::default()
                        },
                    },
                };
                let target = D3D12_DEPTH_STENCIL_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2DARRAY,
                    Anonymous: D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture2DArray: D3D12_TEX2D_ARRAY_DSV {
                            MipSlice: mip,
                            FirstArraySlice: slice,
                            ArraySize: 1,
                        },
                    },
                    ..Default::default()
                };
                let target = host.dsv(depth, Some(&target));
                let (width, height) = ((of.Width >> mip).max(1) as u32, (of.Height >> mip).max(1));
                let cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: pass.cpu + (slot * pass.step) as usize,
                };
                let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
                    ptr: pass.gpu + u64::from(slot * pass.step),
                };
                unsafe {
                    host.device
                        .CreateShaderResourceView(plane, Some(&view), cpu);
                    list.SetDescriptorHeaps(&[Some(pass.views.clone())]);
                    list.SetGraphicsRootSignature(&root);
                    list.SetPipelineState(&pipeline);
                    list.SetGraphicsRootDescriptorTable(0, gpu);
                    list.OMSetRenderTargets(0, None, false, Some(&target));
                    list.RSSetViewports(&[D3D12_VIEWPORT {
                        TopLeftX: 0.0,
                        TopLeftY: 0.0,
                        Width: width as f32,
                        Height: height as f32,
                        MinDepth: 0.0,
                        MaxDepth: 1.0,
                    }]);
                    list.RSSetScissorRects(&[RECT {
                        left: 0,
                        top: 0,
                        right: width as i32,
                        bottom: height as i32,
                    }]);
                    list.IASetPrimitiveTopology(
                        windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(4),
                    );
                    list.DrawInstanced(3, 1, 0, 0);
                }
                crate::resource::PLANES[0].fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn record_twin_copy(
        list: &ID3D12GraphicsCommandList,
        depth: &ID3D12Resource,
        twin: &crate::resource::Twin,
        colour: bool,
    ) {
        let (from, to) = if colour {
            (depth, &twin.colour.texture)
        } else {
            (&twin.colour.texture, depth)
        };
        crate::resource::TWINS[2 + usize::from(colour)].fetch_add(1, Ordering::Relaxed);
        if colour && twin.plane.is_some() {
            crate::resource::PLANES[1].fetch_add(1, Ordering::Relaxed);
        }
        let Some(through) = &twin.through else {
            unsafe { list.CopyResource(to, from) };
            return;
        };
        let Some(host) = Host::instance() else { return };
        let of = unsafe { depth.GetDesc() };
        let planes = u32::from(of.MipLevels) * u32::from(of.DepthOrArraySize);
        let mut rows = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); planes as usize];
        unsafe {
            host.device.GetCopyableFootprints(
                &of,
                planes,
                planes,
                0,
                Some(rows.as_mut_ptr()),
                None,
                None,
                None,
            );
        }
        let (first_from, first_to) = if colour { (planes, 0) } else { (0, planes) };
        let subresource = |texture: &ID3D12Resource, index: u32| D3D12_TEXTURE_COPY_LOCATION {
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: index,
            },
        };
        let placed = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
            pResource: unsafe { std::mem::transmute_copy(through) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *row,
            },
        };
        let (common, source, dest) = (
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        unsafe {
            list.ResourceBarrier(&[StateBits::transition(through, common, dest)]);
            for (i, row) in rows.iter().enumerate() {
                let index = first_from + i as u32;
                list.CopyTextureRegion(&placed(row), 0, 0, 0, &subresource(from, index), None);
            }
            list.ResourceBarrier(&[StateBits::transition(through, dest, source)]);
            for (i, row) in rows.iter().enumerate() {
                let index = first_to + i as u32;
                list.CopyTextureRegion(&subresource(to, index), 0, 0, 0, &placed(row), None);
            }
            list.ResourceBarrier(&[StateBits::transition(through, source, common)]);
        }
    }
}

impl ScratchPool {
    fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_SCRATCH_POOL").is_none_or(|v| v != "0"))
    }

    fn take(host: &Host, size: u64) -> Option<Chunk> {
        if ScratchPool::enabled()
            && let Some(chunk) = SPARE.lock().unwrap_or_else(|e| e.into_inner()).take(size)
        {
            ALIVE[3].fetch_sub(1, Ordering::Relaxed);
            ALIVE[4].fetch_sub(size, Ordering::Relaxed);
            ALIVE[5].fetch_add(1, Ordering::Relaxed);
            return Some(chunk);
        }
        let (buffer, data) = host.upload_buffer(size)?;
        HostSetup::name(&buffer, || "list scratch".into());
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        ALIVE[1].fetch_add(1, Ordering::Relaxed);
        ALIVE[2].fetch_add(size, Ordering::Relaxed);
        Some(Chunk {
            _buffer: buffer,
            data: data as usize,
            gpu,
        })
    }

    fn give_back(chunks: Vec<(Chunk, u64)>) {
        if chunks.is_empty() {
            return;
        }
        let mut freed = Vec::new();
        {
            let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
            for (chunk, size) in chunks {
                let back = if ScratchPool::enabled() {
                    spare.give(chunk, size, SPARE_MOST)
                } else {
                    Some(chunk)
                };
                match back {
                    None => {
                        ALIVE[3].fetch_add(1, Ordering::Relaxed);
                        ALIVE[4].fetch_add(size, Ordering::Relaxed);
                    }
                    Some(chunk) => {
                        ALIVE[1].fetch_sub(1, Ordering::Relaxed);
                        ALIVE[2].fetch_sub(size, Ordering::Relaxed);
                        freed.push(chunk);
                    }
                }
            }
        }
        drop(freed);
    }
}

impl LateRun {
    fn rehash_runs() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_CARD_REHASH_RUNS").is_some_and(|v| v == "1"))
    }

    fn wide_merge() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_WIDE_MERGE").is_some_and(|v| v == "1"))
    }

    pub(crate) fn on() -> bool {
        LateRun::late_stage() > 0
    }

    fn late_stage() -> u8 {
        static STAGE: OnceLock<u8> = OnceLock::new();
        *STAGE.get_or_init(|| {
            if std::env::var_os("LEONIDA_GPU_TIMING").is_some() {
                return 0;
            }
            match std::env::var("LEONIDA_LATE_WAIT").as_deref() {
                Ok("1") => 1,
                Ok("2") => 2,
                Ok("3") => 3,
                Err(_) if crate::shadow::ShadowRelay::enabled() => 3,
                _ => 0,
            }
        })
    }

    fn late_exact() -> bool {
        LateRun::late_stage() > 1
    }

    pub(crate) fn late_free_present() -> bool {
        LateRun::late_stage() > 2
    }

    pub(crate) fn relay() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| LateRun::late_stage() > 0 && crate::shadow::ShadowRelay::enabled())
    }

    fn relay_exact() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            LateRun::relay() && std::env::var_os("LEONIDA_RELAY").is_none_or(|v| v == "2")
        })
    }

    fn landable(line: &[Option<u64>], completed: u64) -> usize {
        line.iter()
            .position(|f| f.is_some_and(|f| f > completed))
            .unwrap_or(line.len())
    }

    fn flagged_pieces(
        flags: impl Iterator<Item = (usize, u32, u16)>,
    ) -> std::collections::HashMap<(usize, u32), u16> {
        let mut masks = std::collections::HashMap::new();
        for (copy, piece, mask) in flags.filter(|f| f.2 != 0) {
            *masks.entry((copy, piece)).or_default() |= mask;
        }
        masks
    }

    pub(crate) fn copy_fetch_runs<'a>(
        list: &ID3D12GraphicsCommandList,
        destination: &ID3D12Resource,
        pieces: impl Iterator<Item = (&'a ID3D12Resource, usize, usize, u64)>,
    ) {
        let mut pieces = pieces.peekable();
        let mut copies = 0;
        while let Some((source, lo, mut hi, at)) = pieces.next() {
            while let Some(&(next, from, to, out)) = pieces.peek()
                && next == source
                && from == hi
                && out == at + (hi - lo) as u64
            {
                hi = to;
                pieces.next();
            }
            unsafe { list.CopyBufferRegion(destination, at, source, lo as u64, (hi - lo) as u64) };
            copies += 1;
        }
        CARD_FETCH_COPIES.fetch_add(copies, Ordering::Relaxed);
    }

    fn late_most() -> usize {
        static MOST: OnceLock<usize> = OnceLock::new();
        *MOST.get_or_init(|| {
            std::env::var("LEONIDA_LATE_WAIT_LISTS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(256)
                .max(1)
        })
    }

    fn late_writes() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_LATE_WAIT_WRITES").is_none_or(|v| v != "0"))
    }

    fn writeback_owners<'a>(
        mut flights: impl Iterator<Item = &'a Flight>,
    ) -> crate::writeback::Owners {
        let first = flights.next().expect("at least one finished flight");
        let mut owners = crate::writeback::Owners::single(first.number);
        if !crate::writeback::changed_since(first.number) {
            return owners;
        }
        for flight in flights {
            for (shadow, ranges) in &flight.back {
                owners.declare(flight.number, shadow.id(), ranges);
            }
        }
        owners
    }

    fn lock_line() -> std::sync::MutexGuard<'static, Line> {
        LINE.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn touches(ranges: &[(usize, usize)], from: usize, to: usize) -> bool {
        if from >= to {
            return false;
        }
        let at = ranges.partition_point(|r| r.1 <= from);
        ranges.get(at).is_some_and(|r| r.0 < to)
    }

    fn join_ranges(
        ranges: &mut Vec<(usize, usize)>,
        more: impl IntoIterator<Item = (usize, usize)>,
    ) {
        ranges.extend(more.into_iter().filter(|r| r.0 < r.1));
        ranges.sort_unstable();
        let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for &(from, to) in ranges.iter() {
            match out.last_mut() {
                Some(last) if from <= last.1 => last.1 = last.1.max(to),
                _ => out.push((from, to)),
            }
        }
        *ranges = out;
    }

    fn texture_dependency_cut(runs: &[&HostList]) -> Option<usize> {
        if runs.len() < 2 {
            return None;
        }
        let mut pending = PendingWrites::default();
        for (index, list) in runs.iter().enumerate() {
            if !pending.is_empty()
                && LateRun::blocked_by(&pending, std::slice::from_ref(list), false).is_some()
            {
                return Some(index);
            }
            if index + 1 < runs.len() {
                let mut destinations: Vec<_> = list
                    .deferred()
                    .texture_backs
                    .iter()
                    .map(crate::texback::Back::destination)
                    .collect();
                if !destinations.is_empty() {
                    crate::texback::add_destination_aliases(&mut destinations);
                    LateRun::join_ranges(&mut pending.textures, destinations);
                }
            }
        }
        None
    }

    fn flying() -> bool {
        if !LateRun::on() {
            return false;
        }
        let line = LateRun::lock_line();
        line.settling || line.line.iter().any(|l| matches!(l, Late::Run(_)))
    }

    fn late_blocked(runs: &[&HostList]) -> Option<usize> {
        let written = {
            let line = LateRun::lock_line();
            if line.written.is_empty() {
                return None;
            }
            line.written.clone()
        };
        LateRun::blocked_by(&written, runs, true)
    }

    fn blocked_by(written: &PendingWrites, runs: &[&HostList], count: bool) -> Option<usize> {
        if written.is_empty() {
            return None;
        }
        for list in runs {
            for step in &list.deferred().copies {
                match step {
                    &Step::Copy(dst, src, len) => {
                        if written.touches(src, src + len) || written.touches(dst, dst + len) {
                            if count {
                                LATE_STATS[15].fetch_add(1, Ordering::Relaxed);
                            }
                            return Some(WHY_READ);
                        }
                    }
                    Step::Fill(_) | Step::Fills(_) => {
                        if step
                            .fills()
                            .iter()
                            .any(|f| written.touches(f.src, f.src.saturating_add(f.len)))
                        {
                            if count {
                                LATE_STATS[16].fetch_add(1, Ordering::Relaxed);
                            }
                            return Some(WHY_READ);
                        }
                    }
                    Step::Texture(staging) => {
                        let (at, len) = staging.memory();
                        if written.touches(at, at + len) {
                            if count {
                                LATE_STATS[17].fetch_add(1, Ordering::Relaxed);
                            }
                            return Some(WHY_READ);
                        }
                    }
                    Step::Run(_) => return Some(WHY_RUN),
                    Step::Refresh(_, at, len) | Step::GpuWrite(_, at, len) => {
                        if crate::writeback::needs_new_snapshot(*at, at + len, &written.buffers) {
                            return Some(WHY_READ);
                        }
                        if LateRun::touches(&written.textures, *at, at + len) {
                            return Some(WHY_READ);
                        }
                    }
                    Step::Host(_) => {}
                }
            }
        }
        None
    }

    fn stale_under_flights(runs: &[&HostList]) -> bool {
        let mut spans = Batch::default();
        for list in runs {
            for step in &list.deferred().copies {
                if let Step::Refresh(shadow, address, len) = step
                    && shadow.writes_pending()
                {
                    spans.add(shadow, *address, *len);
                }
            }
        }
        if LateRun::late_exact() {
            let mut runs_left = 512;
            return spans.refreshes.iter().any(|(shadow, from, to)| {
                shadow.merge_under_writes(*from, *to - *from, &mut runs_left)
            });
        }
        spans
            .refreshes
            .iter()
            .any(|(shadow, from, to)| shadow.stale_under_writes(*from, *to - *from))
    }

    pub(crate) fn late_signal(fence: usize, value: u64) -> bool {
        if !LateRun::on() {
            return false;
        }
        let mut line = LateRun::lock_line();
        if line.line.is_empty() && !line.settling {
            return false;
        }
        line.line.push(Late::Signal(fence, value));
        LATE_STATS[14].fetch_add(1, Ordering::Relaxed);
        drop(line);
        LINE_MOVED.notify_one();
        RELAY_MOVED.notify_one();
        true
    }

    pub(crate) fn late_met(wait: &crate::list::ValueWait) -> bool {
        let (address, bytes) = wait.place();
        {
            let line = LateRun::lock_line();
            if line.line.is_empty() && line.in_hand.is_empty() && !line.settling {
                return false;
            }
            let mut unsure = line.written.touches(address, address + bytes);
            let writes = line
                .in_hand
                .iter()
                .map(|(at, what, n)| (at, what, n))
                .chain(line.line.iter().filter_map(|item| match item {
                    Late::Write(at, what, n) => Some((at, what, n)),
                    _ => None,
                }));
            for (at, what, n) in writes {
                if *at >= address + bytes || address >= *at + *n {
                    continue;
                }
                match what {
                    crate::queue::Written::Value(v) if *at == address && *n >= bytes => {
                        if wait.holds_for(*v) {
                            LATE_STATS[13].fetch_add(1, Ordering::Relaxed);
                            return true;
                        }
                    }
                    _ => unsure = true,
                }
            }
            if !unsure {
                return false;
            }
        }
        LateRun::settle_now(WHY_VALUE);
        wait.met()
    }

    pub(crate) fn settle_now(why: usize) {
        if !LateRun::on() {
            return;
        }
        let Some(host) = Host::instance() else {
            return;
        };
        let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
        host.settle(&mut submitted, why);
    }

    pub(crate) fn settle_for_present() {
        LateRun::settle_now(WHY_SYNC);
    }

    pub(crate) fn settle_for_fence() {
        LateRun::settle_now(WHY_FENCE);
    }

    fn land() {
        let landed = std::mem::take(&mut LateRun::lock_line().landed);
        for (lists, order) in landed {
            crate::list::ListPool::recycle(lists, &order);
        }
    }

    fn late_helper() {
        let Some(host) = Host::instance() else {
            return;
        };
        let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
            tracing::warn!("no event for the thread that settles submissions sent ahead");
            return;
        };
        loop {
            let target = {
                let mut line = LateRun::lock_line();
                loop {
                    if line.poke {
                        line.poke = false;
                        break None;
                    }
                    if !LateRun::relay() && !line.line.is_empty() && !line.settling {
                        break Some(line.target);
                    }
                    line = LINE_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
                }
            };
            let Some(target) = target else {
                LateRun::land();
                crate::queue::Scheduler::release_held(false);
                continue;
            };
            unsafe {
                if host.fence.SetEventOnCompletion(target, event).is_ok() {
                    WaitForSingleObject(event, INFINITE);
                }
            }
            {
                let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
                if LateRun::lock_line().target != target {
                    continue;
                }
                host.settle(&mut submitted, WHY_CARD_DONE);
            }
            LateRun::land();
            crate::queue::Scheduler::release_held(false);
        }
    }

    fn late_threads() {
        static HELPER: std::sync::Once = std::sync::Once::new();
        HELPER.call_once(|| {
            if let Err(e) = std::thread::Builder::new().name("leonida-late-wait".into()).spawn(LateRun::late_helper) {
                tracing::warn!(error = %e, "no thread to settle submissions sent ahead: they are settled by the calls that follow");
            }
            tracing::info!(
                most = LateRun::late_most(),
                writes = LateRun::late_writes(),
                "submissions are sent to the card without a wait for each (LEONIDA_LATE_WAIT=1)"
            );
            if LateRun::relay() {
                if let Err(e) = std::thread::Builder::new().name("leonida-relay".into()).spawn(LateRun::relay_worker) {
                    HostSetup::fail(format_args!("no thread to land submissions sent ahead (LEONIDA_RELAY=1): {e}"));
                }
                tracing::info!(
                    exact = LateRun::relay_exact(),
                    "what shaders wrote is copied back by a thread of its own, beside the submitting thread (LEONIDA_RELAY=1)"
                );
            }
        });
    }

    fn relay_wait(why: usize, done: impl Fn(&Line) -> bool) {
        let mut line = LateRun::lock_line();
        if done(&line) {
            return;
        }
        LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
        LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
        let was_working = HostTrail::working(false);
        let waiting = std::time::Instant::now();
        while !done(&line) {
            line = LINE_LANDED.wait(line).unwrap_or_else(|e| e.into_inner());
        }
        drop(line);
        HostTrail::stood(waiting);
        HostTrail::working(was_working);
        let us = waiting.elapsed().as_micros() as u64;
        LATE_STATS[12].fetch_add(us, Ordering::Relaxed);
        LATE_WAITED[why.min(10)].fetch_add(us, Ordering::Relaxed);
        SUBMISSION_US[1].fetch_add(us, Ordering::Relaxed);
    }

    fn relay_worker() {
        let Some(host) = Host::instance() else {
            return;
        };
        let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
            HostSetup::fail(format_args!(
                "no event for the thread that lands submissions sent ahead (LEONIDA_RELAY=1)"
            ));
        };
        let mut fetch: Option<RelayFetch> = None;
        loop {
            let next = {
                let mut line = LateRun::lock_line();
                loop {
                    if line.line.is_empty() {
                        line = RELAY_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
                        continue;
                    }
                    let fences: Vec<Option<u64>> = line
                        .line
                        .iter()
                        .map(|l| match l {
                            Late::Run(flight) => Some(flight.fence),
                            _ => None,
                        })
                        .collect();
                    let completed = unsafe { host.fence.GetCompletedValue() };
                    let n = LateRun::landable(&fences, completed);
                    if n == 0 {
                        break Err(fences[0].unwrap_or(0));
                    }
                    line.settling = true;
                    let items: Vec<Late> = line.line.drain(..n).collect();
                    line.in_hand = items
                        .iter()
                        .filter_map(|l| match l {
                            Late::Write(at, what, bytes) => Some((*at, *what, *bytes)),
                            _ => None,
                        })
                        .collect();
                    break Ok(items);
                }
            };
            match next {
                Ok(items) => host.relay_land(items, &mut fetch),
                Err(target) => {
                    let waiting = std::time::Instant::now();
                    let waited = unsafe {
                        host.fence.SetEventOnCompletion(target, event).map(|()| {
                            WaitForSingleObject(event, INFINITE);
                        })
                    };
                    if waited.is_err() {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    RELAY_STATS[2]
                        .fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
                }
            }
        }
    }

    fn relay_hand_back(back: &[Declared], owners: &crate::writeback::Owners) {
        let mut work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
        for (shadow, declared) in back {
            if shadow.carded() {
                continue;
            }
            let mut spans: Vec<(usize, usize)> = Vec::new();
            LateRun::join_ranges(&mut spans, declared.iter().copied());
            for &(from, to) in &spans {
                shadow.relay_compared(from, to - from);
            }
            work.extend(
                spans
                    .into_iter()
                    .map(|(from, to)| (shadow.clone(), from, to)),
            );
        }
        if work.is_empty() {
            return;
        }
        #[cfg(feature = "diagnostics")]
        let scanning = std::time::Instant::now();
        let found = crate::shadow::WideCopy::find_all(&work);
        #[cfg(feature = "diagnostics")]
        crate::copystat::C::add(
            crate::copystat::C::BackScanUs,
            scanning.elapsed().as_micros() as u64,
        );
        let (mut now, mut title) = (Vec::new(), Vec::new());
        for ((shadow, from, to), found) in work.iter().zip(found) {
            let mut written = 0;
            for (at, n) in found {
                written += shadow.relay_merge(at, n, &mut now, &mut title, owners);
            }
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::CopyStats::compared(*from, to - from, written);
            let _ = (from, to);
        }
    }

    fn late_line(frames: f64) {
        if !LateRun::on() {
            return;
        }
        let s: Vec<f64> = LATE_STATS
            .iter()
            .map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames)
            .collect();
        tracing::info!(
            "submissions sent ahead per frame: {:.1} in {:.1} settles ({:.1} flights each time), {:.1} ms waiting for the card in them; \
             settled for: a synchronous submission or present {:.1}, the card had run them {:.1}, a wait for a memory value {:.1}, \
             title memory changed under shader writes {:.1}, title memory read {:.1} (by a buffer copy {:.1}, a fill of upload memory {:.1}, \
             a texture {:.1}), a step of ours {:.1}, {} lists in the air {:.1}, \
             bottom-of-pipe writes (LEONIDA_LATE_WAIT_WRITES=0) {:.1}, a blocking fence wait {:.1}; \
             {:.1} waits for memory values met by a flight's bottom-of-pipe write, {:.1} fence signals set behind flights",
            s[0],
            s[1],
            if s[1] > 0.0 { s[11] / s[1] } else { 0.0 },
            s[12] / 1000.0,
            s[WHY_SYNC],
            s[WHY_CARD_DONE],
            s[WHY_VALUE],
            s[WHY_STALE],
            s[WHY_READ],
            s[15],
            s[16],
            s[17],
            s[WHY_RUN],
            LateRun::late_most(),
            s[WHY_MOST],
            s[WHY_WRITES],
            s[WHY_FENCE],
            s[13],
            s[14],
        );
        {
            let w: Vec<f64> = LATE_WAITED
                .iter()
                .map(|w| w.swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames)
                .collect();
            tracing::info!(
                "of the waiting for the card in settles, per frame, by what settled: a synchronous submission or present {:.1} ms, \
                 the card had run them {:.1}, a wait for a memory value {:.1}, title memory changed under shader writes {:.1}, \
                 title memory read {:.1}, a step of ours {:.1}, lists in the air {:.1}, bottom-of-pipe writes {:.1}, a blocking fence wait {:.1}",
                w[WHY_SYNC],
                w[WHY_CARD_DONE],
                w[WHY_VALUE],
                w[WHY_STALE],
                w[WHY_READ],
                w[WHY_RUN],
                w[WHY_MOST],
                w[WHY_WRITES],
                w[WHY_FENCE],
            );
        }
        if LateRun::relay() {
            let r: Vec<f64> = RELAY_STATS
                .iter()
                .map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames)
                .collect();
            tracing::info!(
                "the relay per frame (LEONIDA_RELAY=1): {:.1} flights landed in {:.1} landings by the thread that copies back, \
                 beside the submitting thread; that thread waited {:.1} ms for the card with nothing to land and {:.1} ms for \
                 fetches of card copies' pieces, and was {:.1} ms landing in all (the frame line's \"copying back\" is its time; \
                 \"settles\" above are waits of other threads for it)",
                r[1],
                r[0],
                r[2] / 1000.0,
                r[3] / 1000.0,
                r[4] / 1000.0,
            );
        }
        if LateRun::late_exact() {
            let [pages, runs, bytes] =
                crate::shadow::ByteDiff::merged_stats().map(|n| n as f64 / frames);
            tracing::info!(
                "title bytes put in under flights' shader writes per frame (LEONIDA_LATE_WAIT=2): {pages:.0} pages looked at, \
                 {runs:.0} changed runs, {:.2} MB",
                bytes / (1 << 20) as f64
            );
        }
    }

    fn join_copies() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_JOIN_COPIES").is_some_and(|v| v == "1"))
    }

    fn continues(last: (usize, usize, usize), next: (usize, usize, usize)) -> bool {
        let ((d, s, n), (dst, src, len)) = (last, next);
        let Some(all) = n.checked_add(len) else {
            return false;
        };
        d.checked_add(n) == Some(dst)
            && s.checked_add(n) == Some(src)
            && (d.checked_add(all).is_some_and(|end| end <= s)
                || s.checked_add(all).is_some_and(|end| end <= d))
    }

    fn declared_writes(runs: &[&HostList], card_only: bool) -> Vec<Declared> {
        let mut back = WriteGroups::new();
        for list in runs {
            for step in &list.deferred().copies {
                if let Step::GpuWrite(shadow, address, len) = step
                    && (!card_only || shadow.carded())
                {
                    back.add(shadow, &[(*address, *address + *len)]);
                }
            }
        }
        back.groups
    }

    fn read_backs(list: &HostList) {
        for back in &list.deferred().texture_backs {
            crate::texback::land(back);
        }
        for work in &list.deferred().after {
            work();
        }
        for (buffer, at, [row, rows, pitch], source, recorded) in &list.deferred().backs {
            let mut landed = false;
            let mut data = std::ptr::null_mut();
            unsafe {
                if buffer.Map(0, None, Some(&mut data)).is_ok() && !data.is_null() {
                    let from = data.cast::<u8>().cast_const();
                    for line in 0..*rows {
                        std::ptr::copy_nonoverlapping(
                            from.add(line * row.next_multiple_of(256)),
                            (*at as *mut u8).add(line * pitch),
                            *row,
                        );
                    }
                    if HostTrail::walls_trace() && *rows == 1 && *row <= 96 {
                        let mut values = [0f32; 24];
                        std::ptr::copy_nonoverlapping(
                            from,
                            values.as_mut_ptr().cast::<u8>(),
                            *row & !3,
                        );
                        tracing::info!(
                            frame = crate::frame::FramePacer::frame(),
                            to = format_args!("{:#x}", *at),
                            bytes = *row,
                            values = ?&values[..*row / 4],
                            "walls trace: a read-back lands in title memory"
                        );
                    }
                    buffer.Unmap(0, None);
                    landed = true;
                }
            }
            let bytes = row * rows;
            LateRun::walls_landed(
                READ_BACK,
                *at,
                pitch * (rows - 1) + row,
                bytes,
                if landed { bytes } else { 0 },
                *source,
                Some(*recorded),
            );
        }
    }

    pub(crate) fn walls_landed(
        kind: &'static str,
        at: usize,
        len: usize,
        bytes: usize,
        written: usize,
        source: usize,
        recorded: Option<u64>,
    ) {
        if !HostTrail::walls_trace() {
            return;
        }
        static LANDINGS: Mutex<Option<Landings>> = Mutex::new(None);
        let line = LANDINGS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(Landings::default)
            .add(
                crate::frame::FramePacer::frame(),
                kind,
                at,
                len,
                bytes,
                written,
                source,
                recorded,
            );
        if let Some((frame, count, unchanged, landed)) = line {
            tracing::info!(
                frame,
                count,
                unchanged,
                landed = ?landed,
                "walls trace: what the card wrote reached title memory in this frame"
            );
        }
    }

    fn hand_back(back: &[Declared], owners: &crate::writeback::Owners) {
        if crate::shadow::WideCopy::enabled() {
            let mut work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
            for (shadow, declared) in back {
                if shadow.carded() {
                    continue;
                }
                let mut sorted = declared.clone();
                sorted.sort_unstable();
                let mut spans: Vec<(usize, usize)> = Vec::new();
                for (from, to) in sorted {
                    match spans.last_mut() {
                        Some(last) if from <= last.1 => last.1 = last.1.max(to),
                        _ => spans.push((from, to)),
                    }
                }
                work.extend(
                    spans
                        .into_iter()
                        .map(|(from, to)| (shadow.clone(), from, to)),
                );
            }
            #[cfg(feature = "diagnostics")]
            let scanning = std::time::Instant::now();
            let found = crate::shadow::WideCopy::find_all(&work);
            #[cfg(feature = "diagnostics")]
            crate::copystat::C::add(
                crate::copystat::C::BackScanUs,
                scanning.elapsed().as_micros() as u64,
            );
            for ((shadow, from, to), found) in work.iter().zip(found) {
                let written = shadow.copy_back_with(*from, to - from, Some(found), owners);
                LateRun::walls_landed(HOST_COPY, *from, to - from, to - from, written, 0, None);
                COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                crate::copystat::CopyStats::compared(*from, to - from, written);
            }
            for (shadow, declared) in back {
                shadow.copy_back_done(declared);
            }
            return;
        }
        for (shadow, declared) in back {
            if shadow.carded() {
                shadow.copy_back_done(declared);
                continue;
            }
            let mut sorted = declared.clone();
            sorted.sort_unstable();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (from, to) in sorted {
                match spans.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => spans.push((from, to)),
                }
            }
            for (from, to) in spans {
                let written = shadow.copy_back_with(from, to - from, None, owners);
                LateRun::walls_landed(HOST_COPY, from, to - from, to - from, written, 0, None);
                COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                crate::copystat::CopyStats::compared(from, to - from, written);
            }
            shadow.copy_back_done(declared);
        }
    }
}

impl FillWork {
    fn workers() -> usize {
        static COUNT: OnceLock<usize> = OnceLock::new();
        *COUNT.get_or_init(|| {
            std::env::var("LEONIDA_FILL_WORKERS")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(1)
                .clamp(1, 6)
        })
    }

    fn independent(fills: &[Fill]) -> bool {
        let Some(first) = fills.first() else {
            return false;
        };
        let mut end = first.dst;
        for fill in fills {
            let Some(next) = fill.dst.checked_add(fill.len) else {
                return false;
            };
            if fill.dst < end {
                return false;
            }
            end = next;
        }
        fills.iter().all(|f| {
            f.src
                .checked_add(f.len)
                .is_some_and(|src_end| f.src >= end || src_end <= first.dst)
        })
    }

    fn run(fills: &[Fill], workers: usize) -> usize {
        let jobs = workers.min(fills.len() / FILLS_PER_JOB);
        if jobs > 1 && FillWork::independent(fills) {
            let queued = fills
                .chunks(fills.len().div_ceil(jobs))
                .map(|chunk| {
                    let owned = chunk.to_vec();
                    Box::new(move || owned.into_iter().for_each(Fill::run))
                        as Box<dyn FnOnce() + Send>
                })
                .collect();
            crate::shadow::WideCopy::run(queued);
            jobs
        } else {
            fills.iter().copied().for_each(Fill::run);
            1
        }
    }

    fn wide_refresh() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_WIDE_REFRESH").is_some_and(|v| v == "1"))
    }
}

impl TitleCopy {
    pub(crate) fn copy_mapped(dst: *mut u8, src: *const u8, len: usize, title_dst: bool) {
        #[cfg(feature = "diagnostics")]
        match crate::copystat::CopyStats::mode() {
            1 => {
                unsafe { std::ptr::copy(src, dst, len) };
                return;
            }
            2 => return,
            _ => {}
        }
        if len == 0 {
            return;
        }
        static ASK: OnceLock<bool> = OnceLock::new();
        let ask_first = title_dst
            && *ASK.get_or_init(|| std::env::var_os("LEONIDA_COPY_ASK").is_some_and(|v| v == "1"));
        if ask_first && !crate::texture::TitleMemoryMap::committed(dst, len) {
            return;
        }
        if title_dst {
            crate::watch::TitleMemory::will_write(dst as usize, len);
        }
        #[cfg(feature = "diagnostics")]
        let moved = std::time::Instant::now();
        let whole = TitleCopy::read(src, dst, len);
        #[cfg(feature = "diagnostics")]
        crate::copystat::C::add(
            crate::copystat::C::MoveUs,
            moved.elapsed().as_micros() as u64,
        );
        if whole {
            return;
        }
        if title_dst && !ask_first && !crate::texture::TitleMemoryMap::committed(dst, len) {
            return;
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::C::add(crate::copystat::C::Fallbacks, 1);
        #[cfg(feature = "diagnostics")]
        let sparse = std::time::Instant::now();
        if !title_dst && len >= 64 << 10 {
            TitleCopy::read_sparse(src, dst, len);
        } else {
            TitleCopy::read_pieces(src, dst, len);
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::C::add(
            crate::copystat::C::SparseUs,
            sparse.elapsed().as_micros() as u64,
        );
    }

    fn read_sparse(src: *const u8, dst: *mut u8, len: usize) {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery,
        };
        let mut offset = 0;
        while offset < len {
            let at = src.wrapping_add(offset);
            let to = dst.wrapping_add(offset);
            let remaining = len - offset;
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            let size = unsafe { VirtualQuery(at.cast(), &mut info, size_of_val(&info)) };
            let n = (info.BaseAddress as usize)
                .saturating_add(info.RegionSize)
                .saturating_sub(at as usize)
                .min(remaining);
            if size != size_of_val(&info) || n == 0 {
                TitleCopy::read_pieces(at, to, remaining);
                break;
            }
            if info.State != MEM_COMMIT || info.Protect == PAGE_NOACCESS {
                #[cfg(feature = "diagnostics")]
                crate::copystat::C::add(crate::copystat::C::Unmapped, n as u64);
                unsafe { to.write_bytes(0, n) };
            } else if !TitleCopy::read(at, to, n) {
                TitleCopy::read_pieces(at, to, n);
            }
            offset += n;
        }
    }

    fn read_pieces(src: *const u8, dst: *mut u8, len: usize) {
        TitleCopy::for_pieces(src as usize, len, 64 << 10, |at, n| {
            let to = dst.wrapping_add(at - src as usize);
            if !TitleCopy::read(at as *const u8, to, n) {
                TitleCopy::for_pieces(at, n, 4 << 10, |at, n| {
                    let to = dst.wrapping_add(at - src as usize);
                    if !TitleCopy::read(at as *const u8, to, n) {
                        #[cfg(feature = "diagnostics")]
                        crate::copystat::C::add(crate::copystat::C::Unmapped, n as u64);
                        unsafe { to.write_bytes(0, n) };
                    }
                });
            }
        });
    }

    pub(crate) fn read_into(src: *const u8, dst: &mut [u8]) -> bool {
        TitleCopy::read(src, dst.as_mut_ptr(), dst.len())
    }

    pub(crate) fn read(src: *const u8, dst: *mut u8, len: usize) -> bool {
        use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut done = 0;
        unsafe {
            ReadProcessMemory(GetCurrentProcess(), src.cast(), dst.cast(), len, &mut done) != 0
        }
    }

    fn direct_writes() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("LEONIDA_DIRECT_WRITES").is_none_or(|v| v != "0"))
    }

    pub(crate) fn write_mapped(dst: *mut u8, src: *const u8, len: usize) {
        use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let direct = TitleCopy::direct_writes();
        let write = |at: usize, n: usize| {
            let from = src.wrapping_add(at - dst as usize);
            let mut done = 0;
            if direct {
                return unsafe {
                    NtWriteVirtualMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done)
                        >= 0
                };
            }
            unsafe {
                WriteProcessMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done) != 0
            }
        };
        if len == 0 || write(dst as usize, len) {
            return;
        }
        TitleCopy::for_pieces(dst as usize, len, 64 << 10, |at, n| {
            if !write(at, n) {
                TitleCopy::for_pieces(at, n, 4 << 10, |at, n| {
                    write(at, n);
                });
            }
        });
    }

    fn for_pieces(start: usize, len: usize, step: usize, mut f: impl FnMut(usize, usize)) {
        let end = start + len;
        let mut at = start;
        while at < end {
            let next = ((at / step + 1) * step).min(end);
            f(at, next - at);
            at = next;
        }
    }
}

impl HostWindow {
    fn swap_format(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
        Some(match format.0 {
            27..=32 => DXGI_FORMAT_R8G8B8A8_UNORM,
            87 | 90 | 91 => DXGI_FORMAT_B8G8R8A8_UNORM,
            23..=25 => DXGI_FORMAT_R10G10B10A2_UNORM,
            9..=14 => DXGI_FORMAT_R16G16B16A16_FLOAT,
            _ => return None,
        })
    }

    fn caption(figures: &str, up: Duration) -> String {
        let s = up.as_secs();
        let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
        if h == 0 {
            format!("{figures} | up {m}:{s:02}")
        } else {
            format!("{figures} | up {h}:{m:02}:{s:02}")
        }
    }

    fn title_window() -> Option<HWND> {
        extern "system" fn visit(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
            let mut process = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd, Some(&mut process));
                if process == windows::Win32::System::Threading::GetCurrentProcessId()
                    && IsWindowVisible(hwnd).as_bool()
                {
                    *(found.0 as *mut Option<HWND>) = Some(hwnd);
                    return false.into();
                }
            }
            true.into()
        }
        let mut found: Option<HWND> = None;
        let _ = unsafe { EnumWindows(Some(visit), LPARAM(&raw mut found as isize)) };
        found
    }

    fn create(width: u32, height: u32) -> windows::core::Result<HWND> {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("leonida-window".into())
            .spawn(move || {
                let made = HostWindow::make_window(width, height);
                let ok = made.is_ok();
                let _ = sender.send(made.map(|h| h.0 as usize));
                if ok {
                    HostWindow::pump();
                }
            })
            .map_err(|_| windows::core::Error::empty())?;
        let hwnd = receiver
            .recv()
            .map_err(|_| windows::core::Error::empty())??;
        Ok(HWND(hwnd as *mut _))
    }

    extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    fn make_window(width: u32, height: u32) -> windows::core::Result<HWND> {
        const CLASS: PCWSTR = w!("Leonida");
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(HostWindow::window_proc),
                hInstance: instance.into(),
                lpszClassName: CLASS,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            RegisterClassExW(&class);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false)?;
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                w!("Leonida"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                rect.right - rect.left,
                rect.bottom - rect.top,
                None,
                None,
                Some(instance.into()),
                None,
            )
        }
    }

    fn pump() {
        let mut message = MSG::default();
        unsafe {
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}

unsafe impl Send for Retired {}

static RETIREMENTS: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

static TRAIL: Mutex<std::collections::VecDeque<(u64, u32, String)>> =
    Mutex::new(std::collections::VecDeque::new());
const TRAIL_MOST: usize = 96;

static SUBMITTING: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

static SUBMISSION_US: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

static COPY_BACK: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

static CARD_CLOCK_US: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

static WORKING: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

struct CardClock {
    fence: ID3D12Fence,
    state: Mutex<(u64, Option<(std::time::Instant, u64)>)>,
    sent: std::sync::Condvar,
}

unsafe impl Send for CardClock {}
unsafe impl Sync for CardClock {}

pub(crate) static PREPARING: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

pub(crate) static MADE: [std::sync::atomic::AtomicU64; 6] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 6];

pub(crate) static ALIVE: [std::sync::atomic::AtomicU64; 8] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 8];

pub(crate) static LISTS: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

static ON_FAILURE: OnceLock<fn(&str) -> !> = OnceLock::new();

impl Host {
    #[cfg(test)]
    pub(crate) fn is_software(&self) -> bool {
        self.adapter
            .as_ref()
            .and_then(|adapter| adapter.cast::<IDXGIAdapter1>().ok())
            .and_then(|adapter| unsafe { adapter.GetDesc1().ok() })
            .is_some_and(|desc| {
                desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0
                    || (desc.VendorId == 0x1414 && desc.DeviceId == 0x8c)
            })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn raw_memory_reads_follow_each_list_and_guest_copies() {
        let mut first = super::Deferred::default();
        let mut second = super::Deferred::default();
        assert_eq!(first.raw_copies.advance(2, false), (0..2, 2..2));
        assert_eq!(second.raw_copies.advance(5, true), (0..5, 0..5));
        assert_eq!(first.raw_copies.advance(5, true), (2..5, 0..5));
        assert_eq!(first.raw_copies.advance(5, true), (5..5, 5..5));
        first.invalidate_refreshes();
        assert_eq!(first.raw_copies.advance(5, false), (0..5, 5..5));
        assert_eq!(first.raw_copies.advance(6, true), (5..6, 5..6));
    }

    #[test]
    fn sparse_copies_preserve_islands_and_observe_remaps() {
        use windows_sys::Win32::System::Memory::*;
        const N: usize = 5 << 16;
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), N, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            let mut expected = vec![0; N];
            for (offset, value) in [(0, 9), (2 << 16, 37), (N - 4096, 85)] {
                let at = base.add(offset);
                assert_eq!(
                    VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE),
                    at.cast()
                );
                at.write_bytes(value, 4096);
                expected[offset..offset + 4096].fill(value);
            }
            for remap in [false, true] {
                if remap {
                    let mut old = 0;
                    assert_ne!(
                        VirtualProtect(base.cast(), 4096, PAGE_NOACCESS, &mut old),
                        0
                    );
                    expected[..4096].fill(0);
                    let at = base.add(1 << 16);
                    assert_eq!(
                        VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE),
                        at.cast()
                    );
                    at.write_bytes(0xc7, 4096);
                    expected[1 << 16..(1 << 16) + 4096].fill(0xc7);
                }
                let mut out = vec![0xee; N];
                super::TitleCopy::copy_mapped(
                    out.as_mut_ptr().add(27),
                    base.add(27),
                    N - 54,
                    false,
                );
                assert_eq!(out[27..N - 27], expected[27..N - 27]);
                assert!(out[..27].iter().chain(&out[N - 27..]).all(|&v| v == 0xee));
            }
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    use super::*;

    #[test]
    fn the_caption_ends_with_how_long_the_process_has_run() {
        use super::{Duration, HostWindow};

        let figures = "Leonida | 1 fps 953.5 ms/frame | 1526 draws/frame";
        let up = |secs| HostWindow::caption(figures, Duration::from_secs(secs));
        assert_eq!(up(0), format!("{figures} | up 0:00"));
        assert_eq!(up(59), format!("{figures} | up 0:59"));
        assert_eq!(up(754), format!("{figures} | up 12:34"));
        assert_eq!(up(3723), format!("{figures} | up 1:02:03"));
    }

    #[test]
    fn the_running_time_counts_from_the_process_start_and_moves_on() {
        use super::{Duration, HostClock};

        let age = HostClock::process_age().expect("the kernel's creation time");
        assert!(
            age > Duration::ZERO && age < Duration::from_secs(3600),
            "{age:?}"
        );
        let first = HostClock::process_uptime();
        std::thread::sleep(Duration::from_millis(20));
        let later = HostClock::process_uptime();
        assert!(
            first >= age,
            "{first:?} is before the process's age {age:?}"
        );
        assert!(
            later >= first + Duration::from_millis(20),
            "{first:?} then {later:?}"
        );
    }

    #[test]
    fn warp_adapter_accepts_only_software() {
        let raw = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            else {
                return;
            };
            factory.EnumWarpAdapter::<IDXGIAdapter>()
        };
        let Ok(raw) = raw else { return };
        let desc = unsafe { raw.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap() };
        eprintln!(
            "EnumWarpAdapter: vendor={:#x}, flags={:#x}",
            desc.VendorId, desc.Flags
        );
        assert_eq!(
            HostSetup::warp_adapter().is_ok(),
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        );
    }
}

pub(crate) struct HostList {
    allocator: ID3D12CommandAllocator,
    pub(crate) list: ID3D12GraphicsCommandList,
    deferred: Mutex<Deferred>,
    scratch: Mutex<Scratch>,
    states: Mutex<States>,
    twins: Mutex<std::collections::HashMap<usize, TwinUse>>,
    prelude: OnceLock<Option<Prelude>>,
    pipelines: Mutex<std::collections::BTreeSet<String>>,
    budget: OnceLock<Option<(ID3D12Resource, usize)>>,
    indirect_args: Mutex<Option<(ID3D12Resource, u64)>>,
    _alive: Counted,
    #[cfg(feature = "diagnostics")]
    notes: Mutex<Vec<Note>>,
    #[cfg(feature = "diagnostics")]
    stamps: OnceLock<Option<(ID3D12QueryHeap, ID3D12Resource)>>,
}

type Prelude = (ID3D12CommandAllocator, ID3D12GraphicsCommandList);

struct PreludeTransition(ID3D12Resource, u32, u32);

struct PlannedPrelude {
    commands: Prelude,
    to_copy: Vec<PreludeTransition>,
    syncs: Vec<(
        std::sync::Arc<crate::resource::HostTexture>,
        std::sync::Arc<crate::resource::Twin>,
        bool,
    )>,
    barriers: Vec<PreludeTransition>,
}

impl PlannedPrelude {
    fn record(self) {
        let (allocator, list) = self.commands;
        let recorded = unsafe {
            allocator.Reset().and_then(|()| {
                list.Reset(&allocator, None)?;
                Preludes::record_transitions(&list, &self.to_copy);
                for (depth, twin, colour) in &self.syncs {
                    PlanePass::record_twin_copy(&list, &depth.texture, twin, *colour);
                }
                Preludes::record_transitions(&list, &self.barriers);
                list.Close()
            })
        };
        if let Err(e) = recorded {
            HostSetup::fail(format_args!("a host state prelude did not record ({e})"));
        }
    }
}

#[cfg(feature = "diagnostics")]
const STAMPS: u32 = 4096;

#[cfg(feature = "diagnostics")]
type Note = (String, Option<(u64, u64)>);

type States =
    std::collections::HashMap<usize, (u32, u32, std::sync::Arc<crate::resource::HostTexture>)>;

struct TwinUse {
    depth: std::sync::Arc<crate::resource::HostTexture>,
    twin: std::sync::Arc<crate::resource::Twin>,
    needs: [bool; 2],
    behind: [Option<bool>; 2],
    writing: bool,
    late: bool,
}

const PLANE_CORNERS: &str = r#"
float4 corner(uint i : SV_VertexID) : SV_Position {
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, 0.0, 1.0);
}
"#;
const PLANE_STEPS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<uint> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return float(twin.Load(int4(int2(p.xy), 0, 0))) * asfloat(0x37800080);
}
"#;
const PLANE_FLOATS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<float> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return twin.Load(int4(int2(p.xy), 0, 0));
}
"#;

const PLANE_VIEWS: u32 = 256;

pub(crate) struct PlanePass {
    views: ID3D12DescriptorHeap,
    cpu: usize,
    gpu: u64,
    step: u32,
    next: AtomicU32,
    made: Mutex<Vec<(bool, i32, Option<PlanePipeline>)>>,
}

type PlanePipeline = (ID3D12RootSignature, ID3D12PipelineState);

unsafe impl Send for PlanePass {}
unsafe impl Sync for PlanePass {}

impl PlanePass {
    fn pipeline(&self, steps: bool, format: DXGI_FORMAT) -> Option<PlanePipeline> {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, _, pipeline)) = made.iter().find(|m| (m.0, m.1) == (steps, format.0)) {
            return pipeline.clone();
        }
        let pipeline = match Self::make(steps, format) {
            Ok(pipeline) => Some(pipeline),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    steps,
                    format = format.0,
                    "no pipeline to draw a depth plane from its twin: such planes stay as they are"
                );
                None
            }
        };
        made.push((steps, format.0, pipeline.clone()));
        pipeline
    }

    fn make(steps: bool, format: DXGI_FORMAT) -> windows::core::Result<PlanePipeline> {
        use windows::core::s;
        let device = &Host::instance()
            .ok_or_else(windows::core::Error::empty)?
            .device;
        let vs = PlanePass::compile(PLANE_CORNERS, s!("corner"), s!("vs_5_1"))?;
        let source = if steps { PLANE_STEPS } else { PLANE_FLOATS };
        let ps = PlanePass::compile(source, s!("plane"), s!("ps_5_1"))?;
        let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        };
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &ps[..])?;
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: code(&vs[..]),
                PS: code(&ps[..]),
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                    DepthEnable: true.into(),
                    DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                    DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                DSVFormat: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            let pipeline: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc)?;
            Ok((root, pipeline))
        }
    }
}

static MESH_GROUPS: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

struct Counted {
    fresh: std::sync::atomic::AtomicBool,
}

impl Counted {
    fn new() -> Counted {
        ALIVE[0].fetch_add(1, Ordering::Relaxed);
        LISTS[3].fetch_add(1, Ordering::Relaxed);
        Counted {
            fresh: std::sync::atomic::AtomicBool::new(true),
        }
    }

    fn used(&self) {
        if self.fresh.swap(false, Ordering::Relaxed) {
            LISTS[3].fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        ALIVE[0].fetch_sub(1, Ordering::Relaxed);
        self.used();
    }
}

struct Chunk {
    _buffer: ID3D12Resource,
    data: usize,
    gpu: u64,
}

unsafe impl Send for Chunk {}

#[derive(Default)]
struct Scratch {
    chunks: Vec<(Chunk, u64)>,
    current: usize,
    used: u64,
}

unsafe impl Send for Scratch {}

impl Drop for Scratch {
    fn drop(&mut self) {
        ScratchPool::give_back(std::mem::take(&mut self.chunks));
    }
}

const SCRATCH_CHUNK: u64 = 4 << 20;
const SCRATCH_FIRST: u64 = 256 << 10;
const SPARE_MOST: u64 = 128 << 20;

struct Spare<T> {
    kept: Vec<(T, u64)>,
    bytes: u64,
}

impl<T> Spare<T> {
    const fn new() -> Self {
        Spare {
            kept: Vec::new(),
            bytes: 0,
        }
    }

    fn take(&mut self, size: u64) -> Option<T> {
        let at = self.kept.iter().rposition(|(_, s)| *s == size)?;
        self.bytes -= size;
        Some(self.kept.remove(at).0)
    }

    fn give(&mut self, thing: T, size: u64, most: u64) -> Option<T> {
        if self.bytes + size > most {
            return Some(thing);
        }
        self.bytes += size;
        self.kept.push((thing, size));
        None
    }
}

static SPARE: Mutex<Spare<Chunk>> = Mutex::new(Spare::new());

struct CardWork {
    sync: Vec<Prelude>,
    check: Vec<Prelude>,
    fetch: Prelude,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    differs: (ID3D12Resource, usize, u64),
    fetched: Option<(ID3D12Resource, usize, u64)>,
    taken: u32,
    out: u32,
}

unsafe impl Send for CardWork {}

static CARD_WORK: Mutex<Option<CardWork>> = Mutex::new(None);

pub(crate) const CARD_COMPARE: &str = r#"
#define RS "RootConstants(num32BitConstants=6, b0), SRV(t0), UAV(u0), UAV(u1)"
cbuffer Span : register(b0) {
    uint first;
    uint skew;
    uint size;
    uint pieces;
    uint out_at;
    uint compare;
};
ByteAddressBuffer now : register(t0);
RWByteAddressBuffer hashes : register(u0);
RWByteAddressBuffer differs : register(u1);
groupshared uint changed;
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 group : SV_GroupID, uint lane : SV_GroupIndex) {
    if (group.x >= pieces) return;
    uint piece = first + group.x;
    uint page = lane / 4u;
    uint begin = piece * 65536u + page * 4096u;
    uint end = begin + 4096u;
    uint lo = begin > skew ? begin - skew : 0u;
    uint hi = min(end > skew ? end - skew : 0u, size);
    lo = lo & ~15u;
    hi = (hi + 15u) & ~15u;
    uint4 h = uint4(0x9E3779B9u, 0x85EBCA6Bu, 0xC2B2AE35u, 0x27D4EB2Fu);
    [loop]
    for (uint at = lo + (lane % 4u) * 16u; at < hi; at += 4u * 16u) {
        h = (h ^ now.Load4(at)) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
    }
    uint hash_at = (piece * 64u + lane) * 16u;
    if (compare != 0u) {
        if (lane == 0u) changed = 0u;
        GroupMemoryBarrierWithGroupSync();
        if (any(h != hashes.Load4(hash_at))) InterlockedOr(changed, 1u << page);
        GroupMemoryBarrierWithGroupSync();
        if (lane == 0u) differs.Store(out_at + group.x * 4u, changed);
    }
    if (compare != 1u) hashes.Store4(hash_at, h);
}
"#;

static CARD_PASSES: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];
static CARD_FETCH_COPIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct CardRange {
    shadow: std::sync::Arc<crate::shadow::Shadow>,
    declared: (usize, usize),
    span: crate::shadow::CardSpan,
    at: u32,
    compared: bool,
}

impl CardRange {
    fn changed_pages(&self, piece: u32, results: &[u32]) -> u16 {
        if self.compared {
            results
                .get(self.at as usize + piece as usize)
                .copied()
                .unwrap_or(u32::from(u16::MAX)) as u16
        } else {
            u16::MAX
        }
    }
}

struct CardPlan {
    ranges: Vec<CardRange>,
    check: Option<ID3D12CommandList>,
    _synced: Vec<std::sync::Arc<crate::shadow::Shadow>>,
    lists: Vec<(bool, Prelude)>,
}

enum Step {
    Copy(usize, usize, usize),
    Fill(Fill),
    Fills(Vec<Fill>),
    Refresh(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    Run(Box<dyn Fn() + Send>),
    Host(Box<dyn Fn() + Send>),
    GpuWrite(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    Texture(std::sync::Arc<crate::texture::Staging>),
}

#[derive(Default)]
struct Deferred {
    copies: Vec<Step>,
    fill_start: usize,
    uploads: Vec<ID3D12Resource>,
    upload_bytes: u64,
    kept: Vec<windows::core::IUnknown>,
    refreshed: std::collections::HashSet<(usize, usize, usize)>,
    textures: std::collections::HashSet<usize>,
    raw_copies: RawCopyCoverage,
    writes: Vec<(usize, crate::queue::Written, usize)>,
    backs: Vec<(ID3D12Resource, usize, [usize; 3], usize, u64)>,
    back_wanted: Vec<crate::resource::ResourceRef>,
    texture_backs: Vec<crate::texback::Back>,
    budget_zeroed: bool,
    after: Vec<Box<dyn Fn() + Send>>,
}

#[derive(Default)]
struct RawCopyCoverage {
    directory: usize,
    revision: u64,
    read: usize,
    written: usize,
}

impl RawCopyCoverage {
    fn advance(
        &mut self,
        count: usize,
        writes: bool,
    ) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
        let reads = self.read..count;
        self.read = count;
        let new_writes = if writes {
            self.written..count
        } else {
            count..count
        };
        if writes {
            self.written = count;
        }
        (reads, new_writes)
    }
}

impl Deferred {
    fn invalidate_refreshes(&mut self) {
        self.refreshed.clear();
        self.raw_copies.read = 0;
    }

    fn hold_upload(&mut self, buffer: ID3D12Resource, size: u64) {
        ALIVE[6].fetch_add(1, Ordering::Relaxed);
        ALIVE[7].fetch_add(size, Ordering::Relaxed);
        self.upload_bytes += size;
        self.uploads.push(buffer);
    }
}

impl Drop for Deferred {
    fn drop(&mut self) {
        ALIVE[6].fetch_sub(self.uploads.len() as u64, Ordering::Relaxed);
        ALIVE[7].fetch_sub(self.upload_bytes, Ordering::Relaxed);
    }
}

static RELAY_MOVED: std::sync::Condvar = std::sync::Condvar::new();

static LINE_LANDED: std::sync::Condvar = std::sync::Condvar::new();

static QUEUE_ORDER: Mutex<u64> = Mutex::new(0);

static RELAY_STATS: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

#[cfg(test)]
mod relay_tests {
    use super::LateRun;

    #[test]
    fn flights_land_in_the_order_they_were_sent() {
        let line = [Some(5), None, None, Some(6), None];
        assert_eq!(LateRun::landable(&line, 4), 0);
        assert_eq!(LateRun::landable(&line, 5), 3);
        assert_eq!(LateRun::landable(&line, 6), 5);
        assert_eq!(LateRun::landable(&line, u64::MAX), 5);
        assert_eq!(LateRun::landable(&[None, None], 0), 2);
        assert_eq!(LateRun::landable(&[None, Some(9), None], 8), 1);
        assert_eq!(LateRun::landable(&[], 3), 0);
        assert_eq!(LateRun::landable(&line[3..], 5), 0);
        assert_eq!(LateRun::landable(&line[3..], 6), 2);
    }

    #[test]
    fn a_piece_flagged_under_one_range_is_fetched_under_all() {
        let (a, b) = (0x1000usize, 0x2000usize);
        let flags = [
            (a, 3, 0x40),
            (a, 4, 0),
            (a, 3, 0),
            (a, 3, 0x08),
            (b, 3, 0),
            (b, 9, u16::MAX),
        ];
        let fetch = LateRun::flagged_pieces(flags.into_iter());
        assert_eq!(fetch.get(&(a, 3)), Some(&0x48));
        assert!(!fetch.contains_key(&(a, 4)));
        assert!(!fetch.contains_key(&(b, 3)));
        assert_eq!(fetch.get(&(b, 9)), Some(&u16::MAX));
        assert_eq!(fetch.len(), 2);
    }
}

type Declared<T = crate::shadow::Shadow> = (std::sync::Arc<T>, Vec<(usize, usize)>);

struct WriteGroups<T> {
    groups: Vec<Declared<T>>,
    positions: std::collections::HashMap<usize, usize>,
}

impl<T> WriteGroups<T> {
    fn new() -> Self {
        Self {
            groups: Vec::new(),
            positions: std::collections::HashMap::new(),
        }
    }

    fn add(&mut self, shadow: &std::sync::Arc<T>, ranges: &[(usize, usize)]) {
        use std::collections::hash_map::Entry;
        match self
            .positions
            .entry(std::sync::Arc::as_ptr(shadow) as usize)
        {
            Entry::Vacant(entry) => {
                entry.insert(self.groups.len());
                self.groups.push((shadow.clone(), ranges.to_vec()));
            }
            Entry::Occupied(entry) => {
                let existing = &mut self.groups[*entry.get()].1;
                for range in ranges {
                    if !existing.contains(range) {
                        existing.push(*range);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[test]
fn shadow_write_groups_preserve_owner_identity_and_first_seen_range_order() {
    use std::sync::Arc;
    let first = Arc::new(0);
    let second = Arc::new(0);
    let mut writes = WriteGroups::new();
    writes.add(&first, &[(20, 30), (0, 10)]);
    writes.add(&second, &[(0, 10)]);
    writes.add(&first.clone(), &[(0, 10), (9, 22), (20, 30)]);
    writes.add(&second, &[(0, 10), (20, 30)]);
    assert_eq!(writes.groups.len(), 2);
    assert!(Arc::ptr_eq(&writes.groups[0].0, &first));
    assert!(Arc::ptr_eq(&writes.groups[1].0, &second));
    assert_eq!(writes.groups[0].1, [(20, 30), (0, 10), (9, 22)]);
    assert_eq!(writes.groups[1].1, [(0, 10), (20, 30)]);
}

struct Sent {
    number: u64,
    card: Option<CardPlan>,
    back: Vec<Declared>,
    texture_writes: Vec<(usize, usize)>,
    writes: Vec<(usize, crate::queue::Written, usize)>,
}

struct Flight {
    number: u64,
    fence: u64,
    lists: Vec<crate::list::Detached>,
    order: Vec<usize>,
    card: Option<CardPlan>,
    back: Vec<Declared>,
    texture_writes: Vec<(usize, usize)>,
}

enum Late {
    Run(Flight),
    Write(usize, crate::queue::Written, usize),
    Signal(usize, u64),
}

struct Line {
    line: Vec<Late>,
    written: PendingWrites,
    lists: usize,
    target: u64,
    settling: bool,
    poke: bool,
    landed: Vec<(Vec<crate::list::Detached>, Vec<usize>)>,
    in_hand: Vec<(usize, crate::queue::Written, usize)>,
}

unsafe impl Send for Line {}

static LINE: Mutex<Line> = Mutex::new(Line {
    line: Vec::new(),
    written: PendingWrites {
        buffers: Vec::new(),
        textures: Vec::new(),
    },
    lists: 0,
    target: 0,
    settling: false,
    poke: false,
    landed: Vec::new(),
    in_hand: Vec::new(),
});
static LINE_MOVED: std::sync::Condvar = std::sync::Condvar::new();

const WHY_SYNC: usize = 2;
const WHY_CARD_DONE: usize = 3;
const WHY_VALUE: usize = 4;
const WHY_STALE: usize = 5;
const WHY_READ: usize = 6;
const WHY_RUN: usize = 7;
const WHY_MOST: usize = 8;
const WHY_WRITES: usize = 9;
const WHY_FENCE: usize = 10;

static LATE_STATS: [std::sync::atomic::AtomicU64; 18] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 18];

static LATE_WAITED: [std::sync::atomic::AtomicU64; 11] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 11];

#[derive(Default, Clone)]
struct PendingWrites {
    buffers: Vec<(usize, usize)>,
    textures: Vec<(usize, usize)>,
}

impl PendingWrites {
    fn is_empty(&self) -> bool {
        self.buffers.is_empty() && self.textures.is_empty()
    }

    fn touches(&self, start: usize, end: usize) -> bool {
        LateRun::touches(&self.buffers, start, end) || LateRun::touches(&self.textures, start, end)
    }

    fn add(&mut self, back: &[Declared], textures: &[(usize, usize)]) {
        if !back.is_empty() {
            LateRun::join_ranges(
                &mut self.buffers,
                back.iter().flat_map(|(_, r)| r.iter().copied()),
            );
        }
        if !textures.is_empty() {
            LateRun::join_ranges(&mut self.textures, textures.iter().copied());
        }
    }
}

struct RelayFetch {
    list: Prelude,
    fence: ID3D12Fence,
    event: HANDLE,
    sent: u64,
    buffer: Option<(ID3D12Resource, usize, u64)>,
}

#[cfg(test)]
mod late_tests {
    use super::LateRun;

    #[test]
    fn copies_join_only_end_to_end_and_apart() {
        assert!(LateRun::continues((1000, 5000, 16), (1016, 5016, 32)));
        assert!(LateRun::continues((5000, 1000, 16), (5016, 1016, 32)));
        assert!(!LateRun::continues((1000, 5000, 16), (1020, 5016, 32)));
        assert!(!LateRun::continues((1000, 5000, 16), (1016, 5020, 32)));
        assert!(!LateRun::continues((1000, 5000, 16), (1000, 5000, 16)));
        assert!(!LateRun::continues((1000, 1016, 16), (1016, 1032, 16)));
        assert!(!LateRun::continues((1016, 1000, 16), (1032, 1016, 16)));
        assert!(LateRun::continues((1000, 1032, 16), (1016, 1048, 16)));
        assert!(!LateRun::continues((usize::MAX - 8, 0, 8), (0, 8, 8)));
    }

    #[test]
    fn ranges_touch_where_they_overlap() {
        let mut ranges = Vec::new();
        LateRun::join_ranges(
            &mut ranges,
            [(40, 50), (10, 20), (20, 25), (45, 60), (7, 7)],
        );
        assert_eq!(ranges, [(10, 25), (40, 60)]);
        LateRun::join_ranges(&mut ranges, [(25, 40)]);
        assert_eq!(ranges, [(10, 60)]);
        let ranges = [(10usize, 25usize), (40, 60), (100, 101)];
        for (from, to, hit) in [
            (0, 10, false),
            (0, 11, true),
            (24, 25, true),
            (25, 40, false),
            (25, 41, true),
            (59, 200, true),
            (60, 100, false),
            (100, 101, true),
            (101, 500, false),
            (30, 30, false),
            (50, 50, false),
        ] {
            assert_eq!(LateRun::touches(&ranges, from, to), hit, "{from}..{to}");
        }
        assert!(!LateRun::touches(&[], 0, usize::MAX));
    }
}

#[cfg(feature = "diagnostics")]
static FILL_BATCHES: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];
static COPIES_JOINED: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

pub(crate) const READ_BACK: &str = "read-back";
pub(crate) const CARD_COPY: &str = "card copy";
pub(crate) const HOST_COPY: &str = "host copy";

#[derive(Default)]
struct Landings {
    frame: u64,
    entries: std::collections::BTreeMap<(&'static str, usize), Landing>,
    unchanged: u64,
}

#[derive(Clone, Copy)]
struct Landing {
    len: usize,
    count: u32,
    bytes: u64,
    written: u64,
    source: usize,
    recorded: Option<u64>,
}

impl Landings {
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        frame: u64,
        kind: &'static str,
        at: usize,
        len: usize,
        bytes: usize,
        written: usize,
        source: usize,
        recorded: Option<u64>,
    ) -> Option<(u64, usize, u64, Vec<String>)> {
        let line = if self.frame == frame {
            None
        } else {
            self.take()
        };
        self.frame = frame;
        if written == 0 && kind != READ_BACK {
            self.unchanged += 1;
            return line;
        }
        let e = self.entries.entry((kind, at)).or_insert(Landing {
            len,
            count: 0,
            bytes: 0,
            written: 0,
            source,
            recorded,
        });
        e.len = e.len.max(len);
        e.count += 1;
        e.bytes += bytes as u64;
        e.written += written as u64;
        e.recorded = match (e.recorded, recorded) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        line
    }

    fn take(&mut self) -> Option<(u64, usize, u64, Vec<String>)> {
        if self.entries.is_empty() && self.unchanged == 0 {
            return None;
        }
        let landed = self
            .entries
            .iter()
            .take(256)
            .map(|((kind, at), l)| {
                let mut s = format!(
                    "{kind} {at:#x}+{:#x}: x{}, {} bytes, {} written",
                    l.len, l.count, l.bytes, l.written
                );
                if l.source != 0 {
                    s += &format!(", from {:#x}", l.source);
                }
                if let Some(r) = l.recorded {
                    s += &format!(", recorded in frame {r}");
                }
                s
            })
            .collect();
        let line = (self.frame, self.entries.len(), self.unchanged, landed);
        self.entries.clear();
        self.unchanged = 0;
        Some(line)
    }
}

#[cfg(test)]
mod landings_tests {
    use super::{CARD_COPY, HOST_COPY, Landings, READ_BACK};

    #[test]
    fn a_frames_landings_are_one_line_once_the_next_frame_begins() {
        let mut l = Landings::default();
        assert!(
            l.add(5, READ_BACK, 0x1000, 0x200, 0x100, 0x100, 0xabc, Some(4))
                .is_none()
        );
        assert!(
            l.add(5, READ_BACK, 0x1000, 0x200, 0x100, 0x100, 0xabc, Some(3))
                .is_none()
        );
        assert!(
            l.add(5, CARD_COPY, 0x2000, 0x40, 0x40, 0x10, 0, None)
                .is_none()
        );
        assert!(
            l.add(5, HOST_COPY, 0x3000, 0x40, 0x40, 0, 0, None)
                .is_none()
        );
        let (frame, count, unchanged, landed) = l
            .add(6, CARD_COPY, 0x2000, 0x40, 0x40, 0x40, 0, None)
            .expect("frame 5's line");
        assert_eq!((frame, count, unchanged), (5, 2, 1));
        assert_eq!(
            landed,
            [
                "card copy 0x2000+0x40: x1, 64 bytes, 16 written",
                "read-back 0x1000+0x200: x2, 512 bytes, 512 written, from 0xabc, recorded in frame 3",
            ]
        );
        assert_eq!(l.take().map(|t| (t.0, t.1)), Some((6, 1)));
        assert!(l.take().is_none());
    }
}

impl Host {
    pub(crate) fn mesh_shaders(&self) -> bool {
        static MESH: OnceLock<bool> = OnceLock::new();
        *MESH.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
            let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
                HighestShaderModel: D3D_SHADER_MODEL_6_6,
            };
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS7,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
                    && self
                        .device
                        .CheckFeatureSupport(
                            D3D12_FEATURE_SHADER_MODEL,
                            (&raw mut model).cast(),
                            size_of_val(&model) as u32,
                        )
                        .is_ok()
            };
            let mesh = ok
                && options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED
                && model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0;
            tracing::info!(mesh, "host mesh shader support");
            mesh
        })
    }

    pub(crate) fn depth_bounds(&self) -> bool {
        static BOUNDS: OnceLock<bool> = OnceLock::new();
        *BOUNDS.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS2::default();
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS2,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
            };
            let supported = ok && options.DepthBoundsTestSupported.as_bool();
            tracing::info!(supported, "host depth bounds support");
            supported
        })
    }

    pub(crate) fn new_list(&self) -> windows::core::Result<HostList> {
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let before = HostClock::private_bytes();
        let list = self.make_list();
        LISTS[0].fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        LISTS[2].fetch_add(
            HostClock::private_bytes().wrapping_sub(before),
            Ordering::Relaxed,
        );
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::C;
            C::add(C::HostLists, 1);
            C::add(C::HostListNs, started.elapsed().as_nanos() as u64);
        }
        list
    }

    fn make_prelude(&self) -> Option<Prelude> {
        let made: windows::core::Result<Prelude> = unsafe {
            (|| {
                let allocator: ID3D12CommandAllocator = self
                    .device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                    0,
                    D3D12_COMMAND_LIST_TYPE_DIRECT,
                    &allocator,
                    None,
                )?;
                list.Close()?;
                Ok((allocator, list))
            })()
        };
        LISTS[1].fetch_add(1, Ordering::Relaxed);
        match made {
            Ok(prelude) => Some(prelude),
            Err(e) => {
                tracing::warn!(error = %e, "no host list for a prelude");
                None
            }
        }
    }

    fn make_list(&self) -> windows::core::Result<HostList> {
        unsafe {
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            )?;
            let prelude = OnceLock::new();
            if !Preludes::lazy_prelude() {
                let _ = prelude.set(self.make_prelude());
            }
            Ok(HostList {
                allocator,
                list,
                deferred: Mutex::default(),
                scratch: Mutex::default(),
                states: Mutex::default(),
                twins: Mutex::default(),
                prelude,
                pipelines: Mutex::default(),
                budget: OnceLock::new(),
                indirect_args: Mutex::default(),
                _alive: Counted::new(),
                #[cfg(feature = "diagnostics")]
                notes: Mutex::default(),
                #[cfg(feature = "diagnostics")]
                stamps: OnceLock::new(),
            })
        }
    }

    fn card_clock(&self) -> Option<&'static CardClock> {
        static CLOCK: OnceLock<Option<CardClock>> = OnceLock::new();
        static WATCH: std::sync::Once = std::sync::Once::new();
        let clock = CLOCK
            .get_or_init(|| {
                let fence = unsafe {
                    self.device
                        .CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE)
                }
                .ok()?;
                Some(CardClock {
                    fence,
                    state: Mutex::new((0, None)),
                    sent: std::sync::Condvar::new(),
                })
            })
            .as_ref()?;
        WATCH.call_once(|| {
            if let Err(e) = std::thread::Builder::new().name("leonida-card-clock".into()).spawn(move || CardClock::watch(clock)) {
                tracing::warn!(error = %e, "no thread for the card's clock: the frame line's idle times stay 0");
            }
        });
        Some(clock)
    }

    fn card_sent(&self) {
        let Some(clock) = self.card_clock() else {
            return;
        };
        let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 += 1;
        let _ = unsafe { self.queue.Signal(&clock.fence, state.0) };
        if let Some((since, worked_then)) = state.1.take() {
            CARD_CLOCK_US[0].fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
            CARD_CLOCK_US[1].fetch_add(
                HostTrail::worked().saturating_sub(worked_then),
                Ordering::Relaxed,
            );
        }
        drop(state);
        clock.sent.notify_one();
    }

    pub(crate) fn card_ready(&self) -> bool {
        static READY: OnceLock<bool> = OnceLock::new();
        *READY.get_or_init(|| match self.make_card_work() {
            Ok(work) => {
                *CARD_WORK.lock().unwrap_or_else(|e| e.into_inner()) = Some(work);
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "no host copies in the card's memory: the compare could not be made");
                false
            }
        })
    }

    fn make_card_work(&self) -> windows::core::Result<CardWork> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let mut code: Option<ID3DBlob> = None;
        unsafe {
            D3DCompile(
                CARD_COMPARE.as_ptr().cast(),
                CARD_COMPARE.len(),
                windows::core::s!("card compare"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )?;
        }
        let code = code.ok_or_else(windows::core::Error::empty)?;
        let list = || self.make_prelude().ok_or_else(windows::core::Error::empty);
        const RESULTS: u64 = 1 << 20;
        let (differs, data) = self
            .shared_buffer(RESULTS)
            .ok_or_else(windows::core::Error::empty)?;
        HostSetup::name(&differs, || "card compare results".into());
        unsafe {
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
            let pso: ID3D12PipelineState =
                self.device
                    .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: bytes.as_ptr().cast(),
                            BytecodeLength: bytes.len(),
                        },
                        ..Default::default()
                    })?;
            Ok(CardWork {
                sync: vec![list()?],
                check: vec![list()?],
                fetch: list()?,
                root,
                pso,
                differs: (differs, data as usize, RESULTS),
                fetched: None,
                taken: 0,
                out: 0,
            })
        }
    }

    fn card_before(&self, runs: &[&HostList], flown: bool) -> Option<CardPlan> {
        use std::sync::Arc;
        if !crate::shadow::ShadowCards::enabled() {
            return None;
        }
        let declared = LateRun::declared_writes(runs, true);
        let mut order =
            LateRun::relay().then(|| QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()));
        let sent_as = match order.as_mut() {
            Some(order) => {
                **order += 1;
                **order
            }
            None => 0,
        };
        let mut dirty = crate::shadow::ShadowCards::dirty(sent_as);
        if declared.is_empty() && dirty.is_empty() {
            return None;
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let work = work.as_mut()?;
        let mut whole: Vec<Arc<crate::shadow::Shadow>> = Vec::new();
        for (shadow, _) in &declared {
            if shadow.card_make_before(|size| {
                let before = self.uav_buffer(crate::shadow::ShadowCards::hash_bytes(size))?;
                HostSetup::name(&before, || {
                    "title buffer on the card, hashes of its pieces".into()
                });
                Some(before)
            }) {
                whole.push(shadow.clone());
                if !dirty.iter().any(|(s, _)| Arc::ptr_eq(s, shadow)) {
                    dirty.push((shadow.clone(), Vec::new()));
                }
            }
        }
        let mut used: Vec<(bool, Prelude)> = Vec::new();
        if !dirty.is_empty() {
            let Some(sync) = work.sync.pop().or_else(|| self.make_prelude()) else {
                HostSetup::fail(format_args!(
                    "no host list to bring host copies in the card's memory up to date"
                ));
            };
            let (allocator, list) = &sync;
            let recorded = unsafe {
                (|| -> windows::core::Result<()> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    for (shadow, ranges) in &dirty {
                        let Some((staging, before, size)) = shadow.card_sides() else {
                            continue;
                        };
                        for &(a, b) in ranges {
                            list.CopyBufferRegion(
                                &shadow.buffer,
                                a as u64,
                                staging,
                                a as u64,
                                (b - a) as u64,
                            );
                            crate::shadow::CARD_STATS[0]
                                .fetch_add((b - a) as u64, Ordering::Relaxed);
                        }
                        crate::shadow::CARD_STATS[1]
                            .fetch_add(ranges.len() as u64, Ordering::Relaxed);
                        let Some(before) = before else {
                            continue;
                        };
                        if !ranges.is_empty() {
                            list.ResourceBarrier(&[StateBits::transition(
                                &shadow.buffer,
                                D3D12_RESOURCE_STATE_COPY_DEST,
                                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                            )]);
                        }
                        list.SetComputeRootSignature(&work.root);
                        list.SetPipelineState(&work.pso);
                        list.SetComputeRootShaderResourceView(
                            1,
                            shadow.buffer.GetGPUVirtualAddress(),
                        );
                        list.SetComputeRootUnorderedAccessView(2, before.GetGPUVirtualAddress());
                        list.SetComputeRootUnorderedAccessView(
                            3,
                            work.differs.0.GetGPUVirtualAddress(),
                        );
                        let runs: Vec<crate::shadow::CardSpan> =
                            if whole.iter().any(|s| Arc::ptr_eq(s, shadow)) {
                                vec![shadow.card_run(0, usize::MAX)]
                            } else {
                                ranges.iter().map(|&(a, b)| shadow.card_run(a, b)).collect()
                            };
                        let runs = if LateRun::rehash_runs() {
                            crate::shadow::ShadowCards::joined_piece_runs(runs)
                        } else {
                            runs
                        };
                        CARD_PASSES[0].fetch_add(runs.len() as u64, Ordering::Relaxed);
                        CARD_PASSES[1].fetch_add(
                            runs.iter().map(|r| u64::from(r.pieces)).sum::<u64>(),
                            Ordering::Relaxed,
                        );
                        for run in runs {
                            let constants = [run.first, run.skew, size as u32, run.pieces, 0, 0];
                            list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
                            list.Dispatch(run.pieces, 1, 1);
                        }
                    }
                    list.Close()?;
                    self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                    Ok(())
                })()
            };
            if let Err(e) = recorded {
                HostSetup::fail(format_args!(
                    "host copies in the card's memory could not be brought up to date ({e})"
                ));
            }
            used.push((false, sync));
        }
        drop(order);
        let mut plan = CardPlan {
            ranges: Vec::new(),
            check: None,
            _synced: dirty.into_iter().map(|(shadow, _)| shadow).collect(),
            lists: used,
        };
        let mut at = work.taken;
        for (shadow, ranges) in &declared {
            let mut sorted = ranges.clone();
            sorted.sort_unstable();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (from, to) in sorted {
                match spans.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => spans.push((from, to)),
                }
            }
            let twin = shadow
                .card_sides()
                .is_some_and(|(_, before, _)| before.is_some());
            for (from, to) in spans {
                let Some(span) = shadow.card_span(from, to) else {
                    continue;
                };
                let compared = twin && u64::from(at + span.pieces) * 4 <= work.differs.2;
                plan.ranges.push(CardRange {
                    shadow: shadow.clone(),
                    declared: (from, to),
                    span,
                    at,
                    compared,
                });
                if compared {
                    at += span.pieces;
                }
            }
        }
        work.taken = at;
        if plan.ranges.iter().any(|r| r.compared) {
            let Some(check) = work.check.pop().or_else(|| self.make_prelude()) else {
                HostSetup::fail(format_args!(
                    "no host list for the compare of host copies in the card's memory"
                ));
            };
            let (allocator, list) = &check;
            let recorded = unsafe {
                (|| -> windows::core::Result<ID3D12CommandList> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                        Anonymous: D3D12_RESOURCE_BARRIER_0 {
                            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                                pResource: std::mem::ManuallyDrop::new(None),
                            }),
                        },
                    }]);
                    list.SetComputeRootSignature(&work.root);
                    list.SetPipelineState(&work.pso);
                    list.SetComputeRootUnorderedAccessView(
                        3,
                        work.differs.0.GetGPUVirtualAddress(),
                    );
                    for r in plan.ranges.iter().filter(|r| r.compared) {
                        let Some((_, Some(before), size)) = r.shadow.card_sides() else {
                            continue;
                        };
                        let constants = [
                            r.span.first,
                            r.span.skew,
                            size as u32,
                            r.span.pieces,
                            r.at * 4,
                            if flown { 2 } else { 1 },
                        ];
                        list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
                        list.SetComputeRootShaderResourceView(
                            1,
                            r.shadow.buffer.GetGPUVirtualAddress(),
                        );
                        list.SetComputeRootUnorderedAccessView(2, before.GetGPUVirtualAddress());
                        list.Dispatch(r.span.pieces, 1, 1);
                        CARD_PASSES[2].fetch_add(1, Ordering::Relaxed);
                    }
                    list.Close()?;
                    list.cast()
                })()
            };
            match recorded {
                Ok(list) => plan.check = Some(list),
                Err(e) => HostSetup::fail(format_args!(
                    "the compare of host copies in the card's memory could not be recorded ({e})"
                )),
            }
            plan.lists.push((true, check));
        }
        work.out += 1;
        Some(plan)
    }

    fn card_results_low(&self) -> bool {
        CARD_WORK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|work| u64::from(work.taken) * 8 > work.differs.2)
    }

    fn card_done(&self, plan: CardPlan) {
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(work) = work.as_mut() else {
            return;
        };
        for (check, list) in plan.lists {
            if check {
                work.check.push(list)
            } else {
                work.sync.push(list)
            }
        }
        work.out = work.out.saturating_sub(1);
        if !LateRun::relay() || work.out == 0 {
            work.taken = 0;
        }
    }

    fn make_relay_fetch(&self) -> Option<RelayFetch> {
        let list = self.make_prelude()?;
        unsafe {
            let fence: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            Some(RelayFetch {
                list,
                fence,
                event,
                sent: 0,
                buffer: None,
            })
        }
    }

    fn relay_card_back(
        &self,
        plans: &[&CardPlan],
        fetch: &mut Option<RelayFetch>,
        owners: &crate::writeback::Owners,
    ) {
        let differs = {
            let work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
            work.as_ref()
                .map(|work| (work.differs.1, (work.differs.2 / 4) as usize))
        };
        let mut pieces: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, u64)> =
            Vec::new();
        let mut total = 0u64;
        if let Some((differs, dwords)) = differs {
            let differs = unsafe { std::slice::from_raw_parts(differs as *const u32, dwords) };
            let mut seen = std::collections::HashSet::new();
            for plan in plans {
                let fetched = LateRun::flagged_pieces(plan.ranges.iter().flat_map(|r| {
                    let copy = std::sync::Arc::as_ptr(&r.shadow) as usize;
                    (0..r.span.pieces)
                        .map(move |p| (copy, r.span.first + p, r.changed_pages(p, differs)))
                }));
                for r in &plan.ranges {
                    let copy = std::sync::Arc::as_ptr(&r.shadow) as usize;
                    for p in 0..r.span.pieces {
                        let Some(&mask) = fetched.get(&(copy, r.span.first + p)) else {
                            continue;
                        };
                        for (lo, hi) in r.span.changed_ranges(p, mask) {
                            if seen.insert((copy, lo, hi)) {
                                pieces.push((r.shadow.clone(), lo, hi, total));
                                total += (hi - lo) as u64;
                            }
                        }
                    }
                    crate::shadow::CARD_STATS[2]
                        .fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
                }
            }
        }
        if pieces.is_empty() {
            crate::shadow::ShadowRelay::relay_fetched(
                *QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()),
            );
            return;
        }
        if fetch.is_none() {
            *fetch = self.make_relay_fetch();
        }
        let Some(f) = fetch.as_mut() else {
            HostSetup::fail(format_args!(
                "no list, fence or event to fetch what shaders wrote to the card's memory (LEONIDA_RELAY=1)"
            ));
        };
        if f.buffer.as_ref().is_none_or(|b| b.2 < total) {
            let size = total.next_multiple_of(16 << 20);
            f.buffer = self
                .shared_buffer(size)
                .map(|(buffer, data)| (buffer, data as usize, size));
        }
        let Some((buffer, data, _)) = f.buffer.clone() else {
            HostSetup::fail(format_args!(
                "no memory to fetch what shaders wrote to the card's memory ({total} bytes, LEONIDA_RELAY=1)"
            ));
        };
        f.sent += 1;
        let ((allocator, list), fence, event, sent) =
            (f.list.clone(), f.fence.clone(), f.event, f.sent);
        let waiting = std::time::Instant::now();
        let ran = unsafe {
            (|| -> windows::core::Result<u64> {
                allocator.Reset()?;
                list.Reset(&allocator, None)?;
                LateRun::copy_fetch_runs(
                    &list,
                    &buffer,
                    pieces
                        .iter()
                        .map(|(shadow, lo, hi, at)| (&shadow.buffer, *lo, *hi, *at)),
                );
                list.Close()?;
                let lists = [Some(list.cast::<ID3D12CommandList>()?)];
                let order = QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner());
                self.queue.ExecuteCommandLists(&lists);
                let after = *order;
                self.queue.Signal(&fence, sent)?;
                drop(order);
                self.card_sent();
                fence.SetEventOnCompletion(sent, event)?;
                WaitForSingleObject(event, INFINITE);
                Ok(after)
            })()
        };
        let after = match ran {
            Ok(after) => after,
            Err(e) => HostSetup::fail(format_args!(
                "what shaders wrote to the card's memory could not be fetched ({e}, LEONIDA_RELAY=1)"
            )),
        };
        let us = waiting.elapsed().as_micros() as u64;
        RELAY_STATS[3].fetch_add(us, Ordering::Relaxed);
        {
            use crate::shadow::CARD_STATS;
            CARD_STATS[3].fetch_add(pieces.len() as u64, Ordering::Relaxed);
            CARD_STATS[4].fetch_add(total, Ordering::Relaxed);
            CARD_STATS[5].fetch_add(1, Ordering::Relaxed);
            CARD_STATS[6].fetch_add(us, Ordering::Relaxed);
        }
        let mut title = Vec::new();
        let mut written = 0;
        for (shadow, lo, hi, at) in &pieces {
            let gpu =
                unsafe { std::slice::from_raw_parts((data + *at as usize) as *const u8, hi - lo) };
            let merged = shadow.card_merge_relay(*lo, gpu, after, &mut title, owners);
            shadow.walls_merged(gpu.len(), merged);
            written += merged;
        }
        COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
        crate::shadow::ShadowRelay::relay_fetched(after);
    }

    fn relay_land(&self, items: Vec<Late>, fetch: &mut Option<RelayFetch>) {
        let landing = std::time::Instant::now();
        {
            let flights: Vec<&Flight> = items
                .iter()
                .filter_map(|l| match l {
                    Late::Run(flight) => Some(flight),
                    _ => None,
                })
                .collect();
            if !flights.is_empty() {
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    let runs: Vec<&HostList> = flights
                        .iter()
                        .flat_map(|f| f.order.iter().map(|&i| f.lists[i].host()))
                        .collect();
                    HostSetup::fail(format_args!(
                        "host GPU device removed after {} submissions sent ahead (LEONIDA_RELAY=1) of {} lists{}{}",
                        flights.len(),
                        runs.len(),
                        self.removal_data(&runs),
                        HostTrail::last_notes(&runs)
                    ));
                }
                let plans: Vec<&CardPlan> =
                    flights.iter().filter_map(|f| f.card.as_ref()).collect();
                let owners = LateRun::writeback_owners(flights.iter().copied());
                self.relay_card_back(&plans, fetch, &owners);
                let mut back = WriteGroups::new();
                for flight in &flights {
                    for (shadow, ranges) in &flight.back {
                        back.add(shadow, ranges);
                    }
                }
                LateRun::relay_hand_back(&back.groups, &owners);
                for flight in &flights {
                    for (shadow, ranges) in &flight.back {
                        shadow.copy_back_done(ranges);
                    }
                }
                COPY_BACK[0].fetch_add(landing.elapsed().as_micros() as u64, Ordering::Relaxed);
                RELAY_STATS[0].fetch_add(1, Ordering::Relaxed);
                RELAY_STATS[1].fetch_add(flights.len() as u64, Ordering::Relaxed);
            }
        }
        let mut last = 0;
        let mut landed = Vec::new();
        for item in items {
            match item {
                Late::Run(flight) => {
                    last = flight.number;
                    for &i in &flight.order {
                        LateRun::read_backs(flight.lists[i].host());
                    }
                    if let Some(card) = flight.card {
                        self.card_done(card);
                    }
                    landed.push((flight.lists, flight.order));
                }
                Late::Write(at, what, bytes) => what.write(at as *mut u8, bytes),
                Late::Signal(fence, value) => {
                    crate::fence::Fence::advance(fence as crate::object::This, value)
                }
            }
        }
        if last > 0 {
            crate::writeback::published(last);
            FINISHED.store(last, Ordering::Release);
            Retirement::bury(last);
        }
        self.log_messages();
        RELAY_STATS[4].fetch_add(landing.elapsed().as_micros() as u64, Ordering::Relaxed);
        {
            let mut line = LateRun::lock_line();
            let Line {
                line: items,
                written,
                lists,
                settling,
                poke,
                landed: kept,
                in_hand,
                ..
            } = &mut *line;
            kept.extend(landed);
            in_hand.clear();
            *written = PendingWrites::default();
            *lists = 0;
            let mut ranges = Vec::new();
            let mut texture_ranges = Vec::new();
            for item in items.iter() {
                if let Late::Run(flight) = item {
                    *lists += flight.order.len().max(1);
                    ranges.extend(flight.back.iter().flat_map(|(_, r)| r.iter().copied()));
                    texture_ranges.extend_from_slice(&flight.texture_writes);
                }
            }
            LateRun::join_ranges(&mut written.buffers, ranges);
            LateRun::join_ranges(&mut written.textures, texture_ranges);
            *settling = false;
            *poke = true;
        }
        LINE_MOVED.notify_all();
        LINE_LANDED.notify_all();
    }

    fn card_back(
        &self,
        plan: &CardPlan,
        submitted: &mut u64,
        seen: &mut std::collections::HashSet<(usize, usize, usize)>,
        owners: &crate::writeback::Owners,
    ) {
        if plan.ranges.is_empty() {
            return;
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(work) = work.as_mut() else {
            return;
        };
        let differs = unsafe {
            std::slice::from_raw_parts(work.differs.1 as *const u32, (work.differs.2 / 4) as usize)
        };
        let mut pieces: Vec<(usize, usize, usize, u64)> = Vec::new();
        let mut total = 0u64;
        for (i, r) in plan.ranges.iter().enumerate() {
            for p in 0..r.span.pieces {
                for (lo, hi) in r.span.changed_ranges(p, r.changed_pages(p, differs)) {
                    if seen.insert((std::sync::Arc::as_ptr(&r.shadow) as usize, lo, hi)) {
                        pieces.push((i, lo, hi, total));
                        total += (hi - lo) as u64;
                    }
                }
            }
            crate::shadow::CARD_STATS[2].fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
        }
        let mut written = vec![0usize; plan.ranges.len()];
        if !pieces.is_empty() {
            if work.fetched.as_ref().is_none_or(|f| f.2 < total) {
                let size = total.next_multiple_of(16 << 20);
                work.fetched = self
                    .shared_buffer(size)
                    .map(|(buffer, data)| (buffer, data as usize, size));
            }
            let Some((fetched, data, _)) = &work.fetched else {
                HostSetup::fail(format_args!(
                    "no memory to fetch what shaders wrote to the card's memory ({total} bytes)"
                ));
            };
            let waiting = std::time::Instant::now();
            let (allocator, list) = &work.fetch;
            let ran = unsafe {
                (|| -> windows::core::Result<()> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    LateRun::copy_fetch_runs(
                        list,
                        fetched,
                        pieces
                            .iter()
                            .map(|&(i, lo, hi, at)| (&plan.ranges[i].shadow.buffer, lo, hi, at)),
                    );
                    list.Close()?;
                    self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                    *submitted += 1;
                    self.queue.Signal(&self.fence, *submitted)?;
                    self.card_sent();
                    self.fence.SetEventOnCompletion(*submitted, self.event)?;
                    let was_working = HostTrail::working(false);
                    WaitForSingleObject(self.event, INFINITE);
                    HostTrail::stood(waiting);
                    HostTrail::working(was_working);
                    Ok(())
                })()
            };
            if let Err(e) = ran {
                HostSetup::fail(format_args!(
                    "what shaders wrote to the card's memory could not be fetched ({e})"
                ));
            }
            {
                use crate::shadow::CARD_STATS;
                CARD_STATS[3].fetch_add(pieces.len() as u64, Ordering::Relaxed);
                CARD_STATS[4].fetch_add(total, Ordering::Relaxed);
                CARD_STATS[5].fetch_add(1, Ordering::Relaxed);
                CARD_STATS[6].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            if LateRun::wide_merge() && pieces.len() >= 8 {
                let fetched = *data;
                let counts: std::sync::Arc<Vec<std::sync::atomic::AtomicUsize>> =
                    std::sync::Arc::new(
                        (0..plan.ranges.len())
                            .map(|_| std::sync::atomic::AtomicUsize::new(0))
                            .collect(),
                    );
                let each = pieces.len().div_ceil(8);
                let jobs: Vec<Box<dyn FnOnce() + Send>> = pieces
                    .chunks(each)
                    .map(|chunk| {
                        let chunk: Vec<(
                            std::sync::Arc<crate::shadow::Shadow>,
                            usize,
                            usize,
                            usize,
                            u64,
                        )> = chunk
                            .iter()
                            .map(|&(i, lo, hi, at)| (plan.ranges[i].shadow.clone(), i, lo, hi, at))
                            .collect();
                        let counts = counts.clone();
                        let owners = owners.clone();
                        Box::new(move || {
                            let mut title = Vec::new();
                            for (shadow, i, lo, hi, at) in chunk {
                                let gpu = unsafe {
                                    std::slice::from_raw_parts(
                                        (fetched + at as usize) as *const u8,
                                        hi - lo,
                                    )
                                };
                                let merged = shadow.card_merge(lo, gpu, &mut title, &owners);
                                shadow.walls_merged(gpu.len(), merged);
                                counts[i].fetch_add(merged, Ordering::Relaxed);
                            }
                        }) as Box<dyn FnOnce() + Send>
                    })
                    .collect();
                crate::shadow::WideCopy::run(jobs);
                for (written, count) in written.iter_mut().zip(counts.iter()) {
                    *written += count.load(Ordering::Relaxed);
                }
            } else {
                let mut title = Vec::new();
                for &(i, lo, hi, at) in &pieces {
                    let gpu = unsafe {
                        std::slice::from_raw_parts((*data + at as usize) as *const u8, hi - lo)
                    };
                    let merged = plan.ranges[i]
                        .shadow
                        .card_merge(lo, gpu, &mut title, owners);
                    plan.ranges[i].shadow.walls_merged(gpu.len(), merged);
                    written[i] += merged;
                }
            }
        }
        for (r, written) in plan.ranges.iter().zip(written) {
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            let (from, to) = r.declared;
            #[cfg(feature = "diagnostics")]
            crate::copystat::CopyStats::compared(from, to - from, written);
            let _ = (from, to);
        }
    }

    pub(crate) fn execute(&self, runs: &[&HostList]) {
        let _ = self.submit(runs, false);
    }

    #[cfg(test)]
    pub(crate) fn gate_queue_for_test(&self) -> ID3D12Fence {
        unsafe {
            let gate: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            self.queue.Wait(&gate, 1).unwrap();
            gate
        }
    }

    pub(crate) fn execute_late(&self, lists: Vec<crate::list::Detached>, mut order: Vec<usize>) {
        if order.is_empty() {
            crate::list::ListPool::recycle(lists, &order);
            return;
        }
        LateRun::late_threads();
        let sent = {
            let runs: Vec<&HostList> = order.iter().map(|&i| lists[i].host()).collect();
            self.submit(&runs, true)
        };
        let Some((mut submitted, sent, already_run)) = sent else {
            crate::list::ListPool::recycle(lists, &order);
            return;
        };
        order.drain(..already_run);
        let told = !sent.writes.is_empty();
        let most = {
            let mut line = LateRun::lock_line();
            let Line {
                line: items,
                written,
                lists: flying,
                target,
                ..
            } = &mut *line;
            written.add(&sent.back, &sent.texture_writes);
            *flying += order.len();
            *target = *submitted;
            items.push(Late::Run(Flight {
                number: sent.number,
                fence: *target,
                lists,
                order,
                card: sent.card,
                back: sent.back,
                texture_writes: sent.texture_writes,
            }));
            items.extend(
                sent.writes
                    .into_iter()
                    .map(|(at, what, bytes)| Late::Write(at, what, bytes)),
            );
            *flying >= LateRun::late_most()
        };
        LATE_STATS[0].fetch_add(1, Ordering::Relaxed);
        RELAY_MOVED.notify_one();
        if told && !LateRun::late_writes() {
            self.settle(&mut submitted, WHY_WRITES);
        } else if most && LateRun::relay_exact() {
            LateRun::relay_wait(WHY_MOST, |line| line.lists < LateRun::late_most());
        } else if most {
            self.settle(&mut submitted, WHY_MOST);
        } else {
            LINE_MOVED.notify_one();
        }
        drop(submitted);
        LateRun::land();
    }

    fn execute_own(&self, list: &HostList) -> u64 {
        if !list.deferred().texture_backs.is_empty() {
            self.execute(&[list]);
            return 0;
        }
        LateRun::late_threads();
        let Some((submitted, sent, _)) = self.submit(&[list], true) else {
            return 0;
        };
        let number = sent.number;
        {
            let mut line = LateRun::lock_line();
            let Line {
                line: items,
                written,
                lists: flying,
                target,
                ..
            } = &mut *line;
            written.add(&sent.back, &sent.texture_writes);
            *flying += 1;
            *target = *submitted;
            items.push(Late::Run(Flight {
                number,
                fence: *target,
                lists: Vec::new(),
                order: Vec::new(),
                card: sent.card,
                back: sent.back,
                texture_writes: sent.texture_writes,
            }));
            items.extend(
                sent.writes
                    .into_iter()
                    .map(|(at, what, bytes)| Late::Write(at, what, bytes)),
            );
        }
        LATE_STATS[0].fetch_add(1, Ordering::Relaxed);
        LINE_MOVED.notify_one();
        RELAY_MOVED.notify_one();
        drop(submitted);
        number
    }

    fn settle(&self, submitted: &mut u64, why: usize) {
        if !LateRun::on() {
            return;
        }
        if LateRun::relay() {
            LateRun::relay_wait(why, |line| line.line.is_empty() && !line.settling);
            return;
        }
        let was_working = HostTrail::working(true);
        let mut counted = false;
        loop {
            let items = {
                let mut line = LateRun::lock_line();
                if line.line.is_empty() {
                    line.settling = false;
                    break;
                }
                line.settling = true;
                line.written = PendingWrites::default();
                line.lists = 0;
                std::mem::take(&mut line.line)
            };
            let flights = items.iter().filter(|i| matches!(i, Late::Run(_))).count();
            if !counted {
                counted = true;
                LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
                LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
            }
            LATE_STATS[11].fetch_add(flights as u64, Ordering::Relaxed);
            if flights > 0 {
                HostTrail::submitting(2, flights);
                let waiting = std::time::Instant::now();
                unsafe {
                    match self.fence.SetEventOnCompletion(*submitted, self.event) {
                        Ok(()) => {
                            HostTrail::working(false);
                            WaitForSingleObject(self.event, INFINITE);
                            HostTrail::stood(waiting);
                            HostTrail::working(true);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "submissions sent ahead not waited for")
                        }
                    }
                }
                let gpu = waiting.elapsed();
                LATE_STATS[12].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                LATE_WAITED[why.min(10)].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                crate::copystat::CopyStats::card_waited(gpu.as_micros() as u64, || {
                    format!("{flights} submissions sent ahead, settled together")
                });
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    let runs: Vec<&HostList> = items
                        .iter()
                        .filter_map(|i| match i {
                            Late::Run(f) => Some(f),
                            _ => None,
                        })
                        .flat_map(|f| f.order.iter().map(|&i| f.lists[i].host()))
                        .collect();
                    HostSetup::fail(format_args!(
                        "host GPU device removed after {flights} submissions sent ahead (LEONIDA_LATE_WAIT=1) of {} lists{}{}",
                        runs.len(),
                        self.removal_data(&runs),
                        HostTrail::last_notes(&runs)
                    ));
                }
                HostTrail::submitting(3, flights);
                let copying = std::time::Instant::now();
                let owners =
                    LateRun::writeback_owners(items.iter().filter_map(|item| match item {
                        Late::Run(flight) => Some(flight),
                        _ => None,
                    }));
                let mut seen = std::collections::HashSet::new();
                let mut back = WriteGroups::new();
                for item in &items {
                    let Late::Run(flight) = item else {
                        continue;
                    };
                    if let Some(card) = &flight.card {
                        self.card_back(card, submitted, &mut seen, &owners);
                    }
                    for (shadow, ranges) in &flight.back {
                        back.add(shadow, ranges);
                    }
                }
                LateRun::hand_back(&back.groups, &owners);
                COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            let mut last = 0;
            let mut landed = Vec::new();
            for item in items {
                match item {
                    Late::Run(flight) => {
                        last = flight.number;
                        for &i in &flight.order {
                            LateRun::read_backs(flight.lists[i].host());
                        }
                        if let Some(card) = flight.card {
                            self.card_done(card);
                        }
                        landed.push((flight.lists, flight.order));
                    }
                    Late::Write(at, what, bytes) => what.write(at as *mut u8, bytes),
                    Late::Signal(fence, value) => {
                        crate::fence::Fence::advance(fence as crate::object::This, value)
                    }
                }
            }
            if last > 0 {
                crate::writeback::published(last);
                FINISHED.store(last, Ordering::Release);
                Retirement::bury(last);
            }
            LateRun::lock_line().landed.extend(landed);
        }
        if counted {
            self.log_messages();
            HostTrail::submitting(0, 0);
            if why != WHY_CARD_DONE {
                LateRun::lock_line().poke = true;
                LINE_MOVED.notify_one();
            }
        }
        HostTrail::working(was_working);
    }

    fn submit<'a>(
        &'a self,
        mut runs: &[&HostList],
        late: bool,
    ) -> Option<(std::sync::MutexGuard<'a, u64>, Sent, usize)> {
        #[cfg(feature = "diagnostics")]
        let mut called = std::time::Instant::now();
        let mut submitted = self.submitted.lock().unwrap_or_else(|e| e.into_inner());
        let mut already_run = 0;
        while let Some(cut) = LateRun::texture_dependency_cut(runs) {
            (submitted, _) = self.submit_part(
                &runs[..cut],
                false,
                submitted,
                #[cfg(feature = "diagnostics")]
                called,
            );
            already_run += cut;
            runs = &runs[cut..];
            #[cfg(feature = "diagnostics")]
            {
                called = std::time::Instant::now();
            }
        }
        let (submitted, sent) = self.submit_part(
            runs,
            late,
            submitted,
            #[cfg(feature = "diagnostics")]
            called,
        );
        sent.map(|sent| (submitted, sent, already_run))
    }

    fn submit_part<'a>(
        &'a self,
        runs: &[&HostList],
        late: bool,
        mut submitted: std::sync::MutexGuard<'a, u64>,
        #[cfg(feature = "diagnostics")] called: std::time::Instant,
    ) -> (std::sync::MutexGuard<'a, u64>, Option<Sent>) {
        #[cfg(feature = "diagnostics")]
        crate::copystat::CopyStats::submission_begins();
        let was_working = HostTrail::working(true);
        HostTrail::submitting(1, runs.len());
        crate::writeback::starting(STARTED.load(Ordering::Relaxed) + 1);
        if !late {
            self.settle(&mut submitted, WHY_SYNC);
        } else if let Some(why) = LateRun::late_blocked(runs) {
            if LateRun::relay_exact() && why == WHY_READ {
                LateRun::relay_wait(why, |line| {
                    LateRun::blocked_by(&line.written, runs, false).is_none()
                });
            } else {
                self.settle(&mut submitted, why);
            }
        }
        let behind = late && LateRun::flying();
        static SWEPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if !behind {
            Retirement::sweep();
            SWEPT.store(HostClock::now_ms(), Ordering::Relaxed);
        } else if LateRun::late_free_present()
            && HostClock::now_ms() > SWEPT.load(Ordering::Relaxed) + 1500
        {
            self.settle(&mut submitted, WHY_SYNC);
            Retirement::sweep();
            SWEPT.store(HostClock::now_ms(), Ordering::Relaxed);
        }
        let behind = late && LateRun::flying();
        crate::descriptor::ViewWaits::make_all();
        let this_submission = STARTED.fetch_add(1, Ordering::AcqRel) + 1;
        crate::shadow::ShadowRelay::preparing(this_submission);
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let lock = (started - called).as_micros() as u64;
        #[cfg(feature = "diagnostics")]
        crate::copystat::C::add(crate::copystat::C::Submissions, 1);
        let mut lists: Vec<Option<ID3D12CommandList>> = Vec::with_capacity(2 * runs.len());
        let mut firsts = Vec::with_capacity(runs.len());
        let mut batch = Batch::default();
        let mut settled = std::time::Duration::ZERO;
        if crate::watch::WriteJournal::once() {
            let mut spans: std::collections::HashMap<usize, (usize, usize)> =
                std::collections::HashMap::new();
            for list in runs {
                for step in &list.deferred().copies {
                    if let Step::Refresh(shadow, address, len) = step
                        && !shadow.unwatched()
                    {
                        let span = spans
                            .entry(std::sync::Arc::as_ptr(shadow) as usize)
                            .or_insert((*address, *address + *len));
                        *span = (span.0.min(*address), span.1.max(*address + *len));
                    }
                }
            }
            let ranges: Vec<(usize, usize)> = spans.into_values().collect();
            crate::watch::WriteJournal::ask_once(&ranges);
            if behind && LateRun::stale_under_flights(runs) {
                let settling = std::time::Instant::now();
                self.settle(&mut submitted, WHY_STALE);
                settled += settling.elapsed();
                crate::watch::WriteJournal::ask_once(&ranges);
            }
        } else if behind && LateRun::stale_under_flights(runs) {
            let settling = std::time::Instant::now();
            self.settle(&mut submitted, WHY_STALE);
            settled += settling.elapsed();
        }
        let parallel = Preludes::parallel_preludes(runs);
        let mut plans = Vec::new();
        for list in runs {
            list.run_deferred(&mut batch);
            firsts.push(lists.len());
            if let Some(plan) = list.plan_prelude() {
                lists.push(plan.commands.1.cast().ok());
                if parallel {
                    plans.push(plan);
                } else {
                    plan.record();
                }
            }
            lists.push(list.list.cast().ok());
        }
        Preludes::record_preludes(plans);
        batch.flush();
        crate::watch::WriteJournal::ask_once_end();
        {
            #[cfg(feature = "diagnostics")]
            let snapshots = std::time::Instant::now();
            crate::shadow::WideCopy::take_snapshots();
            #[cfg(feature = "diagnostics")]
            crate::copystat::C::add(
                crate::copystat::C::SnapshotUs,
                snapshots.elapsed().as_micros() as u64,
            );
        }
        let flown = late && LateRun::relay();
        if flown && self.card_results_low() {
            let settling = std::time::Instant::now();
            self.settle(&mut submitted, WHY_MOST);
            settled += settling.elapsed();
        }
        let card = self.card_before(runs, flown);
        if let Some(check) = card.as_ref().and_then(|c| c.check.clone()) {
            lists.push(Some(check));
        }
        let prepared = std::time::Instant::now();
        if late {
            *submitted += 1;
            let signalled = unsafe {
                self.queue.ExecuteCommandLists(&lists);
                self.queue.Signal(&self.fence, *submitted)
            };
            self.card_sent();
            if let Err(e) = signalled {
                HostSetup::fail(format_args!(
                    "a submission sent ahead could not be followed by a signal ({e})"
                ));
            }
            let back = LateRun::declared_writes(runs, false);
            let mut texture_writes = Vec::new();
            for list in runs {
                texture_writes.extend(
                    list.deferred()
                        .texture_backs
                        .iter()
                        .map(crate::texback::Back::destination),
                );
            }
            crate::texback::add_destination_aliases(&mut texture_writes);
            let writes = runs
                .iter()
                .flat_map(|l| l.deferred().writes.clone())
                .collect();
            SUBMISSION_US[0].fetch_add(
                (prepared - started).saturating_sub(settled).as_micros() as u64,
                Ordering::Relaxed,
            );
            HostTrail::submitting(0, 0);
            #[cfg(feature = "diagnostics")]
            crate::copystat::CopyStats::submission_ends(lock);
            let sent = Sent {
                number: this_submission,
                card,
                back,
                texture_writes,
                writes,
            };
            HostTrail::working(was_working);
            return (submitted, Some(sent));
        }
        #[cfg(feature = "diagnostics")]
        let groups: Vec<&[Option<ID3D12CommandList>]> = if HostSetup::timing() {
            firsts
                .iter()
                .zip(firsts.iter().skip(1).chain([&lists.len()]))
                .map(|(&a, &b)| &lists[a..b])
                .collect()
        } else {
            vec![&lists[..]]
        };
        #[cfg(not(feature = "diagnostics"))]
        let groups = [&lists[..]];
        HostTrail::submitting(2, runs.len());
        for (i, group) in groups.iter().enumerate() {
            *submitted += 1;
            let started = std::time::Instant::now();
            unsafe {
                self.queue.ExecuteCommandLists(group);
                self.card_sent();
                let waited = self
                    .queue
                    .Signal(&self.fence, *submitted)
                    .and_then(|()| self.fence.SetEventOnCompletion(*submitted, self.event));
                match waited {
                    Ok(()) => {
                        HostTrail::working(false);
                        WaitForSingleObject(self.event, INFINITE);
                        HostTrail::stood(started);
                        HostTrail::working(true);
                    }
                    Err(e) => tracing::warn!(error = %e, "host submission not waited for"),
                }
            }
            #[cfg(feature = "diagnostics")]
            if HostSetup::timing() {
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                for (ms, op) in runs[i].slowest(frequency, usize::MAX) {
                    crate::copystat::CopyStats::command_took(&op, ms);
                }
            }
            #[cfg(feature = "diagnostics")]
            if HostSetup::timing() && started.elapsed() > std::time::Duration::from_millis(50) {
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                let commands = runs[i]
                    .notes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .len();
                tracing::warn!(
                    ms = started.elapsed().as_millis(),
                    commands,
                    "slow host list"
                );
                for (ms, op) in runs[i].slowest(frequency, 6) {
                    tracing::warn!("slow list command, {ms:.1} ms: {op}");
                }
            }
            let _ = (i, started);
        }
        let _ = &firsts;
        FINISHED.store(this_submission, Ordering::Release);
        Retirement::bury(this_submission);
        let gpu = prepared.elapsed();
        SUBMISSION_US[0].fetch_add((prepared - started).as_micros() as u64, Ordering::Relaxed);
        SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        crate::copystat::CopyStats::card_waited(gpu.as_micros() as u64, || {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| {
                    l.pipelines
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                })
                .collect();
            let text = format!("{} lists, pipelines {pipelines:?}", runs.len());
            text.chars().take(1200).collect()
        });
        if gpu > std::time::Duration::from_millis(250) {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| {
                    l.pipelines
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                })
                .collect();
            tracing::warn!(
                ms = gpu.as_millis(),
                lists = runs.len(),
                ?pipelines,
                "slow host submission"
            );
        }
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            HostSetup::fail(format_args!(
                "host GPU device removed after a submission of {} lists{}{}",
                runs.len(),
                self.removal_data(runs),
                HostTrail::last_notes(runs)
            ));
        }
        HostTrail::submitting(3, runs.len());
        let copying = std::time::Instant::now();
        let owners = crate::writeback::Owners::single(this_submission);
        if let Some(card) = &card {
            self.card_back(
                card,
                &mut submitted,
                &mut std::collections::HashSet::new(),
                &owners,
            );
        }
        let back = LateRun::declared_writes(runs, false);
        LateRun::hand_back(&back, &owners);
        crate::writeback::published(this_submission);
        if let Some(card) = card {
            self.card_done(card);
        }
        #[cfg(feature = "diagnostics")]
        if !back.is_empty() {
            crate::copystat::C::add(crate::copystat::C::BackSubmissions, 1);
        }
        #[cfg(feature = "diagnostics")]
        if back.is_empty() && runs.iter().all(|l| l.deferred().writes.is_empty()) {
            use crate::copystat::C;
            C::add(C::NothingBack, 1);
            C::add(C::NothingBackUs, gpu.as_micros() as u64);
        }
        COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
        for list in runs {
            LateRun::read_backs(list);
            for &(at, what, bytes) in &list.deferred().writes {
                what.write(at as *mut u8, bytes);
            }
        }
        self.log_messages();
        HostTrail::submitting(0, 0);
        #[cfg(feature = "diagnostics")]
        crate::copystat::CopyStats::submission_ends(lock);
        if LateRun::relay() {
            crate::shadow::ShadowRelay::relay_fetched(
                *QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()),
            );
        }
        HostTrail::working(was_working);
        (submitted, None)
    }

    fn removal_data(&self, runs: &[&HostList]) -> String {
        let Ok(dred) = self.device.cast::<ID3D12DeviceRemovedExtendedData1>() else {
            return String::new();
        };
        let mut text = String::new();
        let mut fault_va = None;
        unsafe {
            if let Ok(fault) = dred.GetPageFaultAllocationOutput1()
                && fault.PageFaultVA != 0
            {
                let va = fault.PageFaultVA;
                fault_va = Some(va);
                text.push_str(&format!("; page fault at {va:#x}"));
                for (what, head) in [
                    ("inside", fault.pHeadExistingAllocationNode),
                    ("inside freed", fault.pHeadRecentFreedAllocationNode),
                ] {
                    let mut names = Vec::new();
                    let mut node = head;
                    while let Some(n) = node.as_ref()
                        && names.len() < 8
                    {
                        let name = if n.ObjectNameW.is_null() {
                            "unnamed".to_owned()
                        } else {
                            n.ObjectNameW.to_string().unwrap_or_default()
                        };
                        names.push(format!("{name} (type {})", n.AllocationType.0));
                        node = n.pNext;
                    }
                    if !names.is_empty() {
                        text.push_str(&format!("; {what} {names:?}"));
                    }
                }
                text.push_str(&crate::shadow::ShadowRegistry::near(va));
                text.push_str(&crate::descriptor::host::HeapDiagnostics::near_heaps(va));
            }
            let crumbs = match dred.GetAutoBreadcrumbsOutput() {
                Ok(crumbs) => crumbs,
                Err(e) => {
                    text.push_str(&format!("; no breadcrumbs ({e})"));
                    return text;
                }
            };
            let mut progress = Vec::new();
            let mut node = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = node.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                let ours = p.pCommandList.as_ref().map(|l| l.as_raw());
                let index = runs.iter().position(|r| Some(r.list.as_raw()) == ours);
                progress.push(format!("{done}/{} {index:?}", p.BreadcrumbCount));
                node = p.pNext;
            }
            text.push_str(&format!("; breadcrumbs (done/count, list) {progress:?}"));
            let mut plain = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = plain.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                if done < p.BreadcrumbCount && !p.pCommandHistory.is_null() {
                    let history =
                        std::slice::from_raw_parts(p.pCommandHistory, p.BreadcrumbCount as usize);
                    text.push_str(&format!(
                        "; a list stopped at command {done} of {} (op {})",
                        p.BreadcrumbCount, history[done as usize].0
                    ));
                    let list = p.pCommandList.as_ref().map(|l| l.as_raw());
                    let run = runs.iter().find(|r| Some(r.list.as_raw()) == list);
                    #[cfg(feature = "diagnostics")]
                    let markers: Vec<(u32, String)> = run.map_or_else(Vec::new, |r| {
                        let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
                        history
                            .iter()
                            .enumerate()
                            .filter(|(_, op)| **op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
                            .zip(notes.iter())
                            .map(|((at, _), (note, range))| {
                                let place = fault_va
                                    .zip(*range)
                                    .map(|(va, r)| HostTrail::fault_in(va, r));
                                (at as u32, format!("{note}{}", place.unwrap_or_default()))
                            })
                            .collect()
                    });
                    #[cfg(not(feature = "diagnostics"))]
                    let markers: Vec<(u32, String)> = Vec::new();
                    #[cfg(not(feature = "diagnostics"))]
                    let _ = fault_va;
                    if done > 0 {
                        text.push_str(&HostTrail::in_flight(history, &markers, done as usize));
                    } else if let Some(r) = run {
                        let pipelines = r.pipelines.lock().unwrap_or_else(|e| e.into_inner());
                        text.push_str(&format!(", its pipelines {pipelines:?}"));
                    }
                }
                plain = p.pNext;
            }
        }
        text
    }

    pub(crate) fn log_messages(&self) -> Option<String> {
        let Some(queue) = &self.messages else {
            return None;
        };
        let mut first_error = None;
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                    continue;
                }
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                    let m = &*message;
                    let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                    let text = String::from_utf8_lossy(text);
                    let text = text.trim_end_matches('\0');
                    tracing::warn!(id = m.ID.0, severity = m.Severity.0, "D3D12: {text}");
                    if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 && first_error.is_none() {
                        first_error = Some(text.to_owned());
                    }
                }
            }
            queue.ClearStoredMessages();
        }
        first_error
    }

    pub(crate) fn rtv(
        &self,
        resource: Option<&ID3D12Resource>,
        desc: Option<&D3D12_RENDER_TARGET_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_rtv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        unsafe {
            let mut handle = self.rtvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.rtv_size) as usize;
            self.device
                .CreateRenderTargetView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }

    pub(crate) fn dsv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_dsv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        unsafe {
            let mut handle = self.dsvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.dsv_size) as usize;
            self.device
                .CreateDepthStencilView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }
}

impl HostList {
    #[cfg(feature = "diagnostics")]
    fn stamps(&self) -> Option<&(ID3D12QueryHeap, ID3D12Resource)> {
        if !HostSetup::timing() {
            return None;
        }
        self.stamps
            .get_or_init(|| {
                let device = &Host::instance()?.device;
                let mut heap: Option<ID3D12QueryHeap> = None;
                let mut back: Option<ID3D12Resource> = None;
                let readback = D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_READBACK,
                    ..Default::default()
                };
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: u64::from(STAMPS) * 8,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                };
                unsafe {
                    device
                        .CreateQueryHeap(
                            &D3D12_QUERY_HEAP_DESC {
                                Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                                Count: STAMPS,
                                NodeMask: 0,
                            },
                            &mut heap,
                        )
                        .ok()?;
                    device
                        .CreateCommittedResource(
                            &readback,
                            D3D12_HEAP_FLAG_NONE,
                            &desc,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            None,
                            &mut back,
                        )
                        .ok()?;
                }
                Some((heap?, back?))
            })
            .as_ref()
    }

    #[cfg(feature = "diagnostics")]
    fn slowest(&self, frequency: u64, n: usize) -> Vec<(f64, String)> {
        let notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        let (Some((_, back)), count) = (self.stamps(), notes.len().min(STAMPS as usize - 1)) else {
            return Vec::new();
        };
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE {
            Begin: 0,
            End: (count + 1) * 8,
        };
        let stamps: Vec<u64> = unsafe {
            if back.Map(0, Some(&range), Some(&mut data)).is_err() {
                return Vec::new();
            }
            let s = std::slice::from_raw_parts(data.cast::<u64>(), count + 1).to_vec();
            back.Unmap(0, None);
            s
        };
        let mut times: Vec<(f64, String)> = (0..count)
            .map(|i| {
                let ticks = stamps[i + 1].saturating_sub(stamps[i]);
                (
                    ticks as f64 * 1e3 / frequency.max(1) as f64,
                    notes[i].0.chars().take(300).collect(),
                )
            })
            .collect();
        times.sort_by(|a, b| b.0.total_cmp(&a.0));
        times.truncate(n);
        times
    }

    pub(crate) fn close(&self) {
        self.twins_to_depth();
        self.textures_back();
        #[cfg(feature = "diagnostics")]
        if let Some((heap, back)) = self.stamps() {
            let count =
                (self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32).min(STAMPS - 1);
            unsafe {
                self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, count);
                self.list
                    .ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, count + 1, back, 0);
            }
        }
        if let Err(e) = unsafe { self.list.Close() } {
            HostSetup::fail(format_args!(
                "a host command list did not close ({e}): a command recorded into it was invalid"
            ));
        }
    }

    pub(crate) fn reset(&self) {
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        self._alive.used();
        let old = std::mem::take(&mut *self.deferred());
        drop(old);
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.twins.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.pipelines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        #[cfg(feature = "diagnostics")]
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        {
            let mut scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
            (scratch.current, scratch.used) = (0, 0);
            if ScratchPool::enabled() {
                ScratchPool::give_back(std::mem::take(&mut scratch.chunks));
            }
        }
        let reset = unsafe {
            self.allocator
                .Reset()
                .and_then(|()| self.list.Reset(&self.allocator, None))
        };
        if let Err(e) = reset {
            HostSetup::fail(format_args!("a host command list did not reset ({e})"));
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::C::add(
            crate::copystat::C::ResetNs,
            started.elapsed().as_nanos() as u64,
        );
    }

    pub(crate) fn transition(
        &self,
        resource: crate::object::This,
        wanted: u32,
    ) -> Option<(&'static ID3D12Resource, u32)> {
        let texture = crate::resource::HostTextures::host_resource(resource)?;
        let copy = crate::resource::HostTextures::host_copy(resource)?;
        let wanted = StateBits::legal_state(texture, wanted);
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        match states.entry(std::sync::Arc::as_ptr(&copy) as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((wanted, wanted, copy));
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let before = std::mem::replace(&mut o.get_mut().1, wanted);
                (before != wanted).then_some((texture, before))
            }
        }
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn read_bound_input(
        &self,
        device: &ID3D12Device,
        copy: &std::sync::Arc<crate::resource::HostTexture>,
    ) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
        let key = std::sync::Arc::as_ptr(copy) as usize;
        let state = self
            .states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)?
            .1;
        if state & D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0 as u32 == 0 {
            return None;
        }
        self.settle(copy, D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32);
        let result = crate::capture::TargetDump::read_back(device, &self.list, &copy.texture);
        self.settle(copy, state);
        result
    }

    fn settle(&self, copy: &std::sync::Arc<crate::resource::HostTexture>, wanted: u32) {
        let wanted = StateBits::legal_state(&copy.texture, wanted);
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let before = match states.entry(std::sync::Arc::as_ptr(copy) as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((wanted, wanted, copy.clone()));
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let before = std::mem::replace(&mut o.get_mut().1, wanted);
                (before != wanted).then_some(before)
            }
        };
        drop(states);
        if let Some(before) = before {
            let barrier = StateBits::transition(
                &copy.texture,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATES(wanted as i32),
            );
            unsafe { self.list.ResourceBarrier(&[barrier]) };
        }
    }

    pub(crate) fn twin_use(
        &self,
        resource: crate::object::This,
        colour: bool,
        write: bool,
    ) -> bool {
        const UNORDERED_ACCESS: u32 = 0x8;
        let Some(twin) = crate::resource::Twin::of(resource) else {
            return false;
        };
        let Some(depth) = crate::resource::HostTextures::host_copy(resource) else {
            return false;
        };
        let side = usize::from(colour);
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        let u = twins
            .entry(std::sync::Arc::as_ptr(&twin) as usize)
            .or_insert_with(|| TwinUse {
                depth: depth.clone(),
                twin: twin.clone(),
                needs: [false; 2],
                behind: [None; 2],
                writing: false,
                late: false,
            });
        let copy = match u.behind[side] {
            Some(behind) => behind,
            None => {
                u.needs[side] = true;
                false
            }
        };
        u.behind[side] = Some(false);
        match (write, colour) {
            (true, true) => u.writing = true,
            (true, false) => u.behind[1] = Some(true),
            (false, _) => {}
        }
        if copy && !colour && twin.plane.is_some() {
            u.late = true;
            crate::resource::PLANES[2].fetch_add(1, Ordering::Relaxed);
        }
        drop(twins);
        if copy {
            self.twin_copy(&depth, &twin, colour);
        } else if colour {
            self.settle(&twin.colour, UNORDERED_ACCESS);
        }
        if colour && let Some(plane) = &twin.plane {
            self.settle(plane, UNORDERED_ACCESS);
        }
        true
    }

    pub(crate) fn planes_now(&self) -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("LEONIDA_PLANE_NOW").is_none_or(|v| v != "0")) {
            return false;
        }
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        if twins.is_empty() {
            return false;
        }
        let due: Vec<_> = twins
            .values_mut()
            .filter(|u| u.behind[0] == Some(true) && u.twin.plane.is_some())
            .map(|u| {
                u.behind[0] = Some(false);
                u.late = false;
                (u.depth.clone(), u.twin.clone())
            })
            .collect();
        drop(twins);
        for (depth, twin) in &due {
            self.twin_copy(depth, twin, false);
            self.pass_plane(depth, twin);
            let readable = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE
                | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
            self.settle(depth, readable.0 as u32);
        }
        if !due.is_empty() {
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| {
                tracing::info!("depth planes written by shaders are drawn into the plane before the next draw or dispatch, not when the list closes (LEONIDA_PLANE_NOW=0: as before)");
            });
        }
        !due.is_empty()
    }

    fn pass_plane(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const DEPTH_WRITE: u32 = 0x10;
        const PIXEL_SHADER_RESOURCE: u32 = 0x80;
        let Some(plane) = &twin.plane else { return };
        self.settle(depth, DEPTH_WRITE);
        self.settle(plane, PIXEL_SHADER_RESOURCE);
        PlanePass::record_plane_pass(&self.list, &depth.texture, &plane.texture);
        self.settle(plane, UNORDERED_ACCESS);
    }

    fn twin_copy(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
        colour: bool,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let (from, to) = if colour {
            (depth, &twin.colour)
        } else {
            (&twin.colour, depth)
        };
        self.settle(from, COPY_SOURCE);
        self.settle(to, COPY_DEST);
        PlanePass::record_twin_copy(&self.list, &depth.texture, twin, colour);
        self.settle(&twin.colour, UNORDERED_ACCESS);
    }

    fn twins_to_depth(&self) {
        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        let written: Vec<_> = twins
            .values_mut()
            .filter(|u| u.behind[0] == Some(true) || u.late)
            .map(|u| {
                let copy = u.behind[0] == Some(true);
                u.behind[0] = Some(false);
                u.late = false;
                (u.depth.clone(), u.twin.clone(), copy)
            })
            .collect();
        drop(twins);
        for (depth, twin, copy) in &written {
            if *copy {
                self.twin_copy(depth, twin, false);
            }
            self.pass_plane(depth, twin);
            self.settle(depth, SHADER_RESOURCE);
        }
    }

    pub(crate) fn texture_written(&self, resource: crate::object::This) {
        if !crate::texback::wanted(resource) {
            return;
        }
        let mut deferred = self.deferred();
        if !deferred
            .back_wanted
            .iter()
            .any(|held| held.as_ptr() == resource)
        {
            deferred
                .back_wanted
                .push(crate::resource::ResourceRef::new(resource));
        }
    }

    fn textures_back(&self) {
        const COPY_SOURCE: u32 = 0x800;
        let wanted = std::mem::take(&mut self.deferred().back_wanted);
        for resource in wanted {
            let Some(copy) = crate::resource::HostTextures::host_copy(resource.as_ptr()) else {
                continue;
            };
            self.settle(&copy, COPY_SOURCE);
            if let Some(back) = crate::texback::record(&self.list, resource, &copy.texture) {
                self.deferred().texture_backs.push(back);
            }
        }
    }

    pub(crate) fn twins_written(&self) {
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        for u in twins.values_mut().filter(|u| u.writing) {
            u.writing = false;
            u.behind[0] = Some(true);
        }
    }

    fn plan_prelude(&self) -> Option<PlannedPrelude> {
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let mut syncs = Vec::new();
        let twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        for u in twins.values() {
            let real = u.twin.behind.load(Ordering::Relaxed);
            let mut behind = [real & 1 != 0, real & 2 != 0];
            for side in [0, 1] {
                if u.needs[side] && behind[side] {
                    syncs.push((u.depth.clone(), u.twin.clone(), side == 1));
                    behind = [false; 2];
                }
            }
            let left = [0, 1].map(|side| u.behind[side].unwrap_or(behind[side]));
            let left = u32::from(left[0]) | (u32::from(left[1]) << 1);
            u.twin.behind.store(left, Ordering::Relaxed);
        }
        drop(twins);
        let mut to_copy = Vec::new();
        let mut barriers = Vec::new();
        let mut copying = std::collections::HashMap::new();
        for (depth, twin, colour) in &syncs {
            let (from, to) = if *colour {
                (depth, &twin.colour)
            } else {
                (&twin.colour, depth)
            };
            for (texture, state) in [(from, COPY_SOURCE), (to, COPY_DEST)] {
                let key = std::sync::Arc::as_ptr(texture) as usize;
                let real = texture.state.load(Ordering::Relaxed);
                copying.insert(key, state);
                if real != state {
                    to_copy.push(PreludeTransition(texture.texture.clone(), real, state));
                    if !states.contains_key(&key) {
                        barriers.push(PreludeTransition(texture.texture.clone(), state, real));
                    }
                }
            }
        }
        barriers.extend(states.iter().filter_map(|(key, (first, last, copy))| {
            let real = copy.state.swap(*last, Ordering::Relaxed);
            let real = copying.get(key).copied().unwrap_or(real);
            (real != *first).then(|| PreludeTransition(copy.texture.clone(), real, *first))
        }));
        if barriers.is_empty() && syncs.is_empty() {
            return None;
        }
        let commands = self
            .prelude
            .get_or_init(|| Host::instance().and_then(|h| h.make_prelude()))
            .as_ref()
            .unwrap_or_else(|| HostSetup::fail(format_args!("no host list for a state prelude")));
        Some(PlannedPrelude {
            commands: commands.clone(),
            to_copy,
            syncs,
            barriers,
        })
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_op(&self, what: impl FnOnce() -> String) {
        self.note(what, None);
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_read(&self, what: impl FnOnce() -> String, range: (u64, u64)) {
        self.note(what, Some(range));
    }

    #[cfg(feature = "diagnostics")]
    fn note(&self, what: impl FnOnce() -> String, range: Option<(u64, u64)>) {
        if !HostSetup::dred_on() {
            crate::capture::CaptureFrames::op(std::ptr::from_ref(self) as usize, what);
            return;
        }
        {
            let started = std::time::Instant::now();
            let at = self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32;
            if let Some((heap, _)) = self.stamps()
                && at < STAMPS - 1
            {
                unsafe { self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
            }
            let what = what();
            crate::capture::CaptureFrames::op(std::ptr::from_ref(self) as usize, || what.clone());
            let text: Vec<u16> = what.encode_utf16().chain([0]).collect();
            const UNICODE: u32 = 0;
            unsafe {
                self.list
                    .SetMarker(UNICODE, Some(text.as_ptr().cast()), 2 * text.len() as u32)
            };
            self.notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((what, range));
            crate::copystat::C::add(
                crate::copystat::C::NoteNs,
                started.elapsed().as_nanos() as u64,
            );
        }
    }

    pub(crate) fn untouched(&self) -> bool {
        {
            let deferred = self.deferred();
            if !(deferred.copies.is_empty()
                && deferred.uploads.is_empty()
                && deferred.kept.is_empty()
                && deferred.textures.is_empty()
                && deferred.writes.is_empty()
                && deferred.backs.is_empty()
                && deferred.texture_backs.is_empty())
            {
                return false;
            }
        }
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
            && self
                .twins
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
    }

    pub(crate) fn used_pipeline(&self, name: &str) {
        let mut pipelines = self.pipelines.lock().unwrap_or_else(|e| e.into_inner());
        if !pipelines.contains(name) {
            pipelines.insert(name.to_owned());
        }
    }

    fn deferred(&self) -> std::sync::MutexGuard<'_, Deferred> {
        self.deferred.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn defer_copy(&self, dst: *mut u8, src: *const u8, len: usize) {
        let mut deferred = self.deferred();
        COPIES_JOINED[0].fetch_add(1, Ordering::Relaxed);
        if let Some(Step::Copy(d, s, n)) = deferred.copies.last_mut()
            && LateRun::continues((*d, *s, *n), (dst as usize, src as usize, len))
        {
            COPIES_JOINED[1].fetch_add(1, Ordering::Relaxed);
            if LateRun::join_copies() {
                *n += len;
                return;
            }
        }
        deferred
            .copies
            .push(Step::Copy(dst as usize, src as usize, len));
        deferred.invalidate_refreshes();
    }

    pub(crate) fn defer_fill(&self, dst: *mut u8, src: *const u8, len: usize) {
        let fill = Fill {
            dst: dst as usize,
            src: src as usize,
            len,
        };
        self.deferred().append_fill(fill, FillWork::workers());
    }

    pub(crate) fn defer_refresh(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        let mut deferred = self.deferred();
        let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
        if deferred.refreshed.insert(key) {
            deferred
                .copies
                .push(Step::Refresh(shadow.clone(), address, len));
        }
    }

    pub(crate) fn defer_gpu_write(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        self.deferred()
            .copies
            .push(Step::GpuWrite(shadow.clone(), address, len));
    }

    pub(crate) fn defer_read_back(
        &self,
        buffer: ID3D12Resource,
        dst: *mut u8,
        [row, rows, pitch]: [usize; 3],
        source: crate::object::This,
    ) {
        let recorded = crate::frame::FramePacer::frame();
        self.deferred().backs.push((
            buffer,
            dst as usize,
            [row, rows, pitch],
            source as usize,
            recorded,
        ));
    }

    pub(crate) fn defer_after(&self, work: impl Fn() + Send + 'static) {
        self.deferred().after.push(Box::new(work));
    }

    pub(crate) fn defer_write(&self, dst: *mut u8, what: crate::queue::Written, bytes: usize) {
        self.deferred().writes.push((dst as usize, what, bytes));
    }

    pub(crate) fn upload(&self, host: &Host, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let (buffer, data) = host.upload_buffer(size)?;
        HostSetup::name(&buffer, || format!("list upload {size:#x}"));
        self.deferred().hold_upload(buffer.clone(), size);
        Some((buffer, data))
    }

    pub(crate) fn scratch(&self, len: u64) -> Option<(u64, *mut u8)> {
        let host = Host::instance()?;
        let mut s = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        let len = len.max(1).next_multiple_of(256);
        if len > SCRATCH_CHUNK {
            let (buffer, data) = host.upload_buffer(len)?;
            HostSetup::name(&buffer, || format!("list scratch {len:#x}"));
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            self.deferred().hold_upload(buffer, len);
            return Some((gpu, data));
        }
        while s.current < s.chunks.len() && s.used + len > s.chunks[s.current].1 {
            (s.current, s.used) = (s.current + 1, 0);
        }
        if s.current == s.chunks.len() {
            let size = if ScratchPool::enabled() && s.chunks.is_empty() && len <= SCRATCH_FIRST {
                SCRATCH_FIRST
            } else {
                SCRATCH_CHUNK
            };
            let chunk = ScratchPool::take(host, size)?;
            s.chunks.push((chunk, size));
            s.used = 0;
        }
        let (data, gpu) = (s.chunks[s.current].0.data, s.chunks[s.current].0.gpu);
        let at = s.used;
        s.used += len;
        Some((gpu + at, (data + at as usize) as *mut u8))
    }

    pub(crate) fn scratch_kept(&self) -> u64 {
        let scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        scratch.chunks.iter().map(|c| c.1).sum()
    }

    pub(crate) fn budget(&self) -> Option<u64> {
        let (buffer, data) = self
            .budget
            .get_or_init(|| {
                let (b, d) = Host::instance()?.shared_buffer(256)?;
                Some((b, d as usize))
            })
            .as_ref()?;
        let mut deferred = self.deferred();
        if !deferred.budget_zeroed {
            deferred.budget_zeroed = true;
            let at = *data;
            deferred.copies.push(Step::Host(Box::new(move || unsafe {
                let words = at as *mut u32;
                let (largest, all) = (words.add(1).read_volatile(), words.add(2).read_volatile());
                MESH_GROUPS[0].fetch_max(u64::from(largest), Ordering::Relaxed);
                MESH_GROUPS[1].fetch_add(u64::from(all), Ordering::Relaxed);
                for k in 0..3 {
                    words.add(k).write_volatile(0);
                }
            })));
        }
        Some(unsafe { buffer.GetGPUVirtualAddress() })
    }

    pub(crate) fn indirect_scratch(&self, size: u64) -> Option<ID3D12Resource> {
        let needed = size.max(64 << 10);
        let mut slot = self.indirect_args.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((buffer, cap)) = &*slot
            && *cap >= needed
        {
            return Some(buffer.clone());
        }
        let prev = slot.as_ref().map_or(0, |(_, cap)| *cap);
        let alloc = prev
            .saturating_mul(2)
            .max(needed)
            .next_multiple_of(64 << 10);
        let buffer = Host::instance()?.uav_buffer(alloc)?;
        HostSetup::name(&buffer, || format!("indirect arguments {alloc:#x}"));
        let old = slot.replace((buffer.clone(), alloc));
        drop(slot);
        if let Some((old_buf, _)) = old {
            self.keep(&old_buf);
        }
        Some(buffer)
    }

    pub(crate) fn keep(&self, object: &impl Interface) {
        if let Ok(object) = object.cast::<windows::core::IUnknown>() {
            self.deferred().kept.push(object);
        }
    }

    pub(crate) fn first_use(&self, key: usize) -> bool {
        self.deferred().textures.insert(key)
    }

    pub(crate) fn raw_memory_ranges(
        &self,
        directory: &ID3D12Resource,
        revision: u64,
        count: usize,
        writes: bool,
    ) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
        let mut deferred = self.deferred();
        let id = directory.as_raw() as usize;
        if deferred.raw_copies.directory != id {
            deferred.kept.push(directory.clone().into());
        }
        if deferred.raw_copies.directory != id || deferred.raw_copies.revision != revision {
            deferred.raw_copies = RawCopyCoverage {
                directory: id,
                revision,
                ..Default::default()
            };
        }
        deferred.raw_copies.advance(count, writes)
    }

    pub(crate) fn defer(&self, work: impl Fn() + Send + 'static) {
        let mut deferred = self.deferred();
        deferred.copies.push(Step::Run(Box::new(work)));
        deferred.invalidate_refreshes();
    }

    pub(crate) fn defer_host(&self, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Host(Box::new(work)));
    }

    pub(crate) fn defer_texture(&self, staging: std::sync::Arc<crate::texture::Staging>) {
        self.deferred().copies.push(Step::Texture(staging));
    }

    fn run_deferred(&self, batch: &mut Batch) {
        #[cfg(feature = "diagnostics")]
        use crate::copystat::C;
        for step in &self.deferred().copies {
            let started = std::time::Instant::now();
            let copied = match step {
                &Step::Copy(dst, src, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        C::add(C::Copies, 1);
                        C::add(C::CopyBytes, len as u64);
                    }
                    batch.title_written();
                    TitleCopy::copy_mapped(dst as *mut u8, src as *const u8, len, true);
                    crate::watch::WriteJournal::wrote(dst, len);
                    len
                }
                Step::Fill(_) | Step::Fills(_) => {
                    let fills = step.fills();
                    let bytes = fills.iter().map(|f| f.len).sum::<usize>();
                    let jobs = FillWork::run(fills, FillWork::workers());
                    #[cfg(feature = "diagnostics")]
                    {
                        C::add(C::Copies, fills.len() as u64);
                        C::add(C::CopyBytes, bytes as u64);
                        if jobs > 1 {
                            FILL_BATCHES[0].fetch_add(1, Ordering::Relaxed);
                            FILL_BATCHES[1].fetch_add(fills.len() as u64, Ordering::Relaxed);
                        }
                    }
                    let _ = jobs;
                    bytes
                }
                Step::Refresh(shadow, address, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        C::add(C::Refreshes, 1);
                        C::add(C::RefreshBytes, *len as u64);
                    }
                    batch.add(shadow, *address, *len);
                    continue;
                }
                Step::Run(work) | Step::Host(work) => {
                    if matches!(step, Step::Run(_)) {
                        batch.title_written();
                    }
                    work();
                    if matches!(step, Step::Run(_)) {
                        crate::watch::WriteJournal::ask_once_end();
                    }
                    #[cfg(feature = "diagnostics")]
                    {
                        C::add(C::Runs, 1);
                        C::add(C::RunUs, started.elapsed().as_micros() as u64);
                    }
                    continue;
                }
                Step::GpuWrite(shadow, address, len) => {
                    batch.flush_one(shadow);
                    #[cfg(feature = "diagnostics")]
                    let snapshot = std::time::Instant::now();
                    shadow.writing(*address, *len);
                    #[cfg(feature = "diagnostics")]
                    {
                        C::add(C::GpuWrites, 1);
                        C::add(C::SnapshotUs, snapshot.elapsed().as_micros() as u64);
                    }
                    continue;
                }
                Step::Texture(staging) => {
                    if batch.texture_due(staging) {
                        staging.refresh();
                    }
                    continue;
                }
            };
            PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
            PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy)]
struct Fill {
    dst: usize,
    src: usize,
    len: usize,
}

impl Fill {
    fn run(self) {
        TitleCopy::copy_mapped(self.dst as *mut u8, self.src as *const u8, self.len, false);
    }
}

impl Step {
    fn fills(&self) -> &[Fill] {
        match self {
            Self::Fill(fill) => std::slice::from_ref(fill),
            Self::Fills(group) => group,
            _ => &[],
        }
    }
}

const FILLS_PER_JOB: usize = 128;

impl Deferred {
    fn append_fill(&mut self, fill: Fill, workers: usize) {
        if workers > 1 {
            match self.copies.last_mut() {
                Some(Step::Fills(group)) if group.len() < 4096 => {
                    group.push(fill);
                    return;
                }
                Some(Step::Fill(_)) => {}
                _ => self.fill_start = self.copies.len(),
            }
            if self.copies.len() - self.fill_start + 1 == 2 * FILLS_PER_JOB {
                let mut group = Vec::with_capacity(2 * FILLS_PER_JOB);
                for step in self.copies.drain(self.fill_start..) {
                    let Step::Fill(f) = step else { unreachable!() };
                    group.push(f);
                }
                group.push(fill);
                self.copies.push(Step::Fills(group));
                return;
            }
        }
        self.copies.push(Step::Fill(fill));
    }
}

#[cfg(test)]
mod fill_tests {
    use super::*;

    fn fills(source: &[u8], dst: &mut [u8], bytes: usize) -> Vec<Fill> {
        dst.chunks_exact_mut(bytes)
            .enumerate()
            .map(|(i, out)| Fill {
                dst: out.as_mut_ptr() as usize,
                src: source[i * bytes..].as_ptr() as usize,
                len: bytes,
            })
            .collect()
    }

    #[test]
    fn parallel_upload_fills_match_serial_bytes_and_follow_later_source_edits() {
        let mut input: Vec<u8> = (0..512 * 448).map(|i| (i % 251) as u8).collect();
        let mut serial = vec![0; input.len()];
        let mut parallel = vec![0; input.len()];
        let serial_jobs = fills(&input, &mut serial, 448);
        let jobs = fills(&input, &mut parallel, 448);
        for turn in [0x5a, 0x37] {
            for b in &mut input {
                *b ^= turn;
            }
            assert_eq!(FillWork::run(&serial_jobs, 1), 1);
            assert_eq!(FillWork::run(&jobs, 4), 4);
            assert_eq!(parallel, input);
            assert_eq!(parallel, serial);
        }
    }

    #[test]
    fn overlapping_fills_keep_the_last_write_and_source_aliases_remain_ordered() {
        let source: Vec<u8> = (0..256).map(|n| n as u8).collect();
        let mut output = [0; 256];
        let mut jobs = fills(&source, &mut output, 1);
        jobs[255].dst = jobs[0].dst;
        assert_eq!(FillWork::run(&jobs, 2), 1);
        assert_eq!(output[0], 255);
        let mut jobs = fills(&source, &mut output, 1);
        jobs[0].src = source[167..].as_ptr() as usize;
        jobs[1].src = jobs[0].dst;
        assert_eq!(FillWork::run(&jobs, 2), 1);
        assert_eq!(output[1], 167);
    }

    #[test]
    fn a_title_copy_splits_fill_batches_on_a_replayed_command_list() {
        let Some(host) = Host::instance() else { return };
        let list = host.new_list().unwrap();
        let mut source = vec![0x31; 512];
        let mut before = vec![0; 512];
        let mut after = vec![0; 512];
        let next = vec![0xc4; 512];
        for fill in fills(&source, &mut before, 1) {
            list.deferred().append_fill(fill, 4);
        }
        let last_byte = source.as_ptr() as usize + source.len() - 1;
        let pending = PendingWrites {
            buffers: vec![(last_byte, last_byte + 1)],
            textures: Vec::new(),
        };
        assert_eq!(
            LateRun::blocked_by(&pending, &[&list], false),
            Some(WHY_READ)
        );
        list.defer_copy(source.as_mut_ptr(), next.as_ptr(), 512);
        for fill in fills(&source, &mut after, 1) {
            list.deferred().append_fill(fill, 4);
        }
        assert!(matches!(
            list.deferred().copies.as_slice(),
            [Step::Fills(_), Step::Copy(..), Step::Fills(_)]
        ));
        list.close();
        for initial in [0x31, 0x72] {
            source.fill(initial);
            host.execute(&[&list]);
            assert!(before.iter().all(|&b| b == initial));
            assert_eq!(after, next);
        }
        list.reset();
        assert!(list.deferred().copies.is_empty());
    }
}

#[derive(Default)]
struct Batch {
    refreshes: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
    index: std::collections::HashMap<usize, usize>,
    textures: std::collections::HashSet<usize>,
}

impl Batch {
    fn add(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, address: usize, len: usize) {
        let end = address + len;
        let key = std::sync::Arc::as_ptr(shadow) as usize;
        match self.index.get(&key) {
            Some(&at) => {
                let r = &mut self.refreshes[at];
                (r.1, r.2) = (r.1.min(address), r.2.max(end));
            }
            None => {
                self.index.insert(key, self.refreshes.len());
                self.refreshes.push((shadow.clone(), address, end));
            }
        }
    }

    fn flush_one(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>) {
        let Some(at) = self
            .index
            .remove(&(std::sync::Arc::as_ptr(shadow) as usize))
        else {
            return;
        };
        let (shadow, from, to) = self.refreshes.swap_remove(at);
        if let Some(moved) = self.refreshes.get(at) {
            self.index
                .insert(std::sync::Arc::as_ptr(&moved.0) as usize, at);
        }
        Self::update(&shadow, from, to);
    }

    fn update(shadow: &crate::shadow::Shadow, from: usize, to: usize) {
        let started = std::time::Instant::now();
        let copied = shadow.update(from, to - from);
        let us = started.elapsed().as_micros() as u64;
        #[cfg(feature = "diagnostics")]
        crate::copystat::CopyStats::range(from, to - from, us, 0);
        PREPARING[0].fetch_add(us, Ordering::Relaxed);
        PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
    }

    fn texture_due<T>(&mut self, staging: &std::sync::Arc<T>) -> bool {
        self.textures
            .insert(std::sync::Arc::as_ptr(staging) as *const () as usize)
    }

    fn title_written(&mut self) {
        self.flush();
        self.textures.clear();
    }

    fn flush(&mut self) {
        self.index.clear();
        let ahead = self.refreshes.len() > 1;
        if ahead {
            let started = std::time::Instant::now();
            let ranges: Vec<(usize, usize)> = self
                .refreshes
                .iter()
                .filter(|r| !r.0.unwatched())
                .map(|r| (r.1, r.2))
                .collect();
            crate::watch::WriteJournal::ask_ahead(&ranges);
            PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
        if FillWork::wide_refresh() && self.refreshes.len() >= 4 {
            let started = std::time::Instant::now();
            let before = PREPARING[0].load(Ordering::Relaxed);
            let jobs: Vec<Box<dyn FnOnce() + Send>> = self
                .refreshes
                .drain(..)
                .map(|(shadow, from, to)| {
                    Box::new(move || Self::update(&shadow, from, to)) as Box<dyn FnOnce() + Send>
                })
                .collect();
            crate::shadow::WideCopy::run(jobs);
            let summed = PREPARING[0].load(Ordering::Relaxed).saturating_sub(before);
            PREPARING[0].fetch_sub(
                summed.saturating_sub(started.elapsed().as_micros() as u64),
                Ordering::Relaxed,
            );
        } else {
            for (shadow, from, to) in self.refreshes.drain(..) {
                Self::update(&shadow, from, to);
            }
        }
        if ahead {
            crate::watch::WriteJournal::ask_ahead_done();
        }
    }
}

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    fn NtWriteVirtualMemory(
        process: *mut std::ffi::c_void,
        base: *mut std::ffi::c_void,
        buffer: *const std::ffi::c_void,
        size: usize,
        written: *mut usize,
    ) -> i32;
}

impl Host {
    pub(crate) fn read_back_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let made = unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
        };
        match made {
            Ok(()) => buffer,
            Err(e) => {
                tracing::warn!(error = %e, size, "no read-back buffer");
                None
            }
        }
    }

    pub(crate) fn upload_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        MADE[0].fetch_add(1, Ordering::Relaxed);
        MADE[1].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no upload buffer");
                None
            }
        }
    }
}

const BUFFERS: u32 = 2;

struct Presenter {
    swap_chain: IDXGISwapChain3,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    copy: HostList,
    ahead: Vec<(HostList, u64)>,
    hwnd: HWND,
    meter: Meter,
}

struct Meter {
    since: std::time::Instant,
    frames: u32,
    #[cfg(feature = "diagnostics")]
    copy_stats_frames: u64,
    drawn: u64,
    logged: std::time::Instant,
    cpu_us: u64,
    cpu_pct: f64,
    gpu_pct: f64,
    ram_mb: f64,
}

impl Meter {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Meter {
            since: now,
            frames: 0,
            #[cfg(feature = "diagnostics")]
            copy_stats_frames: 0,
            drawn: crate::draw::DrawCounters::drawn(),
            logged: now,
            cpu_us: HostClock::process_cpu_us().unwrap_or(0),
            cpu_pct: 0.0,
            gpu_pct: 0.0,
            ram_mb: 0.0,
        }
    }

    fn frame(&mut self, hwnd: HWND) {
        #[cfg(feature = "diagnostics")]
        tracing::debug!(target: "leonida_graphics::frame", token = crate::frame::FramePacer::frame(), "present");
        #[cfg(feature = "diagnostics")]
        crate::copystat::CopyStats::presented();
        self.frames += 1;
        #[cfg(feature = "diagnostics")]
        {
            self.copy_stats_frames += 1;
        }
        let elapsed = self.since.elapsed();
        if elapsed < std::time::Duration::from_secs(1) {
            return;
        }
        let drawn = crate::draw::DrawCounters::drawn();
        let frames = f64::from(self.frames);
        let fps = frames / elapsed.as_secs_f64();
        let ms = elapsed.as_secs_f64() * 1000.0 / frames;
        let draws = (drawn - self.drawn) as f64 / frames;
        (self.since, self.frames, self.drawn) = (std::time::Instant::now(), 0, drawn);
        let [cpu, gpu] =
            [0, 1].map(|i| SUBMISSION_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
        self.cpu_pct = {
            let now = HostClock::process_cpu_us().unwrap_or(self.cpu_us);
            let pct = (now.saturating_sub(self.cpu_us)) as f64
                / 1e6
                / elapsed.as_secs_f64()
                / f64::from(HostClock::logical_processors())
                * 100.0;
            self.cpu_us = now;
            pct
        };
        self.gpu_pct = if ms > 0.0 { gpu / ms * 100.0 } else { 0.0 };
        self.ram_mb = HostClock::process_working_set().unwrap_or(0) as f64 / (1024.0 * 1024.0);
        let [copy_ms, copy_mb, texture_ms, texture_mb] = [0, 1, 2, 3].map(|i| {
            let scale = if i % 2 == 0 { 1e3 } else { 1e6 };
            PREPARING[i].swap(0, Ordering::Relaxed) as f64 / scale / frames
        });
        let [back_ms, back_mb] =
            [0, 1].map(|i| COPY_BACK[i].swap(0, Ordering::Relaxed) as f64 / [1e3, 1e6][i] / frames);
        let dropped = crate::draw::DrawCounters::take_dropped() as f64 / frames;
        let indirect = crate::list::WorkCounters::take_indirect() as f64 / frames;
        let mesh_largest = MESH_GROUPS[0].swap(0, Ordering::Relaxed);
        let mesh_groups = MESH_GROUPS[1].swap(0, Ordering::Relaxed) as f64 / frames;
        tracing::info!(
            "indirect mesh dispatches: the largest group count asked for {mesh_largest} (run: at most 65535), {mesh_groups:.0} groups asked per frame"
        );
        LateRun::late_line(frames);
        CardClock::line(frames, ms);
        crate::shadow::WideCopy::back_line(frames);
        {
            let [passes, pieces, compares] =
                [0, 1, 2].map(|i| CARD_PASSES[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if passes + compares > 0.0 {
                tracing::info!(
                    "our passes over copies in the card's memory per frame: {passes:.0} hash passes over {pieces:.0} pieces after \
                     ranges were sent up (LEONIDA_CARD_REHASH_RUNS=1 joins them: now {}), {compares:.0} compare passes",
                    if LateRun::rehash_runs() { "on" } else { "off" }
                );
            }
        }
        {
            let [own, asking, still, all] = [0, 1, 2, 3].map(|i| {
                crate::texture::REFRESH_WAITS[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames
            });
            if all >= 1.0 {
                tracing::info!(
                    "texture refreshes per frame, all threads: {all:.1} ms, of them {own:.1} ms waiting for a texture another thread \
                     is refreshing, {asking:.1} ms asking the write watch and the guards, {still:.1} ms waiting for title memory to \
                     hold still (the runtime remapping it); the rest is reading, hashing and detiling"
                );
            }
            let [through, left] = [0, 1]
                .map(|i| crate::texture::UNLOCKED[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if through + left > 0.0 {
                tracing::info!(
                    "texture refreshes that did not wait for title memory to hold still, per frame (LEONIDA_TEXTURE_NO_WAIT=1): \
                     {through:.1} read through the kernel, {left:.1} left for the next refresh (a page was not there)"
                );
            }
        }
        {
            let [copies, joined] =
                [0, 1].map(|i| COPIES_JOINED[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if copies > 0.0 {
                tracing::info!(
                    "title buffer copies recorded per frame: {copies:.0}, of them {joined:.0} continue the one before \
                     (LEONIDA_JOIN_COPIES=1 makes those one copy with it: now {})",
                    if LateRun::join_copies() { "on" } else { "off" }
                );
            }
        }
        #[cfg(feature = "diagnostics")]
        if FillWork::workers() > 1 {
            let [batches, fills] =
                [0, 1].map(|i| FILL_BATCHES[i].swap(0, Ordering::Relaxed) as f64 / frames);
            tracing::info!(
                "parallel upload preparation per frame: {batches:.1} batches, {fills:.0} fills"
            );
        }
        let picked = crate::draw::HEAP_BUFFER_PASSES.swap(0, Ordering::Relaxed) as f64 / frames;
        let picked_ms =
            crate::draw::HEAP_BUFFER_MICROS.swap(0, Ordering::Relaxed) as f64 / 1e3 / frames;
        let [twin_to_depth, twin_to_colour] =
            [2, 3].map(|i| crate::resource::TWINS[i].swap(0, Ordering::Relaxed) as f64 / frames);
        let planes = crate::resource::PLANES[0].swap(0, Ordering::Relaxed) as f64 / frames;
        let card = crate::shadow::ShadowCards::stats();
        let card_fetch_copies = CARD_FETCH_COPIES.swap(0, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        let slow = crate::watch::WriteJournal::slow_asks();
        #[cfg(feature = "diagnostics")]
        let drops = crate::pipeline::PipelineDrops::take();
        if self.logged.elapsed() >= std::time::Duration::from_secs(5) {
            self.logged = std::time::Instant::now();
            tracing::info!(
                "placed texture views: {} writes of another view of the texture a slot showed (it kept the old view until the new was made)",
                crate::descriptor::host::KEPT_VIEWS.load(Ordering::Relaxed)
            );
            {
                let [same, changed, waiting] = [0, 1, 2]
                    .map(|i| crate::draw::HEAP_TEXTURES_RECHECKED[i].load(Ordering::Relaxed));
                tracing::info!(
                    "textures picked by index, looked at again after their upload: {same} looks found them unchanged, \
                     {changed} changed (uploaded again), {waiting} wait for a look"
                );
                let [queued, due, away, partly, oldest] = [0, 1, 2, 3, 4]
                    .map(|i| crate::draw::HEAP_TEXTURES_QUEUE[i].load(Ordering::Relaxed));
                let [same, parts, tails] =
                    [0, 1, 2].map(|i| crate::draw::HEAP_TEXTURES_PARTLY[i].load(Ordering::Relaxed));
                tracing::info!(
                    "textures picked by index, the queue: {queued} slots, {due} of them due (the oldest written {oldest} frames ago), \
                     {away} waiting for their memory, {partly} uploaded in part; since the start: {} slots found uploaded already by \
                     the draws that read them through tables, {} writes of tables' slots (due some frames later), {} slots taken out \
                     as read through a table, {parts} uploads of textures not all mapped ({tails} of them without their first page), \
                     {same} looks that found no more mapped; {} slots made stale by a newer view of other memory there, \
                     {} ranges of memory mapped, unmapped or written under texture views ({} full-queue rescans), {} slots made due by them, \
                     {} made slots found unmapped and made null",
                    crate::draw::HEAP_TEXTURES_TABLED.load(Ordering::Relaxed),
                    crate::descriptor::host::TABLED_WRITES[0].load(Ordering::Relaxed),
                    crate::descriptor::host::TABLED_WRITES[1].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[0].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[1].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[2].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[3].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[4].load(Ordering::Relaxed)
                );
                let [
                    views,
                    shown,
                    waiting,
                    tables,
                    gone,
                    own,
                    free,
                    start,
                    other,
                    shows,
                    back,
                ] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
                    .map(|i: usize| crate::descriptor::host::SWEPT[i].load(Ordering::Relaxed));
                tracing::info!(
                    "placed texture views in the heap at the last look: {views}, of them {shown} shown, {waiting} in the queue, \
                     {tables} read through tables, {gone} without memory; with memory and nothing to bring them up to date: \
                     {own} of their own memory, {free} of memory no other view describes, {start} of memory another texture \
                     begins at, {other} of another's memory ({shows} of all these show another view); {back} put back into the \
                     queue since the start"
                );
            }
            #[cfg(feature = "diagnostics")]
            tracing::info!(
                "write-watch askings of a millisecond or more: {:.1} a frame, {:.1} ms a frame; the longest asking {:.1} ms",
                slow[0] as f64 / frames,
                slow[1] as f64 / 1e3 / frames,
                slow[2] as f64 / 1e3
            );
            if crate::shadow::ShadowCards::enabled() {
                let per_frame = |i: usize| card[i] as f64 / frames;
                tracing::info!(
                    "host copies in the card's memory: {} copies {} MB, their bytes from before {} MB; per frame: \
                     {:.1} MB sent up in {:.0} ranges, {:.0} pieces compared on the card, {:.0} dirty ranges fetched \
                     ({:.1} MB) with {:.1} more waits for the card ({:.1} ms), {:.0} readback copy commands",
                    card[7],
                    card[8] >> 20,
                    card[9] >> 20,
                    per_frame(0) / 1e6,
                    per_frame(1),
                    per_frame(2),
                    per_frame(3),
                    per_frame(4) / 1e6,
                    per_frame(5),
                    per_frame(6) / 1e3,
                    card_fetch_copies as f64 / frames
                );
            }
            tracing::info!(
                "{fps:.1} fps, {ms:.1} ms/frame ({cpu:.1} preparing: copies {copy_ms:.1} ms \
                 {copy_mb:.1} MB, textures {texture_ms:.1} ms {texture_mb:.1} MB; {gpu:.1} on the \
                 GPU; {back_ms:.1} copying back {back_mb:.1} MB), {draws:.0} draws/frame, \
                 dropped {dropped:.0} without a host pipeline ({making} being made) and \
                 {indirect:.0} indirect; heap buffers {picked:.1} passes {picked_ms:.1} ms;                  texture slots written so far: {placed} placed, {objects} of objects; \
                 textures picked by index: {picked_textures} uploaded so far ({picked_mb} MB), \
                 {picked_waiting} waiting, {picked_away} passed over unmapped; depth textures with                  unordered access: {du0} made, bound as depth {du1}x, cleared as depth {du2}x,                  unordered views refused {du3}x; buffer views left null as the host would refuse them: {refused}; \
                 cpu {cpu_pct:.1}% gpu {gpu_pct:.0}% busy ram {ram_mb:.0} MB",
                picked_textures = crate::draw::HEAP_TEXTURES.load(Ordering::Relaxed),
                picked_mb = crate::draw::HEAP_TEXTURE_BYTES.load(Ordering::Relaxed) >> 20,
                picked_waiting = crate::draw::HEAP_TEXTURES_WAITING.load(Ordering::Relaxed),
                picked_away = crate::draw::HEAP_TEXTURES_AWAY.load(Ordering::Relaxed),
                du0 = crate::resource::DEPTH_UNORDERED[0].load(Ordering::Relaxed),
                du1 = crate::resource::DEPTH_UNORDERED[1].load(Ordering::Relaxed),
                du2 = crate::resource::DEPTH_UNORDERED[2].load(Ordering::Relaxed),
                du3 = crate::resource::DEPTH_UNORDERED[3].load(Ordering::Relaxed),
                refused = format!(
                    "{}; depth twins: {} alive, {} depth textures left without one; twin copies per frame: {twin_to_depth:.1} into depth textures, {twin_to_colour:.1} into twins; depth planes: {planes:.1} drawn from their twins per frame, {} times not given back to a twin, {} times used before the pass",
                    crate::descriptor::host::REFUSED_VIEWS.load(Ordering::Relaxed),
                    crate::resource::TWINS[0].load(Ordering::Relaxed),
                    crate::resource::TWINS[1].load(Ordering::Relaxed),
                    crate::resource::PLANES[1].load(Ordering::Relaxed),
                    crate::resource::PLANES[2].load(Ordering::Relaxed)
                ),
                making = crate::pipeline::PipelineCache::making(),
                placed = crate::descriptor::host::PLACED_TEXTURE_SLOTS.load(Ordering::Relaxed),
                objects = crate::descriptor::host::OBJECT_TEXTURE_SLOTS.load(Ordering::Relaxed),
                cpu_pct = self.cpu_pct,
                gpu_pct = self.gpu_pct,
                ram_mb = self.ram_mb
            );
            #[cfg(feature = "diagnostics")]
            {
                crate::copystat::CopyStats::log(std::mem::take(&mut self.copy_stats_frames) as f64);
                for (why, n) in drops.iter().take(40) {
                    tracing::info!(
                        "dropped {:.1}/frame without a host pipeline: {why}",
                        *n as f64 / frames
                    );
                }
            }
        }
        let (cpu_pct, gpu_pct, ram_mb) = (self.cpu_pct, self.gpu_pct, self.ram_mb);
        crate::debug_hud::update(hwnd, || {
            format!(
                "Leonida diagnostics\n{fps:.1} FPS   {ms:.1} ms/frame   {draws:.0} draws/frame\nGPU wait {gpu:.1} ms   CPU prep {cpu:.1} ms   Copy-back {back_ms:.1} ms\nDropped {dropped:.0}/frame   RAM {ram_mb:.0} MB"
            )
        });
        let text = format!(
            "Leonida | {fps:.0} fps {ms:.1} ms/frame | gpu {gpu_pct:.0}% busy \
             cpu {cpu_pct:.0}% ram {ram_mb:.0} MB | {draws:.0} draws/frame"
        );
        static PENDING: Mutex<Option<(usize, String)>> = Mutex::new(None);
        static SETTER: std::sync::Once = std::sync::Once::new();
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((hwnd.0 as usize, text));
        SETTER.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("leonida-caption".into())
                .spawn(|| {
                    let (mut newest, mut shown) = (None, String::new());
                    loop {
                        let next = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
                        newest = next.or(newest);
                        if let Some((hwnd, figures)) = &newest {
                            let text = HostWindow::caption(figures, HostClock::process_uptime());
                            if text != shown {
                                let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
                                unsafe {
                                    let _ = SetWindowTextW(
                                        HWND(*hwnd as *mut _),
                                        PCWSTR(wide.as_ptr()),
                                    );
                                }
                                shown = text;
                            }
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                });
        });
    }
}

impl Host {
    pub(crate) fn present(&self, texture: &ID3D12Resource, state: u32, frame: u64) -> Option<bool> {
        let desc = unsafe { texture.GetDesc() };
        let Some(format) = HostWindow::swap_format(desc.Format) else {
            tracing::warn!(format = desc.Format.0, "cannot present this format");
            return None;
        };
        let (width, height) = (desc.Width as u32, desc.Height);
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        if presenter.is_none() {
            *presenter = match self.presenter(width, height, format) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "no swap chain");
                    return None;
                }
            };
        }
        let p = presenter.as_mut()?;
        if (p.width, p.height, p.format) != (width, height, format) {
            LateRun::settle_now(WHY_SYNC);
            let resized = unsafe {
                p.swap_chain
                    .ResizeBuffers(BUFFERS, width, height, format, DXGI_SWAP_CHAIN_FLAG(0))
            };
            match resized {
                Ok(()) => {
                    (p.width, p.height, p.format) = (width, height, format);
                    tracing::info!(
                        width,
                        height,
                        format = format.0,
                        frame,
                        "the swap chain's buffers were made again for another size or format"
                    );
                    self.log_video_memory("right after the swap chain's buffers were made again");
                }
                Err(e) => {
                    tracing::warn!(error = %e, width, height, "swap chain not resized");
                    return None;
                }
            }
        }
        let state = D3D12_RESOURCE_STATES(state as i32);
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            #[cfg(feature = "diagnostics")]
            let read_now = crate::capture::CaptureFrames::wanted(frame)
                || crate::capture::CaptureFrames::listed(frame);
            #[cfg(not(feature = "diagnostics"))]
            let read_now = false;
            let slot = if LateRun::late_free_present() && !read_now {
                let finished = FINISHED.load(Ordering::Acquire);
                p.ahead
                    .iter()
                    .position(|(_, flown)| *flown <= finished)
                    .or_else(|| {
                        let list = self.new_list().ok()?;
                        list.close();
                        p.ahead.push((list, 0));
                        Some(p.ahead.len() - 1)
                    })
            } else {
                None
            };
            let copy = match slot {
                Some(i) => &p.ahead[i].0,
                None => &p.copy,
            };
            copy.reset();
            let list = &copy.list;
            list.ResourceBarrier(&[
                StateBits::transition(texture, state, D3D12_RESOURCE_STATE_COPY_SOURCE),
                StateBits::transition(
                    &back,
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ),
            ]);
            list.CopyResource(&back, texture);
            #[cfg(feature = "diagnostics")]
            let capture = crate::capture::CaptureFrames::wanted(frame)
                .then(|| crate::capture::TargetDump::read_back(&self.device, list, texture))
                .flatten();
            #[cfg(feature = "diagnostics")]
            let probe = if slot.is_some() {
                None
            } else {
                crate::capture::CaptureFrames::probe(&self.device, list, texture)
            };
            #[cfg(not(feature = "diagnostics"))]
            let _ = frame;
            list.ResourceBarrier(&[
                StateBits::transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, state),
                StateBits::transition(
                    &back,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_PRESENT,
                ),
            ]);
            copy.close();
            match slot {
                Some(i) => {
                    let flown = self.execute_own(copy);
                    p.ahead[i].1 = flown;
                }
                None => self.execute(&[copy]),
            }
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                crate::capture::TargetDump::save_read_back(frame, &buffer, &footprint);
            }
            let _ = p.swap_chain.Present(0, DXGI_PRESENT(0));
            p.meter.frame(p.hwnd);
            #[cfg(feature = "diagnostics")]
            return Some(probe.is_some_and(|b| crate::capture::CaptureFrames::black(&b)));
            #[cfg(not(feature = "diagnostics"))]
            Some(false)
        }
    }

    fn presenter(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<Presenter> {
        let hwnd = match HostWindow::title_window() {
            Some(hwnd) => hwnd,
            None => HostWindow::create(width, height)?,
        };
        self.log_video_memory("right before the swap chain is made");
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFERS,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                ..Default::default()
            };
            let swap_chain = factory
                .CreateSwapChainForHwnd(&self.queue, hwnd, &desc, None, None)?
                .cast::<IDXGISwapChain3>()?;
            let copy = self.new_list()?;
            copy.close();
            tracing::info!(width, height, format = format.0, "presenting to a window");
            self.log_video_memory("right after the swap chain is made");
            Ok(Presenter {
                swap_chain,
                width,
                height,
                format,
                copy,
                ahead: Vec::new(),
                hwnd,
                meter: Meter::new(),
            })
        }
    }
}

impl Host {
    pub(crate) fn video_memory(&self) -> Option<[u64; 4]> {
        let adapter = self.adapter.as_ref()?;
        let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut shared = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        unsafe {
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own)
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut shared)
                .ok()?;
        }
        static LAST: [std::sync::atomic::AtomicU64; 3] =
            [const { std::sync::atomic::AtomicU64::new(0) }; 3];
        let now = [own.Budget, own.CurrentUsage, shared.CurrentUsage];
        let last = [0, 1, 2].map(|i| LAST[i].swap(now[i], Ordering::Relaxed));
        if last[0] != 0 && last[0].abs_diff(own.Budget) >= (256u64 << 20) {
            tracing::info!(
                "the system changed the budget of the card's memory from {} MB to {} MB (frame {}); at the last look {} MB of the card's and {} MB of the PC's shared memory were in use",
                last[0] >> 20,
                own.Budget >> 20,
                crate::frame::FramePacer::frame(),
                last[1] >> 20,
                last[2] >> 20
            );
            self.log_video_memory("right after that change");
        }
        Some([
            own.Budget,
            own.CurrentUsage,
            shared.Budget,
            shared.CurrentUsage,
        ])
    }

    pub(crate) fn video_memory_groups(&self) -> Option<[[u64; 4]; 2]> {
        let adapter = self.adapter.as_ref()?;
        let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut shared = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        unsafe {
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own)
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut shared)
                .ok()?;
        }
        let all = |m: &DXGI_QUERY_VIDEO_MEMORY_INFO| {
            [
                m.Budget,
                m.CurrentUsage,
                m.AvailableForReservation,
                m.CurrentReservation,
            ]
        };
        Some([all(&own), all(&shared)])
    }

    pub(crate) fn log_video_memory(&self, when: &str) {
        let Some([own, shared]) = self.video_memory_groups() else {
            return;
        };
        tracing::info!(
            "video memory as the system counts it, {}: the card's own: budget {} MB, in use {} MB, {} MB could still be reserved, {} MB reserved; the PC's shared with the card: budget {} MB, in use {} MB, {} MB could still be reserved, {} MB reserved (the layer reserves none)",
            when,
            own[0] >> 20,
            own[1] >> 20,
            own[2] >> 20,
            own[3] >> 20,
            shared[0] >> 20,
            shared[1] >> 20,
            shared[2] >> 20,
            shared[3] >> 20
        );
    }

    pub(crate) fn shared_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        MADE[2].fetch_add(1, Ordering::Relaxed);
        MADE[3].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no shared buffer");
                None
            }
        }
    }

    pub(crate) fn uav_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        MADE[4].fetch_add(1, Ordering::Relaxed);
        MADE[5].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let made = unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut buffer,
            )
        };
        match made {
            Ok(()) => buffer,
            Err(e) => {
                tracing::warn!(error = %e, size, "no unordered-access buffer");
                None
            }
        }
    }
}

#[cfg(test)]
mod trail {
    #[test]
    fn the_removal_report_says_the_newest_things_done() {
        for n in 0..super::TRAIL_MOST + 5 {
            super::HostTrail::trail(|| format!("thing {n} done"));
        }
        let text = super::HostTrail::removal_context();
        assert!(text.contains("the submitting thread was"));
        assert!(text.contains("pipeline library: no call running"));
        assert!(text.contains(&format!("thing {} done", super::TRAIL_MOST + 4)));
        assert!(
            !text.contains("thing 4 done"),
            "the oldest entries are dropped"
        );
    }
}

#[cfg(test)]
mod spare {
    use super::Spare;

    #[test]
    fn kept_things_come_back_by_size_within_the_limit() {
        let mut spare: Spare<u32> = Spare::new();
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.give(1, 4, 10), None);
        assert_eq!(spare.give(2, 1, 10), None);
        assert_eq!(spare.give(3, 4, 10), None);
        assert_eq!(spare.bytes, 9);
        assert_eq!(spare.give(4, 4, 10), Some(4));
        assert_eq!(spare.bytes, 9);
        assert_eq!(spare.take(2), None, "no thing of that size");
        assert_eq!(spare.take(4), Some(3));
        assert_eq!(spare.take(4), Some(1));
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.take(1), Some(2));
        assert_eq!(spare.bytes, 0);
        assert!(spare.kept.is_empty());
    }
}

#[cfg(test)]
mod batch {
    use super::Batch;
    use std::sync::Arc;

    #[test]
    fn a_texture_is_checked_once_per_submission() {
        let mut batch = Batch::default();
        let (a, b) = (Arc::new([0u8; 16]), Arc::new([0u8; 16]));
        assert!(batch.texture_due(&a));
        assert!(batch.texture_due(&b));
        assert!(!batch.texture_due(&a));
        assert!(!batch.texture_due(&a.clone()));
        assert!(!batch.texture_due(&b));
    }

    #[test]
    fn title_writes_make_textures_due_again() {
        let mut batch = Batch::default();
        let a = Arc::new(1u32);
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        batch.title_written();
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        let mut next = Batch::default();
        assert!(next.texture_due(&a));
    }

    #[test]
    #[ignore = "benchmark"]
    fn texture_checks_per_submission() {
        const TEXTURES: usize = 48;
        const SIZE: usize = 4 << 20;
        const LISTS: usize = 12;
        let textures: Vec<Arc<Vec<u8>>> = (0..TEXTURES)
            .map(|t| Arc::new((0..SIZE).map(|i| (i * 31 + t) as u8).collect()))
            .collect();
        let submit = |dedupe: bool| {
            let mut batch = Batch::default();
            let mut hashed = 0usize;
            let started = std::time::Instant::now();
            for _ in 0..LISTS {
                for t in &textures {
                    if !dedupe || batch.texture_due(t) {
                        std::hint::black_box(blake3::hash(t));
                        hashed += 1;
                    }
                }
            }
            (started.elapsed(), hashed)
        };
        submit(false);
        let (before, n_before) = submit(false);
        let (after, n_after) = submit(true);
        assert_eq!((n_before, n_after), (TEXTURES * LISTS, TEXTURES));
        println!(
            "{LISTS} lists sampling {TEXTURES} textures of {} MiB: \
             before {n_before} hashes in {before:?}, after {n_after} in {after:?} ({:.1}x)",
            SIZE >> 20,
            before.as_secs_f64() / after.as_secs_f64()
        );
    }

    #[test]
    fn vram_idle_ms_scales_with_pressure() {
        use super::HostClock;
        let gib = 1024 * 1024 * 1024;
        assert_eq!(HostClock::vram_idle_ms(Some([10 * gib, 0, 0, 0])), 5_000);
        assert_eq!(
            HostClock::vram_idle_ms(Some([10 * gib, 7 * gib, 0, 0])),
            5_000
        );
        assert_eq!(HostClock::vram_idle_ms(Some([100, 74, 0, 0])), 5_000);
        assert_eq!(HostClock::vram_idle_ms(Some([100, 75, 0, 0])), 2_500);
        assert_eq!(HostClock::vram_idle_ms(Some([100, 84, 0, 0])), 2_500);
        assert_eq!(HostClock::vram_idle_ms(Some([100, 85, 0, 0])), 1_000);
        assert_eq!(
            HostClock::vram_idle_ms(Some([10 * gib, 12 * gib, 0, 0])),
            1_000
        );
        assert_eq!(HostClock::vram_idle_ms(None), 5_000);
        assert_eq!(HostClock::vram_idle_ms(Some([0, 100, 0, 0])), 5_000);
    }

    #[test]
    fn ram_idle_ms_scales_with_pressure() {
        use super::HostClock;
        let gib = 1024 * 1024 * 1024;
        assert_eq!(HostClock::ram_idle_ms(Some(8 * gib)), 10_000);
        assert_eq!(HostClock::ram_idle_ms(Some(4 * gib)), 10_000);
        assert_eq!(HostClock::ram_idle_ms(Some(4 * gib - 1)), 5_000);
        assert_eq!(HostClock::ram_idle_ms(Some(2 * gib)), 5_000);
        assert_eq!(HostClock::ram_idle_ms(Some(2 * gib - 1)), 2_000);
        assert_eq!(HostClock::ram_idle_ms(Some(0)), 2_000);
        assert_eq!(HostClock::ram_idle_ms(None), 10_000);
    }
}
