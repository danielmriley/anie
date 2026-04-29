//! Core read/write/bash tool implementations and file-mutation serialization.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod bash;
mod edit;
mod file_mutation_queue;
mod find;
mod grep;
mod ls;
mod read;
mod recurse;
mod shared;
mod write;

pub use bash::{BashPolicy, BashTool};
pub use edit::EditTool;
pub use file_mutation_queue::FileMutationQueue;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use recurse::RecurseTool;
pub use write::WriteTool;

#[cfg(test)]
mod tests;
