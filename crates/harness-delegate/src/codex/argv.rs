//! The `codex app-server` command line. It speaks JSON-RPC on stdio (the default transport,
//! named explicitly); the sandbox, the approval policy, the model and the task all go in the
//! messages, never on the command line. The one thing that does is the switching off of the
//! user's own MCP servers (`-c`), which codex would otherwise start from `~/.codex/config.toml`.

use std::ffi::OsString;

/// A TOML basic string (a JSON string is one, for the characters a server name holds).
fn quoted(name: &str) -> String {
    serde_json::to_string(name).expect("a string serializes")
}

/// The command line; `disabled_mcp` are the names of the user's MCP servers to switch off. A
/// whole-table override (`mcp_servers={}`) would only be merged into the user's, so each is
/// named, in one inline table that also takes a name with a dot or a space.
pub fn build(disabled_mcp: &[String]) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["app-server", "--listen", "stdio://"]
        .map(OsString::from)
        .to_vec();
    if !disabled_mcp.is_empty() {
        let entries: Vec<String> = disabled_mcp
            .iter()
            .map(|name| format!("{}={{enabled=false}}", quoted(name)))
            .collect();
        args.push("-c".into());
        args.push(format!("mcp_servers={{{}}}", entries.join(",")).into());
    }
    args
}
