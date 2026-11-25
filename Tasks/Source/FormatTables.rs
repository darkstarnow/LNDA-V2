use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::workspace::Workspace;

pub struct FormatTables {
    destination: PathBuf,
}

impl FormatTables {
    const RELATIVE_PATH: &'static str = "Crates/Formats/Source/BcPackTables.bin";
    const EXPECTED_SHA256: &'static str =
        "1f1d4760419202bb5672f0c428ce51e456b5d08fbaec8f371d043fd94e01c36b";
    const SOURCE_URL: &'static str = "https://raw.githubusercontent.com/Marvisak/VIctory/2849477415a46f5a079c85cf3e64a3dff32f713c/crates/victory-formats/src/bcpack-tables.bin";

    pub fn new(workspace: &Workspace) -> Self {
        Self {
            destination: workspace.path(Self::RELATIVE_PATH),
        }
    }

    pub fn install(&self, from: Option<&Path>) -> Result<()> {
        if self.destination.is_file() {
            Self::verify(&self.destination)?;
            println!("BCPack format tables verified");
            return Ok(());
        }
        let staging = self.destination.with_extension("partial");
        let outcome = self.stage(&staging, from);
        if outcome.is_err() {
            let _ = fs::remove_file(&staging);
        }
        outcome?;
        fs::rename(&staging, &self.destination)
            .with_context(|| format!("moving tables into {}", self.destination.display()))?;
        println!("BCPack format tables verified and installed");
        Ok(())
    }

    fn stage(&self, staging: &Path, from: Option<&Path>) -> Result<()> {
        match from {
            Some(source) => {
                fs::copy(source, staging)
                    .with_context(|| format!("copying {}", source.display()))?;
            }
            None => Self::download(staging)?,
        }
        Self::verify(staging)
    }

    fn download(target: &Path) -> Result<()> {
        let status = Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--output",
            ])
            .arg(target)
            .arg(Self::SOURCE_URL)
            .status()
            .context("running curl to download the BCPack format tables")?;
        ensure!(
            status.success(),
            "downloading the BCPack format tables failed"
        );
        Ok(())
    }

    fn verify(path: &Path) -> Result<()> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let actual: String = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if actual != Self::EXPECTED_SHA256 {
            bail!(
                "unexpected BCPack table SHA-256 for {}: expected {}, received {actual}",
                path.display(),
                Self::EXPECTED_SHA256
            );
        }
        Ok(())
    }
}
