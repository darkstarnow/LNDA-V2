#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::mem::offset_of;
use std::sync::{Arc, Mutex, OnceLock};

use crate::object::{
    self, ComObject, E_INVALIDARG, HRESULT, Iid, Object, PIPELINE, Table, This, guid,
};
use crate::resource::PAGEABLE_IIDS;

const PIPELINE_IIDS: [Iid; 4] = [
    guid(
        0x765a30f3,
        0xf624,
        0x4c6f,
        [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
pub(crate) struct Bytecode {
    code: *const u8,
    size: usize,
}

#[repr(C)]
pub(crate) struct GraphicsHead {
    root: This,
    stages: [Bytecode; 5],
}

#[repr(C)]
pub(crate) struct ComputeHead {
    root: This,
    stage: Bytecode,
}

pub(crate) struct Shader {
    pub(crate) stage: &'static str,
    pub(crate) hash: String,
    container: Vec<u8>,
    #[cfg(windows)]
    translated: OnceLock<Option<Translated>>,
}

impl std::fmt::Debug for Shader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.stage, self.hash)
    }
}

static SHADERS: Mutex<Option<HashMap<[u8; 32], Arc<Shader>>>> = Mutex::new(None);

static KEPT: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

#[cfg(windows)]
pub(crate) struct Translated {
    pub(crate) host: leonida_shader::emit::HostShader,
    pub(crate) compute_threads: Option<[u32; 3]>,
    pub(crate) used: Vec<leonida_shader::UsedResource>,
    pub(crate) stores: bool,
}

#[cfg(windows)]
impl Shader {
    pub(crate) fn translated(&self) -> Option<&Translated> {
        self.translated
            .get_or_init(|| {
                let done = ShaderStore::translate(&self.container);
                if let Some(dir) = std::env::var_os("LEONIDA_SHADER_SAVE") {
                    let dir = std::path::PathBuf::from(dir);
                    let _ = std::fs::create_dir_all(&dir);
                    let stem = format!("{}.{}", self.hash, self.stage);
                    let _ = std::fs::write(dir.join(format!("{stem}.cdxil")), &self.container);
                    if let Ok(t) = &done {
                        let _ = std::fs::write(dir.join(format!("{stem}.dxil")), &t.host.dxil);
                    }
                }
                if let Ok(t) = &done {
                    KEPT[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    KEPT[3].fetch_add(
                        t.host.dxil.len() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                match &done {
                    Ok(t) => tracing::info!(
                        stage = self.stage,
                        hash = %self.hash,
                        user_data = t.host.user_data.len(),
                        samplers = t.host.samplers.len(),
                        "shader translated"
                    ),
                    Err(e) => tracing::warn!(
                        stage = self.stage,
                        hash = %self.hash,
                        error = %e,
                        "shader does not translate; its pipelines' draws are dropped"
                    ),
                }
                done.ok()
            })
            .as_ref()
    }
}

#[cfg(windows)]
struct Builder {
    pipelines: Vec<Arc<make::Pending>>,
    whole: bool,
    off: bool,
}

#[cfg(windows)]
static BUILDER: Mutex<Builder> = Mutex::new(Builder {
    pipelines: Vec::new(),
    whole: false,
    off: false,
});

#[repr(C)]
pub(crate) struct PipelineState {
    object: Object,
    packet: [u32; 8],
    shaders: Vec<Arc<Shader>>,
    bound: std::sync::atomic::AtomicBool,
    #[cfg(windows)]
    host: Arc<make::Pending>,
    #[cfg(windows)]
    builder: bool,
    #[cfg(all(windows, feature = "diagnostics"))]
    builder_recorded: std::sync::atomic::AtomicBool,
}
const _: () = assert!(offset_of!(PipelineState, packet) == 0x10);

unsafe impl ComObject for PipelineState {
    const KIND: usize = PIPELINE;
    const INTERFACES: &'static [Iid] = &PIPELINE_IIDS;
}

pub struct ShaderStore;

pub(crate) struct BvhBuilder;

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) struct PipelineDrops;

pub struct PipelineCache;

impl ShaderStore {
    #[cfg(windows)]
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    pub(crate) fn kept() -> [u64; 7] {
        use std::sync::atomic::Ordering::Relaxed;
        [
            KEPT[0].load(Relaxed),
            KEPT[1].load(Relaxed),
            KEPT[2].load(Relaxed),
            KEPT[3].load(Relaxed),
            library::BLOB.load(Relaxed),
            library::SAVED.load(Relaxed),
            library::TRIMMED_BYTES.load(Relaxed),
        ]
    }

    fn shader(stage: &'static str, bytes: &[u8]) -> Arc<Shader> {
        let full = blake3::hash(bytes);
        let mut shaders = SHADERS.lock().unwrap_or_else(|e| e.into_inner());
        shaders
            .get_or_insert_with(HashMap::new)
            .entry(*full.as_bytes())
            .or_insert_with(|| {
                KEPT[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                KEPT[1].fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
                Arc::new(Shader {
                    stage,
                    hash: full.to_hex()[..16].to_owned(),
                    container: bytes.to_vec(),
                    #[cfg(windows)]
                    translated: OnceLock::new(),
                })
            })
            .clone()
    }

    #[cfg(windows)]
    pub fn warm(stage: &'static str, container: &[u8], compute: bool) -> (bool, Option<bool>) {
        let full = blake3::hash(container);
        let shader = Arc::new(Shader {
            stage,
            hash: full.to_hex()[..16].to_owned(),
            container: container.to_vec(),
            translated: OnceLock::new(),
        });
        let translated = shader.translated().is_some();
        let made = (translated && compute)
            .then(|| make::PipelineMaker::warm(vec![shader]))
            .flatten();
        (translated, made)
    }

    #[cfg(windows)]
    fn translate(container: &[u8]) -> Result<Translated, String> {
        use leonida_shader::{Container, FourCC, HwStage};
        crate::descriptor::host::HeapDiagnostics::probe_view_limit();
        let c = Container::parse(container).map_err(|e| e.to_string())?;
        let programs = match c.part(FourCC::DXIL) {
            Some(part) => leonida_shader::native_programs(part.data).map_err(|e| e.to_string())?,
            None => Vec::new(),
        };
        let host = crate::shader_cache::ShaderCache::host_shader(container)?;
        let stores = programs.is_empty() || programs.iter().any(ShaderStore::stores);
        let compute_threads = programs.iter().find_map(|p| match &p.io {
            leonida_shader::native::StageIo::Cs(cs) if cs.threads.iter().all(|&n| n > 0) => {
                Some(cs.threads)
            }
            _ => None,
        });
        let used = programs
            .into_iter()
            .find(|p| {
                matches!(
                    p.stage,
                    HwStage::Vs | HwStage::Ps | HwStage::Ms | HwStage::Cs
                )
            })
            .map(|p| p.used)
            .unwrap_or_default();
        Ok(Translated {
            host,
            compute_threads,
            used,
            stores,
        })
    }

    #[cfg(windows)]
    fn stores(program: &leonida_shader::NativeProgram) -> bool {
        use leonida_shader::gfx10::{Encoding, decode};
        let Ok(code) = decode(&program.code) else {
            return true;
        };
        code.iter().any(|i| {
            let name = i.name();
            match i.encoding {
                Encoding::Mubuf
                | Encoding::Mtbuf
                | Encoding::Smem
                | Encoding::Flat
                | Encoding::Global => name.is_none_or(|n| {
                    (n.contains("store") || n.contains("atomic")) && !n.contains("scratch")
                }),
                Encoding::Sop1 => name.is_none_or(|n| matches!(n, "s_setpc_b64" | "s_swappc_b64")),
                _ => false,
            }
        })
    }

    fn copy(stage: &'static str, code: &Bytecode) -> Option<Arc<Shader>> {
        if code.code.is_null() || code.size == 0 {
            return None;
        }
        Some(ShaderStore::shader(stage, unsafe {
            std::slice::from_raw_parts(code.code, code.size)
        }))
    }
}

impl BvhBuilder {
    #[cfg(windows)]
    fn built_from(shaders: &[Arc<Shader>]) -> bool {
        use leonida_shader::{Container, FourCC};
        let [shader] = shaders else { return false };
        if shader.stage != "compute" {
            return false;
        }
        let Ok(c) = Container::parse(&shader.container) else {
            return false;
        };
        c.part(FourCC::DXIL)
            .and_then(|part| leonida_shader::native_programs(part.data).ok())
            .is_some_and(|programs| programs.iter().any(leonida_shader::spill::is_spilled))
    }

    #[cfg(windows)]
    pub(crate) fn quarantined(this: This) -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        BvhBuilder::is(this)
            && *ON.get_or_init(|| {
                std::env::var_os("LEONIDA_BVH_QUARANTINE").is_some_and(|v| v == "1")
            })
    }

    #[cfg(windows)]
    pub(crate) fn is(this: This) -> bool {
        !this.is_null() && PipelineState::from_this(this).builder
    }

    #[cfg(windows)]
    pub(crate) fn turn_off(why: &'static str) {
        if !BvhBuilder::allowed() {
            return;
        }
        let mut b = BUILDER.lock().unwrap_or_else(|e| e.into_inner());
        if !b.off {
            b.off = true;
            tracing::warn!(
                why,
                "the ray-tracing structure builder is turned off for the rest of the run: its stages run all or none"
            );
        }
    }

    #[cfg(windows)]
    pub(crate) fn runs() -> bool {
        if !BvhBuilder::allowed() {
            return false;
        }
        let mut b = BUILDER.lock().unwrap_or_else(|e| e.into_inner());
        if b.off {
            return false;
        }
        if b.whole {
            return true;
        }
        let missing = b
            .pipelines
            .iter()
            .filter(|p| !make::PipelineMaker::now(p))
            .count();
        if missing > 0 {
            b.off = true;
            tracing::warn!(
                missing,
                of = b.pipelines.len(),
                "the ray-tracing structure builder stays off: not all of its programs have a host pipeline, and its stages run all or none"
            );
            return false;
        }
        tracing::info!(
            pipelines = b.pipelines.len(),
            "the ray-tracing structure builder runs: all of its programs have host pipelines"
        );
        b.whole = true;
        true
    }

    #[cfg(windows)]
    fn allowed() -> bool {
        static ALLOWED: OnceLock<bool> = OnceLock::new();
        *ALLOWED.get_or_init(|| std::env::var_os("LEONIDA_BVH_BUILDER").is_some_and(|v| v == "1"))
    }

    #[cfg(all(windows, feature = "diagnostics"))]
    pub(crate) fn recorded(this: This, indirect: bool) {
        let p = PipelineState::from_this(this);
        if !p
            .builder_recorded
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::info!(hash = %p.shaders[0].hash, indirect,
                "recorded first ray-tracing structure builder dispatch for this pipeline");
        }
    }

    #[cfg(all(windows, feature = "diagnostics"))]
    pub(crate) fn trace_args(this: This, log: impl FnOnce(&str, usize)) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SELECTED: OnceLock<HashMap<String, AtomicUsize>> = OnceLock::new();
        let selected = SELECTED.get_or_init(|| {
            let value = std::env::var("LEONIDA_BVH_TRACE").unwrap_or_default();
            PipelineDrops::parse_hashes(&value, "LEONIDA_BVH_TRACE")
                .into_iter()
                .map(|hash| (hash, AtomicUsize::new(0)))
                .collect()
        });
        if selected.is_empty() {
            return;
        }
        let hash = &PipelineState::from_this(this).shaders[0].hash;
        if let Some(n) = selected.get(hash)
            && let Ok(previous) = n.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < 256).then_some(n + 1)
            })
        {
            log(hash, previous + 1);
        }
    }
}

impl PipelineState {
    #[cfg(all(windows, feature = "diagnostics"))]
    pub(crate) fn shaders_of(this: This) -> String {
        if this.is_null() {
            return "no pipeline".to_owned();
        }
        let names: Vec<String> = PipelineState::from_this(this)
            .shaders
            .iter()
            .map(|s| format!("{} {}", s.stage, s.hash))
            .collect();
        format!("[{}]", names.join(", "))
    }

    pub(crate) fn bound(this: This) {
        if this.is_null() {
            return;
        }
        let p = PipelineState::from_this(this);
        if p.bound.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let frame = crate::frame::FramePacer::frame();
        for shader in &p.shaders {
            tracing::info!(pipeline = ?this, frame, stage = shader.stage, size = shader.container.len(), hash = %shader.hash, "first bind");
        }
    }

    #[cfg(windows)]
    pub(crate) fn host_pipeline<'a>(this: This) -> Option<&'a host::HostPipeline> {
        if this.is_null() {
            return None;
        }
        make::PipelineMaker::ready(&PipelineState::from_this(this).host)
    }

    fn hand_out_new(
        shaders: Vec<Arc<Shader>>,
        #[cfg(windows)] graphics: Option<Box<host::GraphicsState>>,
        iid: *const Iid,
        out: *mut This,
    ) -> HRESULT {
        tracing::trace!(?shaders, "pipeline state");
        #[cfg(windows)]
        let host = Arc::new(make::Pending::new(shaders.clone(), graphics));
        #[cfg(windows)]
        make::PipelineMaker::early(&host);
        #[cfg(windows)]
        let builder = BvhBuilder::built_from(&shaders);
        #[cfg(windows)]
        if builder {
            let mut b = BUILDER.lock().unwrap_or_else(|e| e.into_inner());
            b.pipelines.push(host.clone());
            b.whole = false;
        }
        let pipeline = Box::new(PipelineState {
            object: Object::new(&FUNCTIONS),
            packet: [0; 8],
            #[cfg(windows)]
            host,
            #[cfg(windows)]
            builder,
            #[cfg(all(windows, feature = "diagnostics"))]
            builder_recorded: std::sync::atomic::AtomicBool::new(false),
            shaders,
            bound: std::sync::atomic::AtomicBool::new(false),
        });
        pipeline.hand_out(iid, out)
    }

    pub(crate) extern "system" fn create_graphics(
        _: This,
        desc: *const GraphicsHead,
        iid: *const Iid,
        out: *mut This,
    ) -> HRESULT {
        let Some(desc) = (unsafe { desc.as_ref() }) else {
            return E_INVALIDARG;
        };
        let stages = GRAPHICS_STAGES.iter().zip(&desc.stages);
        let shaders = stages
            .filter_map(|(s, c)| ShaderStore::copy(s, c))
            .collect();
        PipelineState::hand_out_new(
            shaders,
            #[cfg(windows)]
            None,
            iid,
            out,
        )
    }

    pub(crate) extern "system" fn create_compute(
        _: This,
        desc: *const ComputeHead,
        iid: *const Iid,
        out: *mut This,
    ) -> HRESULT {
        let Some(desc) = (unsafe { desc.as_ref() }) else {
            return E_INVALIDARG;
        };
        PipelineState::hand_out_new(
            ShaderStore::copy("compute", &desc.stage)
                .into_iter()
                .collect(),
            #[cfg(windows)]
            None,
            iid,
            out,
        )
    }

    pub(crate) extern "system" fn create_compute_x(
        this: This,
        desc: *const ComputeHead,
        _extensions: u32,
        _extended: *const u8,
        iid: *const Iid,
        out: *mut This,
    ) -> HRESULT {
        PipelineState::create_compute(this, desc, iid, out)
    }

    fn subobject(kind: u32) -> Option<(usize, Option<&'static str>)> {
        let stage = |name| Some((24, Some(name)));
        match kind {
            1 => stage("vertex"),
            2 => stage("pixel"),
            3 => stage("domain"),
            4 => stage("hull"),
            5 => stage("geometry"),
            6 => stage("compute"),
            23 => stage("amplification"),
            24 => stage("mesh"),
            0 => Some((16, None)),
            12 | 19 => Some((24, None)),
            7 | 15 => Some((40, None)),
            8 => Some((336, None)),
            10 => Some((48, None)),
            11 => Some((56, None)),
            17 => Some((16, None)),
            21 => Some((64, None)),
            22 => Some((32, None)),
            9 | 13 | 14 | 16 | 18 | 20 => Some((8, None)),
            _ => None,
        }
    }

    pub(crate) extern "system" fn create_stream(
        _: This,
        desc: *const StreamDesc,
        iid: *const Iid,
        out: *mut This,
    ) -> HRESULT {
        let Some(desc) = (unsafe { desc.as_ref() }) else {
            return E_INVALIDARG;
        };
        if desc.stream.is_null() {
            return E_INVALIDARG;
        }
        let mut shaders = Vec::new();
        #[cfg(windows)]
        let mut graphics = host::GraphicsState::default();
        let mut at = 0;
        while at + 8 <= desc.size {
            let kind = unsafe { desc.stream.add(at).cast::<u32>().read_unaligned() };
            let Some((size, stage)) = PipelineState::subobject(kind) else {
                tracing::warn!(
                    kind,
                    at,
                    "pipeline subobject of unknown size; the rest is ignored"
                );
                break;
            };
            if let Some(stage) = stage {
                let code = unsafe { &*desc.stream.add(at + 8).cast::<Bytecode>() };
                shaders.extend(ShaderStore::copy(stage, code));
            }
            #[cfg(windows)]
            unsafe {
                graphics.read(kind, desc.stream.add(at))
            };
            at += size;
        }
        #[cfg(windows)]
        graphics.apply_integer_blend_bypass();
        PipelineState::hand_out_new(
            shaders,
            #[cfg(windows)]
            Some(Box::new(graphics)),
            iid,
            out,
        )
    }
}

impl PipelineCache {
    #[cfg(windows)]
    pub(crate) fn making() -> u64 {
        make::PipelineMaker::waiting()
    }

    #[cfg(windows)]
    pub(crate) fn trim() {
        library::PipelineLibrary::trim();
    }

    #[cfg(windows)]
    pub(crate) fn removal_note() -> String {
        library::PipelineLibrary::removal_note()
    }

    #[cfg(windows)]
    pub fn use_dir(dir: std::path::PathBuf) {
        library::PipelineLibrary::use_pipeline_cache(dir);
    }

    #[cfg(windows)]
    pub fn save() {
        library::PipelineLibrary::save(true);
    }
}

#[cfg(all(windows, feature = "diagnostics"))]
impl PipelineDrops {
    fn parse_hashes(value: &str, setting: &str) -> Vec<String> {
        value
            .split(',')
            .map(str::trim)
            .filter(|hash| !hash.is_empty())
            .filter_map(|hash| {
                if hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    Some(hash.to_ascii_lowercase())
                } else {
                    tracing::warn!(hash, setting, "expected a 16-digit shader hash");
                    None
                }
            })
            .collect()
    }

    fn skipped_by(p: &PipelineState) -> Option<&str> {
        static HASHES: OnceLock<Vec<String>> = OnceLock::new();
        let hashes = HASHES.get_or_init(|| {
            let value = std::env::var("LEONIDA_SKIP_PIPELINES").unwrap_or_default();
            PipelineDrops::parse_hashes(&value, "LEONIDA_SKIP_PIPELINES")
        });
        if hashes.is_empty() {
            return None;
        }
        p.shaders
            .iter()
            .find(|shader| hashes.contains(&shader.hash))
            .map(|shader| shader.hash.as_str())
    }

    pub(crate) fn skips(this: This) -> bool {
        !this.is_null() && PipelineDrops::skipped_by(PipelineState::from_this(this)).is_some()
    }

    pub(crate) fn note(this: This) {
        let why = if this.is_null() {
            Some("no pipeline set".to_owned())
        } else {
            let p = PipelineState::from_this(this);
            PipelineDrops::skipped_by(p)
                .map(|hash| format!("skipped shader {hash}"))
                .or_else(|| make::PipelineMaker::missing(&p.host))
        };
        if let Some(why) = why {
            PipelineDrops::note_why(why);
        }
    }

    pub(crate) fn note_why(why: String) {
        crate::capture::CaptureFrames::op(0, || format!("DROPPED {why}"));
        let mut drops = DROPS.lock().unwrap_or_else(|e| e.into_inner());
        *drops
            .get_or_insert_with(HashMap::new)
            .entry(why)
            .or_default() += 1;
    }

    pub(crate) fn take() -> Vec<(String, u64)> {
        let drops = DROPS.lock().unwrap_or_else(|e| e.into_inner()).take();
        let mut drops: Vec<(String, u64)> = drops.into_iter().flatten().collect();
        drops.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        drops
    }
}

const GRAPHICS_STAGES: [&str; 5] = ["vertex", "pixel", "domain", "hull", "geometry"];

#[cfg(all(windows, feature = "diagnostics"))]
static DROPS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

#[repr(C)]
pub(crate) struct StreamDesc {
    size: usize,
    stream: *const u8,
}

static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<PIPELINE, 24>());
    f[0] = PipelineState::query_interface as *const ();
    f[2] = PipelineState::release as *const ();
    f[7] = object::parent_device as *const ();
    Table(f)
};

#[cfg(windows)]
#[path = "Pipeline/Host.rs"]
pub(crate) mod host;
#[cfg(windows)]
#[path = "Pipeline/Library.rs"]
mod library;
#[cfg(windows)]
#[path = "Pipeline/Make.rs"]
mod make;

#[cfg(test)]
mod tests {
    #[cfg(all(windows, feature = "diagnostics"))]
    #[test]
    fn skip_hashes_accepts_full_hashes_only() {
        assert!(super::PipelineDrops::parse_hashes(" , ", "test").is_empty());
        assert_eq!(
            super::PipelineDrops::parse_hashes(
                " 0123456789ABCDEF, 0123456789abcde, xyz, 0011223344556677,",
                "test"
            ),
            ["0123456789abcdef", "0011223344556677"]
        );
    }

    use super::*;
    use crate::object::{S_OK, slot};

    fn shaders(p: This) -> Vec<(&'static str, Vec<u8>)> {
        PipelineState::from_this(p)
            .shaders
            .iter()
            .map(|s| (s.stage, s.container.clone()))
            .collect()
    }

    #[test]
    fn pipelines_keep_their_shaders() {
        let (vs, ps) = ([1u8, 2, 3], [4u8; 5]);
        let none = || Bytecode {
            code: std::ptr::null(),
            size: 0,
        };
        let desc = GraphicsHead {
            root: std::ptr::null(),
            stages: [
                Bytecode {
                    code: vs.as_ptr(),
                    size: vs.len(),
                },
                Bytecode {
                    code: ps.as_ptr(),
                    size: ps.len(),
                },
                none(),
                none(),
                none(),
            ],
        };
        let mut p = std::ptr::null();
        assert_eq!(
            PipelineState::create_graphics(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut p),
            S_OK
        );
        assert_eq!(
            shaders(p),
            [("vertex", vs.to_vec()), ("pixel", ps.to_vec())]
        );
        let mut q = std::ptr::null();
        PipelineState::create_graphics(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut q);
        assert!(Arc::ptr_eq(
            &PipelineState::from_this(p).shaders[1],
            &PipelineState::from_this(q).shaders[1]
        ));
        assert_eq!(slot::<extern "system" fn(This) -> u32>(p, 0x10)(p), 0);
    }

    #[test]
    fn streams_yield_their_shaders() {
        let (ms, ps) = ([9u8; 7], [4u8; 5]);
        let mut stream = vec![0u8; 0x2b0];
        let mut put = |at: usize, v: u64| stream[at..at + 8].copy_from_slice(&v.to_le_bytes());
        for (at, kind) in [
            (0x10, 24),
            (0x28, 2),
            (0x40, 3),
            (0x58, 4),
            (0x70, 5),
            (0x88, 8),
            (0x1d8, 9),
            (0x1e0, 10),
            (0x210, 21),
            (0x250, 17),
            (0x260, 12),
            (0x278, 14),
            (0x280, 15),
            (0x2a8, 16),
        ] {
            put(at, kind);
        }
        put(0x18, ms.as_ptr() as u64);
        put(0x20, ms.len() as u64);
        put(0x30, ps.as_ptr() as u64);
        put(0x38, ps.len() as u64);
        put(0x278, 14 | 3 << 32);
        put(0x280, 15 | 28 << 32);
        put(0x2a0, 1 << 32);
        let desc = StreamDesc {
            size: stream.len(),
            stream: stream.as_ptr(),
        };
        let mut p = std::ptr::null();
        let hr = PipelineState::create_stream(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut p);
        assert_eq!(hr, S_OK);
        assert_eq!(shaders(p), [("mesh", ms.to_vec()), ("pixel", ps.to_vec())]);
        #[cfg(windows)]
        {
            let g = PipelineState::from_this(p)
                .host
                .graphics
                .as_deref()
                .unwrap();
            assert_eq!(g.topology.0, 3);
            assert_eq!(
                (g.targets.NumRenderTargets, g.targets.RTFormats[0].0),
                (1, 28)
            );
        }
    }
}
