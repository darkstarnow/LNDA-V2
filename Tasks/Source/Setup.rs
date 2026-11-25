use std::fs::{self, File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::format_tables::FormatTables;
use crate::shell::{ComApartment, Shortcut};
use crate::workspace::Workspace;

pub struct Installer<'a> {
    workspace: &'a Workspace,
    shell: ComApartment,
    install_dir: PathBuf,
    shortcut_dir: PathBuf,
}

impl<'a> Installer<'a> {
    const TARGET: &'static str = "x86_64-pc-windows-msvc";
    const BINARIES: [&'static str; 3] = ["Leonida.exe", "LeonidaPlay.exe", "LeonidaWarm.exe"];
    const GAME_PATH_FILE: &'static str = "GamePath.txt";

    pub fn new(
        workspace: &'a Workspace,
        install_dir: Option<PathBuf>,
        shortcut_dir: Option<PathBuf>,
    ) -> Result<Self> {
        ensure!(
            cfg!(target_arch = "x86_64"),
            "Leonida requires 64-bit Windows"
        );
        let shell = ComApartment::enter()?;
        let install_dir = match install_dir {
            Some(dir) => dir,
            None => std::env::var_os("LOCALAPPDATA")
                .map(|base| PathBuf::from(base).join("Leonida"))
                .context("LOCALAPPDATA is not set")?,
        };
        let install_dir = std::path::absolute(install_dir)?;
        let shortcut_dir = match shortcut_dir {
            Some(dir) => dir,
            None => shell.programs_folder()?,
        };
        Ok(Self {
            workspace,
            shell,
            install_dir,
            shortcut_dir,
        })
    }

    pub fn run(&self, game: Option<PathBuf>) -> Result<()> {
        if Workspace::encloses(&self.install_dir) {
            bail!("choose an install directory outside the source checkout");
        }
        let Some(game) = self.resolve_game(game)? else {
            println!("Setup cancelled.");
            return Ok(());
        };
        FormatTables::new(self.workspace).install(None)?;
        let release = self.build()?;
        Self::validate(&release, &game)?;
        self.deploy(&release, &game)?;
        self.create_shortcuts()?;
        println!(
            "Leonida is installed in {}. Open it from the Start menu.",
            self.install_dir.display()
        );
        Ok(())
    }

    fn resolve_game(&self, game: Option<PathBuf>) -> Result<Option<PathBuf>> {
        let game = match game.or_else(|| self.saved_game()) {
            Some(path) => path,
            None => match self.shell.pick_executable(
                "Select the Series X game executable from your extracted game installation",
            )? {
                Some(path) => path,
                None => return Ok(None),
            },
        };
        let is_exe = game
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"));
        ensure!(
            game.is_file() && is_exe,
            "select the game executable inside its original installation directory"
        );
        Ok(Some(std::path::absolute(game)?))
    }

    fn saved_game(&self) -> Option<PathBuf> {
        let saved = fs::read_to_string(self.install_dir.join(Self::GAME_PATH_FILE)).ok()?;
        let path = PathBuf::from(saved.trim());
        path.is_file().then_some(path)
    }

    fn build(&self) -> Result<PathBuf> {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let target_dir = self.workspace.path("target");
        let status = Command::new(cargo)
            .args(["build", "--locked", "--release", "--package", "leonida"])
            .args([
                "--features",
                "diagnostics",
                "--bins",
                "--target",
                Self::TARGET,
            ])
            .arg("--target-dir")
            .arg(&target_dir)
            .current_dir(self.workspace.root())
            .status()
            .context("running cargo")?;
        ensure!(
            status.success(),
            "build failed; check the compiler output, the C++ workload and the Windows SDK"
        );
        Ok(target_dir.join(Self::TARGET).join("release"))
    }

    fn validate(release: &Path, game: &Path) -> Result<()> {
        let status = Command::new(release.join("Leonida.exe"))
            .arg("inspect")
            .arg(game)
            .stdout(Stdio::null())
            .status()
            .context("starting Leonida.exe")?;
        ensure!(
            status.success(),
            "Leonida could not read the selected game executable"
        );
        Ok(())
    }

    fn deploy(&self, release: &Path, game: &Path) -> Result<()> {
        let state = self.install_dir.join("State");
        fs::create_dir_all(&state)?;
        let _lock = Self::lock(&state.join("Launcher.lock"))?;
        for binary in Self::BINARIES {
            let staging = self.install_dir.join(format!("{binary}.partial"));
            fs::copy(release.join(binary), &staging)
                .with_context(|| format!("copying {binary}"))?;
            fs::rename(&staging, self.install_dir.join(binary))
                .with_context(|| format!("replacing {binary}"))?;
        }
        fs::write(
            self.install_dir.join(Self::GAME_PATH_FILE),
            game.to_string_lossy().as_bytes(),
        )?;
        Ok(())
    }

    fn lock(path: &Path) -> Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(path)
            .context(
                "could not lock the install directory; close Leonida and the shader preload first",
            )
    }

    fn create_shortcuts(&self) -> Result<()> {
        fs::create_dir_all(&self.shortcut_dir)?;
        let launcher = self.install_dir.join("LeonidaPlay.exe");
        for (name, arguments) in [("Leonida", ""), ("Leonida (Experimental RT)", "--rt")] {
            self.shell
                .create_shortcut(&Shortcut::new(
                    &self.shortcut_dir,
                    name,
                    &launcher,
                    arguments,
                ))
                .with_context(|| format!("creating the {name} shortcut"))?;
        }
        Ok(())
    }
}
