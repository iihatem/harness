use std::{fmt, path::PathBuf, str::FromStr};

use serde::{Deserialize, Serialize};

/// Approval mode. See the permissions-sandbox spec for the exact semantics of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Plan,
    ReadOnly,
    Ask,
    Auto,
    FullAccess,
}

impl Mode {
    /// Modes that grant no more than `ask` does. Only these may come from an untrusted project config.
    pub fn is_narrow(self) -> bool {
        matches!(self, Mode::Plan | Mode::ReadOnly | Mode::Ask)
    }
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "plan" => Ok(Mode::Plan),
            "read-only" => Ok(Mode::ReadOnly),
            "ask" => Ok(Mode::Ask),
            "auto" => Ok(Mode::Auto),
            "full-access" => Ok(Mode::FullAccess),
            other => Err(format!(
                "unknown mode `{other}` (expected plan, read-only, ask, auto, or full-access)"
            )),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Plan => "plan",
            Mode::ReadOnly => "read-only",
            Mode::Ask => "ask",
            Mode::Auto => "auto",
            Mode::FullAccess => "full-access",
        })
    }
}

/// What a tool call is about to do, as seen by the permission policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Read(PathBuf),
    Write(PathBuf),
    Bash(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask(String),
    Deny(String),
}
