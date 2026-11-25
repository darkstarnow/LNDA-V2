#[path = "Bvh.rs"]
pub mod bvh;
#[path = "Dxbc.rs"]
pub mod dxbc;
#[path = "Dxil.rs"]
pub mod dxil;
#[path = "Emit.rs"]
pub mod emit;
#[path = "Gfx10.rs"]
pub mod gfx10;
#[path = "Io.rs"]
pub mod io;
#[path = "Ir.rs"]
pub mod ir;
#[path = "Native.rs"]
pub mod native;
#[path = "Resource.rs"]
pub mod resource;
#[path = "Spill.rs"]
pub mod spill;
#[path = "Structure.rs"]
pub mod structure;

pub use dxbc::{Container, FourCC, Part};
pub use dxil::{DxilProgram, ShaderKind, XboxExtras};
pub use native::{
    HwStage, NativeProgram, ResourceType, RootParam, RootType, UsedResource, native_programs,
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShaderError {
    #[error("not a DXBC container")]
    NotDxbc,
    #[error("malformed shader: {0}")]
    Malformed(&'static str),
    #[error("GFX10 code at {offset:#x}: {reason}")]
    Isa { offset: usize, reason: &'static str },
    #[error("GFX10 code at {offset:#x}: {what} is not supported")]
    Unsupported { offset: usize, what: &'static str },
}

macro_rules! translation_inputs {
    ($($path:literal),+ $(,)?) => {
        pub fn sources() -> &'static [&'static str] {
            &[$(include_str!($path)),+]
        }

        #[cfg(test)]
        const INPUT_PATHS: &[&str] = &[$($path),+];
    };
}

translation_inputs! {
    "Bvh.rs",
    "Dxbc.rs",
    "Dxil.rs",
    "Emit/Bitcode.rs",
    "Emit/Bitcode/Census.rs",
    "Emit/Bitcode/Opt.rs",
    "Emit/Container.rs",
    "Emit/GuestAddress.rs",
    "Emit/Intersect.rs",
    "Emit/Lower.rs",
    "Emit.rs",
    "Emit/OwnBits.rs",
    "Emit/PackedMask.rs",
    "Emit/Rect.rs",
    "Emit/Sampler.rs",
    "Emit/Shader.rs",
    "Emit/Threading.rs",
    "Emit/TypedViews.rs",
    "Emit/Validator.rs",
    "Emit/Wave.rs",
    "Gfx10/Fields.rs",
    "Gfx10.rs",
    "Gfx10/Opcodes.rs",
    "Gfx10/Sp3.rs",
    "Io.rs",
    "Ir.rs",
    "Lib.rs",
    "Native.rs",
    "Resource.rs",
    "Spill.rs",
    "Structure.rs",
}

#[cfg(test)]
mod source_tests {
    use std::path::{Path, PathBuf};

    const OUTSIDE_TRANSLATION: &[&str] = &[
        "Emit/CubeLodTests.rs",
        "Emit/DiffTests.rs",
        "Emit/HelperLaneTests.rs",
        "Emit/Host.rs",
        "Emit/IntersectTests.rs",
        "Emit/ValuTests.rs",
        "Emit/WaterfallTests.rs",
        "Emit/Wave64Tests.rs",
    ];

    #[test]
    fn every_shader_source_is_classified_by_path() {
        fn collect(dir: &Path, root: &Path, paths: &mut Vec<PathBuf>) {
            for entry in dir.read_dir().unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    collect(&path, root, paths);
                } else if path.extension().is_some_and(|s| s == "rs") {
                    paths.push(path.strip_prefix(root).unwrap().to_path_buf());
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("Source");
        let mut files = Vec::new();
        collect(&root, &root, &mut files);
        let mut classified: Vec<PathBuf> = super::INPUT_PATHS
            .iter()
            .chain(OUTSIDE_TRANSLATION)
            .map(PathBuf::from)
            .collect();
        files.sort();
        classified.sort();
        assert_eq!(classified, files);
    }
}
