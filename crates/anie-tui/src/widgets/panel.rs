//! Shared layout / styling helpers for overlay panels.

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
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
        Style::default().fg(Color::DarkGray),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn footer_line_applies_dark_gray() {
        let line = footer_line("hello");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content, "hello");
        assert_eq!(line.spans[0].style.fg, Some(Color::DarkGray));
    }
}
