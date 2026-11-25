#[cfg(all(windows, feature = "diagnostics"))]
#[path = "Capture.rs"]
mod capture;
#[path = "Console.rs"]
pub mod console;
#[cfg(all(windows, feature = "diagnostics"))]
#[path = "CopyStat.rs"]
mod copystat;
#[cfg(windows)]
#[path = "DebugHud.rs"]
mod debug_hud;
#[path = "Descriptor.rs"]
pub mod descriptor;
#[path = "Device.rs"]
pub mod device;
#[cfg(windows)]
#[path = "Draw.rs"]
mod draw;
#[path = "Dxgi.rs"]
pub mod dxgi;
#[path = "Fence.rs"]
pub mod fence;
#[path = "Frame.rs"]
pub mod frame;
#[cfg(windows)]
#[path = "Host.rs"]
mod host;
#[cfg(windows)]
#[path = "Indirect.rs"]
mod indirect;
#[cfg(windows)]
#[path = "InPlace.rs"]
mod inplace;
#[path = "List.rs"]
pub mod list;
#[cfg(windows)]
#[path = "MaterialDemand.rs"]
mod material_demand;
#[cfg(windows)]
#[path = "Memory.rs"]
mod memory;
#[path = "Object.rs"]
pub mod object;
#[path = "Pipeline.rs"]
pub mod pipeline;
#[cfg(windows)]
#[path = "Primitive.rs"]
mod primitive;
#[path = "Query.rs"]
pub mod query;
#[path = "Queue.rs"]
pub mod queue;
#[path = "Resource.rs"]
pub mod resource;
#[path = "Root.rs"]
pub mod root;
#[cfg(windows)]
#[path = "Sampler.rs"]
mod sampler;
#[cfg(windows)]
#[path = "ShaderCache.rs"]
pub mod shader_cache;
#[cfg(windows)]
#[path = "Shadow.rs"]
mod shadow;
#[path = "Signature.rs"]
pub mod signature;
#[cfg(windows)]
#[path = "TexBack.rs"]
mod texback;
#[cfg(windows)]
#[path = "Texture.rs"]
mod texture;
#[cfg(windows)]
#[path = "Watch.rs"]
mod watch;
#[cfg(windows)]
#[path = "Writeback.rs"]
mod writeback;

#[cfg(windows)]
pub use host::HostSetup;
#[cfg(windows)]
pub use inplace::{title_memory_freeing, title_memory_section, title_memory_view};
#[cfg(windows)]
pub use watch::TitleMemory;
