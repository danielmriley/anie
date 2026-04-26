//! Shared layout / styling helpers for overlay panels.

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap},
};

/// Compute a rectangle centered in `area` bounded by percentage-of-area
/// limits with explicit minimums.
///
/// - `max_width_pct` / `max_height_pct` cap the size at a fraction of
///   the outer rectangle.
/// - `min_width` / `min_height` define a floor so the panel is still
///   usable when `area` is small.
pub(crate) fn centered_rect(
    area: Rect,
    max_width_pct: u16,
    max_height_pct: u16,
    min_width: u16,
    min_height: u16,
) -> Rect {
    let width = ((area.width as u32 * max_width_pct as u32) / 100)
        .max(min_width as u32)
        .min(area.width as u32) as u16;
    let height = ((area.height as u32 * max_height_pct as u32) / 100)
        .max(min_height as u32)
        .min(area.height as u32) as u16;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

/// Render a one-line footer hint in the overlay's muted style.
pub(crate) fn footer_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().add_modifier(Modifier::DIM),
    ))
}

/// Render a centered placeholder panel for not-yet-implemented overlays.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn render_placeholder_panel(frame: &mut Frame<'_>, area: Rect, title: &str, body: &str) {
    Clear.render(area, frame.buffer_mut());
    let panel = centered_rect(area, 70, 45, 36, 9);
    let block = Block::default()
        .title(Line::from(vec![Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().add_modifier(Modifier::DIM));
    let inner = block.inner(panel);

    Clear.render(panel, frame.buffer_mut());
    block.render(panel, frame.buffer_mut());

    let mut lines = body
        .lines()
        .map(|line| Line::from(line.to_string()))
        .collect::<Vec<_>>();
    lines.push(Line::default());
    lines.push(footer_line("Press any key to close."));

    Paragraph::new(lines)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .render(inner, frame.buffer_mut());
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    use super::*;

    fn render_buffer_to_string(buffer: &Buffer) -> String {
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
    fn centered_rect_caps_to_percentage() {
        let area = Rect::new(0, 0, 100, 50);
        let inner = centered_rect(area, 50, 40, 10, 5);
        assert_eq!(inner.width, 50);
        assert_eq!(inner.height, 20);
        // Centered: x = (100-50)/2 = 25, y = (50-20)/2 = 15.
        assert_eq!(inner.x, 25);
        assert_eq!(inner.y, 15);
    }

    #[test]
    fn centered_rect_respects_minimums() {
        let area = Rect::new(0, 0, 100, 50);
        // Percentage would give 10x5, but mins are 40x20.
        let inner = centered_rect(area, 10, 10, 40, 20);
        assert_eq!(inner.width, 40);
        assert_eq!(inner.height, 20);
    }

    #[test]
    fn centered_rect_clamps_to_area() {
        let area = Rect::new(0, 0, 20, 10);
        // Minimums exceed the area; clamp to the area size.
        let inner = centered_rect(area, 50, 50, 100, 100);
        assert_eq!(inner.width, 20);
        assert_eq!(inner.height, 10);
    }

    #[test]
    fn footer_line_applies_dim_modifier() {
        let line = footer_line("hello");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content, "hello");
        assert_eq!(line.spans[0].style.fg, None);
        assert!(line.spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn placeholder_panel_renders_body_and_footer() {
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_placeholder_panel(
                    frame,
                    frame.area(),
                    "Settings",
                    "Configuration screen not yet implemented.",
                );
            })
            .expect("draw placeholder panel");
        let rendered = render_buffer_to_string(terminal.backend().buffer());
        assert!(rendered.contains("Settings"));
        assert!(rendered.contains("Configuration"));
        assert!(rendered.contains("implemented"));
        assert!(rendered.contains("Press any key to close"));
    }
}
