//! Verification gates: the settings (`[gates]`) and the commands harness proposes for a workspace.

use std::path::Path;

/// The longest a gate command may be given: what the bash tool allows any command.
pub const MAX_TIMEOUT_S: u64 = 600;

/// The effective `[gates]` settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gates {
    /// A lint command run after each successful edit.
    pub after_edit: Option<String>,
    /// A test command run when a turn that changed files ends.
    pub test: Option<String>,
    pub timeout_s: u64,
    /// How many times a failed test may continue the turn.
    pub max_retries: u32,
    /// How many lines of a failing command's output the model gets.
    pub output_tail_lines: usize,
}

impl Default for Gates {
    fn default() -> Self {
        Gates {
            after_edit: None,
            test: None,
            timeout_s: 300,
            max_retries: 3,
            output_tail_lines: 60,
        }
    }
}

impl Gates {
    /// Whether any gate command is set.
    pub fn is_configured(&self) -> bool {
        self.after_edit.is_some() || self.test.is_some()
    }
}

/// The gate commands harness proposes for a workspace, from the files in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// The file the proposal comes from.
    pub source: &'static str,
    pub after_edit: Option<String>,
    pub test: Option<String>,
}

impl Proposal {
    /// One line saying what would run and when.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(command) = &self.after_edit {
            parts.push(format!("`{command}` after each edit"));
        }
        if let Some(command) = &self.test {
            parts.push(format!("`{command}` when a turn that changed files ends"));
        }
        format!("{}: run {}", self.source, parts.join(", and "))
    }
}

/// The text `npm init` writes as the test script: it only fails, so it is no gate.
const NPM_PLACEHOLDER: &str = "no test specified";

/// The commands to propose for the project in `workspace`, from the first of `Cargo.toml`,
/// `package.json`, `pyproject.toml` (when it uses pytest) and `go.mod` that gives any.
pub fn detect(workspace: &Path) -> Option<Proposal> {
    let read = |name: &str| std::fs::read_to_string(workspace.join(name)).ok();
    if workspace.join("Cargo.toml").is_file() {
        return Some(Proposal {
            source: "Cargo.toml",
            after_edit: None,
            test: Some("cargo test".into()),
        });
    }
    if let Some(proposal) = read("package.json").and_then(|text| npm_scripts(&text)) {
        return Some(proposal);
    }
    if read("pyproject.toml").is_some_and(|text| text.contains("pytest")) {
        return Some(Proposal {
            source: "pyproject.toml",
            after_edit: None,
            test: Some("pytest".into()),
        });
    }
    if workspace.join("go.mod").is_file() {
        return Some(Proposal {
            source: "go.mod",
            after_edit: None,
            test: Some("go test ./...".into()),
        });
    }
    None
}

/// `package.json`'s `test` and `lint` scripts, as the `npm` commands that run them.
fn npm_scripts(text: &str) -> Option<Proposal> {
    let json: serde_json::Value = serde_json::from_str(text).ok()?;
    let scripts = json.get("scripts")?.as_object()?;
    let script = |name: &str| scripts.get(name).and_then(|s| s.as_str());
    let test = script("test")
        .filter(|body| !body.contains(NPM_PLACEHOLDER))
        .map(|_| "npm test".to_string());
    let after_edit = script("lint").map(|_| "npm run lint".to_string());
    if test.is_none() && after_edit.is_none() {
        return None;
    }
    Some(Proposal {
        source: "package.json",
        after_edit,
        test,
    })
}
