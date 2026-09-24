//! The built-in tools: read, write, edit, bash, grep, glob.

use std::sync::Arc;

use harness_core::tool::ToolRegistry;

pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod walk;
pub mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use write::WriteTool;

/// The six built-in tools in their fixed order.
pub fn builtin() -> ToolRegistry {
    ToolRegistry::new(vec![
        Arc::new(ReadTool),
        Arc::new(WriteTool),
        Arc::new(EditTool),
        Arc::new(BashTool),
        Arc::new(GrepTool),
        Arc::new(GlobTool),
    ])
}
