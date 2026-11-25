#[path = "CityHash.rs"]
pub mod cityhash;
#[path = "Pipelines.rs"]
pub mod pipelines;
#[path = "Sgd5.rs"]
pub mod sgd5;
#[path = "ShaderPack.rs"]
pub mod shaderpack;

#[derive(Debug, Clone, Copy)]
pub struct KnownBuild {
    pub label: &'static str,
    pub blake3: &'static str,
}

pub const KNOWN_BUILDS: &[KnownBuild] = &[];

pub fn identify(blake3_hex: &str) -> Option<&'static KnownBuild> {
    KNOWN_BUILDS
        .iter()
        .find(|b| b.blake3.eq_ignore_ascii_case(blake3_hex))
}
