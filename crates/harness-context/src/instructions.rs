//! Instruction files: the global `AGENTS.md` in the harness config directory, then, in each
//! directory from the discovery root down to the working directory, that directory's `AGENTS.md`
//! (or its `CLAUDE.md` when there is no `AGENTS.md`), with `@path` import lines expanded.

use std::{
    collections::HashSet,
    io::Read,
    path::{Path, PathBuf},
};

use crate::project::{discovery_root, repo_root};

/// Imports nest at most this deep; a top-level instruction file is depth 0.
pub const MAX_IMPORT_DEPTH: usize = 5;
/// Bytes read from one instruction file; the rest is dropped with a warning.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;

/// One loaded instruction file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    /// The file where it was found (not resolved through symlinks).
    pub path: PathBuf,
    /// Its text, with imports expanded in place.
    pub content: String,
}

/// The instruction files for a session, and what went wrong loading them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instructions {
    /// From the most general (the global file) to the most specific (the working directory).
    pub files: Vec<InstructionFile>,
    pub warnings: Vec<String>,
}

/// Loads the instruction files for a session in `cwd`. `config_dir` is the harness config
/// directory; `home` is the user's home directory, which is the discovery root outside a
/// repository. Missing, unreadable or disallowed files and imports produce warnings, never errors.
pub fn discover(cwd: &Path, config_dir: &Path, home: Option<&Path>) -> Instructions {
    let root = discovery_root(cwd, home);
    let mut loader = Loader {
        seen: HashSet::new(),
        warnings: Vec::new(),
        home: home.map(Path::to_path_buf),
        in_repository: repo_root(cwd).is_some(),
        root: canonical(&root),
        config_dir: canonical(config_dir),
    };
    let mut files = Vec::new();
    if let Some(file) = loader.top_level(&config_dir.join("AGENTS.md"), true) {
        files.push(file);
    }
    let dirs: Vec<&Path> = cwd
        .ancestors()
        .take_while(|dir| dir.starts_with(&root))
        .collect();
    for dir in dirs.into_iter().rev() {
        let agents = dir.join("AGENTS.md");
        let claude = dir.join("CLAUDE.md");
        let path = if agents.exists() {
            agents
        } else if claude.exists() {
            claude
        } else {
            continue;
        };
        if let Some(file) = loader.top_level(&path, false) {
            files.push(file);
        }
    }
    Instructions {
        files,
        warnings: loader.warnings,
    }
}

/// `path` resolved through symlinks, or unchanged when that fails (it then matches nothing).
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The target of an import line: a line whose trimmed text is `@` followed by a path without
/// spaces.
fn import_target(line: &str) -> Option<&str> {
    let target = line.trim().strip_prefix('@')?;
    (!target.is_empty() && !target.contains(char::is_whitespace)).then_some(target)
}

/// Whether `line` opens or closes a fenced code block.
fn is_fence(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

struct Loader {
    /// Canonical paths already included, so each file appears at most once.
    seen: HashSet<PathBuf>,
    warnings: Vec<String>,
    home: Option<PathBuf>,
    in_repository: bool,
    /// The discovery root and the config directory, resolved.
    root: PathBuf,
    config_dir: PathBuf,
}

/// What an import line becomes.
enum Import {
    /// The imported file's expanded text.
    Content(String),
    /// Nothing: the file is already included.
    Duplicate,
    /// The line itself, unchanged (the import failed and a warning says why).
    Keep,
}

impl Loader {
    /// Loads one discovered file. Project files must resolve inside the discovery root or the
    /// config directory; the global file is the user's own and may link anywhere.
    fn top_level(&mut self, path: &Path, global: bool) -> Option<InstructionFile> {
        if !path.exists() {
            return None;
        }
        let real = canonical(path);
        if !global && !real.starts_with(&self.root) && !real.starts_with(&self.config_dir) {
            self.warnings.push(format!(
                "skipped {}: it links to {}, outside the project",
                path.display(),
                real.display()
            ));
            return None;
        }
        if !self.seen.insert(real.clone()) {
            return None;
        }
        let text = self.read(path, &real)?;
        // In a repository, imports may reach anywhere in it. Outside one, the discovery root can
        // be the home directory, so imports stay in the importing file's own directory.
        let scope = if self.in_repository {
            vec![self.root.clone(), self.config_dir.clone()]
        } else if global {
            vec![self.config_dir.clone()]
        } else {
            let own_dir = real.parent().unwrap_or(&real).to_path_buf();
            vec![own_dir, self.config_dir.clone()]
        };
        let base = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let content = self.expand(&text, path, &base, 1, &scope);
        Some(InstructionFile {
            path: path.to_path_buf(),
            content,
        })
    }

    /// Reads a regular file, at most [`MAX_FILE_BYTES`] of it.
    fn read(&mut self, path: &Path, real: &Path) -> Option<String> {
        if !std::fs::metadata(real).is_ok_and(|m| m.is_file()) {
            self.warnings
                .push(format!("skipped {}: not a regular file", path.display()));
            return None;
        }
        let mut bytes = Vec::new();
        let read = std::fs::File::open(real)
            .and_then(|f| f.take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut bytes));
        if let Err(e) = read {
            self.warnings
                .push(format!("cannot read {}: {e}", path.display()));
            return None;
        }
        if bytes.len() > MAX_FILE_BYTES {
            bytes.truncate(MAX_FILE_BYTES);
            self.warnings.push(format!(
                "{} is larger than {MAX_FILE_BYTES} bytes; only the start is used",
                path.display()
            ));
        }
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// `text` with each import line outside code fences replaced by what it imports.
    fn expand(
        &mut self,
        text: &str,
        from: &Path,
        base: &Path,
        depth: usize,
        scope: &[PathBuf],
    ) -> String {
        let mut out = String::with_capacity(text.len());
        let mut in_fence = false;
        for line in text.split_inclusive('\n') {
            if is_fence(line) {
                in_fence = !in_fence;
            } else if !in_fence && let Some(target) = import_target(line) {
                match self.import(target, from, base, depth, scope) {
                    Import::Content(content) => {
                        out.push_str(&content);
                        if !content.is_empty() && !content.ends_with('\n') {
                            out.push('\n');
                        }
                        continue;
                    }
                    Import::Duplicate => continue,
                    Import::Keep => {}
                }
            }
            out.push_str(line);
        }
        out
    }

    fn import(
        &mut self,
        target: &str,
        from: &Path,
        base: &Path,
        depth: usize,
        scope: &[PathBuf],
    ) -> Import {
        if depth > MAX_IMPORT_DEPTH {
            self.warnings.push(format!(
                "skipped import @{target} in {}: imports nest at most {MAX_IMPORT_DEPTH} levels deep",
                from.display()
            ));
            return Import::Keep;
        }
        let path = match (target.strip_prefix("~/"), &self.home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => base.join(target),
        };
        let Ok(real) = path.canonicalize() else {
            self.warnings.push(format!(
                "skipped import @{target} in {}: {} does not exist",
                from.display(),
                path.display()
            ));
            return Import::Keep;
        };
        if !scope.iter().any(|allowed| real.starts_with(allowed)) {
            self.warnings.push(format!(
                "skipped import @{target} in {}: {} is outside the project and the harness config directory",
                from.display(),
                real.display()
            ));
            return Import::Keep;
        }
        if !self.seen.insert(real.clone()) {
            return Import::Duplicate;
        }
        let Some(text) = self.read(&path, &real) else {
            return Import::Keep;
        };
        let base = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        Import::Content(self.expand(&text, &path, &base, depth + 1, scope))
    }
}
