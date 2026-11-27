use std::collections::HashSet;

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

fn program(skip: u32) -> NativeProgram {
    let mut words = vec![
        0x7E0A_0281,
        0x2C0C_0081,
        0x340E_0B06,
        0x7D88_0000,
        0xBE8C_246A,
        skip | 23,
    ];
    words.extend_from_slice(&super::wave::WAVE_OR_V7_TO_S7);
    words.extend_from_slice(&[
        0x7E00_0207,
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16,
        0xBEFE_040C,
    ]);
    native(&words)
}

fn native(body: &[u32]) -> NativeProgram {
    native_of(body, true, 64)
}

fn native_of(body: &[u32], wave64: bool, threads: u32) -> NativeProgram {
    let mut words = vec![0xBE85_0380, 0x3D << 26 | 0x02 << 18 | 8 << 6 | 2, 125 << 25];
    words.extend_from_slice(body);
    words.push(S_ENDPGM);
    NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 16,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

fn lowered(
    native: &NativeProgram,
    p: &ir::Program,
    wide: &HashSet<usize>,
) -> Result<HostShader, ShaderError> {
    lowered_for_width(native, p, wide, 32)
}

fn lowered_for_width(
    native: &NativeProgram,
    p: &ir::Program,
    wide: &HashSet<usize>,
    host_wave: u32,
) -> Result<HostShader, ShaderError> {
    lowered_for_dispatch(native, p, wide, host_wave, false)
}

fn lowered_for_dispatch(
    native: &NativeProgram,
    p: &ir::Program,
    wide: &HashSet<usize>,
    host_wave: u32,
    partial: bool,
) -> Result<HostShader, ShaderError> {
    let st = structure::structure_wide(p, wide).expect("structure");
    let res = resource::track(p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(host_wave));
    let lowered = lower::lower_with_thread_limit(
        &lower::Parts {
            native,
            p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
        },
        partial,
    );
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered?;
    Ok(HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        written_root_uavs: lowered.written_root_uavs,
        writes_guest_memory: lowered.writes_guest_memory,
        root_indexed_views: res.root_indexed_views,
    })
}

#[test]
fn partial_thread_dispatch_masks_the_final_groups_memory_writes() {
    let native = native(&[0x3402_0082, 0x7e04_0281, 0xe0c8_1000, 0x8002_0201]);
    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let shader = lowered_for_dispatch(&native, &p, &HashSet::new(), 64, true).unwrap();
    let limit = shader
        .user_data
        .iter()
        .find(|u| u.root == RootType::Other(super::USER_DISPATCH_THREADS_X))
        .unwrap();
    assert_eq!((limit.slot, limit.dword), (0, 64));
    let mut validator =
        validator::Validator::load(validator::Validator::default_path().as_deref()).unwrap();
    validator.validate(&mut shader.dxil.clone()).unwrap();
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(true) else { return };
    let (root, pso) = host.compute_pipeline(&shader).unwrap();
    for count in [0u32, 1, 7, 63, 64, 65, 74, 77, 127, 128] {
        let mut constants = vec![0; shader.user_data_dwords() as usize];
        constants[limit.index as usize] = count;
        let result = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(constants))],
                    views: &[View::RawUav(68)],
                    samplers: &[],
                    groups: [2, 1, 1],
                },
            )
            .unwrap();
        let active = if count == 0 { 128 } else { count };
        for (i, &word) in result[0].iter().enumerate() {
            let expected = if i < 64 {
                u32::from((i as u32) < active) + u32::from((i as u32 + 64) < active)
            } else {
                0
            };
            assert_eq!(word, expected, "count {count}, word {i}");
        }
    }
}

#[test]
fn copied_mask_keeps_both_numeric_words_inside_a_lane_branch() {
    let native = native(&[
        0x7D88_0000,
        0xBE94_046A,
        0xBF06_8001,
        0xBF85_0001,
        0xBE94_0402,
        0xBE96_0414,
        0x7D88_0084,
        0xBE8C_246A,
        0xBF88_0009,
        0xBE98_0416,
        0xBE9A_0418,
        0x87FE_1A7E,
        0x7E02_021A,
        0x7E04_021B,
        0xE070_0000,
        0x8002_0100,
        0xE070_0004,
        0x8002_0200,
        0xBEFE_040C,
    ]);
    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let shader = lowered_for_width(&native, &p, &HashSet::new(), 64).expect("lower");
    if let Ok(mut validator) =
        validator::Validator::load(validator::Validator::default_path().as_deref())
    {
        validator
            .validate(&mut shader.dxil.clone())
            .expect("validate");
    }
    let _lock = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no hardware compute device");
        return;
    };
    let Ok((root, pipeline)) = host.compute_pipeline(&shader) else {
        eprintln!("hardware does not take this 64-lane pipeline");
        return;
    };
    for (constants, expected) in [
        ([37, 0, 0, 0], [u32::MAX, 31]),
        ([9, 0, 0, 0], [511, 0]),
        ([1, 1, 0x8000_0001, 0xa5a5_55aa], [0x8000_0001, 0xa5a5_55aa]),
        ([1, 1, 0x3456_7892, 0x8020_1000], [0x3456_7892, 0x8020_1000]),
    ] {
        let mut ud = vec![0; shader.user_data_dwords().max(1) as usize];
        for input in &shader.user_data {
            if input.root == RootType::Constants {
                ud[input.index as usize] = constants[input.dword as usize];
            }
        }
        let out = host
            .dispatch(
                &root,
                &pipeline,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(2)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .expect("dispatch and read both words");
        assert_eq!(out[0][..2], expected);
    }
    eprintln!("four native wave64 low/high word readbacks passed");
}

#[test]
fn a_sum_of_halves_over_the_wave_is_the_consoles_tree_bit_for_bit() {
    let mut words = vec![
        0x340C_0085,
        0x340E_0082,
        0x4A0E_0F06,
        0x4A0E_0F00,
        0x4A0E_0EFF,
        0x0000_3C00,
    ];
    words.extend(super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| {
        if w >> 25 == 28 {
            w & 0x01FF_FFFF | 50 << 25
        } else {
            w
        }
    }));
    words.extend_from_slice(&[0x7E00_0207, 0x38 << 26 | 0x1C << 18, 0x80 << 24 | 2 << 16]);
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert_eq!(
        p.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter(|i| i.name() == "v_add_f16")
            .count(),
        6
    );
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let wide =
        |h: u16| f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13);
    let narrow = |f: f32| {
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    let add = |a: u16, b: u16| narrow(wide(a) + wide(b));
    let terms: Vec<u16> = (0..64u16).map(|t| 0x3C00 + 37 * t).collect();
    let mut tree = terms.clone();
    while tree.len() > 1 {
        tree = tree.chunks(2).map(|c| add(c[0], c[1])).collect();
    }
    let in_a_row = terms.iter().copied().reduce(add).unwrap();
    assert_ne!(
        tree[0], in_a_row,
        "these terms tell the tree from another order"
    );
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!(
                "the hardware device does not take this program (waves of 32?): {e}; not run"
            );
            return;
        }
    };
    let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(1)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    assert_eq!(
        out[0][0],
        u32::from(tree[0]),
        "tree {:#x}, in a row {in_a_row:#x}",
        tree[0]
    );
    eprintln!(
        "ran on the hardware device: the sum {:#x} of the console's tree",
        tree[0]
    );
}

#[test]
fn a_16_bit_sum_rounds_to_nearest_when_asked_to() {
    let words = [
        0x7E0E_02FF,
        0x0000_3C03,
        0x7E0C_02FF,
        0x0000_4000,
        0x6400_0D07,
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16,
    ];
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert!(
        p.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .any(|i| i.name() == "v_add_f16")
    );
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for nearest in [false, true] {
        let was = lower::TEST_F16_NEAREST.replace(Some(nearest));
        let shader = lowered(&native, &p, &HashSet::new());
        lower::TEST_F16_NEAREST.set(was);
        let shader = shader.expect("lower");
        if let Ok(mut v) =
            validator::Validator::load(validator::Validator::default_path().as_deref())
        {
            let mut copy = shader.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
        let Ok(host) = Host::new(false) else {
            eprintln!("no SM 6.6 hardware device; not run");
            return;
        };
        let Ok((root, pso)) = host.compute_pipeline(&shader) else {
            eprintln!("the hardware device does not take this program; not run");
            return;
        };
        let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(1)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        let got = out[0][0] & 0xFFFF;
        eprintln!("v_add_f16 0x3c03 + 0x4000 on the hardware device, nearest {nearest}: {got:#x}");
        if nearest {
            assert_eq!(got, 0x4202);
        } else {
            assert!(got == 0x4201 || got == 0x4202, "{got:#x}");
        }
    }
}

#[test]
fn a_wave_or_inside_a_lane_branch_needs_the_branch_wave_wide() {
    for (skip, name) in [
        (0xBF88_0000, "s_cbranch_execz"),
        (0xBF84_0000, "s_cbranch_scc0"),
    ] {
        a_wave_or_inside(skip, name);
    }
}

fn a_wave_or_inside(skip: u32, name: &str) {
    let native = program(skip);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let branch = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.last().is_some_and(|i| i.name() == name))
        .expect("the branch");
    lower::take_needs();
    let per_lane = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&per_lane, Err(ShaderError::Unsupported { what, .. }) if *what == "a wave reduction where not every lane runs"),
        "{:?}",
        per_lane.err()
    );
    let needs = lower::take_needs();
    let st = structure::structure(&p).expect("structure");
    assert_eq!(lower::branches_around(&st, &needs), Some(vec![branch]));
    assert_eq!(st.wave_any[branch], Some(false));
    let wide = HashSet::from([branch]);
    let st = structure::structure_wide(&p, &wide).expect("structure");
    let inside = match p.blocks[branch].exit {
        ir::Exit::Branch { not_taken, .. } => not_taken,
        _ => unreachable!(),
    };
    assert!(st.guards[inside].iter().any(Option::is_some));
    let shader = lowered(&native, &p, &wide).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!(
                "the hardware device does not take this program (waves of 32?): {e}; not run"
            );
            return;
        }
    };
    for (below, expected) in [
        (64u32, u32::MAX),
        (40, 0x000F_FFFF),
        (33, 0x0001_FFFF),
        (10, 0x1F),
        (1, 1),
    ] {
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if let (RootType::Constants, 0, 0) = (u.root, u.slot, u.dword) {
                ud[u.index as usize] = below;
            }
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(1)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        assert_eq!(out[0][0], expected, "{name}: {below} threads in the branch");
    }
    eprintln!("ran on the hardware device ({name}): the OR of both halves in every case");
}

fn on_the_card(shader: &HostShader, constants: [u32; 2]) -> Option<u32> {
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let Ok((root, pso)) = host.compute_pipeline(shader) else {
        eprintln!("the hardware device does not take this program; not run");
        return None;
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if let (RootType::Constants, 0, d @ 0..=1) = (u.root, u.slot, u.dword) {
            ud[u.index as usize] = constants[d as usize];
        }
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(1)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][0])
}

fn wave_lanes() -> Option<u32> {
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_FEATURE_D3D12_OPTIONS1, D3D12_FEATURE_DATA_D3D12_OPTIONS1,
    };
    let host = Host::new(false).ok()?;
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    unsafe {
        host.device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS1,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
            .ok()?
    };
    Some(options.WaveLaneCountMin)
}

#[test]
fn a_lane_mask_joined_with_a_number_is_read_as_the_number_it_is() {
    let words = [
        0x7D88_0000,
        0xBE94_036A,
        0xBF06_8001,
        0xBF85_0001,
        0xBE94_0385,
        0x7E00_0214,
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16,
    ];
    let native = native_of(&words, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(lanes) = wave_lanes().filter(|&lanes| lanes < 32) {
        eprintln!("the device's waves have {lanes} lanes, fewer than this program's 32; not run");
        return;
    }
    for (below, other, expected) in [
        (32u32, 0u32, u32::MAX),
        (10, 0, 0x3FF),
        (0, 0, 0),
        (10, 1, 5),
    ] {
        let Some(got) = on_the_card(&shader, [below, other]) else {
            return;
        };
        assert_eq!(got, expected, "{below} lanes in the mask, s1 = {other}");
    }
    eprintln!("ran on the hardware device: the mask's bits, or the number");
}

#[test]
fn wave32_small_groups_preserve_lane_masks_on_a_wave64_only_device() {
    let words = [
        0x7D88_0000,
        0xBE94_036A,
        0xBF06_8001,
        0xBF85_0001,
        0xBE94_0385,
        0x7E00_0214,
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16,
    ];
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let hardware_wave64 = wave_lanes() == Some(64);
    for (minimum, threads, expected_wave) in [
        (64, 1, 64),
        (64, 16, 64),
        (64, 32, 64),
        (64, 64, 0),
        (32, 32, 32),
        (0, 32, 32),
    ] {
        let mut native = native_of(&words, false, threads);
        if let StageIo::Cs(ref mut cs) = native.io {
            cs.rsrc2 |= 1 << 15;
        }
        let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
        let previous = lower::TEST_HOST_MIN_WAVE.replace(Some(minimum));
        let shader = lowered_for_width(&native, &p, &HashSet::new(), 64);
        lower::TEST_HOST_MIN_WAVE.set(previous);
        let shader = shader.unwrap();
        let container = crate::dxbc::Container::parse(&shader.dxil).unwrap();
        let psv = container.part(crate::dxbc::FourCC::PSV0).unwrap().data;
        let width = u32::from_le_bytes(psv[20..24].try_into().unwrap());
        assert_eq!(width, expected_wave, "minimum {minimum}, group {threads}");
        let mut validator =
            validator::Validator::load(validator::Validator::default_path().as_deref()).unwrap();
        validator.validate(&mut shader.dxil.clone()).unwrap();
        if minimum == 64 && threads <= 32 && hardware_wave64 {
            for below in [0u32, 1, 10, 32] {
                let active = below.min(threads);
                let expected = if active == 32 {
                    u32::MAX
                } else {
                    (1u32 << active) - 1
                };
                let actual = on_the_card(&shader, [below, 0])
                    .expect("wave64-only device must accept the lifted single-wave shader");
                assert_eq!(actual, expected, "group {threads}, active {below}");
            }
        }
    }
    if hardware_wave64 {
        eprintln!("executed 12 small-group lane-mask cases on the wave64-only hardware device");
    }
}

#[test]
fn scc_of_a_mask_operation_joined_with_a_scalar_test_needs_the_skip_wave_wide() {
    let words = [
        0x7D88_0000,
        0xBE8D_3C6A,
        0xBF88_0001,
        0xBF06_8701,
        0xBEFE_030D,
        0xBF85_0002,
        0x7E0A_0281,
        0xBF82_0001,
        0x7E0A_0282,
        0x7DA4_009F,
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16 | 5 << 8,
    ];
    let native = native_of(&words, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let branch = (0..p.blocks.len())
        .find(|&b| {
            p.blocks[b]
                .insts
                .last()
                .is_some_and(|i| i.name() == "s_cbranch_execz")
        })
        .expect("the skip");
    lower::take_needs();
    let per_lane = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&per_lane, Err(ShaderError::Unsupported { what, .. }) if *what == "a lane mask read as data"),
        "{:?}",
        per_lane.err()
    );
    let st = structure::structure(&p).expect("structure");
    assert_eq!(
        lower::branches_around(&st, &lower::take_needs()),
        Some(vec![branch])
    );
    let shader = lowered(&native, &p, &HashSet::from([branch])).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (below, s1, expected) in [
        (0u32, 7u32, 1u32),
        (10, 7, 2),
        (10, 3, 1),
        (32, 3, 1),
        (32, 7, 2),
    ] {
        let Some(got) = on_the_card(&shader, [below, s1]) else {
            return;
        };
        assert_eq!(got, expected, "{below} lanes inside, s1 = {s1}");
    }
    eprintln!(
        "ran on the hardware device: the inner test's SCC for every lane once some lane is inside"
    );
}
