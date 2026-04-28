//! Defuddle bridge.
//!
//! `web_read` shells out to Defuddle's CLI rather than
//! embedding a JS runtime. This module wraps the subprocess
//! and parses the JSON output.
//!
//! For testing, [`DefuddleRunner`] is a trait so tests can
//! inject a mock implementation that returns canned output.
//! The real implementation, [`SubprocessDefuddleRunner`],
//! locates `defuddle` (or falls through to `npx`), writes the
//! fetched HTML to a tempfile, runs `defuddle parse <file>
//! --markdown --json`, and parses stdout.
//!
//! Defuddle 0.7+ merged the `defuddle-cli` package into the
//! main `defuddle` package and replaced the stdin-pipe
//! invocation with a `parse <source>` subcommand that takes a
//! file path or URL. We pass a tempfile so anie's SSRF guard,
//! robots check, rate limit, and size cap remain in force —
//! handing the URL directly to Defuddle would let it bypass
//! all of those.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::task::spawn_blocking;

use crate::error::WebToolError;

/// Pinned Defuddle version used when falling through to
/// `npx`. Bump deliberately — bumping should be paired with
/// regenerating any output fixtures and rerunning the
/// integration tests.
///
/// Pinned to the 0.18 line, which is the first release where
/// `defuddle-cli` was deprecated in favor of the merged
/// `defuddle` package. Earlier `0.6.x` pins targeted the old
/// stdin-pipe CLI shape and no longer work.
pub const DEFUDDLE_VERSION: &str = "0.18.x";

/// Defuddle's JSON output. Optional fields use
/// `#[serde(default)]` so a Defuddle release that adds or
/// drops a field doesn't break parsing. Field names follow
/// Defuddle's camelCase convention via `rename_all`.
///
/// Defuddle's `content` field carries the cleaned body —
/// markdown when the CLI was invoked with `--markdown` (which
/// `SubprocessDefuddleRunner` always does), HTML otherwise.
/// `parse_defuddle_output` accepts both shapes so fixtures
/// remain stable across CLI flag changes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DefuddleOutput {
    /// Title extracted from `<title>` / OpenGraph / etc.
    pub title: Option<String>,
    /// Article author when extractable.
    pub author: Option<String>,
    /// Page description / lede.
    pub description: Option<String>,
    /// Hostname / domain (`example.com`).
    pub domain: Option<String>,
    /// Site name (often the same as domain, but Defuddle
    /// extracts a friendlier label when available).
    pub site: Option<String>,
    /// Favicon URL.
    pub favicon: Option<String>,
    /// Header / hero image URL.
    pub image: Option<String>,
    /// ISO 639 language code.
    pub language: Option<String>,
    /// Published date as Defuddle returned it (ISO 8601
    /// where extractable, source-format string otherwise).
    pub published: Option<String>,
    /// Word count of the extracted content.
    pub word_count: Option<u64>,
    /// Estimated reading time in minutes.
    pub reading_time: Option<u32>,
    /// Cleaned body. Markdown when `SubprocessDefuddleRunner`
    /// invoked the CLI with `--markdown` (the default). The
    /// JSON key is plain `content`; we keep the Rust field
    /// name to make `markdown_body()` self-documenting.
    #[serde(rename = "content")]
    pub content_markdown: Option<String>,
}

impl DefuddleOutput {
    /// Return the rendered Markdown body, falling back to a
    /// placeholder when Defuddle didn't emit one. The fallback
    /// is rare (only when the CLI was invoked without
    /// `--markdown` for some reason); we surface a clear note
    /// rather than erroring so the agent still gets metadata.
    #[must_use]
    pub fn markdown_body(&self) -> String {
        match &self.content_markdown {
            Some(md) if !md.is_empty() => md.clone(),
            _ => "_(no markdown body extracted)_".to_string(),
        }
    }
}

/// Trait abstracting the Defuddle invocation so tests can
/// substitute a deterministic implementation.
#[async_trait]
pub trait DefuddleRunner: Send + Sync {
    /// Run Defuddle against `html`, with `source_url` provided
    /// for relative-link resolution and metadata.
    async fn run(&self, html: &str, source_url: &str) -> Result<DefuddleOutput, WebToolError>;
}

/// Production implementation: spawns the `defuddle` CLI as a
/// subprocess.
#[derive(Default)]
pub struct SubprocessDefuddleRunner;

#[async_trait]
impl DefuddleRunner for SubprocessDefuddleRunner {
    async fn run(&self, html: &str, _source_url: &str) -> Result<DefuddleOutput, WebToolError> {
        // The Defuddle 0.18 CLI takes a file path or URL — no
        // stdin. We write the already-fetched HTML to a tempfile
        // so anie's SSRF guard, robots check, rate limit, and
        // size cap stay in force; passing the URL would let
        // Defuddle re-fetch and bypass all of those.
        let tmp = spawn_blocking(|| {
            tempfile::Builder::new()
                .prefix("anie-web-")
                .suffix(".html")
                .tempfile()
        })
        .await
        .map_err(|e| WebToolError::DefuddleSpawn(format!("tempfile join: {e}")))?
        .map_err(|e| WebToolError::DefuddleSpawn(format!("tempfile create: {e}")))?;
        let tmp_path = tmp.path().to_path_buf();
        {
            let mut f = tokio::fs::File::create(&tmp_path).await?;
            f.write_all(html.as_bytes()).await?;
            f.flush().await?;
        }

        let cmd = locate_defuddle()?;
        let mut command = Command::new(&cmd.binary);
        command.args(&cmd.args);
        command.arg("parse");
        command.arg(&tmp_path);
        command.args(["--markdown", "--json"]);
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let child = command
            .spawn()
            .map_err(|e| WebToolError::DefuddleSpawn(e.to_string()))?;

        let output = child.wait_with_output().await?;
        // Keep `tmp` alive until after the child has read it.
        drop(tmp);

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(WebToolError::DefuddleFailed {
                exit_code: output.status.code(),
                stderr,
            });
        }
        parse_defuddle_output(&output.stdout)
    }
}

/// Resolved invocation strategy for the Defuddle subprocess.
struct DefuddleCmd {
    binary: PathBuf,
    args: Vec<String>,
}

/// Locate the `defuddle` binary or fall through to `npx`.
///
/// Lookup order:
/// 1. `defuddle` directly on PATH (fastest, no npm overhead).
/// 2. `npx defuddle@<DEFUDDLE_VERSION>` if `npx` is on PATH.
///    First call may pay an install cost; subsequent calls
///    use npm's cache.
/// 3. Neither available — surface a `DefuddleNotFound` error
///    with install instructions in its `Display`.
fn locate_defuddle() -> Result<DefuddleCmd, WebToolError> {
    if let Ok(path) = which::which("defuddle") {
        return Ok(DefuddleCmd {
            binary: path,
            args: vec![],
        });
    }
    if let Ok(npx) = which::which("npx") {
        return Ok(DefuddleCmd {
            binary: npx,
            args: vec!["--yes".into(), format!("defuddle@{DEFUDDLE_VERSION}")],
        });
    }
    Err(WebToolError::DefuddleNotFound)
}

/// Parse Defuddle's stdout JSON into [`DefuddleOutput`].
/// Public so tests can verify the parse against fixtures
/// without spawning a subprocess.
pub fn parse_defuddle_output(bytes: &[u8]) -> Result<DefuddleOutput, WebToolError> {
    let parsed: DefuddleOutput = serde_json::from_slice(bytes)?;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_defuddle_output_handles_full_document() {
        // Mirrors the JSON shape Defuddle 0.18 emits when run
        // with `--markdown --json`: `content` carries the
        // markdown body, no separate `contentMarkdown` field.
        let json = r##"{
            "title": "Hello world",
            "author": "Jane Doe",
            "description": "A test article",
            "domain": "example.com",
            "site": "Example",
            "language": "en",
            "published": "2024-08-15T10:30:00Z",
            "wordCount": 1234,
            "content": "# Hello\n\nBody."
        }"##;
        let parsed = parse_defuddle_output(json.as_bytes()).expect("parses");
        assert_eq!(parsed.title.as_deref(), Some("Hello world"));
        assert_eq!(parsed.author.as_deref(), Some("Jane Doe"));
        assert_eq!(parsed.domain.as_deref(), Some("example.com"));
        assert_eq!(parsed.word_count, Some(1234));
        assert!(
            parsed
                .content_markdown
                .as_deref()
                .unwrap()
                .contains("# Hello")
        );
    }

    #[test]
    fn parse_defuddle_output_handles_minimal_document() {
        // No required fields. Only title.
        let json = r#"{"title": "Minimal"}"#;
        let parsed = parse_defuddle_output(json.as_bytes()).expect("parses");
        assert_eq!(parsed.title.as_deref(), Some("Minimal"));
        assert!(parsed.author.is_none());
        assert!(parsed.content_markdown.is_none());
    }

    #[test]
    fn parse_defuddle_output_handles_extra_fields() {
        // Defuddle ships fields we don't model; parse should
        // ignore unknown keys without erroring.
        let json = r#"{
            "title": "T",
            "metaTags": {"og:locale": "en_US"},
            "schemaOrgData": [],
            "extractorType": "default",
            "futureField": "value"
        }"#;
        let parsed = parse_defuddle_output(json.as_bytes()).expect("parses");
        assert_eq!(parsed.title.as_deref(), Some("T"));
    }

    #[test]
    fn parse_defuddle_output_rejects_garbage() {
        let json = b"not json at all";
        let err = parse_defuddle_output(json).unwrap_err();
        assert!(matches!(err, WebToolError::DefuddleOutputParse(_)));
    }

    #[test]
    fn markdown_body_falls_back_to_placeholder_when_missing() {
        let out = DefuddleOutput {
            title: Some("T".into()),
            ..DefuddleOutput::default()
        };
        let body = out.markdown_body();
        assert!(body.contains("no markdown body"));
    }

    #[test]
    fn markdown_body_returns_content_when_present() {
        let out = DefuddleOutput {
            content_markdown: Some("# Heading\n\nBody.".into()),
            ..DefuddleOutput::default()
        };
        assert_eq!(out.markdown_body(), "# Heading\n\nBody.");
    }
}
