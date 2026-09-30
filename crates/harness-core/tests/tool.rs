use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use harness_core::message::ToolSpec;
use harness_core::output::{DEFAULT_OUTPUT_LIMIT, limit_output};
use harness_core::permission::Action;
use harness_core::tool::{ReadTracker, Tool, ToolContext, ToolOutput, ToolRegistry};
use serde_json::{Value, json};

struct Named(&'static str);

#[async_trait]
impl Tool for Named {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.0.into(),
            description: String::new(),
            parameters: json!({"type": "object"}),
        }
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        ToolOutput::ok(self.0)
    }
}

#[test]
fn registry_keeps_insertion_order() {
    let reg = ToolRegistry::new(vec![
        Arc::new(Named("b")),
        Arc::new(Named("a")),
        Arc::new(Named("c")),
    ]);
    let names: Vec<String> = reg.specs().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["b", "a", "c"]);
    assert!(reg.get("a").is_some());
    assert!(reg.get("zzz").is_none());
}

#[test]
fn tracker_requires_a_prior_unchanged_read() {
    let tracker = ReadTracker::default();
    let path = Path::new("/w/a.txt");
    assert!(
        tracker
            .check_fresh(path, b"v1")
            .unwrap_err()
            .contains("read it first")
    );
    tracker.record(path, b"v1");
    assert!(tracker.check_fresh(path, b"v1").is_ok());
    assert!(
        tracker
            .check_fresh(path, b"v2")
            .unwrap_err()
            .contains("changed on disk")
    );
}

#[test]
fn context_canonicalizes_the_workspace_and_resolves_relative_paths() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    assert_eq!(ctx.workspace, dir.path().canonicalize().unwrap());
    assert_eq!(ctx.resolve("src/lib.rs"), ctx.workspace.join("src/lib.rs"));
}

#[test]
fn small_output_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        limit_output("hello", DEFAULT_OUTPUT_LIMIT, dir.path(), "c1", None),
        "hello"
    );
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
}

#[test]
fn large_output_is_spilled_to_a_file_with_head_and_tail() {
    let dir = tempfile::tempdir().unwrap();
    let content: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
    let limited = limit_output(&content, 1000, dir.path(), "call/7", None);
    assert!(limited.starts_with("line 1\n"));
    assert!(limited.trim_end().ends_with("line 5000"));
    assert!(limited.contains("omitted"));
    let saved = dir.path().join("call_7.txt");
    assert!(limited.contains(&saved.display().to_string()), "{limited}");
    assert_eq!(std::fs::read_to_string(saved).unwrap(), content);
    assert!(limited.len() < 1300);
}

// Review F M3: the tool-output file holds what the session file holds, so only its owner may
// read it, as with session files.
#[test]
fn tool_output_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let spill = dir.path().join("tool-output/run-1");
    let content = "x".repeat(5000);
    limit_output(&content, 1000, &spill, "c1", None);
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir.path().join("tool-output")), 0o700);
    assert_eq!(mode(&spill), 0o700);
    assert_eq!(mode(&spill.join("c1.txt")), 0o600);
    // A file left readable by others is made private when it is written again.
    std::fs::set_permissions(spill.join("c1.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
    limit_output(&content, 1000, &spill, "c1", None);
    assert_eq!(mode(&spill.join("c1.txt")), 0o600);
    assert_eq!(
        std::fs::read_to_string(spill.join("c1.txt")).unwrap(),
        content
    );
}

#[test]
fn limiting_never_splits_a_multibyte_character() {
    let dir = tempfile::tempdir().unwrap();
    let content = "é".repeat(5000);
    let limited = limit_output(&content, 999, dir.path(), "u", None);
    assert!(limited.contains("omitted"));
}

use harness_core::permission::FsAccess;
use harness_core::tool::{CommandSandbox, GitProtection};

/// A sandbox that only implements the required methods.
#[derive(Debug)]
struct Plain;

impl CommandSandbox for Plain {
    fn name(&self) -> &'static str {
        "plain"
    }
    fn command(
        &self,
        _access: FsAccess,
        _workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<tokio::process::Command> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        Ok(cmd)
    }
    fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
        false
    }
}

#[test]
fn a_sandbox_without_a_guard_prepares_its_plain_command() {
    let prepared = Plain
        .prepare(FsAccess::WorkspaceWrite, Path::new("/w"), "echo", &["hi"])
        .unwrap();
    assert!(prepared.guard.is_none());
    let std = prepared.command.as_std();
    assert_eq!(std.get_program(), "echo");
    assert_eq!(std.get_args().collect::<Vec<_>>(), ["hi"]);
    assert_eq!(Plain.git_protection(), GitProtection::Full);
}

#[test]
fn a_sandbox_without_a_session_to_start_does_nothing_when_it_starts() {
    // The default: nothing to read from the workspace, nothing to set up.
    Plain.start_session(Path::new("/nonexistent/workspace"));
}

/// A guard that only implements `finish`.
struct Finishing;

impl harness_core::tool::CommandGuard for Finishing {
    fn finish(self: Box<Self>) -> Option<harness_core::tool::GuardReport> {
        None
    }
}

#[test]
fn a_guard_that_does_not_track_processes_ignores_the_started_command() {
    let mut guard: Box<dyn harness_core::tool::CommandGuard> = Box::new(Finishing);
    guard.started(4242);
    assert_eq!(guard.finish(), None);
}
