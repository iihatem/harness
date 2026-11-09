//! The `codex app-server` command line. It speaks JSON-RPC on stdio (the default transport,
//! named explicitly); the sandbox, the approval policy, the model and the task all go in the
//! messages, never on the command line.

use std::ffi::OsString;

pub fn build() -> Vec<OsString> {
    ["app-server", "--listen", "stdio://"]
        .map(OsString::from)
        .to_vec()
}
