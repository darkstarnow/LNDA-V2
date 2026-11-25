use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

pub const ROOT_MARKER: &str = ".LeonidaRoot";

pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn locate() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the tasks crate lives inside the workspace")
            .to_path_buf();
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        relative
            .split('/')
            .fold(self.root.clone(), |path, part| path.join(part))
    }

    pub fn encloses(path: &Path) -> bool {
        path.ancestors()
            .any(|ancestor| ancestor.join(ROOT_MARKER).is_file())
    }

    pub fn files(&self, staged: bool) -> Result<Vec<String>> {
        if staged {
            return self
                .git_list(&[
                    "diff",
                    "--cached",
                    "--name-only",
                    "--diff-filter=ACMR",
                    "-z",
                ])?
                .context("checking staged files needs a git repository");
        }
        match self.git_list(&[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])? {
            Some(mut files) => {
                files.sort();
                files.dedup();
                Ok(files)
            }
            None => Ok(self.walk()),
        }
    }

    pub fn read(&self, relative: &str, staged: bool) -> Option<Vec<u8>> {
        if !staged {
            return std::fs::read(self.path(relative)).ok();
        }
        let output = Command::new("git")
            .args(["show", &format!(":{relative}")])
            .current_dir(&self.root)
            .output()
            .ok()?;
        output.status.success().then_some(output.stdout)
    }

    fn git_list(&self, args: &[&str]) -> Result<Option<Vec<String>>> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .context("running git")?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(
            output
                .stdout
                .split(|&byte| byte == 0)
                .filter(|entry| !entry.is_empty())
                .map(|entry| String::from_utf8_lossy(entry).replace('\\', "/"))
                .collect(),
        ))
    }

    fn walk(&self) -> Vec<String> {
        walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|entry| !matches!(entry.file_name().to_str(), Some("target" | ".git")))
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .filter_map(|entry| {
                entry
                    .path()
                    .strip_prefix(&self.root)
                    .ok()
                    .map(|path| path.to_string_lossy().replace('\\', "/"))
            })
            .collect()
    }
}
