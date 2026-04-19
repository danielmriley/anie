//! Full-screen overlay screens.
//!
//! Each overlay implements `crate::overlay::OverlayScreen` and is
//! reached via `App`'s `Option<Box<dyn OverlayScreen>>`. Adding a
//! new overlay is: create a file here, implement the trait, wire
//! up the opener and the `OverlayOutcome` variant in `app.rs`.
//!
//! Real overlays and roadmap placeholder stubs both live here so
//! future UI work has a stable landing pad.

pub(crate) mod hotkeys;
pub(crate) mod model_picker;
pub(crate) mod oauth;
pub(crate) mod onboarding;
pub(crate) mod providers;
pub(crate) mod session_picker;
pub(crate) mod settings;
pub(crate) mod theme_picker;
pub(crate) mod tree;

pub use model_picker::{ModelPickerAction, ModelPickerPane};
pub use onboarding::{
    ConfiguredProvider, ConfiguredProviderKind, OnboardingAction, OnboardingCompletion,
    OnboardingScreen, write_configured_providers,
};
pub use providers::{
    ProviderEntry, ProviderManagementAction, ProviderManagementScreen, ProviderType, TestResult,
};
