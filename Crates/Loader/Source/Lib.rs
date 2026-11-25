#[cfg(windows)]
#[path = "ConsoleInsn.rs"]
mod console_insn;
#[path = "Crash.rs"]
mod crash;
#[path = "Prepare.rs"]
pub mod prepare;
#[path = "Resolve.rs"]
pub mod resolve;
#[cfg(all(windows, feature = "diagnostics"))]
#[path = "Stacks.rs"]
mod stacks;
#[cfg(windows)]
#[path = "Threads.rs"]
mod threads;
#[cfg(windows)]
#[path = "Thunks.rs"]
mod thunks;
#[cfg(windows)]
#[path = "Tls.rs"]
mod tls;
#[cfg(windows)]
#[path = "Windows.rs"]
mod windows;

#[cfg(all(windows, feature = "diagnostics"))]
pub use crash::save_on_crash;
#[cfg(windows)]
pub use crash::{
    locate, log_crash, log_crashes_to, note_title_output, report_crashes, report_unhandled,
};
pub use prepare::{PrepareError, PreparedImage, prepare};
pub use resolve::{ImportResolver, Provenance, Resolution};
#[cfg(all(windows, feature = "diagnostics"))]
pub use stacks::{dump_stacks, dump_stacks_on_schedule};
#[cfg(windows)]
pub use threads::disable_thread_calls;
#[cfg(windows)]
pub use thunks::{UnimplementedPolicy, unimplemented_call_count};
#[cfg(windows)]
pub use windows::{
    BindReport, LoadedImage, MissingImport, WithTitleDlls, load, load_title_dll, load_title_dlls,
};

pub const DLL_PROCESS_ATTACH: u32 = 1;
pub const DLL_PROCESS_DETACH: u32 = 0;
