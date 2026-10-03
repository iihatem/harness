//! Developer tasks for the repository. `cargo xtask eval` runs the edit-format eval suite in
//! `eval/`: replay mode applies recorded model outputs without a model.

pub mod oracle;
pub mod record;
pub mod replay;
pub mod task;
