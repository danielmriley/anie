//! Terminal UI rendering for anie-rs.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod app;
mod input;
mod model_picker;
mod onboarding;
mod output;
mod providers;
mod terminal;

pub use app::{AgentUiState, App, Spinner, StatusBarState, ToolCallResult, UiAction, run_tui};
pub use input::InputPane;
pub use model_picker::{ModelPickerAction, ModelPickerPane};
pub use onboarding::{
    ConfiguredProvider, ConfiguredProviderKind, OnboardingAction, OnboardingCompletion,
    OnboardingScreen, write_configured_providers,
};
pub use output::{OutputPane, RenderedBlock};
pub use providers::{
    ProviderEntry, ProviderManagementAction, ProviderManagementScreen, ProviderType, TestResult,
};
pub use terminal::{install_panic_hook, restore_terminal, setup_terminal};

#[cfg(test)]
mod tests;
