//! Terminal UI rendering for anie-rs.

mod app;
mod input;
mod output;
mod terminal;

pub use app::{AgentUiState, App, Spinner, StatusBarState, ToolCallResult, UiAction, run_tui};
pub use input::InputPane;
pub use output::{OutputPane, RenderedBlock};
pub use terminal::{install_panic_hook, restore_terminal, setup_terminal};

#[cfg(test)]
mod tests;
