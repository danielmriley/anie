//! Web reading and search tools for the anie agent.
//!
//! Two tools live here, both registered through the existing
//! `anie_agent::ToolRegistry`:
//!
//! - `web_read` — fetch a URL and return clean Markdown via
//!   the [Defuddle](https://github.com/kepano/defuddle)
//!   reader-mode extractor.
//! - `web_search` — query a search backend (DuckDuckGo HTML
//!   by default) and return ranked URLs + snippets.
//!
//! See `docs/web_tool_2026-04-26/` in the anie repository for
//! the design rationale.
//!
//! ## Prerequisites
//!
//! `web_read` shells out to a `defuddle` (or `npx defuddle`)
//! subprocess at runtime — same pattern as the bash tool
//! requiring `/bin/sh`. Install with `npm i -g defuddle-cli`
//! to enable the tool. Without it, the tool registers but
//! returns a clear `DefuddleNotFound` error on use.
//!
//! `web_read` with `javascript: true` additionally requires a
//! Chrome / Chromium binary on the system. Build the crate
//! with `--features headless` to enable that path.

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod error;
pub mod read;

pub use error::WebToolError;
