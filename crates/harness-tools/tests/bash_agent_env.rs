use harness_core::tool::{Tool, ToolContext};
use harness_tools::BashTool;
use serde_json::json;

#[tokio::test]
async fn a_command_gets_the_variables_its_context_carries() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path()).with_env(vec![
        ("HARNESS_AGENT_ID".to_string(), "a3".to_string()),
        ("HARNESS_PORT_OFFSET".to_string(), "200".to_string()),
    ]);
    let out = BashTool
        .run(
            json!({"command": "echo \"$HARNESS_AGENT_ID:$HARNESS_PORT_OFFSET\""}),
            &ctx,
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("a3:200"), "{}", out.content);
}

#[tokio::test]
async fn a_context_without_variables_leaves_the_environment_alone() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    let out = BashTool
        .run(json!({"command": "echo \"[$HARNESS_AGENT_ID]\""}), &ctx)
        .await;
    assert!(out.content.contains("[]"), "{}", out.content);
}
