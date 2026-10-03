//! The built-in tools: read, write, edit, bash, grep, glob.

use std::sync::Arc;

use harness_core::{
    edit_format::EditFormat,
    tool::{Tool, ToolRegistry},
};

pub mod apply_patch;
pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod hashline;
pub mod patch;
pub mod read;
pub mod walk;
pub mod whole_file;
pub mod write;

pub use apply_patch::ApplyPatchTool;
pub use bash::BashTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use hashline::HashlineEditTool;
pub use read::{HashlineReadTool, ReadTool};
pub use whole_file::WholeFileTool;
pub use write::WriteTool;

/// The six built-in tools in their fixed order: the tools of the `str_replace` edit format.
pub fn builtin() -> ToolRegistry {
    builtin_for(EditFormat::StrReplace)
}

/// The tools a model with edit format `format` is offered, in a fixed order, with exactly one
/// edit tool: `edit`, `apply_patch`, `write` with complete files (limited to small ones), or
/// `hashline_edit` with a `read` that addresses lines.
pub fn builtin_for(format: EditFormat) -> ToolRegistry {
    let (read, middle): (Arc<dyn Tool>, Vec<Arc<dyn Tool>>) = match format {
        EditFormat::StrReplace => (
            Arc::new(ReadTool),
            vec![Arc::new(WriteTool), Arc::new(EditTool)],
        ),
        EditFormat::ApplyPatch => (Arc::new(ReadTool), vec![Arc::new(ApplyPatchTool)]),
        EditFormat::WholeFile => (Arc::new(ReadTool), vec![Arc::new(WholeFileTool)]),
        EditFormat::Hashline => (
            Arc::new(HashlineReadTool),
            vec![Arc::new(WriteTool), Arc::new(HashlineEditTool)],
        ),
    };
    let mut tools = vec![read];
    tools.extend(middle);
    tools.extend::<[Arc<dyn Tool>; 3]>([
        Arc::new(BashTool),
        Arc::new(GrepTool),
        Arc::new(GlobTool),
    ]);
    ToolRegistry::new(tools)
}
