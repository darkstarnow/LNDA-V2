use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queue_state: AtomicU8,
    made: OnceLock<Option<HostPipeline>>,
    key: String,
    used: AtomicBool,
    left_out: Option<&'static str>,
    begun: OnceLock<Instant>,
}

pub(crate) struct PipelineMaker;

pub(crate) struct WaitBudget;

impl PipelineMaker {
    fn left_out(shaders: &[Arc<Shader>]) -> Option<&'static str> {
        if PipelineMaker::without_structures(shaders) {
            Some("shades ray-tracing hits, left out")
        } else if PipelineMaker::too_dear(shaders) {
            Some("too dear to compile, left out")
        } else {
            None
        }
    }

    fn named(shaders: &[Arc<Shader>], name: &[u8]) -> bool {
        shaders
            .iter()
            .any(|s| s.stage == "compute" && s.container.windows(name.len()).any(|w| w == name))
    }

    fn too_dear(shaders: &[Arc<Shader>]) -> bool {
        static OUT: OnceLock<bool> = OnceLock::new();
        let out = *OUT
            .get_or_init(|| std::env::var_os("LEONIDA_ACTIVATE_RAYS").is_some_and(|v| v == "0"));
        out && PipelineMaker::named(shaders, b"CS_ScreenProbes_ActivateRays")
    }

    fn without_structures(shaders: &[Arc<Shader>]) -> bool {
        static OUT: OnceLock<bool> = OnceLock::new();
        let out = *OUT.get_or_init(|| {
            let builder = std::env::var_os("LEONIDA_BVH_BUILDER").is_some_and(|v| v == "1");
            std::env::var_os("LEONIDA_RT_RESOLVE").is_some_and(|v| v == "0") && !builder
        });
        out && PipelineMaker::named(shaders, b"CS_RaytracingResolve")
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn missing(p: &Pending) -> Option<String> {
        match p.made.get() {
            Some(Some(_)) => None,
            None => Some(
                p.left_out
                    .map_or_else(|| format!("being made {:?}", p.shaders), str::to_owned),
            ),
            Some(None) => {
                let failed: Vec<String> = p
                    .shaders
                    .iter()
                    .filter(|s| matches!(s.translated.get(), Some(None)))
                    .map(|s| format!("{} {}", s.stage, s.hash))
                    .collect();
                Some(if failed.is_empty() {
                    format!("refused {:?}", p.shaders)
                } else {
                    failed.join(" + ")
                })
            }
        }
    }

    pub(crate) fn waiting() -> u64 {
        WAITING.load(Ordering::Relaxed)
    }

    pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
        if p.left_out.is_some() {
            return None;
        }
        if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
            super::library::PipelineLibrary::note_used(&p.key);
        }
        if p.made.get().is_none() {
            PipelineMaker::start(p, REQUESTED);
            PipelineMaker::wait(p, crate::frame::FramePacer::frame());
        }
        p.made.get()?.as_ref()
    }

    pub(crate) fn now(p: &Arc<Pending>) -> bool {
        if p.left_out.is_some() {
            return false;
        }
        p.make();
        p.made.get().is_some_and(|made| made.is_some())
    }

    pub(crate) fn warm(shaders: Vec<Arc<Shader>>) -> Option<bool> {
        let p = Pending::new(shaders, None);
        if p.left_out.is_some() {
            return None;
        }
        Some(HostPipeline::new(&p.shaders, None).is_some())
    }

    pub(crate) fn early(p: &Arc<Pending>) {
        if p.left_out.is_none()
            && !p.shaders.is_empty()
            && super::library::PipelineLibrary::wanted_early(&p.key)
        {
            super::library::EARLY.fetch_add(1, Ordering::Relaxed);
            PipelineMaker::start(p, EARLY);
        }
    }

    fn wait(p: &Pending, frame: u64) {
        let base = WaitBudget::base();
        if base.is_zero() || p.slow() {
            return;
        }
        static PAST: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
        let compute = p.computes();
        let past = &PAST[usize::from(compute)];
        let budget = WaitBudget::for_frame(
            base,
            compute,
            WaitBudget::loading(),
            Duration::from_millis(past.load(Ordering::Relaxed)),
        );
        static FRAMES: [Mutex<FrameWait>; 2] = [const { Mutex::new(FrameWait::new()) }; 2];
        let frame_wait = &FRAMES[usize::from(compute)];
        let sharing = WaitBudget::sharing();
        let started = Instant::now();
        let left = frame_wait
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .begin(sharing, frame, budget, started);
        if left.is_zero() {
            return;
        }
        let (lock, done) = &DONE;
        let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
        while p.made.get().is_none() && !p.slow() {
            let rest = left.saturating_sub(started.elapsed());
            if rest.is_zero() {
                break;
            }
            let rest = rest.min(Duration::from_millis(250));
            held = done
                .wait_timeout(held, rest)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        drop(held);
        let waited = started.elapsed().min(left);
        past.fetch_add(
            waited.saturating_sub(base).as_millis() as u64,
            Ordering::Relaxed,
        );
        frame_wait.lock().unwrap_or_else(|e| e.into_inner()).end(
            sharing,
            frame,
            left,
            waited,
            Instant::now(),
        );
        #[cfg(feature = "diagnostics")]
        crate::copystat::C::add(
            crate::copystat::C::PipelineWaitUs,
            started.elapsed().as_micros() as u64,
        );
    }

    fn start(p: &Arc<Pending>, priority: u8) {
        if p.queue_state.load(Ordering::Relaxed) >= priority {
            return;
        }
        if !PipelineMaker::workers() {
            if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
                p.make();
            }
            return;
        }
        let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(new) = queue.push(p, priority) {
            if new {
                PEAK.fetch_max(
                    WAITING.fetch_add(1, Ordering::Relaxed) + 1,
                    Ordering::Relaxed,
                );
            }
            AVAILABLE.notify_one();
        }
    }

    fn next() -> Arc<Pending> {
        let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(p) = queue.pop() {
                return p;
            }
            queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn workers() -> bool {
        static RUNNING: OnceLock<bool> = OnceLock::new();
        *RUNNING.get_or_init(|| {
            let mut running = false;
            let workers =
                std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8));
            for i in 0..workers {
                let spawned = std::thread::Builder::new()
                    .name(format!("leonida-pipelines-{i}"))
                    .spawn(|| {
                        unsafe {
                            use windows::Win32::System::Threading::*;
                            let _ =
                                SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                        }
                        loop {
                            PipelineMaker::next().make();
                            if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                                PipelineMaker::drained();
                            } else {
                                super::library::PipelineLibrary::save(false);
                            }
                        }
                    });
                if let Err(e) = spawned {
                    tracing::warn!(error = %e, "no pipeline worker");
                } else {
                    running = true;
                }
            }
            running
        })
    }

    fn drained() {
        use super::library::{EARLY, LOADED, MADE};
        if PEAK.swap(0, Ordering::Relaxed) >= 10 {
            tracing::info!(
                made = MADE.load(Ordering::Relaxed),
                loaded = LOADED.load(Ordering::Relaxed),
                started_early = EARLY.load(Ordering::Relaxed),
                "host pipelines all made"
            );
        }
        super::library::PipelineLibrary::save(false);
    }
}

impl WaitBudget {
    fn base() -> Duration {
        #[cfg(test)]
        if let Some(budget) = TEST_BUDGET.get() {
            return budget;
        }
        static BUDGET: OnceLock<Duration> = OnceLock::new();
        *BUDGET.get_or_init(|| {
            let millis = std::env::var("LEONIDA_PIPELINE_WAIT_MS")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or(DEFAULT_WAIT_MS);
            tracing::info!(millis, "host pipeline wait budget per frame, per kind");
            Duration::from_millis(millis)
        })
    }

    fn loading() -> Duration {
        static TOTAL: OnceLock<Duration> = OnceLock::new();
        *TOTAL.get_or_init(|| {
            let seconds = std::env::var("LEONIDA_PIPELINE_GATE_S")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or(120);
            tracing::info!(seconds, "host pipeline loading allowance per kind");
            Duration::from_secs(seconds)
        })
    }

    fn for_frame(base: Duration, compute: bool, allowance: Duration, spent: Duration) -> Duration {
        let loading = if compute {
            LOADING_FRAME_COMPUTE
        } else {
            LOADING_FRAME
        };
        base.max(loading.min(allowance.saturating_sub(spent)))
    }

    fn held_too_long(held: Duration, compute: bool, base: Duration) -> bool {
        let most = Duration::from_secs(if compute { 10 } else { 3 });
        base < most && held > most
    }

    fn sharing() -> Sharing {
        static ON: OnceLock<Sharing> = OnceLock::new();
        *ON.get_or_init(|| {
            WaitBudget::sharing_from(
                std::env::var("LEONIDA_PIPELINE_WAIT_SHARED")
                    .ok()
                    .as_deref(),
            )
        })
    }

    fn sharing_from(value: Option<&str>) -> Sharing {
        match value.map(str::trim) {
            Some("0") => Sharing::PerThread,
            Some("reserve") => Sharing::Reserve,
            _ => Sharing::Window,
        }
    }
}

unsafe impl Send for Pending {}
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(
            graphics
                .as_ref()
                .map(|g| g.describe())
                .unwrap_or_default()
                .as_bytes(),
        );
        let left_out = PipelineMaker::left_out(&shaders);
        Pending {
            shaders,
            graphics,
            left_out,
            begun: OnceLock::new(),
            queue_state: AtomicU8::new(NOT_QUEUED),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
        }
    }

    fn computes(&self) -> bool {
        matches!(self.shaders.as_slice(), [only] if only.stage == "compute")
    }

    fn slow(&self) -> bool {
        self.begun.get().is_some_and(|at| {
            WaitBudget::held_too_long(at.elapsed(), self.computes(), WaitBudget::base())
        })
    }

    fn make(&self) {
        let _ = self.begun.set(Instant::now());
        self.made
            .get_or_init(|| HostPipeline::new(&self.shaders, self.graphics.as_deref()));
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

const DEFAULT_WAIT_MS: u64 = 12_000;

const TITLE_WATCHDOG: Duration = Duration::from_secs(30);

const _: () = assert!(2 * DEFAULT_WAIT_MS < TITLE_WATCHDOG.as_millis() as u64);

#[cfg(test)]
thread_local! {
    static TEST_BUDGET: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

const LOADING_FRAME: Duration = Duration::from_secs(4);
const LOADING_FRAME_COMPUTE: Duration = Duration::from_secs(12);

static WAITING: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sharing {
    Window,
    Reserve,
    PerThread,
}

#[derive(Debug)]
struct FrameWait {
    frame: u64,
    spent: Duration,
    waiting: u32,
    since: Option<Instant>,
}

impl FrameWait {
    const fn new() -> FrameWait {
        FrameWait {
            frame: u64::MAX,
            spent: Duration::ZERO,
            waiting: 0,
            since: None,
        }
    }

    fn begin(&mut self, sharing: Sharing, frame: u64, budget: Duration, now: Instant) -> Duration {
        if self.frame != frame {
            *self = FrameWait {
                frame,
                ..FrameWait::new()
            };
        }
        if sharing == Sharing::Window {
            let open = self
                .since
                .map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
            let left = budget.saturating_sub(self.spent + open);
            if !left.is_zero() {
                if self.waiting == 0 {
                    self.since = Some(now);
                }
                self.waiting += 1;
            }
            return left;
        }
        let left = budget.saturating_sub(self.spent);
        if sharing == Sharing::Reserve {
            self.spent += left;
        }
        left
    }

    fn end(
        &mut self,
        sharing: Sharing,
        frame: u64,
        left: Duration,
        waited: Duration,
        now: Instant,
    ) {
        if self.frame != frame {
            return;
        }
        match sharing {
            Sharing::Window => {
                self.waiting = self.waiting.saturating_sub(1);
                if self.waiting == 0
                    && let Some(at) = self.since.take()
                {
                    self.spent += now.saturating_duration_since(at);
                }
            }
            Sharing::Reserve => self.spent = self.spent.saturating_sub(left - waited),
            Sharing::PerThread => self.spent += waited,
        }
    }
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

struct Queue {
    early: VecDeque<Arc<Pending>>,
    early_depth: VecDeque<Arc<Pending>>,
    requested: VecDeque<Arc<Pending>>,
}

impl Queue {
    const fn new() -> Self {
        Self {
            early: VecDeque::new(),
            early_depth: VecDeque::new(),
            requested: VecDeque::new(),
        }
    }

    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        if priority == REQUESTED {
            self.requested.push_back(p.clone());
        } else if p
            .graphics
            .as_ref()
            .is_some_and(|state| state.writes_depth_only())
        {
            self.early_depth.push_back(p.clone());
        } else {
            self.early.push_back(p.clone());
        }
        Some(prev == NOT_QUEUED)
    }

    fn pop(&mut self) -> Option<Arc<Pending>> {
        let p = self.requested.pop_front().or_else(|| {
            std::iter::from_fn(|| {
                self.early_depth
                    .pop_front()
                    .or_else(|| self.early.pop_front())
            })
            .find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
        })?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static AVAILABLE: Condvar = Condvar::new();

static PEAK: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod loading_tests {
    use super::{FrameWait, LOADING_FRAME, LOADING_FRAME_COMPUTE, Sharing, WaitBudget};
    use std::time::{Duration, Instant};

    #[test]
    fn two_threads_in_one_frame_both_wait_inside_the_window() {
        let sharing = WaitBudget::sharing_from(None);
        let (budget, t0, ms) = (
            Duration::from_secs(12),
            Instant::now(),
            Duration::from_millis(1),
        );
        let mut w = FrameWait::new();
        let a = w.begin(sharing, 7, budget, t0);
        let b = w.begin(sharing, 7, budget, t0 + ms);
        assert_eq!(a, budget);
        assert_eq!(
            b,
            budget - ms,
            "the second thread drops its pipeline at once: {sharing:?} gives it {b:?}"
        );
        w.end(
            sharing,
            7,
            a,
            Duration::from_secs(2),
            t0 + Duration::from_secs(2),
        );
        w.end(
            sharing,
            7,
            b,
            Duration::from_secs(2) - ms,
            t0 + Duration::from_secs(2),
        );
        assert_eq!(
            w.begin(sharing, 7, budget, t0 + Duration::from_secs(5)),
            Duration::from_secs(10)
        );
        w.end(
            sharing,
            7,
            Duration::from_secs(10),
            Duration::from_secs(10),
            t0 + Duration::from_secs(15),
        );
        assert_eq!(
            w.begin(sharing, 7, budget, t0 + Duration::from_secs(16)),
            Duration::ZERO
        );
        assert_eq!(
            w.begin(sharing, 8, budget, t0 + Duration::from_secs(17)),
            budget
        );
    }

    #[test]
    fn the_old_ways_of_counting_stay_behind_the_switch() {
        let (budget, t0) = (Duration::from_secs(12), Instant::now());
        let mut w = FrameWait::new();
        assert_eq!(w.begin(Sharing::Reserve, 1, budget, t0), budget);
        assert_eq!(w.begin(Sharing::Reserve, 1, budget, t0), Duration::ZERO);
        let mut w = FrameWait::new();
        assert_eq!(w.begin(Sharing::PerThread, 1, budget, t0), budget);
        assert_eq!(w.begin(Sharing::PerThread, 1, budget, t0), budget);
        w.end(Sharing::PerThread, 1, budget, Duration::from_secs(3), t0);
        assert_eq!(
            w.begin(Sharing::PerThread, 1, budget, t0),
            Duration::from_secs(9)
        );
        assert_eq!(WaitBudget::sharing_from(Some("0")), Sharing::PerThread);
        assert_eq!(WaitBudget::sharing_from(Some("reserve")), Sharing::Reserve);
    }

    #[test]
    fn a_frame_waits_longer_while_the_loading_allowance_lasts() {
        let (base, allowance) = (Duration::from_millis(500), Duration::from_secs(120));
        assert_eq!(
            WaitBudget::for_frame(base, false, allowance, Duration::ZERO),
            LOADING_FRAME
        );
        assert_eq!(
            WaitBudget::for_frame(base, true, allowance, Duration::ZERO),
            LOADING_FRAME_COMPUTE
        );
        assert_eq!(
            WaitBudget::for_frame(base, true, allowance, Duration::from_secs(119)),
            Duration::from_secs(1)
        );
        assert_eq!(
            WaitBudget::for_frame(base, true, allowance, Duration::from_secs(130)),
            base
        );
        assert_eq!(
            WaitBudget::for_frame(base, false, Duration::ZERO, Duration::ZERO),
            base,
            "LEONIDA_PIPELINE_GATE_S=0"
        );
        assert_eq!(
            WaitBudget::for_frame(Duration::from_secs(20), true, allowance, Duration::ZERO),
            Duration::from_secs(20)
        );
    }

    #[test]
    fn a_pipeline_held_too_long_is_not_waited_for() {
        let base = Duration::from_millis(500);
        assert!(!WaitBudget::held_too_long(
            Duration::from_secs(10),
            true,
            base
        ));
        assert!(WaitBudget::held_too_long(
            Duration::from_millis(10_001),
            true,
            base
        ));
        assert!(!WaitBudget::held_too_long(
            Duration::from_secs(3),
            false,
            base
        ));
        assert!(WaitBudget::held_too_long(
            Duration::from_millis(3_001),
            false,
            base
        ));
        let long = Duration::from_secs(12);
        assert!(!WaitBudget::held_too_long(
            Duration::from_secs(60),
            true,
            long
        ));
        assert!(!WaitBudget::held_too_long(
            Duration::from_secs(60),
            false,
            long
        ));
        let between = Duration::from_secs(5);
        assert!(WaitBudget::held_too_long(
            Duration::from_secs(60),
            true,
            between
        ));
        assert!(!WaitBudget::held_too_long(
            Duration::from_secs(60),
            false,
            between
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(PipelineMaker::ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn pending_pipeline_is_recorded_for_the_next_launch() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "pending-pipeline-test".to_owned();
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);
        super::TEST_BUDGET.set(Some(std::time::Duration::ZERO));

        assert!(!super::super::library::PipelineLibrary::wanted_early(
            &p.key
        ));
        assert!(PipelineMaker::ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(super::super::library::PipelineLibrary::wanted_early(&p.key));
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED), None);

        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &earlier));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn early_depth_precedes_other_prewarming_and_requests_precede_both() {
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_D32_FLOAT;
        let depth_pipeline = || {
            let mut depth = Box::new(GraphicsState::default());
            let format = [16u32, DXGI_FORMAT_D32_FLOAT.0 as u32];
            unsafe { depth.read(16, format.as_ptr().cast()) };
            Arc::new(Pending::new(Vec::new(), Some(depth)))
        };
        let normal = Arc::new(Pending::new(Vec::new(), None));
        let promoted = depth_pipeline();
        let shadow = depth_pipeline();
        let mut queue = Queue::new();
        for p in [&normal, &shadow, &promoted] {
            assert_eq!(queue.push(p, EARLY), Some(true));
        }
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        for expected in [&promoted, &shadow, &normal] {
            assert!(Arc::ptr_eq(&queue.pop().unwrap(), expected));
        }
        assert!(queue.pop().is_none());
    }
}
