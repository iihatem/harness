//! The `apply_patch` tool: all or nothing, the same guards as `write` and `edit`, and the
//! checkpoint.

use std::{path::Path, sync::Arc};

use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    checkpoint::Checkpoints,
    engine::{EngineConfig, PermissionEngine},
    event::AgentEvent,
    permission::{Action, Decision, Mode, PermissionPolicy},
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
};
use harness_tools::ApplyPatchTool;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn setup() -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    (dir, ctx)
}

fn put(ctx: &ToolContext, name: &str, text: &str) {
    std::fs::write(ctx.workspace.join(name), text).unwrap();
}

fn read(ctx: &ToolContext, name: &str) -> String {
    std::fs::read_to_string(ctx.workspace.join(name)).unwrap()
}

/// Marks `name` as read, as the `read` tool does.
fn mark_read(ctx: &ToolContext, name: &str) {
    let path = ctx.workspace.join(name);
    ctx.tracker.record(&path, &std::fs::read(&path).unwrap());
}

async fn apply(ctx: &ToolContext, patch: &str) -> ToolOutput {
    ApplyPatchTool.run(json!({"input": patch}), ctx).await
}

fn wrap(body: &str) -> String {
    format!("*** Begin Patch\n{body}*** End Patch\n")
}

// Spec "Update and add in one patch": both applied, the result lists the two files.
#[tokio::test]
async fn an_update_and_an_add_are_applied_together_and_listed() {
    let (_dir, ctx) = setup();
    std::fs::create_dir_all(ctx.workspace.join("src")).unwrap();
    put(&ctx, "src/a.rs", "fn a() {\n    1\n}\n");
    mark_read(&ctx, "src/a.rs");
    let out = apply(
        &ctx,
        &wrap("*** Update File: src/a.rs\n@@\n fn a() {\n-    1\n+    2\n*** Add File: src/b.rs\n+fn b() {}\n"),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(read(&ctx, "src/a.rs"), "fn a() {\n    2\n}\n");
    assert_eq!(read(&ctx, "src/b.rs"), "fn b() {}\n");
    assert!(
        out.content.contains("src/a.rs") && out.content.contains("src/b.rs"),
        "{}",
        out.content
    );
}

// Spec "Context does not match": neither hunk is applied.
#[tokio::test]
async fn a_second_hunk_that_does_not_match_leaves_the_first_unapplied() {
    let (_dir, ctx) = setup();
    put(&ctx, "a.rs", "one\ntwo\n");
    put(&ctx, "other.rs", "keep\n");
    mark_read(&ctx, "a.rs");
    mark_read(&ctx, "other.rs");
    let out = apply(
        &ctx,
        &wrap("*** Update File: other.rs\n@@\n-keep\n+kept\n*** Update File: a.rs\n@@\n-one\n+ONE\n@@\n-absent\n+X\n"),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.content.contains("a.rs") && out.content.contains("hunk 2"),
        "{}",
        out.content
    );
    assert_eq!(read(&ctx, "a.rs"), "one\ntwo\n");
    assert_eq!(read(&ctx, "other.rs"), "keep\n");
}

// Spec "File not read": rejected as `edit` would be.
#[tokio::test]
async fn updating_a_file_that_was_not_read_is_rejected() {
    let (_dir, ctx) = setup();
    put(&ctx, "a.rs", "x\n");
    let out = apply(&ctx, &wrap("*** Update File: a.rs\n@@\n-x\n+y\n")).await;
    assert!(out.is_error);
    assert!(out.content.contains("has not been read"), "{}", out.content);
    assert_eq!(read(&ctx, "a.rs"), "x\n");
}

#[tokio::test]
async fn a_file_that_changed_since_it_was_read_is_rejected() {
    let (_dir, ctx) = setup();
    put(&ctx, "a.rs", "x\n");
    mark_read(&ctx, "a.rs");
    put(&ctx, "a.rs", "x changed\n");
    let out = apply(&ctx, &wrap("*** Update File: a.rs\n@@\n-x changed\n+y\n")).await;
    assert!(out.is_error);
    assert!(out.content.contains("changed on disk"), "{}", out.content);
}

#[tokio::test]
async fn deleting_a_file_needs_it_read_too_and_removes_it() {
    let (_dir, ctx) = setup();
    put(&ctx, "old.txt", "bye\n");
    let out = apply(&ctx, &wrap("*** Delete File: old.txt\n")).await;
    assert!(out.is_error && ctx.workspace.join("old.txt").exists());
    mark_read(&ctx, "old.txt");
    let out = apply(&ctx, &wrap("*** Delete File: old.txt\n")).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(!ctx.workspace.join("old.txt").exists());
    assert!(out.content.contains("old.txt"));
}

#[tokio::test]
async fn adding_a_file_that_exists_is_an_error_and_a_new_directory_is_made() {
    let (_dir, ctx) = setup();
    put(&ctx, "a.txt", "x\n");
    let out = apply(&ctx, &wrap("*** Add File: a.txt\n+y\n")).await;
    assert!(
        out.is_error && out.content.contains("already exists"),
        "{}",
        out.content
    );
    assert_eq!(read(&ctx, "a.txt"), "x\n");
    let out = apply(&ctx, &wrap("*** Add File: deep/er/new.txt\n+hello\n")).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(read(&ctx, "deep/er/new.txt"), "hello\n");
}

#[tokio::test]
async fn a_move_writes_the_new_path_and_removes_the_old_one() {
    let (_dir, ctx) = setup();
    put(&ctx, "a.rs", "x\n");
    mark_read(&ctx, "a.rs");
    let out = apply(
        &ctx,
        &wrap("*** Update File: a.rs\n*** Move to: b/c.rs\n@@\n-x\n+y\n"),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(!ctx.workspace.join("a.rs").exists());
    assert_eq!(read(&ctx, "b/c.rs"), "y\n");
}

#[tokio::test]
async fn a_move_onto_an_existing_file_is_an_error() {
    let (_dir, ctx) = setup();
    put(&ctx, "a.rs", "x\n");
    put(&ctx, "b.rs", "other\n");
    mark_read(&ctx, "a.rs");
    let out = apply(
        &ctx,
        &wrap("*** Update File: a.rs\n*** Move to: b.rs\n@@\n-x\n+y\n"),
    )
    .await;
    assert!(out.is_error);
    assert_eq!(read(&ctx, "b.rs"), "other\n");
    assert_eq!(read(&ctx, "a.rs"), "x\n");
}

// Spec "malformed patches leaving files untouched".
#[tokio::test]
async fn a_malformed_patch_changes_nothing() {
    let (_dir, ctx) = setup();
    put(&ctx, "a.rs", "x\n");
    mark_read(&ctx, "a.rs");
    for text in [
        "not a patch".to_string(),
        "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n".to_string(),
        wrap("*** Update File: a.rs\n@@\n-x\n+y\n*** Add File: b.rs\nbroken\n"),
    ] {
        let out = apply(&ctx, &text).await;
        assert!(out.is_error, "{text}");
        assert_eq!(read(&ctx, "a.rs"), "x\n");
        assert!(!ctx.workspace.join("b.rs").exists());
    }
}

// A write that fails partway puts back what it had written.
#[tokio::test]
async fn a_write_that_fails_partway_is_rolled_back() {
    let (_dir, ctx) = setup();
    put(&ctx, "a.rs", "x\n");
    mark_read(&ctx, "a.rs");
    // `blocked` is a file, so a file below it cannot be created.
    put(&ctx, "blocked", "i am a file\n");
    let out = apply(
        &ctx,
        &wrap("*** Update File: a.rs\n@@\n-x\n+y\n*** Add File: blocked/new.txt\n+z\n"),
    )
    .await;
    assert!(out.is_error, "{}", out.content);
    assert_eq!(read(&ctx, "a.rs"), "x\n");
}

// What `read` shows is what the patch is matched against: a patch applied updates the read
// record, so the next patch to the file needs no new read.
#[tokio::test]
async fn a_patch_keeps_the_read_record_fresh() {
    let (_dir, ctx) = setup();
    put(&ctx, "a.rs", "1\n");
    mark_read(&ctx, "a.rs");
    assert!(
        !apply(&ctx, &wrap("*** Update File: a.rs\n@@\n-1\n+2\n"))
            .await
            .is_error
    );
    assert!(
        !apply(&ctx, &wrap("*** Update File: a.rs\n@@\n-2\n+3\n"))
            .await
            .is_error
    );
    assert_eq!(read(&ctx, "a.rs"), "3\n");
}

#[tokio::test]
async fn the_tool_reports_the_files_it_changed_but_not_the_ones_it_deleted() {
    let (_dir, ctx) = setup();
    let args = json!({"input": wrap("*** Update File: a.rs\n*** Move to: m.rs\n@@\n-1\n+2\n*** Add File: b.rs\n+x\n*** Delete File: gone.rs\n")});
    let paths = ApplyPatchTool.changed_paths(&args, &ctx);
    assert_eq!(
        paths,
        [ctx.workspace.join("m.rs"), ctx.workspace.join("b.rs")]
    );
}

fn engine(mode: Mode, ws: &Path) -> PermissionEngine {
    PermissionEngine::new(EngineConfig {
        mode,
        workspace: ws.to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: true,
        writes_need_approval: false,
    })
}

// Spec "Path outside the workspace": every path is checked as `write` checks it.
#[tokio::test]
async fn every_path_in_a_patch_is_a_write_for_the_permission_check() {
    let (dir, ctx) = setup();
    let args = json!({"input": wrap("*** Add File: src/b.rs\n+x\n*** Add File: /etc/x\n+y\n")});
    let actions = ApplyPatchTool.actions(&args, &ctx);
    assert_eq!(
        actions,
        [
            Action::Write(ctx.workspace.join("src/b.rs")),
            Action::Write(ctx.resolve("/etc/x"))
        ]
    );
    let policy = engine(Mode::Auto, dir.path());
    assert_eq!(policy.check(&actions[0]), Decision::Allow);
    assert!(matches!(
        policy.check(&actions[1]),
        Decision::Ask(_) | Decision::Deny(_)
    ));
}

fn script(patch: &str) -> Arc<MockProvider> {
    MockProvider::new(vec![
        Script::tool_call("p1", "apply_patch", json!({"input": patch})),
        Script::text("done"),
    ])
}

async fn run_agent(agent: &mut Agent) -> Vec<AgentEvent> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    agent
        .run_turn("patch it".to_string(), &tx, CancellationToken::new())
        .await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    events
}

fn agent(dir: &Path, mode: Mode, provider: Arc<MockProvider>) -> Agent {
    Agent::new(
        provider,
        ToolRegistry::new(vec![Arc::new(ApplyPatchTool)]),
        Arc::new(engine(mode, dir)),
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m", "m", "system", dir.join(".spill")),
        ToolContext::new(dir),
    )
}

fn finished(events: &[AgentEvent]) -> (String, bool) {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCallFinished {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap()
}

// Spec "Path outside the workspace" through the agent: not run without approval.
#[tokio::test]
async fn a_patch_adding_a_path_outside_the_workspace_is_not_run_without_approval() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("x");
    let mut agent = agent(
        dir.path(),
        Mode::Auto,
        script(&wrap(&format!("*** Add File: {}\n+y\n", target.display()))),
    );
    let events = run_agent(&mut agent).await;
    assert!(finished(&events).1);
    assert!(!target.exists());
}

// Spec: "its changes MUST be covered by the turn's checkpoint".
#[tokio::test]
async fn a_patch_is_covered_by_the_turns_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let ws = base.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "before\n").unwrap();
    let checkpoints = Arc::new(Checkpoints::open(&base.join("data/cp.git"), &ws, "s1").unwrap());
    let provider = MockProvider::new(vec![
        Script::tool_call("r1", "read", json!({"path": "a.txt"})),
        Script::tool_call(
            "p1",
            "apply_patch",
            json!({"input": wrap("*** Update File: a.txt\n@@\n-before\n+after\n*** Add File: new.txt\n+n\n")}),
        ),
        Script::text("done"),
    ]);
    let mut agent = Agent::new(
        provider,
        ToolRegistry::new(vec![
            Arc::new(harness_tools::ReadTool),
            Arc::new(ApplyPatchTool),
        ]),
        Arc::new(engine(Mode::Auto, &ws)),
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m", "m", "system", base.join("out")),
        ToolContext::new(&ws),
    )
    .with_checkpoints(Some(checkpoints.clone()));
    let events = run_agent(&mut agent).await;
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "after\n"
    );
    let commit = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::CheckpointCreated { commit } => Some(commit.clone()),
            _ => None,
        })
        .expect("a checkpoint before the first change");
    checkpoints.restore(&commit).unwrap();
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "before\n"
    );
    assert!(!ws.join("new.txt").exists());
}

#[test]
fn the_definition_names_the_format_and_stays_small() {
    let spec = ApplyPatchTool.spec();
    assert_eq!(spec.name, "apply_patch");
    assert!(spec.description.contains("*** Begin Patch"));
    let size = serde_json::to_string(&spec).unwrap().len() / 4;
    assert!(size < 450, "~{size} tokens");
    let Value::Object(props) = &spec.parameters["properties"] else {
        panic!()
    };
    assert!(props.contains_key("input"));
}
