use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Position, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Paragraph, Widget},
};

use crate::autocomplete::{AutocompletePopup, AutocompleteProvider, SuggestionKind};

/// Delay between the last keystroke and when the autocomplete
/// provider is re-queried. Set below typing perception
/// threshold so a user never *sees* the debounce, while giving
/// burst-typists one refresh instead of one-per-keystroke.
/// See `docs/refactor_worklist_2026-04-22/tui_perf_06_quick_wins.md`.
const AUTOCOMPLETE_DEBOUNCE: Duration = Duration::from_millis(80);

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
    /// When set, an autocomplete refresh is queued to fire on
    /// or after this instant. `None` means the popup is in
    /// steady state and no refresh is pending. Set on every
    /// mutating keystroke; cleared by `tick_autocomplete` once
    /// the debounce has elapsed and the refresh has fired.
    pending_autocomplete_at: Option<Instant>,
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
            pending_autocomplete_at: None,
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
        // Popup-consuming actions (Enter, Tab, arrow-nav, Esc)
        // need current popup state. Flush any pending refresh
        // BEFORE checking `autocomplete_is_open()` — a flush may
        // close the popup (no matches for current buffer), in
        // which case Enter should fall through to submit rather
        // than be absorbed by the popup branch.
        if matches!(
            (key.modifiers, key.code),
            (_, KeyCode::Enter)
                | (_, KeyCode::Tab)
                | (KeyModifiers::NONE, KeyCode::Up)
                | (KeyModifiers::CONTROL, KeyCode::Char('p'))
                | (KeyModifiers::NONE, KeyCode::Down)
                | (KeyModifiers::CONTROL, KeyCode::Char('n'))
                | (KeyModifiers::NONE, KeyCode::Esc)
        ) {
            self.flush_pending_autocomplete();
        }

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
        // Debounce: rather than re-querying the provider on
        // every keystroke, mark the popup stale and let
        // tick_autocomplete fire the refresh once typing
        // pauses. See AUTOCOMPLETE_DEBOUNCE.
        self.schedule_autocomplete_refresh();
        action
    }

    /// Mark the autocomplete popup as stale.
    ///
    /// Eager-on-first: a keystroke that starts a new burst
    /// (no refresh currently pending) fires the refresh
    /// synchronously so the popup reflects the immediate
    /// change. Subsequent keystrokes *within* the debounce
    /// window only push the deadline forward; the actual
    /// refresh fires from the render loop's
    /// `tick_autocomplete` call once typing pauses for at
    /// least the debounce window, or from
    /// `flush_pending_autocomplete` when a popup-consuming
    /// action is about to read state.
    fn schedule_autocomplete_refresh(&mut self) {
        if self.autocomplete.is_none() {
            return;
        }
        let was_pending = self.pending_autocomplete_at.is_some();
        self.pending_autocomplete_at = Some(Instant::now() + AUTOCOMPLETE_DEBOUNCE);
        if !was_pending {
            // First keystroke of a new burst: refresh now so
            // the popup appears immediately. The deadline we
            // just set squelches the next N keystrokes.
            self.refresh_autocomplete();
        }
    }

    /// Run a pending autocomplete refresh if its debounce has
    /// elapsed. Returns `true` if a refresh actually fired
    /// (useful for tests). Intended to be called from the
    /// app's render / idle-tick path; cheap enough to call
    /// every frame.
    pub fn tick_autocomplete(&mut self) -> bool {
        let Some(deadline) = self.pending_autocomplete_at else {
            return false;
        };
        if Instant::now() < deadline {
            return false;
        }
        self.pending_autocomplete_at = None;
        self.refresh_autocomplete();
        true
    }

    /// Force any pending autocomplete refresh to fire now,
    /// ignoring the debounce deadline. Returns `true` if a
    /// refresh ran.
    ///
    /// Used (a) before popup-consuming actions like Enter /
    /// Tab / arrow-navigation so the popup reflects the
    /// current buffer, and (b) from tests that don't drive
    /// a render cycle between typing and assertion.
    pub fn flush_pending_autocomplete(&mut self) -> bool {
        if self.pending_autocomplete_at.take().is_none() {
            return false;
        }
        self.refresh_autocomplete();
        true
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autocomplete::{AutocompleteProvider, SuggestionSet};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts suggestions() invocations. Every call the input
    /// pane delegates to a real provider increments the
    /// counter; tests assert on how many fired.
    struct CountingProvider {
        calls: Arc<AtomicUsize>,
    }

    impl AutocompleteProvider for CountingProvider {
        fn suggestions(&self, _line: &str, _cursor: usize) -> Option<SuggestionSet> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            None
        }
    }

    fn input_with_counter() -> (InputPane, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider {
            calls: counter.clone(),
        };
        let pane = InputPane::new().with_autocomplete(Arc::new(provider));
        (pane, counter)
    }

    fn type_char(pane: &mut InputPane, ch: char) {
        let _ = pane.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }

    /// A burst of keystrokes fires the provider exactly once
    /// (on the first keystroke, eager-on-first) while the
    /// remaining keystrokes defer to the debounce window.
    #[test]
    fn autocomplete_debounce_collapses_burst_to_one_eager_plus_one_deferred_refresh() {
        let (mut pane, calls) = input_with_counter();
        for ch in ['h', 'e', 'l', 'l', 'o'] {
            type_char(&mut pane, ch);
        }
        // Eager-on-first: the first keystroke in the burst
        // fires. The remaining four defer.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only the first keystroke in a burst should fire the provider"
        );

        // Drive the debounce deadline into the past by
        // rewinding it. Directly adjusting the field avoids
        // a real sleep in tests.
        pane.pending_autocomplete_at = Some(Instant::now() - Duration::from_millis(1));
        let fired = pane.tick_autocomplete();
        assert!(fired, "tick must fire once the debounce has elapsed");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "tick flush after burst adds one more refresh"
        );
    }

    /// tick_autocomplete called before the debounce elapses is
    /// a no-op; the provider isn't queried beyond whatever the
    /// eager-on-first fire produced.
    #[test]
    fn autocomplete_tick_before_deadline_is_noop() {
        let (mut pane, calls) = input_with_counter();
        type_char(&mut pane, 'a');
        // One eager-on-first call fired.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // tick immediately after: pending is ~80ms in the
        // future, so no additional fire.
        let fired = pane.tick_autocomplete();
        assert!(!fired, "tick before deadline must not fire");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Popup-consuming actions (Enter/Tab/Up/Down/Esc) flush
    /// any pending refresh before they read popup state. This
    /// guards against acting on stale suggestion lists.
    #[test]
    fn popup_consuming_action_flushes_pending_refresh() {
        let (mut pane, calls) = input_with_counter();
        // First keystroke fires eager.
        type_char(&mut pane, 'a');
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Second keystroke defers.
        type_char(&mut pane, 'b');
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Enter should flush the pending refresh.
        pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // The provider was queried once more during the flush.
        // (Enter may also produce additional queries if it
        // triggers apply_autocomplete_selection, but the
        // counter-provider returns None so apply is a no-op.)
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "Enter must flush pending refresh before reading popup"
        );
    }

    /// With no autocomplete provider installed, the pane never
    /// schedules a refresh and tick_autocomplete is always a
    /// no-op — regression guard so adding debounce doesn't
    /// change behavior for panes that never installed one.
    #[test]
    fn autocomplete_tick_without_provider_stays_noop() {
        let mut pane = InputPane::new();
        type_char(&mut pane, 'x');
        type_char(&mut pane, 'y');
        assert_eq!(pane.pending_autocomplete_at, None);
        assert!(!pane.tick_autocomplete());
    }
}
