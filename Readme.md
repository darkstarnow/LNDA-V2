# Leonida

Leonida runs an Xbox Series executable from your own game installation on 64-bit Windows. The title's x86-64 code runs natively, the console system APIs are provided by Leonida, and GPU work is translated to Direct3D 12. Game files, saves and shader caches stay on your computer.

## Requirements

- 64-bit Windows with a current Direct3D 12 driver (mesh shaders, Shader Model 6.6)
- [Rust](https://rustup.rs/) 1.98 or newer
- [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with **Desktop development with C++** (x64 MSVC toolset and a Windows 10 or 11 SDK)

## Install

Double-click `Setup.cmd`, then select the Series X game executable inside your own extracted installation. Setup:

1. fetches and verifies the BCPack format tables,
2. builds `Leonida.exe`, `LeonidaPlay.exe` and `LeonidaWarm.exe`,
3. installs them into `%LOCALAPPDATA%\Leonida`,
4. adds **Leonida** and **Leonida (Experimental RT)** to the Start menu.

Saves go to `State`, shader caches to `Cache` and per-run diagnostics to `Logs` inside the install directory. Run `Setup.cmd` again to update. To choose a different game:

```powershell
.\Setup.cmd --game-path "D:\Games\SeriesX\game.exe"
```

`LeonidaWarm.exe` warms the shader cache ahead of play.

The window starts at 1280×720, centered on the primary monitor. Moving or resizing the title window saves its bounds under `State\Window\Placement.txt`; the next launch keeps them inside the primary work area. The renderer's fallback window also starts at 1280×720. Window size does not change the title's render resolution or remove GPU work.

The launcher reports Series X by default. The manual `--console series-s` option changes console memory and texture tiling; it is not a low-end GPU preset. Use the console mode appropriate to the title's assets. A frame counter alone does not confirm correct rendering or playable gameplay.

## Manual build

```powershell
cargo run --package leonida-tasks -- tables
```

```powershell
cargo build --release --package leonida --features diagnostics --bins
```

The binaries land in `target\release`. Keep all three together outside the checkout and pass the game executable to `LeonidaPlay.exe`. Copy `LeonidaExample.toml` to `Leonida.toml` to set custom game, cache and data directories. The `LEONIDA_GAME_DIR`, `LEONIDA_CACHE_DIR` and `LEONIDA_DATA_DIR` environment variables override the file.

## Layout

| Folder | Package | Role |
|--------|---------|------|
| `Apps/Leonida` | `leonida` | Command-line front end, launcher and shader warm-up |
| `Crates/Core` | `leonida-core` | Configuration, logging, asset-policy guard |
| `Crates/Image` | `leonida-image` | Read-only PE32+ and import-library analysis |
| `Crates/Loader` | `leonida-loader` | Prepares, maps, binds and starts the title image |
| `Crates/Services` | `leonida-services` | Console system API implementations |
| `Crates/Graphics` | `leonida-graphics` | D3D12.X objects, command lists and GPU memory on host D3D12 |
| `Crates/Shader` | `leonida-shader` | Console shader containers to host DXIL |
| `Crates/Formats` | `leonida-formats` | Texture layouts, BCPack, audio codecs |
| `Crates/Conversion` | `leonida-conversion` | Converter trait and content-addressed cache |
| `Crates/Title` | `leonida-title` | Title-specific containers, build identities and hooks |
| `Tasks` | `leonida-tasks` | Repository policy, format tables, Windows setup |

See [Docs/Architecture.md](Docs/Architecture.md) and [Docs/AssetPolicy.md](Docs/AssetPolicy.md).

## Conventions

- Every file and folder name is PascalCase.
- Source and configuration files carry no comments.
- Everything that can be Rust is Rust.

`cargo run --package leonida-tasks -- policy` checks all three rules plus the asset policy.

## License

Leonida is licensed under GPL-3.0-or-later. See [License.txt](License.txt) and [Notice.md](Notice.md).
