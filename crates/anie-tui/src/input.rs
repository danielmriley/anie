use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Position, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Paragraph, Widget},
};

use crate::autocomplete::{AutocompletePopup, AutocompleteProvider, SuggestionKind};

/// Outcome of processing an input key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    /// No high-level action was produced.
    None,
    /// Submit the current input contents.
    Submit(String),
}

/// Optional autocomplete runtime attached to an `InputPane`.
///
/// Owns the provider and the currently-open popup (if any). The
/// pane refreshes `popup` after every buffer mutation so the
/// suggestion list stays in sync with the user's typing.
struct AutocompleteState {
    provider: Arc<dyn AutocompleteProvider>,
    popup: Option<AutocompletePopup>,
}

/// Multi-line input editor with simple history support.
pub struct InputPane {
    content: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    saved_content: Option<String>,
    autocomplete: Option<AutocompleteState>,
}

impl InputPane {
    /// Create an empty input pane.
    #[must_use]
    pub fn new() -> Self {
        Self {
            content: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: None,
            saved_content: None,
            autocomplete: None,
        }
    }

    /// Attach an autocomplete provider. Without this, the pane
    /// behaves exactly as it did before plan 12 — no popup ever
    /// opens and history navigation remains on Up/Down.
    #[must_use]
    pub(crate) fn with_autocomplete(mut self, provider: Arc<dyn AutocompleteProvider>) -> Self {
        self.autocomplete = Some(AutocompleteState {
            provider,
            popup: None,
        });
        self
    }

    /// Borrow the currently-open autocomplete popup for rendering.
    ///
    /// The caller (`App::render`) is responsible for computing
    /// the popup's rect via `AutocompletePopup::layout_rect` and
    /// for actually drawing it. Returning a borrow here keeps the
    /// lifecycle owned by `InputPane` — the popup dies when the
    /// pane loses it or when the pane itself is dropped.
    #[must_use]
    pub(crate) fn autocomplete_popup(&self) -> Option<&AutocompletePopup> {
        self.autocomplete
            .as_ref()
            .and_then(|state| state.popup.as_ref())
    }

    /// Whether the popup is currently visible. Exists so callers
    /// can cheaply decide whether to reserve rows without pulling
    /// the popup borrow.
    #[must_use]
    pub fn autocomplete_is_open(&self) -> bool {
        self.autocomplete
            .as_ref()
            .is_some_and(|state| state.popup.is_some())
    }

    /// Return the current input contents.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Insert a newline at the current cursor position.
    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Handle a key press while the editor is focused.
    pub fn handle_key(&mut self, key: KeyEvent) -> InputAction {
        // Popup-open key routing. Keys that don't belong to the
        // popup fall through to the normal editor and then trigger
        // a popup refresh.
        if self.autocomplete_is_open() {
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Esc) => {
                    self.close_autocomplete();
                    return InputAction::None;
                }
                (KeyModifiers::NONE, KeyCode::Up) | (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                    self.move_autocomplete_selection(-1);
                    return InputAction::None;
                }
                (KeyModifiers::NONE, KeyCode::Down)
                | (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
                    self.move_autocomplete_selection(1);
                    return InputAction::None;
                }
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    if self.autocomplete_would_noop_on_apply() {
                        // User has already typed the selected
                        // suggestion exactly; close the popup and
                        // let the submit path run.
                        self.close_autocomplete();
                    } else {
                        self.apply_autocomplete_selection();
                        return InputAction::None;
                    }
                }
                (KeyModifiers::NONE, KeyCode::Tab) => {
                    if !self.autocomplete_would_noop_on_apply() {
                        self.apply_autocomplete_selection();
                    }
                    return InputAction::None;
                }
                _ => {}
            }
        }

        let action = self.dispatch_editor_key(key);
        self.refresh_autocomplete();
        action
    }

    /// Whether applying the highlighted suggestion would be a
    /// pure no-op (the buffer already contains exactly the
    /// completion text). Used to distinguish "accept completion"
    /// from "submit already-complete input" when Enter is pressed
    /// on an open popup.
    fn autocomplete_would_noop_on_apply(&self) -> bool {
        let Some(state) = self.autocomplete.as_ref() else {
            return false;
        };
        let Some(popup) = state.popup.as_ref() else {
            return false;
        };
        let Some(selected) = popup.selected() else {
            return false;
        };
        match popup.kind() {
            SuggestionKind::CommandName => {
                let typed_name = popup.prefix().strip_prefix('/').unwrap_or(popup.prefix());
                typed_name == selected.value
            }
            SuggestionKind::ArgumentValue { .. } => popup.prefix() == selected.value.as_str(),
        }
    }

    fn dispatch_editor_key(&mut self, key: KeyEvent) -> InputAction {
        if let KeyCode::Char(ch) = key.code
            && is_text_input_modifiers(key.modifiers)
        {
            self.insert_char(ch);
            return InputAction::None;
        }

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Enter) => self.submit(),
            (KeyModifiers::SHIFT, KeyCode::Enter) | (KeyModifiers::ALT, KeyCode::Enter) => {
                self.insert_newline();
                InputAction::None
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                self.backspace();
                InputAction::None
            }
            (KeyModifiers::NONE, KeyCode::Delete) => {
                self.delete();
                InputAction::None
            }
            (KeyModifiers::NONE, KeyCode::Left) => {
                self.move_left();
                InputAction::None
            }
            (KeyModifiers::ALT, KeyCode::Left) => {
                self.move_word_left();
                InputAction::None
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                self.move_right();
                InputAction::None
            }
            (KeyModifiers::ALT, KeyCode::Right) => {
                self.move_word_right();
                InputAction::None
            }
            (KeyModifiers::NONE, KeyCode::Home) | (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
                self.move_to_line_start();
                InputAction::None
            }
            (KeyModifiers::NONE, KeyCode::End) | (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
                self.move_to_line_end();
                InputAction::None
            }
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                self.delete_line();
                InputAction::None
            }
            (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
                self.delete_to_line_end();
                InputAction::None
            }
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                self.delete_word_backward();
                InputAction::None
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.history_previous();
                InputAction::None
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                self.history_next();
                InputAction::None
            }
            _ => InputAction::None,
        }
    }

    /// Compute the preferred input height for the given width.
    #[must_use]
    pub fn preferred_height(&self, width: u16) -> u16 {
        let width = width.max(1);
        let (lines, _) = self.layout_lines(width);
        let line_count = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        line_count.clamp(3, 8)
    }

    /// Render the input pane and return the cursor position.
    pub fn render(&self, area: Rect, buf: &mut ratatui::buffer::Buffer) -> Position {
        let block = Block::default();
        let inner = block.inner(area);
        block.render(area, buf);

        let (lines, cursor) = self.layout_lines(inner.width.max(1));
        let rendered_lines = lines
            .into_iter()
            .take(inner.height as usize)
            .map(|line| Line::styled(line, Style::default().fg(Color::White)))
            .collect::<Vec<_>>();
        Paragraph::new(rendered_lines).render(inner, buf);

        Position::new(
            inner
                .x
                .saturating_add(cursor.0.min(inner.width.saturating_sub(1))),
            inner
                .y
                .saturating_add(cursor.1.min(inner.height.saturating_sub(1))),
        )
    }

    fn submit(&mut self) -> InputAction {
        let content = self.content.trim_end().to_string();
        if content.trim().is_empty() {
            return InputAction::None;
        }
        self.history.push(content.clone());
        self.content.clear();
        self.cursor = 0;
        self.history_index = None;
        self.saved_content = None;
        InputAction::Submit(content)
    }

    fn insert_char(&mut self, ch: char) {
        self.content.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.history_index = None;
    }

    fn backspace(&mut self) {
        if let Some(previous) = previous_boundary(&self.content, self.cursor) {
            self.content.drain(previous..self.cursor);
            self.cursor = previous;
        }
    }

    fn delete(&mut self) {
        if let Some(next) = next_boundary(&self.content, self.cursor) {
            self.content.drain(self.cursor..next);
        }
    }

    fn move_left(&mut self) {
        if let Some(previous) = previous_boundary(&self.content, self.cursor) {
            self.cursor = previous;
        }
    }

    fn move_right(&mut self) {
        if let Some(next) = next_boundary(&self.content, self.cursor) {
            self.cursor = next;
        }
    }

    fn move_to_line_start(&mut self) {
        let line_start = self.content[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        self.cursor = line_start;
    }

    fn move_to_line_end(&mut self) {
        let suffix = &self.content[self.cursor..];
        self.cursor += suffix.find('\n').unwrap_or(suffix.len());
    }

    fn move_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut index = self.cursor;
        while let Some(previous) = previous_boundary(&self.content, index) {
            let ch = self.content[previous..index].chars().next().unwrap_or(' ');
            index = previous;
            if !ch.is_whitespace() {
                break;
            }
            if index == 0 {
                self.cursor = 0;
                return;
            }
        }
        while let Some(previous) = previous_boundary(&self.content, index) {
            let ch = self.content[previous..index].chars().next().unwrap_or(' ');
            if ch.is_whitespace() {
                break;
            }
            index = previous;
            if index == 0 {
                break;
            }
        }
        self.cursor = index;
    }

    fn move_word_right(&mut self) {
        if self.cursor >= self.content.len() {
            return;
        }
        let mut index = self.cursor;
        while let Some(next) = next_boundary(&self.content, index) {
            let ch = self.content[index..next].chars().next().unwrap_or(' ');
            index = next;
            if !ch.is_whitespace() {
                break;
            }
            if index >= self.content.len() {
                self.cursor = self.content.len();
                return;
            }
        }
        while let Some(next) = next_boundary(&self.content, index) {
            let ch = self.content[index..next].chars().next().unwrap_or(' ');
            if ch.is_whitespace() {
                break;
            }
            index = next;
            if index >= self.content.len() {
                break;
            }
        }
        self.cursor = index;
    }

    fn delete_line(&mut self) {
        self.content.clear();
        self.cursor = 0;
        self.history_index = None;
    }

    fn delete_to_line_end(&mut self) {
        let line_end = self.cursor
            + self.content[self.cursor..]
                .find('\n')
                .unwrap_or(self.content[self.cursor..].len());
        self.content.drain(self.cursor..line_end);
    }

    fn delete_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut index = self.cursor;
        while let Some(previous) = previous_boundary(&self.content, index) {
            let ch = self.content[previous..index].chars().next().unwrap_or(' ');
            if !ch.is_whitespace() {
                index = previous;
                break;
            }
            index = previous;
            if index == 0 {
                break;
            }
        }
        while let Some(previous) = previous_boundary(&self.content, index) {
            let ch = self.content[previous..index].chars().next().unwrap_or(' ');
            if ch.is_whitespace() {
                break;
            }
            index = previous;
            if index == 0 {
                break;
            }
        }
        self.content.drain(index..self.cursor);
        self.cursor = index;
    }

    /// Move to the previous history item.
    pub fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.saved_content = Some(self.content.clone());
                self.history_index = Some(self.history.len() - 1);
            }
            Some(0) => {}
            Some(index) => self.history_index = Some(index - 1),
        }
        if let Some(index) = self.history_index {
            self.content = self.history[index].clone();
            self.cursor = self.content.len();
        }
    }

    /// Move to the next history item.
    pub fn history_next(&mut self) {
        match self.history_index {
            None => {}
            Some(index) if index + 1 >= self.history.len() => {
                self.history_index = None;
                self.content = self.saved_content.take().unwrap_or_default();
                self.cursor = self.content.len();
            }
            Some(index) => {
                let next = index + 1;
                self.history_index = Some(next);
                self.content = self.history[next].clone();
                self.cursor = self.content.len();
            }
        }
    }

    fn layout_lines(&self, width: u16) -> (Vec<String>, (u16, u16)) {
        let width = width.max(1) as usize;
        let prefix = "> ";
        let mut lines = vec![String::new()];
        let mut row = 0usize;
        let mut col = 0usize;
        let mut cursor_visual = (prefix.len() as u16, 0u16);

        if self.cursor == 0 {
            cursor_visual = (prefix.len() as u16, 0);
        }

        for (index, ch) in self.content.char_indices() {
            if index == self.cursor {
                cursor_visual = (
                    (if row == 0 { prefix.len() } else { 0 } + col) as u16,
                    row as u16,
                );
            }

            let available = if row == 0 {
                width.saturating_sub(prefix.len()).max(1)
            } else {
                width
            };

            if ch == '\n' {
                lines.push(String::new());
                row += 1;
                col = 0;
                continue;
            }

            if col >= available {
                lines.push(String::new());
                row += 1;
                col = 0;
            }

            lines[row].push(ch);
            col += 1;
        }

        if self.cursor == self.content.len() {
            cursor_visual = (
                (if row == 0 { prefix.len() } else { 0 } + col) as u16,
                row as u16,
            );
        }

        if let Some(first) = lines.first_mut() {
            first.insert_str(0, prefix);
        }

        (lines, cursor_visual)
    }

    // =========================================================================
    // Autocomplete integration (plan 12 phase D).
    // =========================================================================

    /// Refresh the popup to reflect the current buffer + cursor.
    ///
    /// Called after every mutating keypress so the visible list
    /// stays in sync. A no-op when no provider is installed.
    fn refresh_autocomplete(&mut self) {
        let Some(state) = self.autocomplete.as_mut() else {
            return;
        };
        let suggestions = state.provider.suggestions(&self.content, self.cursor);
        state.popup = suggestions.map(AutocompletePopup::from_suggestions);
    }

    fn close_autocomplete(&mut self) {
        if let Some(state) = self.autocomplete.as_mut() {
            state.popup = None;
        }
    }

    fn move_autocomplete_selection(&mut self, delta: isize) {
        if let Some(state) = self.autocomplete.as_mut()
            && let Some(popup) = state.popup.as_mut()
        {
            popup.move_selection(delta);
        }
    }

    /// Apply the highlighted suggestion to the buffer.
    ///
    /// Replaces `prefix` with `value`, inserts a trailing space
    /// when completing a command name (so the next argument can
    /// be typed immediately), and re-queries the provider so the
    /// popup flows naturally from command-name mode into
    /// argument-value mode when appropriate.
    fn apply_autocomplete_selection(&mut self) {
        let Some(apply) = self.pending_apply() else {
            return;
        };
        let ApplyPlan {
            start,
            end,
            replacement,
            trailing_space,
        } = apply;
        self.content.replace_range(start..end, &replacement);
        self.cursor = start + replacement.len();
        if trailing_space {
            // Only insert a trailing space when the cursor isn't
            // already followed by one, so repeated completions
            // against edited lines don't accumulate whitespace.
            let already_space = self.content.as_bytes().get(self.cursor).copied() == Some(b' ');
            if !already_space {
                self.content.insert(self.cursor, ' ');
                self.cursor += 1;
            } else {
                self.cursor += 1; // skip over the existing space
            }
        }
        self.history_index = None;
        self.refresh_autocomplete();
    }

    /// Build the replacement plan for the currently-selected
    /// suggestion, if the popup and selection are both alive.
    fn pending_apply(&self) -> Option<ApplyPlan> {
        let state = self.autocomplete.as_ref()?;
        let popup = state.popup.as_ref()?;
        let suggestion = popup.selected()?;
        let prefix = popup.prefix();
        let prefix_len = prefix.len();
        let start = self.cursor.checked_sub(prefix_len)?;

        // For command-name completions, the suggestion's `value`
        // is the bare command name (e.g. "thinking"). Re-emit the
        // leading slash so the replacement stays valid input.
        let (replacement, trailing_space) = match popup.kind() {
            SuggestionKind::CommandName => (format!("/{}", suggestion.value), true),
            SuggestionKind::ArgumentValue { .. } => (suggestion.value.clone(), false),
        };

        Some(ApplyPlan {
            start,
            end: self.cursor,
            replacement,
            trailing_space,
        })
    }
}

/// Represents the edit an autocomplete acceptance performs on the
/// buffer. Extracting it into a struct lets us compute the plan
/// while holding an immutable borrow (`&self.autocomplete`), then
/// apply it under a mutable borrow without overlapping lifetimes.
struct ApplyPlan {
    start: usize,
    end: usize,
    replacement: String,
    trailing_space: bool,
}

impl Default for InputPane {
    fn default() -> Self {
        Self::new()
    }
}

fn previous_boundary(content: &str, cursor: usize) -> Option<usize> {
    if cursor == 0 {
        return None;
    }
    let mut index = cursor - 1;
    while !content.is_char_boundary(index) {
        index -= 1;
    }
    Some(index)
}

fn next_boundary(content: &str, cursor: usize) -> Option<usize> {
    if cursor >= content.len() {
        return None;
    }
    let mut index = cursor + 1;
    while index < content.len() && !content.is_char_boundary(index) {
        index += 1;
    }
    Some(index)
}

fn is_text_input_modifiers(modifiers: KeyModifiers) -> bool {
    modifiers.is_empty() || modifiers == KeyModifiers::SHIFT
}
