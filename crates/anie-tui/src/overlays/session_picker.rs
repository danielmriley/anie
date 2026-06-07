//! Search-first session picker, shown as a `BottomPane` variant.
//!
//! Mirrors [`crate::overlays::model_picker::ModelPickerPane`] in shape
//! (search field, fuzzy filter, wraparound navigation, scroll) but lists
//! sessions rather than models. The text-editing primitive
//! (`SearchField`) and the small display helpers are shared with the
//! model picker rather than duplicated.

// Wired into `app.rs` (the `BottomPane::SessionPicker` variant) in
// session-ux/3; the widget is unit-tested in isolation here first.
#![cfg_attr(not(test), allow(dead_code))]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget, Wrap},
};

use anie_protocol::SessionSummary;

use crate::overlays::model_picker::{SearchField, is_text_input_modifiers, truncate_chars};
use crate::widgets::fuzzy::fuzzy_score_lowered;

/// Outcome of processing a key in the session picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPickerAction {
    /// Keep rendering the picker.
    Continue,
    /// User selected a session (carries its id).
    Selected(String),
    /// User cancelled the picker.
    Cancelled,
}

/// Search-first picker for switching to a previous session.
#[derive(Debug, Clone)]
pub struct SessionPickerPane {
    sessions: Vec<SessionSummary>,
    filtered_indices: Vec<usize>,
    selected: usize,
    scroll: usize,
    search: SearchField,
    current_session_id: String,
}

const VISIBLE_ROWS: usize = 8;

impl SessionPickerPane {
    /// Build a picker over `sessions`, marking `current_session_id`, with
    /// an optional prefilled search.
    #[must_use]
    pub fn new(
        sessions: Vec<SessionSummary>,
        current_session_id: String,
        initial_search: Option<String>,
    ) -> Self {
        let mut pane = Self {
            sessions,
            filtered_indices: Vec::new(),
            selected: 0,
            scroll: 0,
            search: SearchField::from(initial_search.unwrap_or_default()),
            current_session_id,
        };
        pane.apply_filter();
        pane
    }

    /// Current search text.
    #[must_use]
    pub fn search(&self) -> &str {
        self.search.value()
    }

    /// Preferred height in rows for the current content.
    #[must_use]
    pub fn preferred_height(&self, _width: u16) -> u16 {
        let list_rows = self.filtered_indices.len().clamp(3, VISIBLE_ROWS) as u16;
        (5 + list_rows).clamp(8, 14)
    }

    /// Render the picker; returns the cursor position for the search input.
    pub fn render(&self, area: Rect, buf: &mut Buffer) -> Position {
        let block = Block::default()
            .title(Line::from(Span::styled(
                " Switch Session ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )))
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().add_modifier(Modifier::DIM));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.height == 0 || inner.width == 0 {
            return Position::new(area.x, area.y);
        }

        let footer_y = inner.y + inner.height.saturating_sub(1);
        let search_y = inner.y;
        let list_top = search_y.saturating_add(1);
        let list_height = footer_y.saturating_sub(list_top).max(1);

        Paragraph::new(Line::from(vec![
            Span::styled("Search: ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                self.search.render_value(),
                Style::default().fg(Color::White),
            ),
        ]))
        .render(Rect::new(inner.x, search_y, inner.width, 1), buf);

        let list_lines = self.render_list_lines(list_height as usize, inner.width);
        Paragraph::new(list_lines)
            .wrap(Wrap { trim: false })
            .render(Rect::new(inner.x, list_top, inner.width, list_height), buf);

        Paragraph::new(Line::from(Span::styled(
            self.footer_line(),
            Style::default().add_modifier(Modifier::DIM),
        )))
        .render(Rect::new(inner.x, footer_y, inner.width, 1), buf);

        let prefix_width = "Search: ".chars().count() as u16;
        Position::new(
            inner
                .x
                .saturating_add(prefix_width)
                .saturating_add(self.search.cursor_x())
                .min(inner.x + inner.width.saturating_sub(1)),
            search_y,
        )
    }

    /// Handle a key event.
    pub fn handle_key(&mut self, key: KeyEvent) -> SessionPickerAction {
        if let KeyCode::Char(ch) = key.code
            && is_text_input_modifiers(key.modifiers)
        {
            self.search.insert_char(ch);
            self.apply_filter();
            return SessionPickerAction::Continue;
        }

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => SessionPickerAction::Cancelled,
            (KeyModifiers::NONE, KeyCode::Enter) => self
                .selected_session()
                .map(|session| session.id.clone())
                .map_or(SessionPickerAction::Continue, SessionPickerAction::Selected),
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                self.search.backspace();
                self.apply_filter();
                SessionPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Delete) => {
                self.search.delete();
                self.apply_filter();
                SessionPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Left) => {
                self.search.move_left();
                SessionPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                self.search.move_right();
                SessionPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Home) => {
                self.selected = 0;
                self.scroll = 0;
                SessionPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                self.selected = self.filtered_indices.len().saturating_sub(1);
                self.ensure_selection_visible();
                SessionPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.move_selection(-1);
                SessionPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                self.move_selection(1);
                SessionPickerAction::Continue
            }
            _ => SessionPickerAction::Continue,
        }
    }

    fn render_list_lines(&self, list_height: usize, width: u16) -> Vec<Line<'static>> {
        if self.sessions.is_empty() {
            return vec![
                Line::from(""),
                Line::from(Span::styled(
                    "No sessions yet",
                    Style::default().add_modifier(Modifier::DIM),
                )),
            ];
        }
        if self.filtered_indices.is_empty() {
            return vec![
                Line::from(""),
                Line::from(Span::styled(
                    "No matching sessions",
                    Style::default().add_modifier(Modifier::DIM),
                )),
            ];
        }

        let visible = list_height.clamp(1, VISIBLE_ROWS);
        let start = self
            .scroll
            .min(self.filtered_indices.len().saturating_sub(1));
        let end = (start + visible).min(self.filtered_indices.len());
        self.filtered_indices[start..end]
            .iter()
            .enumerate()
            .map(|(offset, index)| {
                let row_index = start + offset;
                let session = &self.sessions[*index];
                let is_selected = row_index == self.selected;
                let is_current = session.id == self.current_session_id;
                render_session_row(session, is_selected, is_current, width)
            })
            .collect()
    }

    fn footer_line(&self) -> String {
        let count = self.filtered_indices.len();
        let position = if count == 0 { 0 } else { self.selected + 1 };
        format!("[↑↓] Navigate  [Enter] Switch  [Esc] Cancel   ({position}/{count})")
    }

    fn selected_session(&self) -> Option<&SessionSummary> {
        self.filtered_indices
            .get(self.selected)
            .and_then(|index| self.sessions.get(*index))
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.filtered_indices.len();
        if len == 0 {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        let current = self.selected as isize;
        let next = (current + delta).rem_euclid(len as isize) as usize;
        self.selected = next;
        self.ensure_selection_visible();
    }

    fn ensure_selection_visible(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + VISIBLE_ROWS {
            self.scroll = self.selected + 1 - VISIBLE_ROWS;
        }
    }

    fn apply_filter(&mut self) {
        let search = self.search.value();
        if search.is_empty() {
            self.filtered_indices = (0..self.sessions.len()).collect();
        } else {
            let search_lower = search.to_ascii_lowercase();
            let tokens: Vec<&str> = search_lower.split_whitespace().collect();
            let mut scored: Vec<(u32, usize)> = self
                .sessions
                .iter()
                .enumerate()
                .filter_map(|(index, session)| {
                    let mut total: u32 = 0;
                    for token in &tokens {
                        let id_score = fuzzy_score_lowered(token, &session.id);
                        let msg_score = fuzzy_score_lowered(token, &session.first_message);
                        match id_score.into_iter().chain(msg_score).max() {
                            Some(score) => total = total.saturating_add(score),
                            None => return None,
                        }
                    }
                    Some((total, index))
                })
                .collect();
            scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
            self.filtered_indices = scored.into_iter().map(|(_, index)| index).collect();
        }

        if self.filtered_indices.is_empty() {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        if self.selected >= self.filtered_indices.len() {
            self.selected = 0;
        }
        self.ensure_selection_visible();
    }
}

fn render_session_row(
    session: &SessionSummary,
    is_selected: bool,
    is_current: bool,
    width: u16,
) -> Line<'static> {
    let prefix = if is_selected { "› " } else { "  " };
    let marker = if is_current { " ✓" } else { "" };
    let count_badge = format!(" [{}]", session.message_count);
    let reserved = prefix.chars().count() + count_badge.chars().count() + marker.chars().count();
    let available = (width as usize).saturating_sub(reserved).max(4);
    let label = truncate_chars(&session_label(session), available);

    let row_style = if is_selected {
        Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    let mut spans = vec![Span::styled(prefix.to_string(), row_style)];
    spans.push(Span::styled(label.clone(), row_style));
    let pad = available.saturating_sub(label.chars().count());
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), row_style));
    }
    let badge_style = if is_selected {
        row_style.fg(Color::Gray)
    } else {
        row_style.add_modifier(Modifier::DIM)
    };
    spans.push(Span::styled(count_badge, badge_style));
    if is_current {
        spans.push(Span::styled(marker, row_style.fg(Color::Green)));
    }
    Line::from(spans)
}

/// Display label for a session row: the first message if present, else
/// a short id fallback, always suffixed with a shortened id for
/// disambiguation.
fn session_label(session: &SessionSummary) -> String {
    let short_id = session.id.chars().take(8).collect::<String>();
    if session.first_message.trim().is_empty() {
        format!("(no messages) — {short_id}")
    } else {
        format!("{} — {short_id}", session.first_message)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn summary(id: &str, first_message: &str, count: u32) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            cwd: "/proj".to_string(),
            created: "2026-06-06T00:00:00Z".to_string(),
            modified_unix: 0,
            message_count: count,
            first_message: first_message.to_string(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    fn render_to_string(pane: &SessionPickerPane, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal
            .draw(|frame| {
                pane.render(frame.area(), frame.buffer_mut());
            })
            .expect("draw");
        let backend = terminal.backend();
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
    fn session_picker_filters_by_id_and_first_message() {
        let mut pane = SessionPickerPane::new(
            vec![
                summary("aaa111", "fix the parser", 3),
                summary("bbb222", "add a widget", 5),
            ],
            "aaa111".to_string(),
            None,
        );
        // Match on the first-message text.
        pane.handle_key(typed('w'));
        pane.handle_key(typed('i'));
        assert_eq!(pane.search(), "wi");
        assert_eq!(pane.filtered_indices.len(), 1);
        assert_eq!(pane.selected_session().unwrap().id, "bbb222");

        // Reset and match on the id instead.
        pane.handle_key(key(KeyCode::Backspace));
        pane.handle_key(key(KeyCode::Backspace));
        pane.handle_key(typed('a'));
        pane.handle_key(typed('a'));
        assert_eq!(pane.selected_session().unwrap().id, "aaa111");
    }

    #[test]
    fn session_picker_enter_returns_selected_id() {
        let mut pane = SessionPickerPane::new(
            vec![
                summary("aaa111", "first", 1),
                summary("bbb222", "second", 2),
            ],
            "aaa111".to_string(),
            None,
        );
        pane.handle_key(key(KeyCode::Down));
        let action = pane.handle_key(key(KeyCode::Enter));
        assert_eq!(action, SessionPickerAction::Selected("bbb222".to_string()));
    }

    #[test]
    fn session_picker_escape_returns_cancelled() {
        let mut pane =
            SessionPickerPane::new(vec![summary("aaa111", "first", 1)], "aaa111".into(), None);
        assert_eq!(
            pane.handle_key(key(KeyCode::Esc)),
            SessionPickerAction::Cancelled
        );
    }

    #[test]
    fn session_picker_navigation_wraps_at_boundaries() {
        let mut pane = SessionPickerPane::new(
            vec![
                summary("a", "one", 1),
                summary("b", "two", 1),
                summary("c", "three", 1),
            ],
            "a".into(),
            None,
        );
        assert_eq!(pane.selected, 0);
        // Up from the first wraps to the last.
        pane.handle_key(key(KeyCode::Up));
        assert_eq!(pane.selected, 2);
        // Down from the last wraps back to the first.
        pane.handle_key(key(KeyCode::Down));
        assert_eq!(pane.selected, 0);
    }

    #[test]
    fn session_picker_marks_current_session_row() {
        let pane = SessionPickerPane::new(
            vec![
                summary("aaa111", "first", 1),
                summary("bbb222", "second", 2),
            ],
            "bbb222".into(),
            None,
        );
        let rendered = render_to_string(&pane, 60, 12);
        // The current session's row carries the ✓ marker.
        assert!(
            rendered.contains('✓'),
            "expected current marker:\n{rendered}"
        );
    }

    #[test]
    fn session_picker_empty_list_renders_hint() {
        let pane = SessionPickerPane::new(Vec::new(), String::new(), None);
        let rendered = render_to_string(&pane, 60, 12);
        assert!(
            rendered.contains("No sessions yet"),
            "expected empty hint:\n{rendered}"
        );
    }

    #[test]
    fn session_picker_preferred_height_grows_with_rows_within_bounds() {
        let few = SessionPickerPane::new(vec![summary("a", "one", 1)], "a".into(), None);
        let many = SessionPickerPane::new(
            (0..20)
                .map(|i| summary(&format!("s{i}"), "msg", 1))
                .collect(),
            "s0".into(),
            None,
        );
        // Clamped to [8, 14]; more rows means a taller (or equal) pane.
        assert!((8..=14).contains(&few.preferred_height(60)));
        assert!(many.preferred_height(60) >= few.preferred_height(60));
        assert!(many.preferred_height(60) <= 14);
    }
}
