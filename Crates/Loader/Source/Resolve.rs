use leonida_image::ImportSymbol;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provenance {
    Service,
    Host,
    Title,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Found {
        address: usize,
        provenance: Provenance,
    },
    Missing,
}

pub trait ImportResolver {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution;
}
