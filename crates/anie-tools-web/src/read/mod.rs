//! `web_read` — fetch a URL and return clean Markdown.

pub mod extract;
pub mod fetch;
pub mod frontmatter;
pub mod headless;
mod tool;

pub use tool::WebReadTool;
