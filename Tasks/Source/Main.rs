#[path = "FormatTables.rs"]
mod format_tables;
#[path = "Lexer.rs"]
mod lexer;
#[path = "Policy.rs"]
mod policy;
#[cfg(windows)]
#[path = "Setup.rs"]
mod setup;
#[cfg(windows)]
#[path = "Shell.rs"]
mod shell;
#[path = "Workspace.rs"]
mod workspace;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::format_tables::FormatTables;
use crate::policy::AssetPolicy;
use crate::workspace::Workspace;

#[derive(Parser)]
#[command(
    name = "leonida-tasks",
    about = "Leonida repository and installation tasks"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Check the asset policy, PascalCase names and the no-comment rule")]
    Policy {
        #[arg(long, help = "Check only what is staged for commit")]
        staged: bool,
    },
    #[command(about = "Install and verify the BCPack format tables")]
    Tables {
        #[arg(
            long,
            help = "Copy the tables from this file instead of downloading them"
        )]
        from: Option<PathBuf>,
    },
    #[command(about = "Build Leonida, install it and add Start menu shortcuts")]
    Setup {
        #[arg(long, help = "The Series X game executable")]
        game_path: Option<PathBuf>,
        #[arg(long, help = r"Install directory, %LOCALAPPDATA%\Leonida by default")]
        install_dir: Option<PathBuf>,
        #[arg(
            long,
            help = "Shortcut directory, the Start menu programs folder by default"
        )]
        shortcut_dir: Option<PathBuf>,
    },
}

impl Command {
    fn execute(self, workspace: &Workspace) -> Result<()> {
        match self {
            Command::Policy { staged } => AssetPolicy::standard().enforce(workspace, staged),
            Command::Tables { from } => FormatTables::new(workspace).install(from.as_deref()),
            Command::Setup {
                game_path,
                install_dir,
                shortcut_dir,
            } => Self::setup(workspace, game_path, install_dir, shortcut_dir),
        }
    }

    #[cfg(windows)]
    fn setup(
        workspace: &Workspace,
        game_path: Option<PathBuf>,
        install_dir: Option<PathBuf>,
        shortcut_dir: Option<PathBuf>,
    ) -> Result<()> {
        setup::Installer::new(workspace, install_dir, shortcut_dir)?.run(game_path)
    }

    #[cfg(not(windows))]
    fn setup(
        _: &Workspace,
        _: Option<PathBuf>,
        _: Option<PathBuf>,
        _: Option<PathBuf>,
    ) -> Result<()> {
        anyhow::bail!("Leonida setup requires 64-bit Windows")
    }
}

fn main() -> Result<()> {
    Cli::parse().command.execute(&Workspace::locate())
}
