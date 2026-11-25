use anyhow::{Result, bail};

use crate::lexer::RustLexer;
use crate::workspace::Workspace;

pub struct TrackedFile<'a> {
    path: &'a str,
    content: &'a [u8],
}

impl<'a> TrackedFile<'a> {
    pub fn new(path: &'a str, content: &'a [u8]) -> Self {
        Self { path, content }
    }

    fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(self.path)
    }

    fn extension(&self) -> Option<String> {
        self.name()
            .rsplit_once('.')
            .filter(|(stem, _)| !stem.is_empty())
            .map(|(_, extension)| extension.to_ascii_lowercase())
    }

    fn text(&self) -> Option<&str> {
        std::str::from_utf8(self.content).ok()
    }
}

pub trait Rule {
    fn inspect(&self, file: &TrackedFile) -> Option<String>;
}

struct AllowedType;

impl AllowedType {
    const EXTENSIONS: &'static [&'static str] = &["rs", "toml", "md", "txt", "lock", "cmd"];
    const NAMES: &'static [&'static str] = &[".gitignore", ".gitattributes", ".LeonidaRoot"];
}

impl Rule for AllowedType {
    fn inspect(&self, file: &TrackedFile) -> Option<String> {
        let allowed = Self::NAMES.contains(&file.name())
            || file
                .extension()
                .is_some_and(|extension| Self::EXTENSIONS.contains(&extension.as_str()));
        (!allowed).then(|| "file type is not on the allowlist".to_owned())
    }
}

struct SizeLimit(usize);

impl Rule for SizeLimit {
    fn inspect(&self, file: &TrackedFile) -> Option<String> {
        (file.content.len() > self.0).then(|| {
            format!(
                "{} bytes exceeds the {}-byte limit",
                file.content.len(),
                self.0
            )
        })
    }
}

struct ForbiddenSignature;

impl ForbiddenSignature {
    const SIGNATURES: &'static [(&'static [u8], &'static str)] = &[
        (b"MZ", "PE executable"),
        (b"DXBC", "shader container"),
        (b"DDS ", "DDS texture"),
        (b"RIFF", "RIFF media"),
        (b"BC\xC0\xDE", "LLVM bitcode"),
        (b"Microsoft C/C++ MSF", "PDB"),
        (b"!<arch>", "library archive"),
        (b"PK\x03\x04", "zip archive"),
        (b"7z\xBC\xAF", "7-zip archive"),
        (b"MSCF", "cabinet"),
        (b"\xD0\xCF\x11\xE0", "OLE container"),
    ];
}

impl Rule for ForbiddenSignature {
    fn inspect(&self, file: &TrackedFile) -> Option<String> {
        Self::SIGNATURES
            .iter()
            .find(|(magic, _)| file.content.starts_with(magic))
            .map(|(_, kind)| format!("looks like a {kind}"))
    }
}

struct EmbeddedPayload(usize);

impl Rule for EmbeddedPayload {
    fn inspect(&self, file: &TrackedFile) -> Option<String> {
        if file.content.contains(&0) {
            return Some("binary content".to_owned());
        }
        let is_payload =
            |byte: &u8| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=');
        let longest = file
            .content
            .split(|byte| !is_payload(byte))
            .map(<[u8]>::len)
            .max()
            .unwrap_or(0);
        (longest > self.0).then(|| format!("{longest}-character encoded run"))
    }
}

struct PascalCaseNames;

impl PascalCaseNames {
    const EXEMPT: &'static [&'static str] = &[".gitignore", ".gitattributes"];

    fn is_pascal(component: &str) -> bool {
        let stem = component.strip_prefix('.').unwrap_or(component);
        let (stem, extensions) = stem.split_once('.').unwrap_or((stem, ""));
        stem.chars()
            .next()
            .is_some_and(|first| first.is_ascii_uppercase())
            && stem.chars().all(|c| c.is_ascii_alphanumeric())
            && extensions.split('.').all(|extension| {
                extension
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            })
    }
}

impl Rule for PascalCaseNames {
    fn inspect(&self, file: &TrackedFile) -> Option<String> {
        file.path
            .split('/')
            .find(|component| !Self::EXEMPT.contains(component) && !Self::is_pascal(component))
            .map(|component| format!("`{component}` is not PascalCase"))
    }
}

struct NoComments;

impl NoComments {
    fn hash_comment_line(text: &str) -> Option<usize> {
        text.lines().position(|line| {
            let mut quote = None;
            for c in line.chars() {
                match (quote, c) {
                    (Some(open), c) if c == open => quote = None,
                    (Some(_), _) => {}
                    (None, '"' | '\'') => quote = Some(c),
                    (None, '#') => return true,
                    _ => {}
                }
            }
            false
        })
    }
}

impl Rule for NoComments {
    fn inspect(&self, file: &TrackedFile) -> Option<String> {
        let text = file.text()?;
        let line = match file.extension().as_deref() {
            Some("rs") => RustLexer::new(text).first_comment(),
            Some("toml") if file.name() != "Cargo.lock" => {
                Self::hash_comment_line(text).map(|l| l + 1)
            }
            _ if Self::hash_files(file.name()) => Self::hash_comment_line(text).map(|l| l + 1),
            _ => None,
        }?;
        Some(format!("comment on line {line}"))
    }
}

impl NoComments {
    fn hash_files(name: &str) -> bool {
        matches!(name, ".gitignore" | ".gitattributes")
    }
}

pub struct AssetPolicy {
    rules: Vec<Box<dyn Rule>>,
}

impl AssetPolicy {
    pub fn standard() -> Self {
        Self {
            rules: vec![
                Box::new(AllowedType),
                Box::new(SizeLimit(512 * 1024)),
                Box::new(ForbiddenSignature),
                Box::new(EmbeddedPayload(512)),
                Box::new(PascalCaseNames),
                Box::new(NoComments),
            ],
        }
    }

    pub fn violations(&self, file: &TrackedFile) -> Vec<String> {
        self.rules
            .iter()
            .filter_map(|rule| rule.inspect(file))
            .collect()
    }

    pub fn enforce(&self, workspace: &Workspace, staged: bool) -> Result<()> {
        let files = workspace.files(staged)?;
        let mut violations = Vec::new();
        for path in &files {
            let Some(content) = workspace.read(path, staged) else {
                continue;
            };
            let file = TrackedFile::new(path, &content);
            violations.extend(
                self.violations(&file)
                    .into_iter()
                    .map(|problem| format!("{path}: {problem}")),
            );
        }
        if violations.is_empty() {
            println!("policy: {} files ok", files.len());
            return Ok(());
        }
        for violation in &violations {
            eprintln!("policy: {violation}");
        }
        bail!("{} policy violation(s)", violations.len())
    }
}

#[cfg(test)]
mod tests {
    use super::{AssetPolicy, TrackedFile};
    use crate::workspace::Workspace;

    fn problems(path: &str, content: &[u8]) -> Vec<String> {
        AssetPolicy::standard().violations(&TrackedFile::new(path, content))
    }

    #[test]
    fn repository_passes_policy() {
        AssetPolicy::standard()
            .enforce(&Workspace::locate(), false)
            .unwrap();
    }

    #[test]
    fn accepts_clean_sources() {
        assert!(problems("Crates/Core/Source/Lib.rs", b"fn main() {}\n").is_empty());
        assert!(problems(".gitignore", b"/target/\n").is_empty());
        assert!(problems("Cargo.toml", b"[workspace]\n").is_empty());
    }

    #[test]
    fn rejects_names_that_are_not_pascal_case() {
        assert!(!problems("crates/core/src/lib.rs", b"fn main() {}\n").is_empty());
        assert!(!problems("Crates/Core/Source/debug_hud.rs", b"fn main() {}\n").is_empty());
    }

    #[test]
    fn rejects_comments() {
        assert!(!problems("Crates/Core/Source/Lib.rs", b"// note\nfn main() {}\n").is_empty());
        assert!(!problems("Cargo.toml", b"# note\n[workspace]\n").is_empty());
    }

    #[test]
    fn rejects_game_like_files() {
        assert!(!problems("Assets/Texture.dds", b"DDS \x7c\0\0\0").is_empty());
        assert!(!problems("Notes.txt", b"MZ\x90\0").is_empty());
        assert!(!problems("Data.bin", b"hello").is_empty());
        assert!(!problems("Big.rs", &vec![b'a'; 600 * 1024]).is_empty());
    }

    #[test]
    fn rejects_data_hidden_in_text() {
        let blob = format!("const DATA: &str = \"{}\";", "A1".repeat(400));
        assert!(!problems("Source/Blob.rs", blob.as_bytes()).is_empty());
    }
}
