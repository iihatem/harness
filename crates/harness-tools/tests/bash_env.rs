use harness_core::tool::{Tool, ToolContext};
use harness_tools::BashTool;
use serde_json::json;

#[tokio::test]
async fn bash_env_startup_files_are_not_sourced() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("sourced");
    let script = dir.path().join("env.sh");
    std::fs::write(&script, format!("touch {}\n", marker.display())).unwrap();
    // SAFETY: this test binary contains a single test, so no other thread reads the environment.
    unsafe {
        std::env::set_var("BASH_ENV", &script);
        std::env::set_var("ENV", &script);
    }
    let ctx = ToolContext::new(dir.path());
    let out = BashTool.run(json!({"command": "true"}), &ctx).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(!marker.exists(), "BASH_ENV/ENV must not be sourced");
}
