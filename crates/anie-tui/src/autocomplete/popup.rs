//! Floating popup that renders an `AutocompleteProvider`'s
//! suggestions above (or below) the input pane.
//!
//! Unlike `ModelPickerPane`, this popup is a **pure overlay**: it
//! doesn't replace the editor, it draws on top of it. Focus stays
//! with the editor; key routing happens in `InputPane` (plan 12
//! phase D).
//!
//! Rendering contract:
//! - Two columns per row: `label` and `description` (if any).
//! - Selected row is painted with a highlighted background so the
//!   active suggestion is unambiguous even on dim terminals.
//! - `Clear` blanks the target rect before drawing, preventing
//!   underlying editor glyphs from leaking through.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};

use super::{Suggestion, SuggestionKind, SuggestionSet};
use crate::widgets::SelectList;

/// Maximum rows rendered. Chosen to match pi's default
/// (`autocompleteMaxVisible: 5`).
const DEFAULT_MAX_VISIBLE: usize = 5;

/// Minimum editor height we'll leave when sizing the popup. If
/// there isn't room, the popup won't render at all (see
/// `AutocompletePopup::layout_rect`).
const MIN_EDITOR_ROWS: u16 = 2;

/// State + render for the inline autocomplete popup.
pub(crate) struct AutocompletePopup {
    list: SelectList<Suggestion>,
    prefix: String,
    kind: SuggestionKind,
    max_visible: usize,
}

impl AutocompletePopup {
    /// Wrap a `SuggestionSet` into a renderable popup, with the
    /// selection seeded to the closest prefix match.
    pub(crate) fn from_suggestions(set: SuggestionSet) -> Self {
        Self::with_max_visible(set, DEFAULT_MAX_VISIBLE)
    }

    pub(crate) fn with_max_visible(set: SuggestionSet, max_visible: usize) -> Self {
        let SuggestionSet { items, prefix, kind } = set;
        let mut list = SelectList::new(items, max_visible.max(1));
        let prefix_for_seed = prefix_without_slash(&prefix);
        if !prefix_for_seed.is_empty() {
            let lowered = prefix_for_seed.to_lowercase();
            list.select_first_where(|item| item.value.to_lowercase().starts_with(&lowered));
        }
        Self {
            list,
            prefix,
            kind,
            max_visible: max_visible.max(1),
        }
    }

    /// Currently-highlighted suggestion, if any.
    #[allow(dead_code)]
    pub(crate) fn selected(&self) -> Option<&Suggestion> {
        self.list.selected()
    }

    /// Prefix that will be replaced when a suggestion is applied.
    #[allow(dead_code)]
    pub(crate) fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Whether this popup is for a command name or an argument
    /// value. The editor consults this to decide whether to
    /// insert a trailing space after apply.
    #[allow(dead_code)]
    pub(crate) fn kind(&self) -> &SuggestionKind {
        &self.kind
    }

    /// Move the highlight up (`-1`) or down (`1`).
    #[allow(dead_code)]
    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.list.move_selection(delta);
    }

    /// Desired row count, including the top + bottom border.
    pub(crate) fn height_hint(&self) -> u16 {
        let rows = self.list.height_hint();
        rows.saturating_add(2) // +2 for top/bottom border
    }

    /// Compute the area the popup should occupy, preferring above
    /// the input when there's room, below otherwise. Returns
    /// `None` if there's no room anywhere — the caller should
    /// skip rendering (and probably cancel autocomplete).
    pub(crate) fn layout_rect(&self, frame_area: Rect, input_area: Rect) -> Option<Rect> {
        let desired = self.height_hint();
        if desired == 0 {
            return None;
        }
        let width = input_area.width;
        if width < 4 {
            return None;
        }

        // Try above.
        let rows_above = input_area.y.saturating_sub(frame_area.y);
        if rows_above >= desired {
            let y = input_area.y - desired;
            return Some(Rect::new(input_area.x, y, width, desired));
        }

        // Try below.
        let input_bottom = input_area.y.saturating_add(input_area.height);
        let frame_bottom = frame_area.y.saturating_add(frame_area.height);
        let rows_below = frame_bottom.saturating_sub(input_bottom);
        if rows_below >= desired && input_area.height >= MIN_EDITOR_ROWS {
            return Some(Rect::new(input_area.x, input_bottom, width, desired));
        }

        // Constrained fallback: use whatever's available above the
        // input, minimum two rows (one border + one item).
        if rows_above >= 3 {
            let clamped = rows_above.min(desired);
            let y = input_area.y - clamped;
            return Some(Rect::new(input_area.x, y, width, clamped));
        }

        None
    }

    /// Render into `area`. Callers are responsible for computing
    /// `area` via `layout_rect`.
    pub(crate) fn render(&self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);

        let title = match &self.kind {
            SuggestionKind::CommandName => " commands ".to_string(),
            SuggestionKind::ArgumentValue { command_name } => {
                format!(" /{command_name} values ")
            }
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                title,
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )))
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        block.render(area, buf);

        if inner.height == 0 || inner.width == 0 {
            return;
        }

        // Reserve a column gap + description column.
        let label_width = self
            .list
            .visible()
            .map(|(_, item, _)| item.label.chars().count())
            .max()
            .unwrap_or(0)
            .min(inner.width as usize / 2)
            .max(6);
        let desc_col = label_width + 2; // label + two-space gutter
        let desc_width = (inner.width as usize).saturating_sub(desc_col);

        let lines: Vec<Line<'static>> = self
            .list
            .visible()
            .take(self.max_visible)
            .map(|(_, item, is_selected)| {
                let label = truncate_chars(&item.label, label_width);
                let description = item
                    .description
                    .as_deref()
                    .map(|desc| truncate_chars(desc, desc_width))
                    .unwrap_or_default();

                // Pad the label so the description column aligns.
                let mut label_padded = label;
                let current = label_padded.chars().count();
                if current < label_width {
                    label_padded.extend(std::iter::repeat_n(' ', label_width - current));
                }
                let row = format!("{label_padded}  {description}");
                let style = if is_selected {
                    Style::default()
                        .bg(Color::DarkGray)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                Line::from(Span::styled(row, style))
            })
            .collect();

        Paragraph::new(lines).render(inner, buf);
    }
}

fn prefix_without_slash(prefix: &str) -> &str {
    prefix.strip_prefix('/').unwrap_or(prefix)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    fn set(items: Vec<(&str, Option<&str>)>, prefix: &str, kind: SuggestionKind) -> SuggestionSet {
        SuggestionSet {
            items: items
                .into_iter()
                .map(|(label, description)| Suggestion {
                    value: label.to_string(),
                    label: label.to_string(),
                    description: description.map(String::from),
                })
                .collect(),
            prefix: prefix.to_string(),
            kind,
        }
    }

    fn buffer_to_string(buffer: &Buffer) -> String {
        let area = buffer.area;
        let mut rows = Vec::new();
        for y in 0..area.height {
            let mut row = String::new();
            for x in 0..area.width {
                row.push_str(buffer[(x, y)].symbol());
            }
            rows.push(row.trim_end().to_string());
        }
        rows.join("\n")
    }

    #[test]
    fn renders_label_and_description_columns() {
        let popup = AutocompletePopup::from_suggestions(set(
            vec![
                ("thinking", Some("[off|low|medium|high] — Set effort")),
                ("help", Some("Show help")),
            ],
            "/",
            SuggestionKind::CommandName,
        ));
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 2, 60, 6);
                popup.render(area, frame.buffer_mut());
            })
            .expect("draw");
        let rendered = buffer_to_string(terminal.backend().buffer());
        assert!(rendered.contains("thinking"), "{rendered}");
        assert!(rendered.contains("help"), "{rendered}");
        assert!(rendered.contains("commands"), "{rendered}");
    }

    #[test]
    fn renders_title_for_argument_popup() {
        let popup = AutocompletePopup::from_suggestions(set(
            vec![("off", None), ("low", None)],
            "",
            SuggestionKind::ArgumentValue {
                command_name: "thinking".into(),
            },
        ));
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                popup.render(Rect::new(0, 0, 40, 4), frame.buffer_mut());
            })
            .expect("draw");
        let rendered = buffer_to_string(terminal.backend().buffer());
        assert!(rendered.contains("/thinking values"), "{rendered}");
    }

    #[test]
    fn layout_above_when_space_available() {
        let popup = AutocompletePopup::from_suggestions(set(
            vec![("a", None), ("b", None), ("c", None)],
            "/",
            SuggestionKind::CommandName,
        ));
        let frame_area = Rect::new(0, 0, 80, 24);
        let input_area = Rect::new(0, 20, 80, 3);
        let rect = popup.layout_rect(frame_area, input_area).expect("rect");
        assert!(rect.y < input_area.y, "{rect:?} should be above {input_area:?}");
        assert_eq!(rect.x, input_area.x);
        assert_eq!(rect.width, input_area.width);
    }

    #[test]
    fn layout_below_when_top_is_cramped() {
        let popup = AutocompletePopup::from_suggestions(set(
            vec![("a", None), ("b", None), ("c", None)],
            "/",
            SuggestionKind::CommandName,
        ));
        let frame_area = Rect::new(0, 0, 80, 24);
        // Input anchored at the very top — no room above.
        let input_area = Rect::new(0, 0, 80, 3);
        let rect = popup.layout_rect(frame_area, input_area).expect("rect");
        assert!(
            rect.y >= input_area.y + input_area.height,
            "expected below, got {rect:?}"
        );
    }

    #[test]
    fn layout_returns_none_when_no_room() {
        let popup = AutocompletePopup::from_suggestions(set(
            vec![("a", None), ("b", None), ("c", None)],
            "/",
            SuggestionKind::CommandName,
        ));
        // Tiny frame: only 2 rows, input takes all of it.
        let frame_area = Rect::new(0, 0, 80, 2);
        let input_area = Rect::new(0, 0, 80, 2);
        assert!(popup.layout_rect(frame_area, input_area).is_none());
    }

    #[test]
    fn selected_row_rendered_with_highlight_attribute() {
        let popup = AutocompletePopup::from_suggestions(set(
            vec![("first", None), ("second", None)],
            "/",
            SuggestionKind::CommandName,
        ));
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                popup.render(Rect::new(0, 0, 30, 4), frame.buffer_mut());
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        // Find the cell containing the first letter of "first" and
        // verify its background is DarkGray (the selection color).
        let mut found = false;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "f" && cell.style().bg == Some(Color::DarkGray) {
                    found = true;
                    break;
                }
            }
        }
        assert!(
            found,
            "expected the 'first' row to be highlighted with DarkGray bg"
        );
    }

    #[test]
    fn long_description_truncates_with_ellipsis() {
        let popup = AutocompletePopup::from_suggestions(set(
            vec![(
                "cmd",
                Some("this description is much longer than the available width for it"),
            )],
            "/",
            SuggestionKind::CommandName,
        ));
        let backend = TestBackend::new(30, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                popup.render(Rect::new(0, 0, 30, 3), frame.buffer_mut());
            })
            .expect("draw");
        let rendered = buffer_to_string(terminal.backend().buffer());
        assert!(rendered.contains('…'), "{rendered}");
    }

    #[test]
    fn height_hint_clamps_to_items_plus_borders() {
        let popup = AutocompletePopup::with_max_visible(
            set(
                vec![("a", None), ("b", None), ("c", None)],
                "/",
                SuggestionKind::CommandName,
            ),
            10,
        );
        // 3 items + 2 border rows = 5
        assert_eq!(popup.height_hint(), 5);
    }

    #[test]
    fn prefix_seeds_selection_to_best_match() {
        let popup = AutocompletePopup::from_suggestions(set(
            vec![("apple", None), ("apricot", None), ("banana", None)],
            "/ba",
            SuggestionKind::CommandName,
        ));
        let selected = popup.selected().expect("selected");
        assert_eq!(selected.label, "banana");
    }
}
