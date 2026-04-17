use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget, Wrap},
};

use anie_provider::ModelInfo;

/// Outcome of processing a key in the model picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelPickerAction {
    /// Keep rendering the picker.
    Continue,
    /// User selected a model.
    Selected(ModelInfo),
    /// User cancelled the picker.
    Cancelled,
    /// User requested a refresh.
    Refresh,
}

/// Search-first picker for choosing a model from a provider.
#[derive(Debug, Clone)]
pub struct ModelPickerPane {
    models: Vec<ModelInfo>,
    filtered_indices: Vec<usize>,
    selected: usize,
    scroll: usize,
    search: SearchField,
    current_provider: String,
    current_model_id: String,
    loading: bool,
    error: Option<String>,
}

impl ModelPickerPane {
    /// Create a picker with an initial model list and optional prefilled search.
    #[must_use]
    pub fn new(
        models: Vec<ModelInfo>,
        current_provider: String,
        current_model_id: String,
        initial_search: Option<String>,
    ) -> Self {
        let mut pane = Self {
            models,
            filtered_indices: Vec::new(),
            selected: 0,
            scroll: 0,
            search: SearchField::from(initial_search.unwrap_or_default()),
            current_provider,
            current_model_id,
            loading: false,
            error: None,
        };
        pane.apply_filter(None);
        pane
    }

    /// Replace the backing model list while preserving search text and selection when possible.
    pub fn set_models(&mut self, models: Vec<ModelInfo>) {
        let selected_id = self.selected_model().map(|model| model.id.clone());
        self.models = models;
        self.loading = false;
        self.error = None;
        self.apply_filter(selected_id.as_deref());
    }

    /// Toggle loading state.
    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
        if loading {
            self.error = None;
        }
    }

    /// Set an inline error message.
    pub fn set_error(&mut self, error: Option<String>) {
        self.loading = false;
        self.error = error;
    }

    /// Return the current search text.
    #[must_use]
    pub fn search(&self) -> &str {
        self.search.value()
    }

    /// Preferred height in rows for the current content.
    #[must_use]
    pub fn preferred_height(&self, _width: u16) -> u16 {
        let list_rows = if self.loading {
            3
        } else {
            self.filtered_indices.len().clamp(3, 8)
        } as u16;
        let error_rows = u16::from(self.error.is_some());
        (5 + list_rows + error_rows).clamp(8, 14)
    }

    /// Render the picker and return the cursor position for the search input.
    pub fn render(&self, area: Rect, buf: &mut Buffer, spinner_frame: &str) -> Position {
        let title = if self.current_provider.is_empty() {
            " Select Model ".to_string()
        } else {
            format!(" Select Model — {} ", self.current_provider)
        };
        let block = Block::default()
            .title(Line::from(Span::styled(
                title,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.height == 0 || inner.width == 0 {
            return Position::new(area.x, area.y);
        }

        let footer = self.footer_line();
        let error_height = u16::from(self.error.is_some());
        let footer_y = inner.y + inner.height.saturating_sub(1);
        let error_y = footer_y.saturating_sub(error_height);
        let search_y = inner.y;
        let list_top = search_y.saturating_add(1);
        let list_bottom_exclusive = error_y;
        let list_height = list_bottom_exclusive.saturating_sub(list_top).max(1);

        Paragraph::new(Line::from(vec![
            Span::styled("Search: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                self.search.render_value(),
                Style::default().fg(Color::White),
            ),
        ]))
        .render(Rect::new(inner.x, search_y, inner.width, 1), buf);

        let list_lines = self.render_list_lines(list_height as usize, inner.width, spinner_frame);
        Paragraph::new(list_lines)
            .wrap(Wrap { trim: false })
            .render(Rect::new(inner.x, list_top, inner.width, list_height), buf);

        if let Some(error) = &self.error {
            Paragraph::new(Line::from(Span::styled(
                truncate_chars(error, inner.width as usize),
                Style::default().fg(Color::Red),
            )))
            .render(Rect::new(inner.x, error_y, inner.width, 1), buf);
        }

        Paragraph::new(Line::from(Span::styled(
            footer,
            Style::default().fg(Color::DarkGray),
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
    pub fn handle_key(&mut self, key: KeyEvent) -> ModelPickerAction {
        if let KeyCode::Char(ch) = key.code
            && is_text_input_modifiers(key.modifiers)
        {
            if (ch == 'r' || ch == 'R') && self.search.value().is_empty() {
                return ModelPickerAction::Refresh;
            }
            self.search.insert_char(ch);
            self.apply_filter(None);
            return ModelPickerAction::Continue;
        }

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => ModelPickerAction::Cancelled,
            (KeyModifiers::NONE, KeyCode::Enter) => self
                .selected_model()
                .cloned()
                .map_or(ModelPickerAction::Continue, ModelPickerAction::Selected),
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                self.search.backspace();
                self.apply_filter(None);
                ModelPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Delete) => {
                self.search.delete();
                self.apply_filter(None);
                ModelPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Left) => {
                self.search.move_left();
                ModelPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                self.search.move_right();
                ModelPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Home) => {
                self.selected = 0;
                self.scroll = 0;
                ModelPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                self.selected = self.filtered_indices.len().saturating_sub(1);
                self.ensure_selection_visible(8);
                ModelPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Up) | (KeyModifiers::NONE, KeyCode::Char('k')) => {
                self.move_selection(-1);
                ModelPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Down) | (KeyModifiers::NONE, KeyCode::Char('j')) => {
                self.move_selection(1);
                ModelPickerAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Char('r'))
            | (KeyModifiers::CONTROL, KeyCode::Char('r'))
            | (KeyModifiers::NONE, KeyCode::F(5)) => ModelPickerAction::Refresh,
            _ => ModelPickerAction::Continue,
        }
    }

    fn render_list_lines(
        &self,
        list_height: usize,
        width: u16,
        spinner_frame: &str,
    ) -> Vec<Line<'static>> {
        if self.loading {
            return vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("{spinner_frame} Discovering models…"),
                    Style::default().fg(Color::Cyan),
                )),
            ];
        }

        if self.filtered_indices.is_empty() {
            return vec![
                Line::from(""),
                Line::from(Span::styled(
                    "No matching models",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
        }

        let visible = list_height.max(1).min(10);
        let start = self
            .scroll
            .min(self.filtered_indices.len().saturating_sub(1));
        let end = (start + visible).min(self.filtered_indices.len());
        let rows = self.filtered_indices[start..end]
            .iter()
            .enumerate()
            .map(|(offset, index)| {
                let row_index = start + offset;
                let model = &self.models[*index];
                let is_selected = row_index == self.selected;
                let is_current = model.id == self.current_model_id;
                render_model_row(model, is_selected, is_current, width)
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            vec![Line::from("")]
        } else {
            rows
        }
    }

    fn footer_line(&self) -> String {
        let count = self.filtered_indices.len();
        let position = if count == 0 { 0 } else { self.selected + 1 };
        format!("[↑↓] Navigate  [Enter] Select  [r] Refresh  [Esc] Cancel   ({position}/{count})")
    }

    fn selected_model(&self) -> Option<&ModelInfo> {
        self.filtered_indices
            .get(self.selected)
            .and_then(|index| self.models.get(*index))
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
        self.ensure_selection_visible(8);
    }

    fn ensure_selection_visible(&mut self, visible_rows: usize) {
        let visible_rows = visible_rows.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible_rows {
            self.scroll = self.selected + 1 - visible_rows;
        }
    }

    fn apply_filter(&mut self, preferred_id: Option<&str>) {
        let search = self.search.value().to_ascii_lowercase();
        self.filtered_indices = self
            .models
            .iter()
            .enumerate()
            .filter(|(_, model)| {
                if search.is_empty() {
                    return true;
                }
                model.id.to_ascii_lowercase().contains(&search)
                    || model.name.to_ascii_lowercase().contains(&search)
            })
            .map(|(index, _)| index)
            .collect();

        if self.filtered_indices.is_empty() {
            self.selected = 0;
            self.scroll = 0;
            return;
        }

        if let Some(preferred_id) = preferred_id
            && let Some(position) = self
                .filtered_indices
                .iter()
                .position(|index| self.models[*index].id == preferred_id)
        {
            self.selected = position;
            self.ensure_selection_visible(8);
            return;
        }

        if self.selected >= self.filtered_indices.len() {
            self.selected = 0;
        }
        self.ensure_selection_visible(8);
    }
}

fn render_model_row(
    model: &ModelInfo,
    is_selected: bool,
    is_current: bool,
    width: u16,
) -> Line<'static> {
    let prefix = if is_selected { "› " } else { "  " };
    let badge = format!(" [{}]", model.provider);
    let marker = if is_current { " ✓" } else { "" };
    let reserved = prefix.chars().count() + badge.chars().count() + marker.chars().count();
    let label_width = width as usize;
    let available = label_width.saturating_sub(reserved).max(4);
    let label = truncate_chars(&display_label(model), available);
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
    spans.push(Span::styled(
        badge,
        row_style.fg(if is_selected {
            Color::Gray
        } else {
            Color::DarkGray
        }),
    ));
    if is_current {
        spans.push(Span::styled(marker, row_style.fg(Color::Green)));
    }
    Line::from(spans)
}

fn display_label(model: &ModelInfo) -> String {
    if model.name == model.id {
        model.id.clone()
    } else {
        format!("{} — {}", model.name, model.id)
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else if max_chars <= 1 {
        "…".to_string()
    } else {
        let truncated = text
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>();
        format!("{truncated}…")
    }
}

fn is_text_input_modifiers(modifiers: KeyModifiers) -> bool {
    matches!(modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
}

#[derive(Debug, Clone, Default)]
struct SearchField {
    value: String,
    cursor: usize,
}

impl SearchField {
    fn from(value: String) -> Self {
        let cursor = value.len();
        Self { value, cursor }
    }

    fn value(&self) -> &str {
        &self.value
    }

    fn render_value(&self) -> String {
        self.value.clone()
    }

    fn cursor_x(&self) -> u16 {
        u16::try_from(self.value[..self.cursor].chars().count()).unwrap_or(u16::MAX)
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

fn previous_boundary(text: &str, cursor: usize) -> Option<usize> {
    if cursor == 0 {
        return None;
    }
    text[..cursor].char_indices().last().map(|(index, _)| index)
}

fn next_boundary(text: &str, cursor: usize) -> Option<usize> {
    if cursor >= text.len() {
        return None;
    }
    let mut iter = text[cursor..].char_indices();
    let _ = iter.next();
    iter.next()
        .map(|(offset, _)| cursor + offset)
        .or(Some(text.len()))
}

#[cfg(test)]
mod tests {
    use ratatui::{
        Terminal,
        backend::{Backend, TestBackend},
    };

    use super::*;

    fn models() -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: "qwen3:32b".into(),
                name: "Qwen 3 32B".into(),
                provider: "ollama".into(),
                context_length: Some(32_768),
                supports_images: Some(false),
                supports_reasoning: Some(true),
            },
            ModelInfo {
                id: "qwen3:8b".into(),
                name: "Qwen 3 8B".into(),
                provider: "ollama".into(),
                context_length: Some(32_768),
                supports_images: Some(false),
                supports_reasoning: Some(true),
            },
            ModelInfo {
                id: "gpt-4o".into(),
                name: "GPT-4o".into(),
                provider: "openai".into(),
                context_length: Some(128_000),
                supports_images: Some(true),
                supports_reasoning: Some(false),
            },
        ]
    }

    fn render_to_string(backend: &TestBackend) -> String {
        backend
            .buffer()
            .content()
            .chunks(backend.size().expect("backend size").width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn preferred_height_scales_with_model_count() {
        let picker = ModelPickerPane::new(models(), "ollama".into(), "qwen3:32b".into(), None);
        assert!(picker.preferred_height(80) >= 8);
    }

    #[test]
    fn search_filters_by_id_and_name() {
        let mut picker = ModelPickerPane::new(models(), "ollama".into(), "qwen3:32b".into(), None);
        let _ = picker.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        let _ = picker.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        let _ = picker.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(picker.filtered_indices.len(), 1);
        assert_eq!(
            picker.selected_model().expect("selected model").id,
            "gpt-4o"
        );
    }

    #[test]
    fn empty_search_shows_all_models() {
        let picker = ModelPickerPane::new(models(), "ollama".into(), "qwen3:32b".into(), None);
        assert_eq!(picker.filtered_indices.len(), 3);
    }

    #[test]
    fn no_results_render_cleanly() {
        let mut picker = ModelPickerPane::new(models(), "ollama".into(), "qwen3:32b".into(), None);
        let _ = picker.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));

        let mut terminal = Terminal::new(TestBackend::new(50, 10)).expect("terminal");
        terminal
            .draw(|frame| {
                let _ = picker.render(frame.area(), frame.buffer_mut(), "⠋");
            })
            .expect("draw frame");
        let screen = render_to_string(terminal.backend());
        assert!(
            screen.contains("No matching models"),
            "screen was:\n{screen}"
        );
    }

    #[test]
    fn selection_wraps_at_list_boundaries() {
        let mut picker = ModelPickerPane::new(models(), "ollama".into(), "qwen3:32b".into(), None);
        let _ = picker.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            picker.selected_model().expect("selected model").id,
            "gpt-4o"
        );
        let _ = picker.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            picker.selected_model().expect("selected model").id,
            "qwen3:32b"
        );
    }

    #[test]
    fn enter_returns_selected_model() {
        let mut picker = ModelPickerPane::new(models(), "ollama".into(), "qwen3:32b".into(), None);
        assert!(matches!(
            picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ModelPickerAction::Selected(model) if model.id == "qwen3:32b"
        ));
    }

    #[test]
    fn escape_returns_cancelled() {
        let mut picker = ModelPickerPane::new(models(), "ollama".into(), "qwen3:32b".into(), None);
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ModelPickerAction::Cancelled
        );
    }

    #[test]
    fn r_requests_refresh_when_search_is_empty() {
        let mut picker = ModelPickerPane::new(models(), "ollama".into(), "qwen3:32b".into(), None);
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            ModelPickerAction::Refresh
        );
    }

    #[test]
    fn loading_state_renders_spinner() {
        let mut picker = ModelPickerPane::new(models(), "ollama".into(), "qwen3:32b".into(), None);
        picker.set_loading(true);

        let mut terminal = Terminal::new(TestBackend::new(50, 10)).expect("terminal");
        terminal
            .draw(|frame| {
                let _ = picker.render(frame.area(), frame.buffer_mut(), "⠙");
            })
            .expect("draw frame");
        let screen = render_to_string(terminal.backend());
        assert!(
            screen.contains("⠙ Discovering models…"),
            "screen was:\n{screen}"
        );
    }

    #[test]
    fn inline_error_renders() {
        let mut picker = ModelPickerPane::new(models(), "ollama".into(), "qwen3:32b".into(), None);
        picker.set_error(Some("Failed to refresh".into()));

        let mut terminal = Terminal::new(TestBackend::new(50, 10)).expect("terminal");
        terminal
            .draw(|frame| {
                let _ = picker.render(frame.area(), frame.buffer_mut(), "⠋");
            })
            .expect("draw frame");
        let screen = render_to_string(terminal.backend());
        assert!(
            screen.contains("Failed to refresh"),
            "screen was:\n{screen}"
        );
    }

    #[test]
    fn current_model_marker_renders_on_matching_row() {
        let picker = ModelPickerPane::new(models(), "ollama".into(), "qwen3:32b".into(), None);

        let mut terminal = Terminal::new(TestBackend::new(50, 10)).expect("terminal");
        terminal
            .draw(|frame| {
                let _ = picker.render(frame.area(), frame.buffer_mut(), "⠋");
            })
            .expect("draw frame");
        let screen = render_to_string(terminal.backend());
        assert!(screen.contains("✓"), "screen was:\n{screen}");
    }
}
