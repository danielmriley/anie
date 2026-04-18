//! Full-screen overlay screens.
//!
//! Each overlay implements `crate::overlay::OverlayScreen` and is
//! reached via `App`'s `Option<Box<dyn OverlayScreen>>`. Adding a
//! new overlay is: create a file here, implement the trait, wire
//! up the opener and the `OverlayOutcome` variant in `app.rs`.
//!
//! pi-mono has ~14 overlays in
//! `packages/coding-agent/src/modes/interactive/components/`. The
//! ones listed in `docs/ideas.md` for anie's near-term roadmap
//! (`/settings`, `/login`, `/tree`, theme picker, hotkeys viewer,
//! session picker) land here as they're implemented.

pub(crate) mod model_picker;
pub(crate) mod onboarding;
pub(crate) mod providers;

pub use model_picker::{ModelPickerAction, ModelPickerPane};
pub use onboarding::{
    ConfiguredProvider, ConfiguredProviderKind, OnboardingAction, OnboardingCompletion,
    OnboardingScreen, write_configured_providers,
};
pub use providers::{
    ProviderEntry, ProviderManagementAction, ProviderManagementScreen, ProviderType, TestResult,
};
