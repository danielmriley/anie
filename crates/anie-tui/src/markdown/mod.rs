//! Markdown → ratatui line rendering.
//!
//! Entry point: [`render_markdown`]. Call on finalized assistant
//! content only — per Plan 05's streaming-block caveat, a
//! streaming block's content changes every delta and would
//! re-parse markdown on every frame. `OutputPane` handles that
//! gating in Plan 05 PR E; until then, `render_markdown` is
//! unused in production and exists only for direct tests and
//! for the final wire-up PR.
//!
//! Adopts pi's per-component `(text, width) -> lines` cache
//! pattern at the `OutputPane` block-cache layer (PR 2 of
//! `tui_responsiveness/`), not inside this module. Rendering is
//! a pure function of `(text, width, theme)` so the existing
//! block cache captures it cleanly.

mod layout;
mod parser;
mod syntax;
mod theme;

// Re-exports for the forthcoming PR E wire-up. `#[allow(unused_imports)]`
// matches the module-level dead-code suppression in lib.rs.
#[allow(unused_imports)]
pub use layout::render as render_markdown;
#[allow(unused_imports)]
pub use theme::MarkdownTheme;
