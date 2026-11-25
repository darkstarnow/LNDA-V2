#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(windows)]
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::object::{
    self, ComObject, E_INVALIDARG, HRESULT, Iid, Object, QUEUE, S_OK, Table, This, guid,
};
use crate::resource::PAGEABLE_IIDS;

const QUEUE_IIDS: [Iid; 4] = [
    guid(
        0x0ec870a6,
        0x5d7e,
        0x4c22,
        [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDesc {
    kind: u32,
    priority: i32,
    flags: u32,
    nodes: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDescX {
    kind: u32,
    flags: u32,
    engine_or_pipe: u32,
    queue: u32,
}

impl QueueDescX {
    fn as_pc(self) -> QueueDesc {
        QueueDesc {
            kind: self.kind,
            priority: 0,
            flags: self.flags,
            nodes: 1,
        }
    }
}

#[cfg(windows)]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(windows)]
pub(crate) struct Switches {
    waits: bool,
    split: bool,
    cut: bool,
    force: bool,
}

#[cfg(windows)]
impl Switches {
    fn get() -> &'static Self {
        static SWITCHES: OnceLock<Switches> = OnceLock::new();
        SWITCHES.get_or_init(|| {
            let on = |name: &str| std::env::var_os(name).is_none_or(|v| v != "0");
            Switches {
                waits: on("LEONIDA_QUEUE_WAIT"),
                split: on("LEONIDA_SPLIT_WAITS"),
                cut: on("LEONIDA_CUT_WAITS"),
                force: on("LEONIDA_WAIT_FORCE"),
            }
        })
    }

    pub(crate) fn cuts() -> bool {
        let switches = Self::get();
        switches.waits && switches.split && switches.cut
    }

    fn splits() -> bool {
        let switches = Self::get();
        switches.waits && switches.split
    }

    fn waits() -> bool {
        Self::get().waits
    }

    fn forces() -> bool {
        Self::get().force
    }
}

#[cfg(windows)]
struct LogBudget(AtomicU64, u64);

#[cfg(windows)]
impl LogBudget {
    const fn new(limit: u64) -> Self {
        Self(AtomicU64::new(0), limit)
    }

    fn spend(&self) -> bool {
        self.0.fetch_add(1, Ordering::Relaxed) < self.1
    }
}

#[cfg(windows)]
enum Held {
    Wait(usize, u64),
    Value(crate::list::ValueWait),
    Run(Vec<crate::list::Detached>, Vec<usize>),
    Signal(usize, u64),
}

#[cfg(windows)]
impl Held {
    fn blocks(&self) -> bool {
        match self {
            Held::Wait(fence, value) => crate::fence::Fence::completed(*fence as This) < *value,
            Held::Value(wait) => !Self::value_met(wait),
            _ => false,
        }
    }

    fn value_met(wait: &crate::list::ValueWait) -> bool {
        wait.met() || (crate::host::late() && crate::host::late_met(wait))
    }

    fn run(self, queue: usize, all: bool, rest: &[Held]) {
        match self {
            Held::Wait(..) => {}
            Held::Value(wait) => {
                static SEEN: LogBudget = LogBudget::new(16);
                if all && !Self::value_met(&wait) && SEEN.spend() {
                    tracing::warn!(
                        queue = format_args!("{queue:#x}"),
                        ?wait,
                        "a wait for a memory value was not met at a present: the queue runs on"
                    );
                }
            }
            Held::Run(lists, order) => Self::run_lists(queue, all, lists, order, rest),
            Held::Signal(fence, value) => {
                if !crate::host::late_signal(fence, value) {
                    crate::fence::Fence::advance(fence as This, value);
                }
            }
        }
    }

    fn run_lists(
        queue: usize,
        all: bool,
        lists: Vec<crate::list::Detached>,
        order: Vec<usize>,
        rest: &[Held],
    ) {
        #[cfg(feature = "diagnostics")]
        {
            crate::capture::op(queue, || {
                format!(
                    "RUN held submission of {} lists (forced {all})",
                    order.len()
                )
            });
            let waits = rest
                .iter()
                .take_while(|h| matches!(h, Held::Value(_)))
                .count();
            if waits > 0
                && matches!(rest.get(waits), Some(Held::Run(..)))
                && rest[..waits]
                    .iter()
                    .all(|h| matches!(h, Held::Value(w) if w.met()))
            {
                crate::copystat::add(crate::copystat::C::PartsTogether, 1);
            }
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = (queue, rest);
        if crate::host::late()
            && let Some(host) = crate::host::host()
        {
            host.execute_late(lists, order);
        } else if let Some(host) = crate::host::host() {
            let runs: Vec<&crate::host::HostList> =
                order.iter().map(|&i| lists[i].host()).collect();
            host.execute(&runs);
            crate::list::ListPool::recycle(lists, &order);
        }
        if all {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Forced, 1);
            static ONCE: AtomicBool = AtomicBool::new(false);
            if !ONCE.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "a queue still held a submission at a present: its fence was not reached"
                );
            }
        }
    }
}

#[cfg(windows)]
pub(crate) struct Scheduler {
    waiting: Mutex<Vec<usize>>,
    releasing: Mutex<()>,
}

#[cfg(windows)]
static SCHEDULER: Scheduler = Scheduler {
    waiting: Mutex::new(Vec::new()),
    releasing: Mutex::new(()),
};

#[cfg(windows)]
impl Scheduler {
    fn enlist(this: This) {
        let mut waiting = lock(&SCHEDULER.waiting);
        if !waiting.contains(&(this as usize)) {
            waiting.push(this as usize);
        }
    }

    fn forget(queue: usize) {
        lock(&SCHEDULER.waiting).retain(|&q| q != queue);
    }

    pub(crate) fn release_held(all: bool) {
        if lock(&SCHEDULER.waiting).is_empty() {
            return;
        }
        let _one = lock(&SCHEDULER.releasing);
        loop {
            let queues = lock(&SCHEDULER.waiting).clone();
            let moved = queues
                .into_iter()
                .fold(false, |moved, q| Self::drain(q, all) | moved);
            if !moved {
                break;
            }
        }
    }

    fn drain(queue: usize, all: bool) -> bool {
        let mut held = CommandQueue::from_this(queue as This).held();
        let mut moved = false;
        while let Some(first) = held.first() {
            if !all && first.blocks() {
                break;
            }
            let next = held.remove(0);
            next.run(queue, all, &held);
            moved = true;
        }
        if held.is_empty() {
            Self::forget(queue);
        }
        moved
    }
}

#[cfg(not(windows))]
pub(crate) struct Scheduler;

#[cfg(not(windows))]
impl Scheduler {
    pub(crate) fn release_held(_: bool) {}
}

#[repr(C)]
pub(crate) struct CommandQueue {
    object: Object,
    kind: u32,
    desc: QueueDesc,
    #[cfg(windows)]
    held: Mutex<Vec<Held>>,
}
const _: () = assert!(std::mem::offset_of!(CommandQueue, kind) == 0x10);

unsafe impl ComObject for CommandQueue {
    const KIND: usize = QUEUE;
    const INTERFACES: &'static [Iid] = &QUEUE_IIDS;

    extern "system" fn release(this: This) -> u32 {
        let left = object::release_boxed::<Self>(this);
        #[cfg(windows)]
        if left == 0 {
            Scheduler::forget(this as usize);
        }
        left
    }
}

impl CommandQueue {
    fn new(desc: QueueDesc) -> Box<Self> {
        Box::new(Self {
            object: if crate::device::Device::newer() {
                Object::new(&NEWER_FUNCTIONS)
            } else {
                Object::new(&FUNCTIONS)
            },
            kind: desc.kind,
            desc,
            #[cfg(windows)]
            held: Default::default(),
        })
    }

    #[cfg(windows)]
    fn held(&self) -> MutexGuard<'_, Vec<Held>> {
        lock(&self.held)
    }

    #[cfg(windows)]
    fn hold(this: This, what: impl FnOnce() -> Held) -> bool {
        if this.is_null() {
            return false;
        }
        let mut held = Self::from_this(this).held();
        if held.is_empty() {
            return false;
        }
        held.push(what());
        true
    }

    #[cfg(all(windows, feature = "diagnostics"))]
    fn describe_submission(this: This, lists: &[This], note: &str) {
        crate::capture::op(this as usize, || {
            let kind = (!this.is_null()).then(|| Self::from_this(this).kind);
            let named: Vec<String> = lists
                .iter()
                .map(|&l| match crate::list::CommandList::host_list(l) {
                    Some(h) => format!("{:#x}", std::ptr::from_ref(h) as usize),
                    None => format!("title {l:?}"),
                })
                .collect();
            format!("SUBMIT queue kind {kind:?} {note} lists {named:?}")
        });
    }

    #[cfg(all(windows, not(feature = "diagnostics")))]
    fn describe_submission(_: This, _: &[This], _: &str) {}

    #[cfg(windows)]
    fn submit(this: This, lists: &[This]) -> bool {
        let Some(host) = crate::host::host() else {
            return false;
        };
        if !this.is_null() && Switches::cuts() {
            Self::from_this(this).submit_cut(this, host, lists);
        } else if !this.is_null() && Switches::splits() {
            Self::from_this(this).submit_split(this, host, lists);
        } else {
            Self::submit_whole(this, host, lists);
        }
        true
    }

    #[cfg(windows)]
    fn submit_cut(&self, this: This, host: &crate::host::Host, lists: &[This]) {
        let first = if self.held().is_empty() {
            lists
                .iter()
                .position(|&l| crate::list::CommandList::cut_list(l))
                .unwrap_or(lists.len())
        } else {
            0
        };
        Self::describe_submission(
            this,
            lists,
            &format!(
                "held {} (the first {first} run at once, the rest part by part)",
                first < lists.len()
            ),
        );
        if first > 0 && crate::host::late() {
            let (taken, order) = crate::list::CommandList::detach(&lists[..first]);
            host.execute_late(taken, order);
        } else if first > 0 {
            let now: Vec<_> = lists[..first]
                .iter()
                .filter_map(|&l| crate::list::CommandList::host_list(l))
                .collect();
            host.execute(&now);
            crate::list::CommandList::tables_done(&lists[..first]);
        }
        if first < lists.len() {
            let parts = crate::list::CommandList::detach_parts(&lists[first..]);
            static SEEN: LogBudget = LogBudget::new(8);
            if SEEN.spend() {
                tracing::info!(
                    kind = self.kind,
                    lists = lists.len(),
                    run_at_once = first,
                    parts = parts.len(),
                    waits = parts.iter().map(|p| p.0.len()).sum::<usize>(),
                    "a submission is given to its queue part by part (its lists wait for memory values)"
                );
            }
            {
                let mut held = self.held();
                for (waits, taken, order) in parts {
                    held.extend(waits.into_iter().map(Held::Value));
                    held.push(Held::Run(taken, order));
                }
            }
            Scheduler::enlist(this);
        }
        Scheduler::release_held(false);
    }

    #[cfg(windows)]
    fn submit_split(&self, this: This, host: &crate::host::Host, lists: &[This]) {
        let by = crate::list::CommandList::waits_by_list(lists);
        let cut = if self.held().is_empty() {
            (0..lists.len())
                .find(|&i| by[i].iter().any(|w| !w.met()))
                .unwrap_or(lists.len())
        } else {
            0
        };
        Self::describe_submission(
            this,
            lists,
            &format!("held {} (the first {cut} run at once)", cut < lists.len()),
        );
        if cut > 0 {
            let now: Vec<_> = lists[..cut]
                .iter()
                .filter_map(|&l| crate::list::CommandList::host_list(l))
                .collect();
            host.execute(&now);
        }
        if cut < lists.len() {
            static SEEN: LogBudget = LogBudget::new(8);
            if SEEN.spend() {
                tracing::info!(
                    kind = self.kind,
                    lists = lists.len(),
                    run_at_once = cut,
                    waits = ?by[cut],
                    "lists of a submission are held for a memory value"
                );
            }
            let pieces = Self::split_pieces(lists, &by, cut);
            self.held().extend(pieces);
            Scheduler::enlist(this);
        }
        Scheduler::release_held(false);
    }

    #[cfg(windows)]
    fn split_pieces(lists: &[This], by: &[Vec<crate::list::ValueWait>], cut: usize) -> Vec<Held> {
        let rest = &lists[cut..];
        let twice = rest.iter().enumerate().any(|(i, l)| rest[..i].contains(l));
        let mut pieces = Vec::new();
        let mut from = cut;
        while from < lists.len() {
            let to = if twice {
                lists.len()
            } else {
                (from + 1..lists.len())
                    .find(|&i| !by[i].is_empty())
                    .unwrap_or(lists.len())
            };
            pieces.extend(by[from..to].concat().into_iter().map(Held::Value));
            let (taken, order) = crate::list::CommandList::detach(&lists[from..to]);
            pieces.push(Held::Run(taken, order));
            from = to;
        }
        pieces
    }

    #[cfg(windows)]
    fn submit_whole(this: This, host: &crate::host::Host, lists: &[This]) {
        if Switches::waits() && !this.is_null() {
            let unmet = crate::list::CommandList::unmet_waits(lists);
            if !unmet.is_empty() {
                let queue = Self::from_this(this);
                static SEEN: LogBudget = LogBudget::new(8);
                if SEEN.spend() {
                    tracing::info!(
                        kind = queue.kind,
                        ?unmet,
                        "a submission is held for a memory value"
                    );
                }
                queue.held().extend(unmet.into_iter().map(Held::Value));
                Scheduler::enlist(this);
            }
        }
        let kept = Self::hold(this, || {
            let (taken, order) = crate::list::CommandList::detach(lists);
            Held::Run(taken, order)
        });
        Self::describe_submission(this, lists, &format!("held {kept}"));
        if kept {
            return;
        }
        let host_lists: Vec<_> = lists
            .iter()
            .filter_map(|&l| crate::list::CommandList::host_list(l))
            .collect();
        host.execute(&host_lists);
        crate::list::CommandList::tables_done(lists);
        Scheduler::release_held(false);
    }

    #[cfg(windows)]
    fn wait_for(&self, this: This, fence: This, value: u64) {
        let now = crate::fence::Fence::completed(fence);
        static CALLS: LogBudget = LogBudget::new(8);
        if CALLS.spend() {
            tracing::info!(
                kind = self.kind,
                now,
                value,
                "a queue is told to wait for a fence"
            );
        }
        #[cfg(feature = "diagnostics")]
        crate::capture::op(this as usize, || {
            format!(
                "WAIT queue kind {} for fence {fence:?} value {value} (now {now})",
                self.kind
            )
        });
        let waits = Switches::waits();
        let behind = {
            let mut held = self.held();
            let behind = !held.is_empty() || now < value;
            if behind && waits {
                held.push(Held::Wait(fence as usize, value));
            }
            behind
        };
        if behind {
            static SEEN: LogBudget = LogBudget::new(8);
            if SEEN.spend() {
                tracing::info!(
                    kind = self.kind,
                    now,
                    value,
                    held = waits,
                    "a queue waits for a fence not reached yet"
                );
            }
        }
        if behind && waits {
            Scheduler::enlist(this);
            Scheduler::release_held(false);
        }
    }

    pub(crate) extern "system" fn create(
        _: This,
        desc: *const QueueDesc,
        iid: *const Iid,
        out: *mut This,
    ) -> HRESULT {
        let Some(&desc) = (unsafe { desc.as_ref() }) else {
            return E_INVALIDARG;
        };
        tracing::debug!(?desc, "command queue");
        Self::new(desc).hand_out(iid, out)
    }

    pub(crate) extern "system" fn create_x(
        this: This,
        desc: *const QueueDescX,
        iid: *const Iid,
        out: *mut This,
    ) -> HRESULT {
        let Some(&x) = (unsafe { desc.as_ref() }) else {
            return E_INVALIDARG;
        };
        tracing::debug!(desc = ?x, "console command queue");
        Self::create(this, &x.as_pc(), iid, out)
    }

    extern "system" fn execute_command_lists(this: This, count: u32, lists: *const This) {
        if lists.is_null() {
            return;
        }
        #[cfg(windows)]
        if Self::submit(this, unsafe {
            std::slice::from_raw_parts(lists, count as usize)
        }) {
            return;
        }
        static ONCE: AtomicBool = AtomicBool::new(false);
        if !ONCE.swap(true, Ordering::Relaxed) {
            tracing::warn!(count, "command lists submitted; no host GPU runs them");
        }
    }

    extern "system" fn execute_command_lists_x(
        this: This,
        count: u32,
        lists: *const This,
        _token: u64,
        _flags: u32,
    ) {
        Self::execute_command_lists(this, count, lists);
    }

    extern "system" fn marker(_: This) {}

    extern "system" fn signal(this: This, fence: This, value: u64) -> HRESULT {
        if fence.is_null() {
            return E_INVALIDARG;
        }
        #[cfg(windows)]
        if Self::hold(this, || Held::Signal(fence as usize, value))
            || crate::host::late_signal(fence as usize, value)
        {
            return S_OK;
        }
        let _ = this;
        crate::fence::Fence::signal(fence, value)
    }

    extern "system" fn signal_x(
        this: This,
        fence: This,
        value: u64,
        _bottom: *const u8,
        _top: *const u8,
    ) -> HRESULT {
        Self::signal(this, fence, value)
    }

    extern "system" fn wait(this: This, fence: This, value: u64) -> HRESULT {
        if fence.is_null() {
            return E_INVALIDARG;
        }
        #[cfg(windows)]
        if !this.is_null() {
            Self::from_this(this).wait_for(this, fence, value);
        }
        let _ = (this, value);
        S_OK
    }

    extern "system" fn get_timestamp_frequency(_: This, out: *mut u64) -> HRESULT {
        if out.is_null() {
            return E_INVALIDARG;
        }
        unsafe { out.write(TIMESTAMP_FREQUENCY) };
        S_OK
    }

    extern "system" fn get_desc(this: This, out: *mut QueueDesc) {
        if !out.is_null() {
            unsafe { out.write(Self::from_this(this).desc) };
        }
    }

    extern "system" fn copy_page_mappings(
        _: This,
        count: u32,
        batches: *const PageBatch,
        pool: u64,
        pool_pages: u32,
        _flags: u32,
    ) -> HRESULT {
        let Some(map) = crate::resource::MemoryHooks::page_mapper() else {
            return E_INVALIDARG;
        };
        if batches.is_null() && count > 0 {
            return E_INVALIDARG;
        }
        let batches =
            (count > 0).then(|| unsafe { std::slice::from_raw_parts(batches, count as usize) });
        let pool = PagePool {
            base: pool,
            pages: pool_pages,
        };
        let ok = batches
            .into_iter()
            .flatten()
            .fold(true, |ok, batch| batch.apply(&pool, map) & ok);
        if !ok {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(
                    pool = format_args!("{:#x}", pool.base),
                    "a page mapping failed"
                )
            });
        }
        S_OK
    }

    extern "system" fn present(
        _: This,
        planes: u32,
        plane_parameters: *const Plane,
        _parameters: *const u8,
    ) -> HRESULT {
        Presenter::present(planes, plane_parameters);
        S_OK
    }
}

const TIMESTAMP_FREQUENCY: u64 = 100_000_000;

#[derive(Debug, Clone, Copy)]
pub(crate) enum Written {
    Value(u64),
    GpuTimestamp,
    SystemTimestamp,
}

impl Written {
    pub(crate) fn write(self, at: *mut u8, bytes: usize) {
        let value = match self {
            Written::Value(v) => v,
            Written::GpuTimestamp => Self::gpu_timestamp(),
            Written::SystemTimestamp => Self::system_timestamp(),
        };
        unsafe {
            if bytes == 4 {
                at.cast::<u32>().write_unaligned(value as u32);
            } else {
                at.cast::<u64>().write_unaligned(value);
            }
        }
    }

    fn gpu_timestamp() -> u64 {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
        (elapsed.as_nanos() / (1_000_000_000 / u128::from(TIMESTAMP_FREQUENCY))) as u64
    }

    #[cfg(windows)]
    fn system_timestamp() -> u64 {
        let mut count = 0;
        unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut count) };
        count as u64
    }

    #[cfg(not(windows))]
    fn system_timestamp() -> u64 {
        0
    }
}

#[repr(C)]
pub(crate) struct Plane {
    token: u64,
    count: u32,
    resources: *const This,
}

impl Plane {
    #[cfg(windows)]
    fn first_resource(planes: u32, plane: *const Plane) -> Option<This> {
        if planes == 0 {
            return None;
        }
        let plane = unsafe { plane.as_ref() }?;
        (plane.count > 0 && !plane.resources.is_null()).then(|| unsafe { *plane.resources })
    }
}

pub(crate) struct Presenter;

static FRAMES: AtomicU64 = AtomicU64::new(0);

impl Presenter {
    #[cfg(feature = "diagnostics")]
    pub(crate) fn presented() -> u64 {
        FRAMES.load(Ordering::Relaxed)
    }

    fn present(planes: u32, plane: *const Plane) {
        #[cfg(all(windows, feature = "diagnostics"))]
        crate::copystat::present_begins();
        #[cfg(windows)]
        Self::settle();
        let frame = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
        if frame.is_power_of_two() {
            Self::report(frame);
        }
        #[cfg(all(windows, feature = "diagnostics"))]
        {
            crate::capture::frame_drew(frame, crate::draw::DrawCounters::drawn());
            if crate::capture::listed(frame) {
                crate::capture::dump_targets(frame);
            }
        }
        #[cfg(windows)]
        if let Some(resource) = Plane::first_resource(planes, plane) {
            Self::show(resource, frame);
        }
        #[cfg(not(windows))]
        let _ = (planes, plane);
    }

    #[cfg(windows)]
    fn settle() {
        if crate::host::late() && !crate::host::late_free_present() {
            crate::host::settle_for_present();
            Scheduler::release_held(false);
            crate::host::settle_for_present();
        } else if crate::host::late() {
            Scheduler::release_held(false);
        }
        Scheduler::release_held(Switches::forces());
    }

    fn report(frame: u64) {
        let (draws, dispatches) = crate::list::WorkCounters::take_counts();
        #[cfg(windows)]
        let (drawn, dispatched) = {
            static DRAWN: AtomicU64 = AtomicU64::new(0);
            static DISPATCHED: AtomicU64 = AtomicU64::new(0);
            let since = |last: &AtomicU64, now: u64| now - last.swap(now, Ordering::Relaxed);
            (
                since(&DRAWN, crate::draw::DrawCounters::drawn()),
                since(&DISPATCHED, crate::draw::DrawCounters::dispatched()),
            )
        };
        #[cfg(not(windows))]
        let (drawn, dispatched) = (0, 0);
        tracing::info!(
            frame,
            draws,
            drawn,
            dispatches,
            dispatched,
            "frame presented (since the last line: draws and dispatches, and those run on the host)"
        );
        #[cfg(windows)]
        if frame >= 256 {
            let (hits, misses) = crate::shader_cache::counts();
            tracing::info!(hits, misses, "shaders from the cache and translated so far");
        }
    }

    #[cfg(windows)]
    fn show(resource: This, frame: u64) {
        let (Some(texture), Some(host)) = (
            crate::resource::HostTextures::host_resource(resource),
            crate::host::host(),
        ) else {
            return;
        };
        const COPY_SOURCE: u32 = 0x800;
        let state = crate::resource::HostTextures::host_transition(resource, COPY_SOURCE)
            .map_or(COPY_SOURCE, |(_, before)| before);
        let shown = host.present(texture, state, frame);
        crate::resource::HostTextures::host_transition(resource, state);
        #[cfg(feature = "diagnostics")]
        BlackFrames::observe(frame, resource, state, shown);
        #[cfg(not(feature = "diagnostics"))]
        let _ = shown;
    }
}

#[cfg(all(windows, feature = "diagnostics"))]
struct BlackFrames {
    black: bool,
    drawn: u64,
    count: u64,
}

#[cfg(all(windows, feature = "diagnostics"))]
impl BlackFrames {
    fn observe(frame: u64, resource: This, state: u32, shown: Option<bool>) {
        static LAST: Mutex<BlackFrames> = Mutex::new(BlackFrames {
            black: false,
            drawn: 0,
            count: 0,
        });
        static AFTER: AtomicU64 = AtomicU64::new(0);
        let mut last = lock(&LAST);
        let drawn = crate::draw::DrawCounters::drawn();
        let black = shown.unwrap_or(false);
        let draws = crate::draw::DrawCounters::take_frame();
        if black && !last.black {
            last.count += 1;
            tracing::warn!(
                frame,
                resource = ?resource,
                state = format_args!("{state:#x}"),
                draws = drawn - last.drawn,
                copied = shown.is_some(),
                black_frames = last.count,
                levels = ?crate::capture::recent_levels(),
                "a black frame after a frame that was not"
            );
            for d in draws {
                tracing::warn!("black frame {frame}: {d}");
            }
            AFTER.store(frame + 6, Ordering::Relaxed);
        }
        if AFTER.load(Ordering::Relaxed) == frame {
            tracing::warn!(frame, levels = ?crate::capture::recent_levels(), "levels after the black frame");
        }
        last.black = black;
        last.drawn = drawn;
    }
}

#[repr(C)]
pub(crate) struct PageRange {
    kind: u32,
    start: u32,
    count: u32,
}

#[repr(C)]
pub(crate) struct PageBatch {
    destination: u64,
    count: u32,
    ranges: *const PageRange,
}

struct PagePool {
    base: u64,
    pages: u32,
}

const PAGE: u64 = 64 << 10;

impl PagePool {
    fn holds(&self, start: u32, pages: u32) -> bool {
        start
            .checked_add(pages)
            .is_some_and(|end| end <= self.pages)
    }
}

impl PageRange {
    fn apply(&self, at: u64, pool: &PagePool, map: crate::resource::PageMapper) -> bool {
        let len = u64::from(self.count) * PAGE;
        let source = pool.base + u64::from(self.start) * PAGE;
        match self.kind {
            0 if pool.holds(self.start, self.count) => map(at, source, len),
            1 if pool.holds(self.start, 1) => {
                (0..u64::from(self.count)).all(|k| map(at + k * PAGE, source, PAGE))
            }
            2 => map(at, 0, len),
            3 => true,
            _ => false,
        }
    }
}

impl PageBatch {
    fn apply(&self, pool: &PagePool, map: crate::resource::PageMapper) -> bool {
        if self.ranges.is_null() {
            return true;
        }
        let ranges = unsafe { std::slice::from_raw_parts(self.ranges, self.count as usize) };
        let mut at = self.destination;
        let mut ok = true;
        for range in ranges {
            ok &= range.apply(at, pool, map);
            at += u64::from(range.count) * PAGE;
        }
        ok
    }
}

const METHODS: [(usize, *const ()); 15] = [
    (0, CommandQueue::query_interface as *const ()),
    (2, <CommandQueue as ComObject>::release as *const ()),
    (7, object::parent_device as *const ()),
    (10, CommandQueue::execute_command_lists as *const ()),
    (11, CommandQueue::marker as *const ()),
    (12, CommandQueue::marker as *const ()),
    (13, CommandQueue::marker as *const ()),
    (14, CommandQueue::signal as *const ()),
    (15, CommandQueue::wait as *const ()),
    (16, CommandQueue::get_timestamp_frequency as *const ()),
    (26, CommandQueue::get_desc as *const ()),
    (29, CommandQueue::copy_page_mappings as *const ()),
    (30, CommandQueue::present as *const ()),
    (33, CommandQueue::execute_command_lists_x as *const ()),
    (32, CommandQueue::signal_x as *const ()),
];

const fn newer_slot(slot: usize) -> usize {
    if slot >= 14 { slot + 2 } else { slot }
}

const fn queue_table(newer: bool) -> [*const (); 36] {
    let mut f = object::d3d12_object(object::missing_table::<QUEUE, 36>());
    let mut i = 0;
    while i < METHODS.len() {
        let (slot, method) = METHODS[i];
        f[if newer { newer_slot(slot) } else { slot }] = method;
        i += 1;
    }
    f
}

static FUNCTIONS: Table<36> = Table(queue_table(false));
static NEWER_FUNCTIONS: Table<36> = Table(queue_table(true));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    fn signal_slot(q: This) -> usize {
        let table = unsafe { *q.cast::<*const *const ()>() };
        if table == NEWER_FUNCTIONS.0.as_ptr() {
            0x80
        } else {
            0x70
        }
    }

    #[test]
    fn queue_signals_complete_at_once() {
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let mut q = null;
        assert_eq!(
            CommandQueue::create(null, &desc, &QUEUE_IIDS[0], &mut q),
            S_OK
        );
        assert_eq!(unsafe { q.cast::<u8>().add(0x10).cast::<u32>().read() }, 0);
        let mut fence = null;
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        assert_eq!(
            crate::fence::Fence::create(null, 0, 0, fence_iid, &mut fence),
            S_OK
        );
        let signal = slot::<extern "system" fn(This, This, u64) -> HRESULT>(q, signal_slot(q));
        assert_eq!(signal(q, fence, 7), S_OK);
        assert_eq!(
            slot::<extern "system" fn(This) -> u64>(fence, 0x40)(fence),
            7
        );
        assert_eq!(slot::<extern "system" fn(This) -> u32>(q, 0x10)(q), 0);
    }

    #[test]
    fn console_queues_are_created_as_on_pc() {
        let null = std::ptr::null();
        let desc = QueueDescX {
            kind: 2,
            flags: 0,
            engine_or_pipe: 1,
            queue: 0,
        };
        let mut q = null;
        assert_eq!(
            CommandQueue::create_x(null, &desc, &QUEUE_IIDS[0], &mut q),
            S_OK
        );
        assert_eq!(CommandQueue::from_this(q).desc.kind, 2);
        let none = std::ptr::null();
        assert_eq!(
            CommandQueue::create_x(null, none, &QUEUE_IIDS[0], &mut q),
            E_INVALIDARG
        );
    }

    #[test]
    fn newer_queues_have_their_methods_two_slots_later() {
        let (old, new) = (queue_table(false), queue_table(true));
        assert_eq!(old[10], new[10]);
        assert_eq!(old[14], new[16]);
        assert_eq!(old[30], new[32]);
        assert_eq!(new[35], CommandQueue::execute_command_lists_x as *const ());
    }

    #[test]
    fn timestamps_and_values_are_written() {
        let mut at = [0u64; 2];
        Written::Value(0x1_0000_0002).write(at.as_mut_ptr().cast(), 4);
        assert_eq!(at[0], 2);
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        let first = at[1];
        std::thread::sleep(std::time::Duration::from_millis(2));
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        assert!(at[1] > first);
    }
}
