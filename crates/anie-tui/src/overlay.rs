//! Full-screen overlay trait.
//!
//! Implemented by each overlay screen (onboarding, provider
//! management, future settings/session/login/etc.). `App` holds an
//! `Option<Box<dyn OverlayScreen>>` and dispatches key / tick /
//! render through the trait, then matches on the returned
//! `OverlayOutcome` to apply screen-specific actions.

use crossterm::event::KeyEvent;
use ratatui::{Frame, layout::Rect};

use crate::overlays::onboarding::OnboardingAction;
use crate::overlays::providers::ProviderManagementAction;

/// Dispatched outcome from an overlay's event handler.
///
/// This is a flat union of overlay-specific action types: each
/// overlay keeps its native Action enum, and the trait returns an
/// `OverlayOutcome` that embeds the concrete variant. Adding a new
/// overlay adds a new variant here; `App` pattern-matches to route
/// to the appropriate `apply_*_action` handler.
///
/// Larger variants (e.g. `ProviderManagementAction::ConfigChanged`
/// carrying a full `Model`) are intentionally inline because an
/// outcome is consumed immediately by `App::apply_overlay_outcome`
/// and never stored or cloned.
#[allow(clippy::large_enum_variant)]
pub(crate) enum OverlayOutcome {
    Onboarding(OnboardingAction),
    ProviderManagement(ProviderManagementAction),
}

/// Behavioural contract for full-screen overlays.
///
/// Each overlay must be able to handle a key, respond to a tick
/// (polling background workers), and render itself. Worker-event
/// delivery is overlay-internal — the overlay's own `handle_tick`
/// drains its channel.
pub(crate) trait OverlayScreen: Send {
    /// Dispatch a key event. Returns the overlay's outcome.
    fn dispatch_key(&mut self, key: KeyEvent) -> OverlayOutcome;

    /// Dispatch a tick (poll background workers, advance spinner).
    fn dispatch_tick(&mut self) -> OverlayOutcome;

    /// Render the overlay into the given `area`.
    fn dispatch_render(&mut self, frame: &mut Frame<'_>, area: Rect);
}
