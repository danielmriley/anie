//! Reusable TUI widgets shared across overlays and the main app.

pub(crate) mod panel;
pub(crate) mod text_field;

pub(crate) use panel::{centered_rect, footer_line};
pub(crate) use text_field::TextField;
