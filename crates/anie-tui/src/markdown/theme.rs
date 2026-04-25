//! Style theme for markdown rendering.
//!
//! One `MarkdownTheme` value per active terminal theme. Built-in
//! constants cover the "dark" default used throughout the rest of
//! the TUI; a "light" variant lands later if we surface a
//! user-configurable theme picker.

use ratatui::style::{Color, Modifier, Style};

/// Styles applied to rendered markdown elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownTheme {
    pub h1: Style,
    pub h2: Style,
    pub h3: Style,
    pub h4_plus: Style,
    pub strong: Style,
    pub emphasis: Style,
    pub strikethrough: Style,
    pub inline_code: Style,
    pub code_block_border: Style,
    pub code_block_lang: Style,
    pub list_bullet: Style,
    pub blockquote_gutter: Style,
    pub blockquote_body: Style,
    pub horizontal_rule: Style,
    pub table_border: Style,
    pub table_header: Style,
    pub table_cell: Style,
    pub link_text: Style,
    pub link_url: Style,
}

impl MarkdownTheme {
    /// Default dark theme matching the existing anie TUI palette.
    /// Tested under iTerm2, Kitty, xterm, and tmux.
    #[must_use]
    pub const fn default_dark() -> Self {
        Self {
            h1: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            h2: Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            h3: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            h4_plus: Style::new().add_modifier(Modifier::BOLD),
            strong: Style::new().add_modifier(Modifier::BOLD),
            emphasis: Style::new().add_modifier(Modifier::ITALIC),
            strikethrough: Style::new().add_modifier(Modifier::CROSSED_OUT),
            inline_code: Style::new().fg(Color::Yellow),
            code_block_border: Style::new().fg(Color::DarkGray),
            code_block_lang: Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
            list_bullet: Style::new().fg(Color::Cyan),
            blockquote_gutter: Style::new().fg(Color::DarkGray),
            blockquote_body: Style::new().add_modifier(Modifier::ITALIC),
            horizontal_rule: Style::new().fg(Color::DarkGray),
            table_border: Style::new().fg(Color::DarkGray),
            table_header: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            table_cell: Style::new(),
            link_text: Style::new()
                .fg(Color::Blue)
                .add_modifier(Modifier::UNDERLINED),
            link_url: Style::new().fg(Color::DarkGray),
        }
    }
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        Self::default_dark()
    }
}
