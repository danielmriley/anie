//! Terminal UI rendering for anie-rs.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod app;
mod input;
mod output;
mod overlay;
mod overlays;
mod terminal;
mod widgets;

pub use app::{AgentUiState, App, Spinner, StatusBarState, ToolCallResult, UiAction, run_tui};
pub use input::InputPane;
pub use output::{OutputPane, RenderedBlock};
pub use overlays::{
    ConfiguredProvider, ConfiguredProviderKind, ModelPickerAction, ModelPickerPane,
    OnboardingAction, OnboardingCompletion, OnboardingScreen, ProviderEntry,
    ProviderManagementAction, ProviderManagementScreen, ProviderType, TestResult,
    write_configured_providers,
};
pub use terminal::{install_panic_hook, restore_terminal, setup_terminal};

#[cfg(test)]
mod tests;
