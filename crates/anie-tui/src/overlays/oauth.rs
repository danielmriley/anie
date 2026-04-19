//! OAuth / Login overlay — placeholder.
//!
//! Tracks the future OAuth sign-in UI from `docs/ideas.md`.
//! Not yet implemented — renders a stub and dismisses on any key.
//! Wire-up is intentionally absent; the opener lands with the real
//! implementation.

#![cfg_attr(not(test), allow(dead_code))]

use crossterm::event::KeyEvent;
use ratatui::{Frame, layout::Rect};

use crate::overlay::{OverlayOutcome, OverlayScreen};
use crate::widgets::render_placeholder_panel;

const TITLE: &str = "OAuth / Login";
const BODY: &str =
    "OAuth sign-in not yet implemented. Use `/onboard` to configure providers with API keys.";

/// Placeholder OAuth/login screen.
pub(crate) struct OAuthScreen;

impl OAuthScreen {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl OverlayScreen for OAuthScreen {
    fn dispatch_key(&mut self, _key: KeyEvent) -> OverlayOutcome {
        OverlayOutcome::Dismiss
    }

    fn dispatch_tick(&mut self) -> OverlayOutcome {
        OverlayOutcome::Idle
    }

    fn dispatch_render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        render_placeholder_panel(frame, area, TITLE, BODY);
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn render_to_string(backend: &TestBackend) -> String {
        let width = backend.buffer().area.width as usize;
        backend
            .buffer()
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn oauth_placeholder_renders_title_and_body() {
        let mut screen = OAuthScreen::new();
        let mut terminal = Terminal::new(TestBackend::new(70, 14)).expect("terminal");
        terminal
            .draw(|frame| screen.dispatch_render(frame, frame.area()))
            .expect("draw placeholder");
        let rendered = render_to_string(terminal.backend());
        assert!(rendered.contains(TITLE));
        assert!(rendered.contains("not yet implemented"));
    }

    #[test]
    fn oauth_placeholder_dismisses_on_any_key() {
        let mut screen = OAuthScreen::new();
        for key in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        ] {
            assert!(matches!(screen.dispatch_key(key), OverlayOutcome::Dismiss));
        }
    }

    #[test]
    fn oauth_placeholder_tick_keeps_open() {
        let mut screen = OAuthScreen::new();
        assert!(matches!(screen.dispatch_tick(), OverlayOutcome::Idle));
    }
}
