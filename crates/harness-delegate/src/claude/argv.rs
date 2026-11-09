//! The `claude -p` command line, from harness's mode and the request.
//!
//! Every flag here is in `claude --help` (2.1.312) except `--permission-prompt-tool`, which is
//! hidden there; the binary accepts the value `stdio` for it (the Agent SDK's path: permission
//! prompts arrive as `can_use_tool` control requests on the same stream). The task is never on
//! the command line, and neither is a key.

use std::{ffi::OsString, path::Path};

use harness_core::permission::Mode;

use crate::{DelegateError, StartRequest};

/// What harness's permission engine alone decides: the child has no allow, ask or deny rules of
/// its own (and `--setting-sources=` loads no settings file that could add any).
const SETTINGS: &str = r#"{"permissions":{"allow":[],"ask":[],"deny":[]}}"#;

/// A value that goes on the command line as its own argument: no leading `-` (the program's
/// parser might take it for a flag) and no character a shell, a path or an id would not hold.
fn checked(field: &'static str, value: &str) -> Result<(), DelegateError> {
    let ok = !value.is_empty()
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/@+=-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(DelegateError::InvalidValue {
            field,
            value: value.to_string(),
        })
    }
}

/// The arguments for `claude`, without the program name, and no instructions file.
pub fn build(request: &StartRequest) -> Result<Vec<OsString>, DelegateError> {
    build_with(request, None)
}

/// The arguments for `claude`; `instructions` is the file `--append-system-prompt-file` names.
pub fn build_with(
    request: &StartRequest,
    instructions: Option<&Path>,
) -> Result<Vec<OsString>, DelegateError> {
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
        // No user, project or local settings: harness is the only approver. Claude then very
        // likely reads no CLAUDE.md either (static evidence, 2.1.312): the instructions harness
        // loads come in with `--append-system-prompt-file` (`StartRequest::instructions`).
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
        checked("model", model)?;
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(session) = &request.resume {
        checked("resume", session)?;
        args.extend(["--resume".into(), session.into()]);
    }
    if let Some(file) = instructions {
        args.extend([
            "--append-system-prompt-file".into(),
            file.as_os_str().to_owned(),
        ]);
    }
    for dir in &request.extra_dirs {
        // A directory is an argument of a variadic flag: one named `-x` would be read as a flag.
        if dir.to_string_lossy().starts_with('-') {
            return Err(DelegateError::InvalidValue {
                field: "add-dir",
                value: dir.display().to_string(),
            });
        }
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
