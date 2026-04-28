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
use tokio_util::sync::CancellationToken;

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

/// Maximum bytes captured from Defuddle's stdout. Cleaned
/// markdown output for a typical article fits in tens of KiB;
/// 1 MiB leaves headroom for unusually long documents while
/// keeping a runaway / malicious page from filling memory via
/// the subprocess pipe. PR 4.2 of `docs/code_review_2026-04-27/`.
pub const DEFAULT_MAX_DEFUDDLE_STDOUT_BYTES: usize = 1024 * 1024;

/// Maximum bytes captured from Defuddle's stderr. Stderr is
/// only used for the exit-code error message; if Defuddle is
/// flooding stderr it's misbehaving and we want a typed error
/// rather than an OOM. 256 KiB is generous for any sensible
/// stderr payload.
pub const DEFAULT_MAX_DEFUDDLE_STDERR_BYTES: usize = 256 * 1024;

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
///
/// `cancel` is honored cooperatively. Production runners must
/// kill the spawned subprocess on cancellation rather than
/// letting it run to completion in the background.
#[async_trait]
pub trait DefuddleRunner: Send + Sync {
    /// Run Defuddle against `html`, with `source_url` provided
    /// for relative-link resolution and metadata. Returns
    /// [`WebToolError::Aborted`] when `cancel` fires.
    async fn run(
        &self,
        html: &str,
        source_url: &str,
        cancel: &CancellationToken,
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
        _source_url: &str,
        cancel: &CancellationToken,
    ) -> Result<DefuddleOutput, WebToolError> {
        if cancel.is_cancelled() {
            return Err(WebToolError::Aborted);
        }
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

        let mut child = command
            .spawn()
            .map_err(|e| WebToolError::DefuddleSpawn(e.to_string()))?;

        // Cancellation: take stdout/stderr into separate read
        // tasks so we can `child.wait()` cooperatively. On
        // cancel, kill the child and abort the readers — the
        // alternative (`wait_with_output()`) consumes `child`
        // and gives us no kill handle. PR 4.1 of
        // `docs/code_review_2026-04-27/`.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| WebToolError::DefuddleSpawn("missing stdout pipe".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| WebToolError::DefuddleSpawn("missing stderr pipe".into()))?;

        let stdout_task = tokio::spawn(async move {
            let mut reader = stdout;
            read_to_end_bounded(&mut reader, DEFAULT_MAX_DEFUDDLE_STDOUT_BYTES).await
        });
        let stderr_task = tokio::spawn(async move {
            let mut reader = stderr;
            read_to_end_bounded(&mut reader, DEFAULT_MAX_DEFUDDLE_STDERR_BYTES).await
        });

        let status = tokio::select! {
            _ = cancel.cancelled() => {
                // start_kill is non-blocking; wait reaps the
                // process so we don't leave a zombie.
                let _ = child.start_kill();
                let _ = child.wait().await;
                stdout_task.abort();
                stderr_task.abort();
                drop(tmp);
                return Err(WebToolError::Aborted);
            }
            res = child.wait() => res?,
        };

        let (stdout_buf, stdout_overflowed) = stdout_task
            .await
            .map_err(|e| WebToolError::DefuddleSpawn(format!("stdout reader join: {e}")))?
            .map_err(|e| WebToolError::DefuddleSpawn(format!("stdout read: {e}")))?;
        let (stderr_buf, stderr_overflowed) = stderr_task
            .await
            .map_err(|e| WebToolError::DefuddleSpawn(format!("stderr reader join: {e}")))?
            .map_err(|e| WebToolError::DefuddleSpawn(format!("stderr read: {e}")))?;

        // Keep `tmp` alive until after the child has read it.
        drop(tmp);

        if !status.success() {
            let mut stderr_text = String::from_utf8_lossy(&stderr_buf).to_string();
            if stderr_overflowed {
                stderr_text.push_str("\n…[truncated]");
            }
            return Err(WebToolError::DefuddleFailed {
                exit_code: status.code(),
                stderr: stderr_text,
            });
        }

        // PR 4.2 of `docs/code_review_2026-04-27/`: a
        // success exit with overflowed stdout means we
        // truncated valid JSON. The truncated bytes will not
        // round-trip through `parse_defuddle_output`, but
        // surface a typed error explaining *why* parsing
        // would fail rather than letting the agent see a
        // confusing "expected `,` or `}`" deserializer
        // message.
        if stdout_overflowed {
            return Err(WebToolError::DefuddleFailed {
                exit_code: status.code(),
                stderr: format!(
                    "defuddle stdout exceeded {DEFAULT_MAX_DEFUDDLE_STDOUT_BYTES} bytes; output truncated and unparseable"
                ),
            });
        }
        parse_defuddle_output(&stdout_buf)
    }
}

/// Read `reader` to EOF, accumulating up to `cap` bytes and
/// silently discarding any beyond. Returns `(buf, overflowed)`.
/// Drains the underlying pipe even after the cap is hit so the
/// child process can finish writing without backpressure
/// stalling its exit. PR 4.2 of `docs/code_review_2026-04-27/`.
async fn read_to_end_bounded<R>(reader: &mut R, cap: usize) -> std::io::Result<(Vec<u8>, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut overflowed = false;
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok((buf, overflowed));
        }
        let remaining = cap.saturating_sub(buf.len());
        if n > remaining {
            buf.extend_from_slice(&chunk[..remaining]);
            overflowed = true;
        } else if !overflowed {
            buf.extend_from_slice(&chunk[..n]);
        }
        // After overflow, we keep reading but drop bytes — the
        // pipe must continue draining or the child blocks on
        // write and never exits.
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

    /// PR 4.2 of `docs/code_review_2026-04-27/`. The bounded
    /// reader keeps at most `cap` bytes in memory and reports
    /// `overflowed = true` when the source produced more,
    /// while still draining the rest of the source so the
    /// underlying pipe doesn't backpressure the writer.
    #[tokio::test]
    async fn read_to_end_bounded_caps_at_size() {
        // 8 KiB source, 4 KiB cap.
        let source: Vec<u8> = (0u8..=255).cycle().take(8 * 1024).collect();
        let mut reader = std::io::Cursor::new(source.clone());
        let (buf, overflowed) = read_to_end_bounded(&mut reader, 4 * 1024).await.unwrap();
        assert_eq!(buf.len(), 4 * 1024);
        assert!(overflowed);
        assert_eq!(&buf[..], &source[..4 * 1024]);
    }

    #[tokio::test]
    async fn read_to_end_bounded_returns_full_buffer_when_under_cap() {
        let source = b"short payload".to_vec();
        let mut reader = std::io::Cursor::new(source.clone());
        let (buf, overflowed) = read_to_end_bounded(&mut reader, 1024).await.unwrap();
        assert_eq!(buf, source);
        assert!(!overflowed);
    }
}
