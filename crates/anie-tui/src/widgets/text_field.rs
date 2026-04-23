//! Single-line editable text field used by overlay screens.
//!
//! Supports optional value masking (for API-key entry), grapheme-aware
//! cursor movement, and the standard bash-ish editing keys
//! (Home/End/Ctrl+A/Ctrl+E/Ctrl+U).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TextField {
    pub(crate) value: String,
    pub(crate) cursor: usize,
    pub(crate) masked: bool,
}

impl TextField {
    /// Create a plain field seeded with the given value; the cursor is
    /// placed at the end.
    pub(crate) fn from(value: &str) -> Self {
        Self {
            value: value.to_string(),
            cursor: value.len(),
            masked: false,
        }
    }

    /// Create an empty masked (password-style) field.
    pub(crate) fn masked() -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            masked: true,
        }
    }

    /// Create a masked field pre-populated with an existing value.
    pub(crate) fn masked_with_value(value: &str) -> Self {
        Self {
            value: value.to_string(),
            cursor: value.len(),
            masked: true,
        }
    }

    /// Apply an edit key. Unknown keys are ignored.
    pub(crate) fn handle_edit_key(&mut self, key: KeyEvent) {
        if let KeyCode::Char(ch) = key.code
            && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
        {
            self.insert_char(ch);
            return;
        }

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Backspace) => self.backspace(),
            (KeyModifiers::NONE, KeyCode::Delete) => self.delete(),
            (KeyModifiers::NONE, KeyCode::Left) => self.move_left(),
            (KeyModifiers::NONE, KeyCode::Right) => self.move_right(),
            (KeyModifiers::NONE, KeyCode::Home) | (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
                self.cursor = 0;
            }
            (KeyModifiers::NONE, KeyCode::End) | (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
                self.cursor = self.value.len();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                self.value.clear();
                self.cursor = 0;
            }
            _ => {}
        }
    }

    /// Render the visible text — bullets if `masked`, the raw value
    /// otherwise.
    pub(crate) fn render_value(&self) -> String {
        if self.masked {
            if self.value.is_empty() {
                String::new()
            } else {
                "•".repeat(self.value.chars().count())
            }
        } else {
            self.value.clone()
        }
    }

    /// Cursor column for rendering in character-based terminals.
    ///
    /// Plan 05 PR-D: count the char-prefix directly — both
    /// masked and unmasked paths render one USV per input
    /// char (mask glyph or the original), so the cell count
    /// is the char count in both. No need to materialize a
    /// `String` of repeated `•` just to count it.
    pub(crate) fn cursor_x(&self) -> u16 {
        let chars_before_cursor = self.value[..self.cursor].chars().count();
        u16::try_from(chars_before_cursor).unwrap_or(u16::MAX)
    }

    /// Value with surrounding whitespace removed.
    pub(crate) fn trimmed(&self) -> String {
        self.value.trim().to_string()
    }

    fn insert_char(&mut self, ch: char) {
        self.value.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    fn backspace(&mut self) {
        if let Some(previous) = previous_boundary(&self.value, self.cursor) {
            self.value.drain(previous..self.cursor);
            self.cursor = previous;
        }
    }

    fn delete(&mut self) {
        if let Some(next) = next_boundary(&self.value, self.cursor) {
            self.value.drain(self.cursor..next);
        }
    }

    fn move_left(&mut self) {
        if let Some(previous) = previous_boundary(&self.value, self.cursor) {
            self.cursor = previous;
        }
    }

    fn move_right(&mut self) {
        if let Some(next) = next_boundary(&self.value, self.cursor) {
            self.cursor = next;
        }
    }
}

/// Byte index of the previous grapheme boundary before `index`.
pub(crate) fn previous_boundary(text: &str, index: usize) -> Option<usize> {
    if index == 0 {
        return None;
    }
    text[..index]
        .char_indices()
        .last()
        .map(|(position, _)| position)
}

/// Byte index of the next grapheme boundary after `index`.
pub(crate) fn next_boundary(text: &str, index: usize) -> Option<usize> {
    if index >= text.len() {
        return None;
    }
    text[index..]
        .char_indices()
        .nth(1)
        .map(|(offset, _)| index + offset)
        .or(Some(text.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn insert_char_at_end_moves_cursor() {
        let mut field = TextField::default();
        field.handle_edit_key(key(KeyCode::Char('a')));
        field.handle_edit_key(key(KeyCode::Char('b')));
        assert_eq!(field.value, "ab");
        assert_eq!(field.cursor, 2);
    }

    #[test]
    fn backspace_deletes_previous_char() {
        let mut field = TextField::from("abc");
        field.handle_edit_key(key(KeyCode::Backspace));
        assert_eq!(field.value, "ab");
        assert_eq!(field.cursor, 2);
    }

    #[test]
    fn backspace_at_start_is_a_noop() {
        let mut field = TextField::from("abc");
        field.cursor = 0;
        field.handle_edit_key(key(KeyCode::Backspace));
        assert_eq!(field.value, "abc");
        assert_eq!(field.cursor, 0);
    }

    #[test]
    fn left_arrow_respects_char_boundary() {
        let mut field = TextField::from("a😀b");
        assert_eq!(field.cursor, 6, "cursor should be at end of UTF-8 bytes");
        field.handle_edit_key(key(KeyCode::Left));
        // Moving left past 'b' (1 byte) lands on the emoji boundary.
        assert_eq!(field.cursor, 5);
        field.handle_edit_key(key(KeyCode::Left));
        // Moving left past the emoji (4 bytes) lands on 'a'.
        assert_eq!(field.cursor, 1);
    }

    #[test]
    fn right_arrow_respects_char_boundary() {
        let mut field = TextField::from("a😀b");
        field.cursor = 0;
        field.handle_edit_key(key(KeyCode::Right));
        assert_eq!(field.cursor, 1);
        field.handle_edit_key(key(KeyCode::Right));
        // Past the emoji.
        assert_eq!(field.cursor, 5);
    }

    #[test]
    fn masked_value_uses_bullets() {
        let field = TextField::masked_with_value("secret");
        assert_eq!(field.render_value(), "••••••");
    }

    #[test]
    fn masked_empty_renders_empty() {
        let field = TextField::masked();
        assert_eq!(field.render_value(), "");
    }

    #[test]
    fn ctrl_a_jumps_to_start() {
        let mut field = TextField::from("abcdef");
        field.handle_edit_key(ctrl(KeyCode::Char('a')));
        assert_eq!(field.cursor, 0);
    }

    #[test]
    fn ctrl_e_jumps_to_end() {
        let mut field = TextField::from("abcdef");
        field.cursor = 0;
        field.handle_edit_key(ctrl(KeyCode::Char('e')));
        assert_eq!(field.cursor, 6);
    }

    #[test]
    fn ctrl_u_clears() {
        let mut field = TextField::from("abcdef");
        field.handle_edit_key(ctrl(KeyCode::Char('u')));
        assert_eq!(field.value, "");
        assert_eq!(field.cursor, 0);
    }

    #[test]
    fn delete_removes_char_at_cursor() {
        let mut field = TextField::from("abcdef");
        field.cursor = 2;
        field.handle_edit_key(key(KeyCode::Delete));
        assert_eq!(field.value, "abdef");
        assert_eq!(field.cursor, 2);
    }

    #[test]
    fn home_and_end_keys() {
        let mut field = TextField::from("abc");
        field.handle_edit_key(key(KeyCode::Home));
        assert_eq!(field.cursor, 0);
        field.handle_edit_key(key(KeyCode::End));
        assert_eq!(field.cursor, 3);
    }

    #[test]
    fn trimmed_strips_whitespace() {
        let field = TextField::from("  hello  ");
        assert_eq!(field.trimmed(), "hello");
    }

    #[test]
    fn cursor_x_respects_char_width_in_plain_mode() {
        let field = TextField::from("a😀b");
        // Default cursor at end (3 chars visible: 'a', emoji, 'b').
        assert_eq!(field.cursor_x(), 3);
    }

    #[test]
    fn cursor_x_respects_mask() {
        let mut field = TextField::masked_with_value("abc");
        field.cursor = 2;
        assert_eq!(field.cursor_x(), 2);
    }
}
