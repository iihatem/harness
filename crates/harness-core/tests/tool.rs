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
        limit_output("hello", DEFAULT_OUTPUT_LIMIT, dir.path(), "c1"),
        "hello"
    );
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
}

#[test]
fn large_output_is_spilled_to_a_file_with_head_and_tail() {
    let dir = tempfile::tempdir().unwrap();
    let content: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
    let limited = limit_output(&content, 1000, dir.path(), "call/7");
    assert!(limited.starts_with("line 1\n"));
    assert!(limited.trim_end().ends_with("line 5000"));
    assert!(limited.contains("omitted"));
    let saved = dir.path().join("call_7.txt");
    assert!(limited.contains(&saved.display().to_string()), "{limited}");
    assert_eq!(std::fs::read_to_string(saved).unwrap(), content);
    assert!(limited.len() < 1300);
}

#[test]
fn limiting_never_splits_a_multibyte_character() {
    let dir = tempfile::tempdir().unwrap();
    let content = "é".repeat(5000);
    let limited = limit_output(&content, 999, dir.path(), "u");
    assert!(limited.contains("omitted"));
}
