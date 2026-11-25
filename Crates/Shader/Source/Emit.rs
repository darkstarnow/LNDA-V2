#[path = "Emit/Bitcode.rs"]
mod bitcode;
#[path = "Emit/Container.rs"]
mod container;
#[cfg(all(test, windows))]
#[path = "Emit/CubeLodTests.rs"]
mod cube_lod_tests;
#[cfg(all(test, windows))]
#[path = "Emit/DiffTests.rs"]
mod diff_tests;
#[cfg(all(test, windows))]
#[path = "Emit/HelperLaneTests.rs"]
mod helper_lane_tests;
#[cfg(all(windows, any(test, feature = "host")))]
#[path = "Emit/Host.rs"]
pub mod host;
#[cfg(all(test, windows))]
#[path = "Emit/IntersectTests.rs"]
mod intersect_tests;
#[path = "Emit/Lower.rs"]
mod lower;
#[path = "Emit/OwnBits.rs"]
mod own_bits;
#[path = "Emit/Rect.rs"]
mod rect;
#[path = "Emit/Sampler.rs"]
mod sampler;
#[path = "Emit/Shader.rs"]
mod shader;
#[path = "Emit/Threading.rs"]
mod threading;
#[cfg(windows)]
#[path = "Emit/Validator.rs"]
pub mod validator;
#[cfg(all(test, windows))]
#[path = "Emit/ValuTests.rs"]
mod valu_tests;
#[cfg(all(test, windows))]
#[path = "Emit/WaterfallTests.rs"]
mod waterfall_tests;
#[path = "Emit/Wave.rs"]
mod wave;
#[cfg(all(test, windows))]
#[path = "Emit/Wave64Tests.rs"]
mod wave64_tests;

use crate::dxbc::{Container, FourCC};
use crate::native::{HwStage, NativeProgram, RootType};
use crate::{ShaderError, io, ir, resource, structure};

pub use bitcode::census;
pub use lower::{host_max_wave, set_host_max_wave};
pub use sampler::StaticSampler;

pub use rect::rect_list_gs;
pub use shader::{
    MEMORY_FAULTS, MEMORY_PAGES, ROOT_CBV_SPACE, SAMPLER_SPACE, SLOT_FAULTS, SLOT_MEMORY,
    SLOT_STRIDES, SLOT_STRIDES_COUNT, SLOT_VIEW_ADDRESSES, USER_DATA_SPACE,
    USER_DISPATCH_THREADS_X, USER_GROUP_BASE, USER_MEMORY_BASE, USER_VIEW_ADDRESS, USER_VIEW_BIAS,
    USER_VIEW_RECORDS, set_view_heap_limit, view_heap_limit,
};

pub const DEFAULT_LOOP_LIMIT: u32 = lower::LOOP_LIMIT;

pub const RAY_PROBE_HASH: &str = "f22a76fcb967ee59";
pub const RAY_PROBE_DWORDS: u32 = 8;

pub fn ray_probe_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    cfg!(feature = "diagnostics")
        && *ENABLED.get_or_init(|| {
            std::env::var("LEONIDA_CAPTURE_RAY_INTERSECTIONS").as_deref() == Ok(RAY_PROBE_HASH)
        })
}

thread_local! {
    static RAY_PROBE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn with_ray_probe<T>(work: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            RAY_PROBE.set(self.0);
        }
    }
    let _restore = Reset(RAY_PROBE.replace(true));
    work()
}

pub fn translate_ray_probe(container: &[u8]) -> Result<HostShader, ShaderError> {
    if ray_probe_enabled() {
        with_ray_probe(|| translate(container))
    } else {
        translate(container)
    }
}

pub fn loop_limit() -> u32 {
    static LIMIT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        if cfg!(feature = "diagnostics") {
            parse_loop_limit(std::env::var("LEONIDA_SHADER_LOOP_LIMIT").ok().as_deref())
        } else {
            lower::LOOP_LIMIT
        }
    })
}

pub fn group_sync() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        match std::env::var("LEONIDA_GROUP_SYNC")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("0") => 0,
            Some("1") => 1,
            _ => 2,
        }
    })
}

pub fn shader_opt() -> u32 {
    static PASSES: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *PASSES.get_or_init(|| parse_shader_opt(std::env::var("LEONIDA_SHADER_OPT").ok().as_deref()))
}

fn parse_shader_opt(value: Option<&str>) -> u32 {
    let parsed = value
        .map(str::trim)
        .and_then(|v| match v.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => v.parse().ok(),
        });
    parsed.map_or(bitcode::opt::ALL, |n| n & bitcode::opt::ALL)
}

fn parse_loop_limit(value: Option<&str>) -> u32 {
    let parsed = value
        .map(str::trim)
        .and_then(|v| match v.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => v.parse().ok(),
        });
    match parsed {
        Some(n) if n > 0 => n,
        _ => lower::LOOP_LIMIT,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostShader {
    pub stage: HwStage,
    pub dxil: Vec<u8>,
    pub user_data: Vec<UserData>,
    pub samplers: Vec<[u32; 4]>,
    pub static_samplers: Vec<StaticSampler>,
    pub root_cbvs: Vec<u8>,
    pub root_srvs: Vec<u8>,
    pub root_uavs: Vec<u8>,
    pub written_root_uavs: Vec<u8>,
    pub writes_guest_memory: bool,
    pub root_indexed_views: Vec<crate::resource::RootIndexedView>,
}

impl HostShader {
    pub fn user_data_dwords(&self) -> u32 {
        let n = self
            .user_data
            .iter()
            .map(|u| u.index + 1)
            .max()
            .unwrap_or(0);
        n.next_multiple_of(4)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserData {
    pub index: u32,
    pub slot: u8,
    pub root: RootType,
    pub dword: u32,
}

pub fn translate(container: &[u8]) -> Result<HostShader, ShaderError> {
    let c = Container::parse(container)?;
    let dxil = c
        .part(FourCC::DXIL)
        .ok_or(ShaderError::Malformed("no DXIL part"))?;
    let native = crate::native_programs(dxil.data)?
        .into_iter()
        .find(|p| {
            matches!(
                p.stage,
                HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms
            )
        })
        .ok_or(ShaderError::Unsupported {
            offset: 0,
            what: "a container without a vertex, pixel, compute or mesh program",
        })?;
    let p = crate::spill::build(&native)?;
    let st = structure::structure(&p);
    let res = resource::track(&p, &native.layout, native.stage, st.as_ref().ok())?;
    let io = io::raise(&p, &native)?;
    let st = st?;
    emit(&c, &native, &p, &st, &io, &res)
}

pub fn emit(
    c: &Container,
    native: &NativeProgram,
    p: &ir::Program,
    st: &structure::Structured,
    io: &io::Io,
    res: &resource::Resources,
) -> Result<HostShader, ShaderError> {
    lower::take_needs();
    let original = emit_program(c, native, p, st, io, res);
    let stuck = match &original {
        Ok((_, wave)) => *wave == Some(64),
        Err(ShaderError::Unsupported { what, .. }) => lower::needs_every_lane(what),
        Err(_) => false,
    };
    let exact_only = matches!(&original, Err(ShaderError::Unsupported { what, .. }) if *what == lower::MASK_ELSEWHERE);
    if stuck && native.stage == HwStage::Cs && native.wave64 && lower::host_max_wave() == 32 {
        let why = std::env::var_os("LEONIDA_WAVE_WHY").is_some();
        let report = |what: &str, r: &Result<(HostShader, Option<u32>), ShaderError>| {
            if why {
                match r {
                    Ok((_, wave)) => eprintln!("WAVE64-RETRY {what}: wave {wave:?}"),
                    Err(e) => eprintln!("WAVE64-RETRY {what}: {e}"),
                }
            }
        };
        let done = |r: &Result<(HostShader, Option<u32>), ShaderError>| matches!(r, Ok((_, wave)) if *wave != Some(64));
        let local_on = std::env::var_os("LEONIDA_WAVE_LOCAL").is_none_or(|v| v != "0");
        let strict = std::env::var_os("LEONIDA_WAVE_LOCAL").is_some_and(|v| v == "idiom");
        let idiom = |p: &ir::Program, st: &structure::Structured| match lower::wave_local(p, st) {
            Ok(()) => true,
            Err((offset, what)) => {
                if why {
                    eprintln!(
                        "WAVE64-RETRY local waves: not the allocation idiom at {offset:#x}: {what}"
                    );
                }
                false
            }
        };
        let form = |p: Option<ir::Program>| {
            p.and_then(|p| {
                let st = structure::structure(&p).ok()?;
                let io = io::raise(&p, native).ok()?;
                let res = resource::track(&p, &native.layout, native.stage, Some(&st)).ok()?;
                Some((p, st, io, res))
            })
        };
        let forms = [
            ("without early-outs", form(ir::without_early_outs(p))),
            ("without skips", form(ir::without_skips(p, false))),
        ];
        for (name, f) in &forms {
            if let Some((p, st, io, res)) = f {
                let again = emit_program(c, native, p, st, io, res);
                report(name, &again);
                if done(&again) {
                    return again.map(|(shader, _)| shader);
                }
            }
        }
        let wide_on = std::env::var_os("LEONIDA_WAVE_WIDE").is_some_and(|v| v == "1");
        for exact in [true, false] {
            if !exact {
                let deep_on = std::env::var_os("LEONIDA_WAVE_DEEP_SKIPS").is_none_or(|v| v != "0");
                let deep = if deep_on {
                    form(ir::without_skips(p, true))
                } else {
                    None
                };
                if let Some((p, st, io, res)) = &deep {
                    let again = emit_program(c, native, p, st, io, res);
                    report("without skips, scalars followed", &again);
                    if done(&again) {
                        return again.map(|(shader, _)| shader);
                    }
                }
                if wide_on {
                    let bases = [
                        deep.as_ref().map(|f| &f.0),
                        forms[1].1.as_ref().map(|f| &f.0),
                        forms[0].1.as_ref().map(|f| &f.0),
                        Some(p),
                    ];
                    if let Some(base) = bases.into_iter().flatten().next() {
                        let again = widen(c, native, base, why);
                        report("wave-wide branches", &again);
                        if done(&again) {
                            return again.map(|(shader, _)| shader);
                        }
                    }
                }
                if strict {
                    break;
                }
            }
            if !local_on || exact_only {
                continue;
            }
            for (name, f) in &forms {
                if let Some((p, st, io, res)) = f
                    && idiom(p, st) == exact
                {
                    let local = lower::with_local_waves(|| emit_program(c, native, p, st, io, res));
                    report(
                        &format!(
                            "local waves ({}), {name}",
                            if exact { "idiom" } else { "APPROXIMATE" }
                        ),
                        &local,
                    );
                    if done(&local) {
                        return local.map(|(shader, _)| shader);
                    }
                }
            }
            if idiom(p, st) == exact {
                let local = lower::with_local_waves(|| emit_program(c, native, p, st, io, res));
                report(
                    if exact {
                        "local waves (idiom)"
                    } else {
                        "local waves (APPROXIMATE)"
                    },
                    &local,
                );
                if done(&local) {
                    return local.map(|(shader, _)| shader);
                }
            }
        }
        if local_on && !exact_only && !strict {
            for (name, f) in &forms {
                if let Some((p, st, io, res)) = f {
                    let local =
                        lower::with_local_waves_any(|| emit_program(c, native, p, st, io, res));
                    report(
                        &format!("local waves (APPROXIMATE, any mask), {name}"),
                        &local,
                    );
                    if done(&local) {
                        return local.map(|(shader, _)| shader);
                    }
                }
            }
            let local = lower::with_local_waves_any(|| emit_program(c, native, p, st, io, res));
            report("local waves (APPROXIMATE, any mask)", &local);
            if done(&local) {
                return local.map(|(shader, _)| shader);
            }
        }
    }
    let original = match original {
        Err(ShaderError::Unsupported { what, .. })
            if native.stage == HwStage::Cs
                && what == lower::LANE_MASK
                && std::env::var_os("LEONIDA_WAVE_WIDE").is_none_or(|v| v != "0") =>
        {
            let again = widen(c, native, p, std::env::var_os("LEONIDA_WAVE_WHY").is_some());
            match again {
                Ok(_) => again,
                Err(_) => emit_program(c, native, p, st, io, res),
            }
        }
        other => other,
    };
    let original = original.map(|(shader, _)| shader);
    if !matches!(original, Err(ShaderError::Unsupported { .. })) {
        return original;
    }
    let edges = threading::constant_edges(p, st);
    if edges.is_empty() {
        return original;
    }
    let p = ir::thread_edges(p, &edges)?;
    let st = structure::structure(&p)?;
    let io = io::raise(&p, native)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    emit_program(c, native, &p, &st, &io, &res).map(|(shader, _)| shader)
}

fn widen(
    c: &Container,
    native: &NativeProgram,
    p: &ir::Program,
    why: bool,
) -> Result<(HostShader, Option<u32>), ShaderError> {
    const NOT: ShaderError = ShaderError::Unsupported {
        offset: 0,
        what: "wave-wide branches in a thread group of several console waves",
    };
    match native.io {
        crate::native::StageIo::Cs(_) if !(native.wave64 && lower::host_max_wave() == 32) => {}
        crate::native::StageIo::Cs(cs) if cs.threads.iter().product::<u32>() == 64 => {}
        crate::native::StageIo::Cs(cs) => {
            if why {
                eprintln!("WAVE64-WIDE not tried: thread groups of {:?}", cs.threads);
            }
            return Err(NOT);
        }
        _ => return Err(NOT),
    }
    let mut wide = std::collections::HashSet::new();
    loop {
        let st = structure::structure_wide(p, &wide)?;
        let io = io::raise(p, native)?;
        let res = resource::track(p, &native.layout, native.stage, Some(&st))?;
        lower::take_needs();
        let r = emit_program(c, native, p, &st, &io, &res);
        let needs = lower::take_needs();
        let stuck = matches!(&r, Err(ShaderError::Unsupported { what, .. })
                if lower::needs_every_lane(what) || *what == lower::LANE_MASK && !needs.is_empty())
            || matches!(&r, Ok((_, Some(64)))) && !needs.is_empty();
        if !stuck {
            if why {
                eprintln!("WAVE64-WIDE {} branches taken wave-wide", wide.len());
            }
            return r;
        }
        let more: Vec<usize> = lower::branches_around(&st, &needs)
            .unwrap_or_default()
            .into_iter()
            .filter(|b| !wide.contains(b))
            .collect();
        if more.is_empty() {
            if why {
                eprintln!(
                    "WAVE64-WIDE stopped with {} branches wave-wide: a branch around it cannot be",
                    wide.len()
                );
            }
            return r;
        }
        wide.extend(more);
    }
}

fn tiled_lighting_uint_srvs(digest: [u8; 16]) -> &'static [(u8, u32, u32)] {
    const DIRECTIONAL_EYE: &[(u8, u32, u32)] = &[
        (0, 32, 10),
        (0, 968, 2),
        (0, 1160, 2),
        (1, 72, 2),
        (1, 80, 4),
    ];
    match u128::from_be_bytes(digest) {
        0x8386ec5b_47a3b2de_49309239_7987b35d => &[(1, 48, 2), (1, 56, 2)],
        0x46545633_f601b707_19765403_267442c5
        | 0xa3c6e3b5_a1226f1b_3b08afe2_ac7b6c3d
        | 0x58795ccc_1d3237cf_683a8a7a_a88391fd => DIRECTIONAL_EYE,
        0x56d5f614_619f530c_f1cf2b90_43b580e8
        | 0xbd4ac3d8_98dc88e0_c0d0f252_2fbe6341
        | 0x50d20881_47c267aa_c6ec8d21_9970b21b => &DIRECTIONAL_EYE[1..],
        _ => &[],
    }
}

fn emit_program(
    c: &Container,
    native: &NativeProgram,
    p: &ir::Program,
    st: &structure::Structured,
    io: &io::Io,
    res: &resource::Resources,
) -> Result<(HostShader, Option<u32>), ShaderError> {
    let sig = |fourcc| -> Result<Vec<container::SigElement>, ShaderError> {
        match c.part(fourcc) {
            Some(part) => signature(part.data),
            None => Ok(Vec::new()),
        }
    };
    let pixel = native.stage == HwStage::Ps;
    let mut inputs = sig(FourCC::ISG1)?;
    let mut outputs = sig(FourCC::OSG1)?;
    let interpolated = if pixel { &mut inputs } else { &mut outputs };
    for e in interpolated {
        e.interp = match (e.kind, e.comp) {
            (3, _) => 4,
            (13, _) => 1,
            (28, _) => 2,
            (0 | 6 | 7, 9) => 2,
            (0, _) => 1,
            _ => 0,
        };
    }
    if native.stage == HwStage::Vs
        && let Some(psv) = c.part(FourCC::PSV0)
    {
        vertex_output_interpolation(psv.data, &mut outputs)?;
    }
    if pixel && outputs.iter().any(|e| matches!(e.kind, 18 | 19)) {
        for e in inputs.iter_mut().filter(|e| e.kind == 3) {
            e.interp = 5;
        }
    }
    let mut prims = sig(FourCC::PSG1)?;
    prims.retain(|e| !matches!(e.kind, 10 | 30));
    for e in &mut prims {
        e.interp = 1;
    }
    let mesh_stride = match &native.io {
        crate::native::StageIo::Ms(ms) => {
            let psv = c.part(FourCC::PSV0).map(|p| p.data);
            let threads = psv
                .filter(|d| crate::dxbc::u32_at(d, 0).is_ok_and(|n| n >= 48))
                .and_then(|d| crate::dxbc::u32_at(d, 4 + 36).ok());
            threads.map_or(ms.threads, |t| {
                t.max(ms.max_vertices).max(ms.max_primitives)
            })
        }
        _ => 0,
    };
    let parts = lower::Parts {
        native,
        p,
        st,
        io,
        res,
        inputs,
        outputs,
        prims,
        mesh_stride,
    };
    let box_rehydration = matches!(
        u128::from_be_bytes(c.digest),
        0xba30_5538_e28d_414c_af39_a51a_4bc2_f308 | 0xd635_3bf6_df15_ff8e_c4aa_aec1_0dbf_3974
    ) && matches!(native.io, crate::native::StageIo::Cs(cs) if cs.threads == [64, 1, 1]);
    let l = lower::lower_program(&parts, box_rehydration, tiled_lighting_uint_srvs(c.digest))?;
    let wave_size = l.wave_size;
    let shader = HostShader {
        stage: native.stage,
        dxil: l.dxil,
        user_data: l.user_data,
        static_samplers: l
            .samplers
            .iter()
            .enumerate()
            .map(|(n, &w)| {
                let mut s = StaticSampler::from_words(w);
                if l.cmp_samplers >> n & 1 != 0 {
                    s.filter |= 0x80;
                }
                s
            })
            .collect(),
        samplers: l.samplers,
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
        root_uavs: l.root_uavs,
        written_root_uavs: l.written_root_uavs,
        writes_guest_memory: l.writes_guest_memory,
        root_indexed_views: res.root_indexed_views.clone(),
    };
    Ok((shader, wave_size))
}

fn vertex_output_interpolation(
    mut data: &[u8],
    outputs: &mut [container::SigElement],
) -> Result<(), ShaderError> {
    const TRUNCATED: ShaderError =
        ShaderError::Malformed("truncated vertex pipeline validation data");
    fn take<'a>(data: &mut &'a [u8], len: usize) -> Result<&'a [u8], ShaderError> {
        let (head, tail) = data.split_at_checked(len).ok_or(TRUNCATED)?;
        *data = tail;
        Ok(head)
    }
    fn size(data: &mut &[u8]) -> Result<usize, ShaderError> {
        Ok(u32::from_le_bytes(take(data, 4)?.try_into().unwrap()) as usize)
    }
    let info_size = size(&mut data)?;
    let info = take(&mut data, info_size)?;
    if info.len() < 36 || info[24] != 1 || info[29] == 0 {
        return Ok(());
    }
    let resources = size(&mut data)?;
    if resources != 0 {
        let stride = size(&mut data)?;
        take(&mut data, resources.checked_mul(stride).ok_or(TRUNCATED)?)?;
    }
    let strings = size(&mut data)?;
    take(&mut data, strings)?;
    let indices = size(&mut data)?;
    take(&mut data, indices.checked_mul(4).ok_or(TRUNCATED)?)?;
    let stride = size(&mut data)?;
    if stride < 16 {
        return Err(TRUNCATED);
    }
    let count = usize::from(info[28]) + usize::from(info[29]);
    let elements = take(&mut data, count.checked_mul(stride).ok_or(TRUNCATED)?)?;
    for record in elements.chunks_exact(stride).skip(usize::from(info[28])) {
        let packed = record[10];
        if packed & 0x40 == 0 {
            continue;
        }
        if record[13] > 7 {
            return Err(ShaderError::Malformed(
                "invalid vertex output interpolation",
            ));
        }
        let first = u32::from(record[9]);
        let rows = first..first + u32::from(record[8]);
        for output in outputs.iter_mut().filter(|e| {
            e.row.is_some_and(|r| rows.contains(&r))
                && e.col == ((packed >> 4) & 3)
                && e.cols == (packed & 15)
                && e.kind == record[11]
        }) {
            output.interp = record[13];
        }
    }
    Ok(())
}

fn signature(part: &[u8]) -> Result<Vec<container::SigElement>, ShaderError> {
    use crate::dxbc::u32_at;
    let count = u32_at(part, 0)? as usize;
    let base = u32_at(part, 4)? as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = base + 32 * i;
        let name_at = u32_at(part, at + 4)? as usize;
        let name = part
            .get(name_at..)
            .and_then(|s| s.split(|&b| b == 0).next())
            .ok_or(ShaderError::Malformed("signature name out of bounds"))?;
        let mask = *part
            .get(at + 24)
            .ok_or(ShaderError::Malformed("truncated"))?;
        if u32_at(part, at + 28)? != 0 {
            return Err(ShaderError::Unsupported {
                offset: 0,
                what: "a minimum-precision signature element",
            });
        }
        if u32_at(part, at)? != 0 {
            return Err(ShaderError::Unsupported {
                offset: 0,
                what: "a signature element of another stream",
            });
        }
        let kind = match u32_at(part, at + 12)? {
            0 => 0,
            1 => 3,
            2 => 6,
            3 => 7,
            4 => 4,
            5 => 5,
            6 => 1,
            7 => 10,
            8 => 2,
            9 => 13,
            10 => 12,
            64 => 16,
            65 => 17,
            66 => 14,
            67 => 19,
            68 => 18,
            69 => 20,
            70 => 15,
            23 => 28,
            25 => 30,
            _ => {
                return Err(ShaderError::Unsupported {
                    offset: 0,
                    what: "a system value in the signature",
                });
            }
        };
        let raw = u32_at(part, at + 16)?;
        let comp = match (kind, raw) {
            (13, _) => 5,
            (_, 1 | 4) => 5,
            (_, 2 | 5) => 4,
            (_, 3 | 6) => 9,
            (_, c) => {
                return Err(ShaderError::Unsupported {
                    offset: 0,
                    what: match c {
                        0 => "a signature element of unknown type",
                        _ => "a 64-bit signature element",
                    },
                });
            }
        };
        let register = u32_at(part, at + 20)?;
        out.push(container::SigElement {
            name: String::from_utf8_lossy(name).into_owned(),
            index: u32_at(part, at + 8)?,
            kind,
            comp,
            interp: 0,
            row: (register != u32::MAX).then_some(register),
            col: mask.trailing_zeros().min(3) as u8,
            cols: mask.count_ones().max(1) as u8,
            usage: 0,
            half: (4..=6).contains(&raw),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::bitcode::Attr;
    use super::lower::LOOP_LIMIT;
    use super::parse_loop_limit;

    #[test]
    fn the_clean_up_switch_parses_or_runs_every_pass() {
        let all = super::bitcode::opt::ALL;
        assert_eq!(parse_shader_opt(None), all);
        assert_eq!(parse_shader_opt(Some("0")), 0);
        assert_eq!(parse_shader_opt(Some(" 1 ")), 1);
        assert_eq!(parse_shader_opt(Some("0x1")), 1);
        assert_eq!(parse_shader_opt(Some("0xffffffff")), all);
        assert_eq!(parse_shader_opt(Some("on")), all);
    }

    #[test]
    fn the_loop_limit_switch_parses_or_falls_back() {
        assert_eq!(parse_loop_limit(None), LOOP_LIMIT);
        assert_eq!(parse_loop_limit(Some("4096")), 4096);
        assert_eq!(parse_loop_limit(Some(" 0x1000 ")), 4096);
        assert_eq!(parse_loop_limit(Some("1")), 1);
        for bad in ["", "0", "-5", "lots", "0x", "4294967296"] {
            assert_eq!(parse_loop_limit(Some(bad)), LOOP_LIMIT, "{bad:?}");
        }
    }
    use super::container::SigElement;
    use super::shader::Shader;
    use super::*;
    use crate::native::{ComputeIo, StageIo};

    #[test]
    fn a_stencil_export_declares_the_stencil_ref_feature() {
        let words = [
            0x7E00_02F2u32,
            0x7E02_0285,
            0xF800_1883,
            0x0000_0100,
            0xBF81_0000,
        ];
        let native = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 2,
            layout: vec![],
            used: vec![],
            io: StageIo::Ps {
                inputs: vec![],
                input_addr: 0,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let el = |name: &str, kind, comp| SigElement {
            name: name.into(),
            index: 0,
            kind,
            comp,
            interp: 0,
            row: None,
            col: 0,
            cols: 1,
            usage: 1,
            half: false,
        };
        let osg1 =
            container::signature(&[el("SV_Depth", 17, 9), el("SV_StencilRef", 20, 5)], false);
        let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
        let c = Container::parse(&bytes).unwrap();
        let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let out = emit(&c, &native, &p, &st, &io, &res).unwrap();
        #[cfg(windows)]
        if let Ok(mut v) = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut out.dxil.clone()).unwrap();
        }
        let host = Container::parse(&out.dxil).unwrap();
        let sfi0 = host.part(FourCC::SFI0).unwrap().data;
        let features = u64::from_le_bytes(sfi0.try_into().unwrap());
        assert_ne!(features & 0x200, 0, "SFI0 {features:#x}: no StencilRef");
    }

    #[test]
    fn an_exec_high_write_does_not_overflow_the_stack() {
        let words = [0xBEFF_0380u32, 0x7E02_0300, 0xBF81_0000];
        let native = NativeProgram {
            stage: HwStage::Cs,
            wave64: true,
            vgprs: 2,
            layout: vec![],
            used: vec![],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 0,
                threads: [64, 1, 1],
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let container = crate::dxbc::build(&[]);
        let c = Container::parse(&container).unwrap();
        if let Err(e) = emit(&c, &native, &p, &st, &io, &res) {
            assert!(e.to_string().contains("its own value"), "{e}");
        }
    }

    fn stores_v1(body: &[u32]) -> Result<(), ShaderError> {
        use crate::native::RootParam;
        use crate::{ResourceType, RootType, UsedResource};
        let mut words = vec![0xBE81_0380, 0xF408_0200, 0xFA00_0000];
        words.extend_from_slice(body);
        words.extend_from_slice(&[0x3406_0082, 0xE070_1000, 0x8002_0103, 0xBF81_0000]);
        let native = NativeProgram {
            stage: HwStage::Cs,
            wave64: true,
            vgprs: 4,
            layout: vec![RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 0,
                slot: 0,
            }],
            used: vec![UsedResource {
                kind: ResourceType::Uav,
                root: RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 0,
                slot: 0,
            }],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 2,
                threads: [64, 1, 1],
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, None).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let st = structure::structure(&p).unwrap();
        let container = crate::dxbc::build(&[]);
        let c = Container::parse(&container).unwrap();
        emit(&c, &native, &p, &st, &io, &res).map(|_| ())
    }

    #[test]
    fn a_mask_whose_high_dword_is_rewritten_is_refused() {
        let select = [0x7D84_0080, 0x7E04_028B, 0x0202_0480];
        let carry = [0x7D84_0080, 0x7E04_028B, 0x5002_0480];
        let mov = [0x7E02_0287];
        for body in [&select[..], &carry, &mov] {
            stores_v1(body).unwrap();
        }
        let select_hi = [0x7D84_0080, 0xBEEB_0380, 0x7E04_028B, 0x0202_0480];
        let carry_hi = [0x7D84_0080, 0xBEEB_0380, 0x7E04_028B, 0x5002_0480];
        let exec_hi = [0xBEFF_0380, 0x7E02_0287];
        let exec_hi_saved = [0xBE82_047E, 0xBEFF_0380, 0x7E02_0287];
        for body in [&select_hi[..], &carry_hi, &exec_hi, &exec_hi_saved] {
            assert!(stores_v1(body).is_err(), "{body:x?}");
        }
    }

    #[test]
    fn a_backward_permute_reads_the_lane_its_address_names() {
        stores_v1(&[0x7E04_028B, 0xDACC_0000, 0x0100_0200]).unwrap();
    }

    #[test]
    fn a_carry_in_from_a_divergent_join_is_refused() {
        let body = [
            0x7E04_028B,
            0x7D84_0080,
            0xBE82_376A,
            0xBF88_0001,
            0x7D84_0081,
            0x88FE_027E,
            0x5002_0480,
        ];
        let e = stores_v1(&body).unwrap_err().to_string();
        assert!(e.contains("a carry-in from an untracked mask"), "{e}");
    }

    #[cfg(windows)]
    fn constant_color() -> Vec<u8> {
        let mut s = Shader::new(true);
        s.outputs.push(SigElement {
            name: "SV_Target".into(),
            index: 0,
            kind: 16,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 15,
            half: false,
        });
        let b = s.entry;
        for (c, v) in [1.0f32, 0.5, 0.25, 1.0].into_iter().enumerate() {
            let args = [
                s.m.i32(0),
                s.m.i32(0),
                s.m.i8(c as u8),
                s.m.f32(v.to_bits()),
            ];
            s.op(b, "storeOutput.f32", Attr::NoUnwind, s.t.void, 5, &args);
        }
        s.m.terminate(b, super::bitcode::Term::Ret);
        s.finish()
    }

    #[test]
    #[cfg(windows)]
    fn stub_pipeline_is_created() {
        let Ok(host) = super::host::Host::new(false) else {
            eprintln!("no SM 6.6 device; skipped");
            return;
        };
        let ps = super::HostShader {
            stage: crate::HwStage::Ps,
            dxil: constant_color(),
            user_data: Vec::new(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
            written_root_uavs: Vec::new(),
            writes_guest_memory: false,
            root_indexed_views: Vec::new(),
        };
        let file = |var: &str, stage| {
            std::env::var(var).ok().map(|p| super::HostShader {
                stage,
                dxil: std::fs::read(p).unwrap(),
                user_data: Vec::new(),
                samplers: Vec::new(),
                static_samplers: Vec::new(),
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
                root_uavs: Vec::new(),
                written_root_uavs: Vec::new(),
                writes_guest_memory: false,
                root_indexed_views: Vec::new(),
            })
        };
        let ps = file("LEONIDA_REF_PS", crate::HwStage::Ps).unwrap_or(ps);
        let vs = file("LEONIDA_REF_VS", crate::HwStage::Vs)
            .unwrap_or_else(|| super::host::stub_vs(&ps).unwrap());
        if let Ok(mut v) = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut vs.dxil.clone()).unwrap();
        }
        host.pipeline(&vs, &ps).unwrap();
        let mut wrong = ps.dxil[4..20].to_vec();
        wrong[0] ^= 1;
        for digest in [&[0u8; 16][..], &wrong] {
            let mut other = ps.clone();
            other.dxil[4..20].copy_from_slice(digest);
            assert!(
                host.pipeline(&vs, &other).is_err(),
                "digest {digest:02x?} accepted"
            );
        }
    }

    #[test]
    #[cfg(windows)]
    fn constant_color_validates() {
        let mut c = constant_color();
        let path = super::validator::Validator::default_path();
        let Ok(mut v) = super::validator::Validator::load(path.as_deref()) else {
            eprintln!("no dxil.dll; skipped");
            return;
        };
        let ours = c[4..20].to_vec();
        v.validate(&mut c).unwrap();
        assert_eq!(c[4..20], ours[..], "our hash differs from the validator's");
    }

    const TRIANGLE: u32 = 1 << 10 | 2 << 20;
    const EDGE_FLAGS: u32 = 1 << 9 | 1 << 19 | 1 << 29;
    const CULL: u32 = 1 << 31;

    fn triangle_mesh(prim: u32) -> Result<HostShader, ShaderError> {
        let words = [
            0xD765_0001u32,
            0x0001_00C1,
            0x3604_0281,
            0x3606_0282,
            0x7E04_0D02,
            0x7E06_0D03,
            0x1008_04F6,
            0x0608_08F3,
            0x100A_06F4,
            0x060A_0AF3,
            0x7E0C_02F0,
            0x7E0E_02F2,
            0x7E12_02FF,
            prim,
            0xF800_0941,
            0x0000_0009,
            0xF800_08CF,
            0x0706_0504,
            0xBF81_0000,
        ];
        let native = NativeProgram {
            stage: HwStage::Ms,
            wave64: false,
            vgprs: 10,
            layout: vec![],
            used: vec![],
            io: StageIo::Ms(crate::native::MeshIo {
                outputs: vec![],
                clip_cntl: 0,
                threads: 32,
                max_vertices: 32,
                max_primitives: 32,
                topology: 2,
                lds_bytes: 0,
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let position = SigElement {
            name: "SV_Position".into(),
            index: 0,
            kind: 3,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 0xF,
            half: false,
        };
        let osg1 = container::signature(&[position], false);
        let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
        let c = Container::parse(&bytes)?;
        let p = ir::build(&native.patched_code(), native.wave64, native.vgprs)?;
        let st = structure::structure(&p)?;
        let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
        let io = io::raise(&p, &native)?;
        emit(&c, &native, &p, &st, &io, &res)
    }

    #[test]
    fn primitive_export_indices_drop_the_edge_flags() {
        #[cfg(windows)]
        let mut validator = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        )
        .ok();
        for prim in [
            TRIANGLE,
            TRIANGLE | EDGE_FLAGS,
            TRIANGLE | EDGE_FLAGS | CULL,
        ] {
            let ms = triangle_mesh(prim).unwrap_or_else(|e| panic!("{prim:#x}: {e}"));
            #[cfg(windows)]
            if let Some(v) = validator.as_mut() {
                v.validate(&mut ms.dxil.clone())
                    .unwrap_or_else(|e| panic!("{prim:#x}: {e}"));
            }
        }
    }

    #[test]
    #[cfg(windows)]
    fn edge_flagged_triangles_draw_as_unflagged_ones() {
        use super::host::{Draw, Host, Root};
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_FEATURE_D3D12_OPTIONS7, D3D12_FEATURE_DATA_D3D12_OPTIONS7,
            D3D12_MESH_SHADER_TIER_NOT_SUPPORTED, D3D12_SHADER_VISIBILITY_MESH,
        };
        let _one = super::valu_tests::ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let ps = constant_color();
        let mut ran = 0;
        for (label, warp) in [("hardware", false), ("warp", true)] {
            let Ok(host) = Host::new(warp) else {
                eprintln!("no SM 6.6 {label} device; skipped");
                continue;
            };
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
            let queried = unsafe {
                host.device.CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS7,
                    (&raw mut options).cast(),
                    size_of_val(&options) as u32,
                )
            };
            if queried.is_err() || options.MeshShaderTier == D3D12_MESH_SHADER_TIER_NOT_SUPPORTED {
                eprintln!("no mesh shaders on the {label} device; skipped");
                continue;
            }
            for (prim, culled) in [
                (TRIANGLE, false),
                (TRIANGLE | EDGE_FLAGS, false),
                (TRIANGLE | EDGE_FLAGS | CULL, true),
            ] {
                let ms = triangle_mesh(prim).unwrap();
                let root = host
                    .root_signature_for(&[(&ms, D3D12_SHADER_VISIBILITY_MESH)])
                    .unwrap();
                let pso = host
                    .mesh_pipeline_with(&root, &ms.dxil, &ps)
                    .unwrap_or_else(|e| panic!("{label}: mesh pipeline: {e}"));
                let texels = host
                    .draw(
                        &root,
                        &pso,
                        &Draw {
                            roots: vec![(0, Root::Constants(vec![0]))],
                            textures: &[],
                            samplers: &[],
                            vertices: &[],
                            stride: 0,
                            size: (16, 16),
                            mesh_groups: 1,
                        },
                    )
                    .unwrap_or_else(|e| panic!("{label}: draw: {e}"));
                let lit = texels.iter().filter(|t| t[3] != 0.0).count();
                if culled {
                    assert_eq!(lit, 0, "{label}, {prim:#x}: a culled triangle lit texels");
                    continue;
                }
                assert!(
                    texels.iter().all(|t| *t == [1.0, 0.5, 0.25, 1.0]),
                    "{label}, {prim:#x}: {lit} of 256 texels lit"
                );
            }
            eprintln!("{label}: edge-flagged triangles drew");
            ran += 1;
        }
        if ran == 0 {
            eprintln!("no device with mesh shaders; skipped");
        }
    }

    #[test]
    fn sample_interpolated_inputs_set_psv_sample_frequency() {
        let words = [
            0xC808_0000u32,
            0xC809_0001,
            0xF800_180F,
            0x0202_0202,
            0xBF81_0000,
        ];
        let psv_of = |input_addr: u32| {
            let native = super::NativeProgram {
                stage: super::HwStage::Ps,
                wave64: false,
                vgprs: 4,
                layout: vec![],
                used: vec![],
                io: crate::native::StageIo::Ps {
                    inputs: vec![crate::native::PsInput {
                        usage: crate::native::Usage::Generic,
                        row: 0,
                        mask: 0b1111,
                        attr: 0,
                        flat: false,
                        fp16: false,
                        pack2: false,
                        row2: 0,
                    }],
                    input_addr,
                    db_shader_control: 0,
                    patches: vec![],
                },
                code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            };
            let el = |name: &str, kind, usage| SigElement {
                name: name.into(),
                index: 0,
                kind,
                comp: 9,
                interp: 0,
                row: Some(0),
                col: 0,
                cols: 4,
                usage,
                half: false,
            };
            let isg1 = super::container::signature(&[el("TEXCOORD", 0, 0)], true);
            let osg1 = super::container::signature(&[el("SV_Target", 16, 15)], false);
            let bytes =
                crate::dxbc::build(&[(super::FourCC::ISG1, &isg1), (super::FourCC::OSG1, &osg1)]);
            let c = super::Container::parse(&bytes).unwrap();
            let p = crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
            let st = crate::structure::structure(&p).unwrap();
            let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
            let io = crate::io::raise(&p, &native).unwrap();
            let out = super::emit(&c, &native, &p, &st, &io, &res).unwrap();
            #[cfg(windows)]
            if let Ok(mut v) = super::validator::Validator::load(
                super::validator::Validator::default_path().as_deref(),
            ) {
                v.validate(&mut out.dxil.clone()).unwrap();
            }
            let host = super::Container::parse(&out.dxil).unwrap();
            host.part(super::FourCC::PSV0).unwrap().data.to_vec()
        };
        let center = psv_of(1 << 1);
        assert_eq!(center[4 + 1], 0, "center interpolation is per pixel");
        let sample = psv_of(1 << 0);
        assert_eq!(sample[4 + 1], 1, "PSInfo.SampleFrequency");
    }

    #[test]
    fn reading_a_channel_below_an_input_element_does_not_underflow() {
        let words = [0xC80E_0202u32, 0xBF81_0000];
        let native = super::NativeProgram {
            stage: super::HwStage::Ps,
            wave64: false,
            vgprs: 4,
            layout: vec![],
            used: vec![],
            io: crate::native::StageIo::Ps {
                inputs: vec![crate::native::PsInput {
                    usage: crate::native::Usage::Generic,
                    row: 0,
                    mask: 0b1000,
                    attr: 0,
                    flat: false,
                    fp16: false,
                    pack2: false,
                    row2: 0,
                }],
                input_addr: 0,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let w_only = SigElement {
            name: "TEXCOORD".into(),
            index: 0,
            kind: 0,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 3,
            cols: 1,
            usage: 0,
            half: false,
        };
        let isg1 = super::container::signature(&[w_only], true);
        let bytes = crate::dxbc::build(&[(super::FourCC::ISG1, &isg1)]);
        let c = super::Container::parse(&bytes).unwrap();
        let p = crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = crate::structure::structure(&p).unwrap();
        let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = crate::io::raise(&p, &native).unwrap();
        let _ = super::emit(&c, &native, &p, &st, &io, &res);
    }

    #[test]
    fn valu_div_f32_sequence_executes_on_gpu() {
        let words: &[u32] = &[
            0xBE85_0380,
            0xF408_0202,
            0xFA00_0000,
            0xF408_0302,
            0xFA00_0020,
            0x340E_0083,
            0xE034_1000,
            0x8002_0107,
            0xD56D_6A03,
            0x0406_0502,
            0xD56D_1008,
            0x0406_0501,
            0x7E08_5503,
            0xD54B_0005,
            0x23CA_0903,
            0xD54B_0004,
            0x0412_0905,
            0x100A_0908,
            0xD54B_0006,
            0x2422_0B03,
            0xD56F_0004,
            0x0416_0906,
            0xD55F_0001,
            0x0406_0504,
            0x340E_0082,
            0xE070_1000,
            0x8003_0107,
            0xBF81_0000,
        ];
        let native = super::NativeProgram {
            stage: super::HwStage::Cs,
            wave64: false,
            vgprs: 9,
            layout: vec![
                crate::RootParam {
                    root: crate::RootType::Constants,
                    table_offset_dwords: 0,
                    sgprs: 4,
                    base_register: 0,
                    user_data: 0,
                    slot: 0,
                },
                crate::RootParam {
                    root: crate::RootType::Table,
                    table_offset_dwords: 0,
                    sgprs: 1,
                    base_register: 0,
                    user_data: 4,
                    slot: 1,
                },
            ],
            used: vec![
                crate::UsedResource {
                    kind: crate::ResourceType::Srv,
                    root: crate::RootType::Table,
                    table_offset_dwords: 0,
                    lower_bound: 0,
                    stride: 4,
                    dwords: 4,
                    user_data: 4,
                    slot: 1,
                },
                crate::UsedResource {
                    kind: crate::ResourceType::Uav,
                    root: crate::RootType::Table,
                    table_offset_dwords: 8,
                    lower_bound: 0,
                    stride: 4,
                    dwords: 4,
                    user_data: 4,
                    slot: 1,
                },
            ],
            io: crate::native::StageIo::Cs(crate::native::ComputeIo {
                rsrc2: 5 << 1,
                threads: [7, 1, 1],
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let p = crate::ir::build(&native.code, native.wave64, native.vgprs).unwrap();
        let res = crate::resource::track(&p, &native.layout, native.stage, None).unwrap();
        let io = crate::io::raise(&p, &native).unwrap();
        let st = crate::structure::structure(&p).unwrap();
        let l = super::lower::lower(&super::lower::Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
        })
        .unwrap();
        let mut dxil = l.dxil;
        #[cfg(windows)]
        if let Ok(mut v) = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut dxil).unwrap();
        }
        #[cfg(windows)]
        if let Ok(host) = super::host::Host::new(false) {
            let cs = super::HostShader {
                stage: super::HwStage::Cs,
                dxil,
                user_data: l.user_data,
                samplers: Vec::new(),
                static_samplers: Vec::new(),
                root_cbvs: l.root_cbvs,
                root_srvs: l.root_srvs,
                root_uavs: l.root_uavs,
                written_root_uavs: l.written_root_uavs,
                writes_guest_memory: l.writes_guest_memory,
                root_indexed_views: Vec::new(),
            };
            let (root, pso) = host.compute_pipeline(&cs).unwrap();
            let inputs: Vec<u32> = [
                7.0f32, 3.0, -1.0, 1e-40, 1.0, 1e35, 1e-35, 1e-36, 0.0, 0.0, -1.0, 0.0, -1e35, 1e35,
            ]
            .into_iter()
            .map(f32::to_bits)
            .collect();
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &super::host::Dispatch {
                        roots: vec![(0, super::host::Root::Constants(vec![0, 0, 0, 0, 0]))],
                        views: &[
                            super::host::View::Buffer {
                                data: &inputs,
                                stride: super::host::Stride::Raw,
                            },
                            super::host::View::RawUav(64),
                        ],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap();
            let got: Vec<f32> = out[0][..7].iter().copied().map(f32::from_bits).collect();
            assert!((got[0] - (7.0f32 / 3.0)).abs() < 1e-6, "got {:?}", got);
            assert_eq!(out[0][1], (-1.0f32 / 1e-40).to_bits());
            assert!(
                (got[2] - 1e-35).abs() <= 1e-40,
                "lane 2 got {:?} ({:#x})",
                got[2],
                out[0][2]
            );
            assert!(
                (got[3] - 10.0).abs() < 1e-5,
                "lane 3 got {:?} ({:#x})",
                got[3],
                out[0][3]
            );
            assert_eq!(out[0][4], 0xFFC0_0000);
            assert_eq!(out[0][5], (-1.0f32 / 0.0).to_bits());
            assert!(
                (got[6] - (-1.0)).abs() < 1e-6,
                "lane 6 got {:?} ({:#x})",
                got[6],
                out[0][6]
            );
        }
    }
}

#[cfg(test)]
mod atomic_tests {
    use super::*;
    use crate::native::{ComputeIo, RootParam, StageIo};

    #[test]
    fn heap_texture_atomic_widths() {
        for (opcode, mask, wide) in [
            (16u32, 3, false),
            (16, 15, true),
            (17, 1, false),
            (17, 3, true),
        ] {
            let words = [
                0xBE81_0380,
                0xF40C_0200,
                0xFA00_0000,
                0x7E00_0280,
                0x7E02_0280,
                0x7E04_0281,
                0x7E06_0282,
                0x7E08_0283,
                0x7E0A_0284,
                0xF000_2008 | opcode << 18 | mask << 8,
                0x0002_0200,
                0xF044_0008 | (if wide { 3 } else { 1 }) << 8,
                0x0002_0200,
                0xBF81_0000,
            ];
            let native = NativeProgram {
                stage: HwStage::Cs,
                wave64: false,
                vgprs: 6,
                layout: vec![RootParam {
                    root: RootType::Table,
                    table_offset_dwords: 0,
                    sgprs: 1,
                    base_register: 0,
                    user_data: 0,
                    slot: 0,
                }],
                used: vec![],
                io: StageIo::Cs(ComputeIo {
                    rsrc2: 2,
                    threads: [32, 1, 1],
                }),
                code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            };
            let p = ir::build(&native.code, false, native.vgprs).unwrap();
            let bytes = crate::dxbc::build(&[]);
            let shader = emit(
                &Container::parse(&bytes).unwrap(),
                &native,
                &p,
                &structure::structure(&p).unwrap(),
                &io::raise(&p, &native).unwrap(),
                &resource::track(&p, &native.layout, native.stage, None).unwrap(),
            )
            .unwrap();
            let container = Container::parse(&shader.dxil).unwrap();
            let features = u64::from_le_bytes(
                container
                    .part(FourCC::SFI0)
                    .unwrap()
                    .data
                    .try_into()
                    .unwrap(),
            );
            let required = 0x1040_8000;
            assert_eq!(features & required, if wide { required } else { 0 });
            #[cfg(windows)]
            if let Ok(mut v) =
                validator::Validator::load(validator::Validator::default_path().as_deref())
            {
                v.validate(&mut shader.dxil.clone()).unwrap();
            }
        }
    }
}

#[cfg(test)]
mod barycentric_tests {
    use super::container::SigElement;
    use super::*;
    use crate::native::{PsInput, StageIo, Usage};

    const PERSP_CENTER: u32 = 1 << 1;
    const LINEAR_CENTER: u32 = 1 << 5;

    fn exp_mrt0(r: u32) -> [u32; 2] {
        [0xF800_180F, r * 0x0101_0101]
    }

    fn v_add_f32(dst: u32, a: u32, b: u32) -> u32 {
        3 << 25 | dst << 17 | b << 9 | (256 + a)
    }

    pub(super) fn element(name: &str, kind: u8, row: u32) -> SigElement {
        SigElement {
            name: name.into(),
            index: 0,
            kind,
            comp: 9,
            interp: 0,
            row: Some(row),
            col: 0,
            cols: 4,
            usage: 0,
            half: false,
        }
    }

    fn pixel(words: &[u32], input_addr: u32) -> Result<HostShader, ShaderError> {
        pixel_with_inputs(words, input_addr, &[element("TEXCOORD", 0, 0)])
    }

    pub(super) fn pixel_with_inputs(
        words: &[u32],
        input_addr: u32,
        inputs: &[SigElement],
    ) -> Result<HostShader, ShaderError> {
        let native = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 8,
            layout: vec![],
            used: vec![],
            io: StageIo::Ps {
                inputs: vec![PsInput {
                    usage: Usage::Generic,
                    row: 0,
                    mask: 0b1111,
                    attr: 0,
                    flat: false,
                    fp16: false,
                    pack2: false,
                    row2: 0,
                }],
                input_addr,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let mut target = element("SV_Target", 16, 0);
        target.usage = 15;
        let isg1 = container::signature(inputs, true);
        let osg1 = container::signature(&[target], false);
        let bytes = crate::dxbc::build(&[(FourCC::ISG1, &isg1), (FourCC::OSG1, &osg1)]);
        let c = Container::parse(&bytes).unwrap();
        let p = ir::build(&native.patched_code(), native.wave64, native.vgprs)?;
        let res = resource::track(&p, &native.layout, native.stage, None)?;
        let io = io::raise(&p, &native)?;
        let st = structure::structure(&p)?;
        emit(&c, &native, &p, &st, &io, &res)
    }

    fn parts(shader: &HostShader) -> (u64, Vec<SigElement>) {
        let c = Container::parse(&shader.dxil).unwrap();
        let sfi0 = c.part(FourCC::SFI0).unwrap().data.try_into().unwrap();
        (
            u64::from_le_bytes(sfi0),
            signature(c.part(FourCC::ISG1).unwrap().data).unwrap(),
        )
    }

    fn validate(shader: &HostShader) {
        #[cfg(windows)]
        if let Ok(mut v) =
            validator::Validator::load(validator::Validator::default_path().as_deref())
        {
            v.validate(&mut shader.dxil.clone()).unwrap();
        }
        let _ = shader;
    }

    #[test]
    fn pixel_quad_swizzles_validate_and_dpp8_cannot_cross_quads() {
        let [e0, e1] = exp_mrt0(3);
        let swizzle = |instruction: [u32; 2]| {
            let words = [
                0xc808_0000,
                0xc809_0001,
                instruction[0],
                instruction[1],
                e0,
                e1,
                0xbf81_0000,
            ];
            pixel(&words, PERSP_CENTER)
        };
        for permutation in [0x00, 0x55, 0xaa, 0xff, 0xb1, 0x4e, 0x1b, 0xe4, 0x27] {
            validate(&swizzle([0xd8d4_8000 | permutation, 0x0300_0002]).unwrap());
            validate(&swizzle([0x7e06_02fa, 0xff00_0002 | permutation << 8]).unwrap());
        }
        let selections = [3u32, 0, 2, 1, 4, 6, 5, 7];
        let sel = selections
            .into_iter()
            .enumerate()
            .fold(0, |s, (i, n)| s | n << (3 * i));
        let mov_dpp8 = |sel: u32| [0x7e06_02e9, (sel << 8) | 2];
        validate(&swizzle(mov_dpp8(sel)).unwrap());
        assert!(
            swizzle(mov_dpp8(sel ^ 4)).is_err(),
            "lane 0 now reads lane 7"
        );
    }

    #[test]
    fn barycentrics_read_as_values_are_sv_barycentrics() {
        let [e0, e1] = exp_mrt0(2);
        let words = [v_add_f32(2, 0, 1), e0, e1, 0xBF81_0000];
        let shader = pixel(&words, PERSP_CENTER).unwrap();
        let (features, inputs) = parts(&shader);
        assert_ne!(features & 0x2_0000, 0, "no Barycentrics feature");
        let bary: Vec<_> = inputs.iter().filter(|e| e.kind == 28).collect();
        assert_eq!(bary.len(), 1, "{inputs:?}");
        assert_eq!((bary[0].index, bary[0].col, bary[0].cols), (0, 0, 3));
        assert_eq!(bary[0].row, None);
        let isg1 = Container::parse(&shader.dxil).unwrap();
        let isg1 = isg1.part(FourCC::ISG1).unwrap().data;
        let rw: Vec<u8> = (0..2).map(|k| isg1[8 + 32 * k + 25]).collect();
        assert!(rw.contains(&0b110), "{rw:?}");
        validate(&shader);
    }

    #[test]
    fn two_barycentric_modes_get_two_elements() {
        let [e0, e1] = exp_mrt0(4);
        let words = [v_add_f32(4, 0, 3), e0, e1, 0xBF81_0000];
        let shader = pixel(&words, PERSP_CENTER | LINEAR_CENTER).unwrap();
        let (_, inputs) = parts(&shader);
        let mut indices: Vec<u32> = inputs
            .iter()
            .filter(|e| e.kind == 28)
            .map(|e| e.index)
            .collect();
        indices.sort();
        assert_eq!(indices, [0, 1], "{inputs:?}");
        validate(&shader);
    }

    #[test]
    fn interpolation_alone_declares_no_barycentrics() {
        let [e0, e1] = exp_mrt0(2);
        let words = [0xC808_0000, 0xC809_0001, e0, e1, 0xBF81_0000];
        let shader = pixel(&words, PERSP_CENTER).unwrap();
        let (features, inputs) = parts(&shader);
        assert_eq!(features & 0x2_0000, 0);
        assert!(inputs.iter().all(|e| e.kind != 28), "{inputs:?}");
        validate(&shader);
    }

    #[test]
    fn an_unused_packed_input_matches_its_provoking_vertex_sibling() {
        let [e0, e1] = exp_mrt0(3);
        let words = [0xC80E_0202, e0, e1, 0xBF81_0000];
        let xy = SigElement {
            cols: 2,
            ..element("TEXCOORD", 0, 0)
        };
        let zw = SigElement {
            col: 2,
            cols: 2,
            ..element("NORMAL", 0, 0)
        };
        let shader = pixel_with_inputs(&words, PERSP_CENTER, &[xy, zw]).unwrap();
        let c = Container::parse(&shader.dxil).unwrap();
        let psv = c.part(FourCC::PSV0).unwrap().data;
        for columns in [0x42, 0x62] {
            let record = [1, 0, columns, 0, 3, 1];
            assert!(psv.windows(record.len()).any(|w| w == record));
        }
        let inputs = c.part(FourCC::ISG1).unwrap().data;
        assert_eq!((inputs[8 + 25], inputs[8 + 32 + 25]), (0, 4));
        validate(&shader);
    }

    #[test]
    fn interpolation_and_a_barycentric_value_together() {
        let [e0, e1] = exp_mrt0(3);
        let words = [
            0xC808_0000,
            0xC809_0001,
            v_add_f32(3, 2, 1),
            e0,
            e1,
            0xBF81_0000,
        ];
        let shader = pixel(&words, PERSP_CENTER).unwrap();
        let (features, inputs) = parts(&shader);
        assert_ne!(features & 0x2_0000, 0);
        assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
        validate(&shader);
    }
    fn texcoord_mode(shader: &HostShader) -> u8 {
        let c = Container::parse(&shader.dxil).unwrap();
        let psv = c.part(FourCC::PSV0).unwrap().data;
        let row = [1, 0, 0x44, 0, 3];
        let at = psv.windows(5).position(|w| w == row).unwrap();
        psv[at + 5]
    }

    const S_ENDPGM: u32 = 0xBF81_0000;

    #[test]
    fn computed_barycentrics_read_the_vertices() {
        let [e0, e1] = exp_mrt0(2);
        let words = [0x1006_00F0, 0xC808_0003, 0xC809_0001, e0, e1, S_ENDPGM];
        let shader = pixel(&words, PERSP_CENTER).unwrap();
        let (features, inputs) = parts(&shader);
        assert_ne!(features & 0x2_0000, 0, "no Barycentrics feature");
        assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
        assert_eq!(texcoord_mode(&shader), 1);
        validate(&shader);
    }

    #[test]
    fn constant_weights_need_no_barycentrics() {
        let [e0, e1] = exp_mrt0(2);
        let words = [0x7E06_02F0, 0xC808_0003, 0xC809_0003, e0, e1, S_ENDPGM];
        let shader = pixel(&words, PERSP_CENTER).unwrap();
        let (features, inputs) = parts(&shader);
        assert_ne!(features & 0x2_0000, 0, "no Barycentrics feature");
        assert!(inputs.iter().all(|e| e.kind != 28), "{inputs:?}");
        assert_eq!(texcoord_mode(&shader), 1);
        validate(&shader);
    }

    #[test]
    fn computed_and_loaded_barycentrics_on_one_attribute() {
        let [e0, e1] = exp_mrt0(2);
        let words = [
            0xC810_0100,
            0xC811_0101,
            0x1006_02F0,
            0xC808_0000,
            0xC809_0003,
            v_add_f32(2, 2, 4),
            e0,
            e1,
            S_ENDPGM,
        ];
        let shader = pixel(&words, PERSP_CENTER).unwrap();
        let (_, inputs) = parts(&shader);
        assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
        assert_eq!(texcoord_mode(&shader), 1);
        validate(&shader);
    }

    #[test]
    fn p10_of_an_interpolated_attribute() {
        let [e0, e1] = exp_mrt0(2);
        let words = [
            0xC808_0000,
            0xC809_0001,
            0xC812_0000,
            v_add_f32(2, 2, 4),
            e0,
            e1,
            S_ENDPGM,
        ];
        let shader = pixel(&words, PERSP_CENTER).unwrap();
        assert_eq!(texcoord_mode(&shader), 1);
        validate(&shader);
        let [e0, e1] = exp_mrt0(4);
        let words = [0xC812_0000, e0, e1, S_ENDPGM];
        assert!(pixel(&words, PERSP_CENTER).is_err());
    }
}
