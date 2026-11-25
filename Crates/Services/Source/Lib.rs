#[path = "Host.rs"]
mod host;
#[path = "Modules.rs"]
pub mod modules;
#[path = "Registry.rs"]
mod registry;
#[path = "Runtime.rs"]
pub mod runtime;

pub use host::HostPolicy;
pub use leonida_graphics::console;
pub use registry::{ServiceExport, ServiceModule, registry};

use leonida_image::ImportSymbol;
use leonida_loader::{ImportResolver, Provenance, Resolution};

#[derive(Debug, Default)]
pub struct ServiceResolver {
    pub host: HostPolicy,
}

impl ServiceResolver {
    pub fn new(host: HostPolicy) -> Self {
        ServiceResolver { host }
    }

    pub fn classify(&self, dll: &str, symbol: &ImportSymbol) -> Option<Provenance> {
        match self.resolve(dll, symbol) {
            Resolution::Found { provenance, .. } => Some(provenance),
            Resolution::Missing => None,
        }
    }
}

impl ImportResolver for ServiceResolver {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        if let Some(address) = registry::lookup(dll, &symbol.to_string()) {
            return Resolution::Found {
                address,
                provenance: Provenance::Service,
            };
        }
        match self.host.resolve(dll, symbol) {
            Some(address) => Resolution::Found {
                address,
                provenance: Provenance::Host,
            },
            None => Resolution::Missing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(n: &str) -> ImportSymbol {
        ImportSymbol::Name {
            hint: 0,
            name: n.into(),
        }
    }

    #[test]
    fn console_api_resolves_to_service() {
        let r = ServiceResolver::new(HostPolicy::none());
        assert_eq!(
            r.classify("XGameRuntime.dll", &name("XGameRuntimeInitialize")),
            Some(Provenance::Service)
        );
    }

    #[test]
    fn exit_process_resolves_to_service() {
        let r = ServiceResolver::new(HostPolicy::default());
        for dll in [
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-processthreads-l1-1-0.dll",
        ] {
            assert_eq!(
                r.classify(dll, &name("ExitProcess")),
                Some(Provenance::Service),
                "{dll}"
            );
        }
    }

    #[test]
    fn strict_policy_reports_missing() {
        let r = ServiceResolver::new(HostPolicy::none());
        assert_eq!(
            r.classify("api-ms-win-core-synch-l1-1-0.dll", &name("Sleep")),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn shared_api_sets_pass_through_to_host() {
        let r = ServiceResolver::default();
        assert_eq!(
            r.classify("api-ms-win-core-synch-l1-2-0.dll", &name("Sleep")),
            Some(Provenance::Host)
        );
        assert_eq!(
            r.classify("xmem.dll", &name("XMemSetAllocationHooks")),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn overrides_win_over_host() {
        let r = ServiceResolver::default();
        assert_eq!(
            r.classify(
                "api-ms-win-core-libraryloader-l1-2-0.dll",
                &name("GetModuleHandleW")
            ),
            Some(Provenance::Service)
        );
    }
}
