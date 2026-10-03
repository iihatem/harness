//! How a model edits files: one of four formats, chosen by its profile.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditFormat {
    /// The `edit` tool: replace an exact string. The default.
    #[default]
    StrReplace,
    /// The `apply_patch` tool: a V4A patch.
    ApplyPatch,
    /// `write` with complete files, small ones only.
    WholeFile,
    /// `hashline_edit`, with the lines `read` shows addressed by hash. Experimental.
    Hashline,
}

impl EditFormat {
    pub const ALL: [EditFormat; 4] = [
        EditFormat::StrReplace,
        EditFormat::ApplyPatch,
        EditFormat::WholeFile,
        EditFormat::Hashline,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EditFormat::StrReplace => "str_replace",
            EditFormat::ApplyPatch => "apply_patch",
            EditFormat::WholeFile => "whole_file",
            EditFormat::Hashline => "hashline",
        }
    }
}

impl fmt::Display for EditFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EditFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        EditFormat::ALL
            .into_iter()
            .find(|format| format.as_str() == s)
            .ok_or_else(|| {
                format!(
                    "unknown edit format `{s}` (expected {})",
                    EditFormat::ALL.map(|f| f.as_str()).join(", ")
                )
            })
    }
}
