#[cfg(windows)]
#[path = "Modules/Apu.rs"]
pub mod apu;
#[path = "Modules/Audio.rs"]
pub mod audio;
#[path = "Modules/D3D12X.rs"]
pub mod d3d12x;
#[path = "Modules/Display.rs"]
pub mod display;
#[path = "Modules/Dsp.rs"]
pub mod dsp;
#[cfg(windows)]
#[path = "Modules/DirectStorage.rs"]
pub mod dstorage;
#[cfg(windows)]
#[path = "Modules/Files.rs"]
pub mod files;
#[path = "Modules/GameInput.rs"]
pub mod gameinput;
#[path = "Modules/Kernel.rs"]
pub mod kernel;
#[cfg(windows)]
#[path = "Modules/Lifecycle.rs"]
pub mod lifecycle;
#[cfg(windows)]
#[path = "Modules/Media.rs"]
pub mod media;
#[cfg(windows)]
#[path = "Modules/Net.rs"]
pub mod net;
#[path = "Modules/Speech.rs"]
pub mod speech;
#[path = "Modules/Storage.rs"]
pub mod storage;
#[path = "Modules/XAsync.rs"]
pub mod xasync;
#[path = "Modules/Xg.rs"]
pub mod xg;
#[path = "Modules/XGame.rs"]
pub mod xgame;
#[path = "Modules/XGameRuntime.rs"]
pub mod xgameruntime;
#[path = "Modules/XGameSave.rs"]
pub mod xgamesave;
#[path = "Modules/XGameUi.rs"]
pub mod xgameui;
#[cfg(windows)]
#[path = "Modules/XMem.rs"]
pub mod xmem;
#[path = "Modules/XNetworking.rs"]
pub mod xnetworking;
#[path = "Modules/XPackage.rs"]
pub mod xpackage;
#[path = "Modules/XStore.rs"]
pub mod xstore;
#[path = "Modules/XSystem.rs"]
pub mod xsystem;
#[path = "Modules/XUser.rs"]
pub mod xuser;

use crate::ServiceModule;

pub(crate) fn all() -> Vec<ServiceModule> {
    let mut all = vec![
        kernel::module(),
        xgameruntime::module(),
        gameinput::module(),
        d3d12x::module(),
        audio::module(),
        dsp::module(),
        xg::module(),
    ];
    #[cfg(windows)]
    all.push(apu::module());
    #[cfg(windows)]
    all.push(media::module());
    #[cfg(windows)]
    all.push(dstorage::module());
    #[cfg(windows)]
    all.push(files::module());
    #[cfg(windows)]
    all.push(lifecycle::module());
    #[cfg(windows)]
    all.push(net::module());
    #[cfg(windows)]
    all.push(xmem::module());
    #[cfg(windows)]
    all.push(xsystem::module());
    all
}
