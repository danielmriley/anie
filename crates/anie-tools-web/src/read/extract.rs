//! Defuddle bridge.
//!
//! `web_read` shells out to Defuddle's CLI rather than
//! embedding a JS runtime. This module wraps the subprocess
//! and parses the JSON output.
//!
//! For testing, [`DefuddleRunner`] is a trait so tests can
//! inject a mock implementation that returns canned output.
//! The real implementation, [`SubprocessDefuddleRunner`],
//! locates `defuddle` (or falls through to `npx`), spawns it,
//! pipes HTML to stdin, and parses stdout.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::WebToolError;

/// Pinned Defuddle version used when falling through to
/// `npx`. Bump deliberately — bumping should be paired with
/// regenerating any output fixtures and rerunning the
/// integration tests.
pub const DEFUDDLE_VERSION: &str = "0.6.x";

/// Defuddle's JSON output. Optional fields use
/// `#[serde(default)]` so a Defuddle release that adds or
/// drops a field doesn't break parsing. Field names follow
/// Defuddle's camelCase convention via `rename_all`.
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
    /// Cleaned Markdown body. Present when the CLI was run
    /// with the `--markdown` flag.
    pub content_markdown: Option<String>,
    /// Cleaned HTML body. Always present.
    pub content: Option<String>,
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
    async fn run(
        &self,
        html: &str,
        source_url: &str,
    ) -> Result<DefuddleOutput, WebToolError>;
}

/// Production implementation: spawns the `defuddle` CLI as a
/// subprocess.
#[derive(Default)]
pub struct SubprocessDefuddleRunner;

#[async_trait]
impl DefuddleRunner for SubprocessDefuddleRunner {
    async fn run(
        &self,
        html: &str,
        source_url: &str,
    ) -> Result<DefuddleOutput, WebToolError> {
        let cmd = locate_defuddle()?;
        let mut command = Command::new(&cmd.binary);
        command.args(&cmd.args);
        command.args(["--url", source_url, "--markdown", "--json"]);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|e| WebToolError::DefuddleSpawn(e.to_string()))?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| WebToolError::DefuddleSpawn("no stdin pipe".into()))?;
        stdin.write_all(html.as_bytes()).await?;
        // Explicit drop closes stdin so defuddle sees EOF.
        drop(stdin);

        let output = child.wait_with_output().await?;
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
            args: vec![
                "--yes".into(),
                format!("defuddle@{DEFUDDLE_VERSION}"),
            ],
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
        let json = r##"{
            "title": "Hello world",
            "author": "Jane Doe",
            "description": "A test article",
            "domain": "example.com",
            "site": "Example",
            "language": "en",
            "published": "2024-08-15T10:30:00Z",
            "wordCount": 1234,
            "contentMarkdown": "# Hello\n\nBody.",
            "content": "<p>Body.</p>"
        }"##;
        let parsed = parse_defuddle_output(json.as_bytes()).expect("parses");
        assert_eq!(parsed.title.as_deref(), Some("Hello world"));
        assert_eq!(parsed.author.as_deref(), Some("Jane Doe"));
        assert_eq!(parsed.domain.as_deref(), Some("example.com"));
        assert_eq!(parsed.word_count, Some(1234));
        assert!(parsed.content_markdown.as_deref().unwrap().contains("# Hello"));
    }

    #[test]
    fn parse_defuddle_output_handles_minimal_document() {
        // No required fields. Only title and content.
        let json = r#"{"title": "Minimal", "content": "<p>x</p>"}"#;
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
