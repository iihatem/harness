//! What a subagent is: a name, a description the model reads to choose it, the tools and mode it
//! gets, optionally its own model and step limit, and instructions. Definitions come from files
//! (found by `harness-context`) and from the built-ins here; a file with a built-in's name
//! replaces it.

use std::path::PathBuf;

use crate::permission::Mode;

/// The tools that can change files. A definition with a shell is not read-only either.
const WRITE_TOOLS: [&str; 4] = ["write", "edit", "apply_patch", "hashline_edit"];

/// Where a definition came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionSource {
    /// Built into harness.
    Builtin,
    /// A file in the project: it comes with the repository, so what it asks for that costs money
    /// or reaches beyond the session (its model) needs the workspace to be trusted.
    Project(PathBuf),
    /// A file of the user's own.
    User(PathBuf),
}

impl DefinitionSource {
    /// The file it came from, for listings; `built-in` for the built-ins.
    pub fn label(&self) -> String {
        match self {
            DefinitionSource::Builtin => "built-in".into(),
            DefinitionSource::Project(path) | DefinitionSource::User(path) => {
                path.display().to_string()
            }
        }
    }
}

/// A subagent the model can start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    /// What the model names it by.
    pub name: String,
    /// What the model reads to choose it.
    pub description: String,
    /// The tools it is offered, by harness name; `None` is all of its parent's (the spawn tool
    /// never among them).
    pub tools: Option<Vec<String>>,
    /// The `<provider>/<model>` it runs on, when it names one that is used.
    pub model: Option<String>,
    /// Its mode. Never wider than its parent's: a child's permissions are narrowed from them.
    pub mode: Option<Mode>,
    pub max_steps: Option<u32>,
    /// Kept for worktree isolation (`worktree`); no effect yet.
    pub isolation: Option<String>,
    /// Kept for background agents; no effect yet.
    pub background: Option<bool>,
    /// Its instructions: what the file says after its frontmatter.
    pub body: String,
    pub source: DefinitionSource,
    /// The model a project file names that is not used because the workspace is not trusted.
    pub model_ignored: Option<String>,
}

impl AgentDefinition {
    /// A built-in with all tools, in no particular mode.
    pub fn new(name: &str, description: &str, body: &str) -> Self {
        AgentDefinition {
            name: name.into(),
            description: description.into(),
            tools: None,
            model: None,
            mode: None,
            max_steps: None,
            isolation: None,
            background: None,
            body: body.into(),
            source: DefinitionSource::Builtin,
            model_ignored: None,
        }
    }

    /// Whether it can change nothing: its mode is plan or read-only, or it lists tools and none
    /// of them writes (a shell can). Such an agent does helper work, so it runs on the
    /// `background` role's model unless it names one.
    pub fn is_read_only(&self) -> bool {
        if matches!(self.mode, Some(Mode::Plan | Mode::ReadOnly)) {
            return true;
        }
        self.tools.as_ref().is_some_and(|tools| {
            !tools
                .iter()
                .any(|t| t == "bash" || WRITE_TOOLS.contains(&t.as_str()))
        })
    }
}

/// `names` as harness's tool names, and the names that match no tool. Harness's names and
/// Claude's (`Read`, `Grep`, `Glob`, `Bash`, `Write`, `Edit`, `MultiEdit`, `NotebookEdit`) both
/// work, in any case; a name that can write selects every tool that can, since which of them a
/// model has depends on its edit format. No name repeats.
pub fn normalize_tools(names: &[String]) -> (Vec<String>, Vec<String>) {
    let mut tools: Vec<String> = Vec::new();
    let mut unknown = Vec::new();
    let add = |tools: &mut Vec<String>, name: &str| {
        if !tools.iter().any(|t| t == name) {
            tools.push(name.to_string());
        }
    };
    for name in names {
        match name.to_ascii_lowercase().as_str() {
            "read" => add(&mut tools, "read"),
            "grep" => add(&mut tools, "grep"),
            "glob" => add(&mut tools, "glob"),
            "bash" => add(&mut tools, "bash"),
            "write" | "edit" | "multiedit" | "notebookedit" | "apply_patch" | "hashline_edit" => {
                for tool in WRITE_TOOLS {
                    add(&mut tools, tool);
                }
            }
            _ => unknown.push(name.clone()),
        }
    }
    (tools, unknown)
}

const EXPLORE: &str = "You are a read-only research agent. Find what the task asks about by searching and reading: files, definitions, call sites, history. You cannot change anything, so do not try.\n\nFinish with a short report: what you found, with file paths and line numbers, and what you could not find. Say plainly when you are unsure.";

const REVIEW: &str = "You are a code reviewer. Read the change or the code the task names, and look for bugs, missed cases, broken assumptions and things that will be hard to maintain. You cannot change anything.\n\nFinish with a report: each problem with its file and line, why it is a problem, and how serious it is. Say so plainly when you find none, and say what you checked.";

const GENERAL: &str = "";

/// The agents harness ships: `explore` and `review` read only; `general` can do anything its
/// parent can.
pub fn builtins() -> Vec<AgentDefinition> {
    let readers = |name: &str, description: &str, body: &str| AgentDefinition {
        tools: Some(vec![
            "read".into(),
            "grep".into(),
            "glob".into(),
            "bash".into(),
        ]),
        mode: Some(Mode::ReadOnly),
        ..AgentDefinition::new(name, description, body)
    };
    vec![
        readers(
            "explore",
            "Read-only research: searches and reads the code and reports what it found. Cheap and quick; use it for broad searches and questions about the codebase.",
            EXPLORE,
        ),
        readers(
            "review",
            "Read-only reviewer: reads a change or some code and reports problems, with file and line.",
            REVIEW,
        ),
        AgentDefinition::new(
            "general",
            "A helper with all tools, for a self-contained task that may change files.",
            GENERAL,
        ),
    ]
}

/// The agents the model may start: the built-ins and those found in files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Definitions {
    list: Vec<AgentDefinition>,
}

impl Definitions {
    /// The built-ins, a file of the same name in a built-in's place, and the rest after them by
    /// name. Of several files with one name the first wins (callers pass them in order of
    /// precedence).
    pub fn new(found: Vec<AgentDefinition>) -> Self {
        let mut chosen: Vec<AgentDefinition> = Vec::new();
        for def in found {
            if !chosen.iter().any(|c| c.name == def.name) {
                chosen.push(def);
            }
        }
        let mut list = Vec::new();
        for builtin in builtins() {
            match chosen.iter().position(|c| c.name == builtin.name) {
                Some(at) => list.push(chosen.remove(at)),
                None => list.push(builtin),
            }
        }
        chosen.sort_by(|a, b| a.name.cmp(&b.name));
        list.extend(chosen);
        Definitions { list }
    }

    /// With a project definition's `model` kept only when `trusted`; otherwise it is set aside in
    /// `model_ignored`, to be said where the definition is shown or used.
    pub fn with_trust(mut self, trusted: bool) -> Self {
        if !trusted {
            for def in &mut self.list {
                if matches!(def.source, DefinitionSource::Project(_))
                    && let Some(model) = def.model.take()
                {
                    def.model_ignored = Some(model);
                }
            }
        }
        self
    }

    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.list.iter().find(|d| d.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &AgentDefinition> {
        self.list.iter()
    }

    pub fn names(&self) -> Vec<&str> {
        self.list.iter().map(|d| d.name.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::permission::Mode;

    fn file(name: &str, source: DefinitionSource) -> AgentDefinition {
        AgentDefinition {
            name: name.into(),
            description: format!("{name} from a file"),
            source,
            ..AgentDefinition::new(name, "d", "")
        }
    }

    #[test]
    fn the_built_ins_are_explore_review_and_general() {
        let all = Definitions::new(Vec::new());
        let names: Vec<&str> = all.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["explore", "review", "general"]);
        assert!(all.get("explore").unwrap().is_read_only());
        assert!(all.get("review").unwrap().is_read_only());
        let general = all.get("general").unwrap();
        assert!(!general.is_read_only());
        assert_eq!(general.tools, None);
        assert_eq!(general.mode, None);
        assert_eq!(all.get("explore").unwrap().mode, Some(Mode::ReadOnly));
        assert_eq!(
            all.get("explore").unwrap().source,
            DefinitionSource::Builtin
        );
    }

    #[test]
    fn a_file_with_a_built_ins_name_replaces_it_in_the_same_place() {
        let mine = file(
            "explore",
            DefinitionSource::User(PathBuf::from("/u/explore.md")),
        );
        let all = Definitions::new(vec![mine, file("zeta", DefinitionSource::Builtin)]);
        let names: Vec<&str> = all.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["explore", "review", "general", "zeta"]);
        assert_eq!(
            all.get("explore").unwrap().source,
            DefinitionSource::User(PathBuf::from("/u/explore.md"))
        );
    }

    #[test]
    fn the_first_file_of_a_name_wins_and_others_are_sorted_by_name() {
        let first = AgentDefinition {
            description: "first".into(),
            ..file("b", DefinitionSource::Project(PathBuf::from("/p/b.md")))
        };
        let second = AgentDefinition {
            description: "second".into(),
            ..file("b", DefinitionSource::User(PathBuf::from("/u/b.md")))
        };
        let all = Definitions::new(vec![file("c", DefinitionSource::Builtin), first, second]);
        let names: Vec<&str> = all.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["explore", "review", "general", "b", "c"]);
        assert_eq!(all.get("b").unwrap().description, "first");
    }

    #[test]
    fn read_only_means_a_read_only_mode_or_tools_that_cannot_write() {
        let with = |tools: Option<&[&str]>, mode: Option<Mode>| AgentDefinition {
            tools: tools.map(|t| t.iter().map(|s| s.to_string()).collect()),
            mode,
            ..AgentDefinition::new("x", "d", "")
        };
        assert!(with(None, Some(Mode::ReadOnly)).is_read_only());
        assert!(with(None, Some(Mode::Plan)).is_read_only());
        assert!(with(Some(&["read", "grep"]), None).is_read_only());
        // A shell can write.
        assert!(!with(Some(&["read", "bash"]), None).is_read_only());
        assert!(!with(Some(&["read", "edit"]), None).is_read_only());
        assert!(!with(None, None).is_read_only());
        assert!(!with(None, Some(Mode::Auto)).is_read_only());
        // Read-only mode wins over a tool list that names a writer: it cannot write.
        assert!(with(Some(&["bash", "edit"]), Some(Mode::ReadOnly)).is_read_only());
    }

    #[test]
    fn tool_names_are_harness_names_or_claudes_in_any_case() {
        let (tools, unknown) = normalize_tools(&[
            "Read".into(),
            "GREP".into(),
            "glob".into(),
            "Bash".into(),
            "WebFetch".into(),
            "read".into(),
        ]);
        assert_eq!(tools, ["read", "grep", "glob", "bash"]);
        assert_eq!(unknown, ["WebFetch"]);
        let (tools, _) = normalize_tools(&["Edit".into()]);
        assert_eq!(tools, ["write", "edit", "apply_patch", "hashline_edit"]);
        let (tools, _) = normalize_tools(&["Write".into(), "MultiEdit".into()]);
        assert_eq!(tools, ["write", "edit", "apply_patch", "hashline_edit"]);
    }

    #[test]
    fn a_project_definitions_model_needs_trust_and_a_users_does_not() {
        let project = AgentDefinition {
            model: Some("openai/gpt-5".into()),
            ..file("p", DefinitionSource::Project(PathBuf::from("/p/p.md")))
        };
        let user = AgentDefinition {
            model: Some("openai/gpt-5".into()),
            ..file("u", DefinitionSource::User(PathBuf::from("/u/u.md")))
        };
        let untrusted = Definitions::new(vec![project.clone(), user.clone()]).with_trust(false);
        let p = untrusted.get("p").unwrap();
        assert_eq!(p.model, None);
        assert_eq!(p.model_ignored.as_deref(), Some("openai/gpt-5"));
        assert_eq!(
            untrusted.get("u").unwrap().model.as_deref(),
            Some("openai/gpt-5")
        );
        let trusted = Definitions::new(vec![project, user]).with_trust(true);
        assert_eq!(
            trusted.get("p").unwrap().model.as_deref(),
            Some("openai/gpt-5")
        );
        assert_eq!(trusted.get("p").unwrap().model_ignored, None);
    }
}
