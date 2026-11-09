//! The `claude -p` command line, from harness's mode and the request.
//!
//! Every flag here is in `claude --help` (2.1.312) except `--permission-prompt-tool`, which is
//! hidden there; the binary accepts the value `stdio` for it (the Agent SDK's path: permission
//! prompts arrive as `can_use_tool` control requests on the same stream). The task is never on
//! the command line, and neither is a key.

use std::ffi::OsString;

use harness_core::permission::Mode;

use crate::{DelegateError, StartRequest};

/// What harness's permission engine alone decides: the child has no allow, ask or deny rules of
/// its own (and `--setting-sources=` loads no settings file that could add any).
const SETTINGS: &str = r#"{"permissions":{"allow":[],"ask":[],"deny":[]}}"#;

/// The arguments for `claude`, without the program name.
pub fn build(request: &StartRequest) -> Result<Vec<OsString>, DelegateError> {
    let mut args: Vec<OsString> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
        // No user, project or local settings: harness is the only approver (CLAUDE.md still loads).
        "--setting-sources=",
        "--settings",
        SETTINGS,
        // No MCP server starts that the user did not name to harness.
        "--strict-mcp-config",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if let Some(model) = &request.model {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(session) = &request.resume {
        args.extend(["--resume".into(), session.into()]);
    }
    match request.mode {
        Mode::Plan => args.extend(["--permission-mode".into(), "plan".into()]),
        Mode::ReadOnly => args.extend(
            [
                "--restricted",
                "--tools",
                "Read,Glob,Grep",
                "--permission-mode",
                "manual",
            ]
            .map(OsString::from),
        ),
        Mode::Ask => args.extend(["--permission-mode".into(), "manual".into()]),
        Mode::Auto => args.extend(["--permission-mode".into(), "acceptEdits".into()]),
        Mode::FullAccess => {
            if !request.full_access_confirmed {
                return Err(DelegateError::FullAccessUnconfirmed);
            }
            args.extend(
                [
                    "--permission-mode",
                    "bypassPermissions",
                    "--allow-dangerously-skip-permissions",
                ]
                .map(OsString::from),
            );
        }
    }
    // A variadic flag takes every argument after it: it goes last.
    if !request.extra_dirs.is_empty() {
        args.push("--add-dir".into());
        args.extend(
            request
                .extra_dirs
                .iter()
                .map(|d| d.clone().into_os_string()),
        );
    }
    Ok(args)
}
