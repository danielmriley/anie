//! `web_read` — fetch a URL and return clean Markdown.
//!
//! PR 1 (this PR) wires up the fetch path: URL validation +
//! SSRF guard, robots.txt check, per-host rate limiting,
//! HTTP fetching with size and redirect bounds.
//!
//! PR 2 will add the Defuddle subprocess bridge and the
//! `WebReadTool` impl that consumes this fetch surface.

pub mod fetch;
