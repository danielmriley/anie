//! Text embedder trait + implementations.
//!
//! Plan 08 of `docs/rlm_2026-04-29/`. Used by the rlm
//! relevance reranker to score candidates by semantic
//! similarity instead of keyword overlap.
//!
//! Two impls in this crate:
//! - [`OllamaEmbedder`] — production. Issues a single
//!   HTTP call per text against `<base_url>/api/embed`.
//!   Targets `nomic-embed-text` (768-dim) by default but
//!   any Ollama embedding model works.
//! - `FixedEmbedder` (test-only, in `tests` mod) —
//!   deterministic vectors keyed off input text. Used by
//!   reranker tests so they don't need a real Ollama.
//!
//! The trait surface is intentionally minimal: one async
//! `embed(text) -> Result<Vec<f32>, String>`. Future
//! providers (Cohere, OpenAI, BAAI) plug into the same
//! trait without touching callers.
//!
//! Errors propagate as `String` rather than typed because
//! all callers (the reranker, the bg embed worker) treat
//! any failure as a fallback signal — they log and move
//! on. There's no value in distinguishing HTTP-level vs
//! parse-level failures at the trait boundary.

use std::{borrow::Cow, time::Duration};

use async_trait::async_trait;
use tracing::warn;

/// How many characters of the failing input to include in
/// the error string and the WARN log line. Long enough to
/// see the shape of the input (HTML preamble, leading code
/// fence, …) without dumping kilobytes into every log
/// message.
const FAIL_LOG_INPUT_PREVIEW_CHARS: usize = 160;

/// How many characters of the response body to include
/// when Ollama returns a non-2xx. Ollama errors typically
/// fit in 200 chars (`{"error":"…"}`); this gives us
/// headroom without bloating logs on the rare verbose
/// case.
const FAIL_LOG_BODY_PREVIEW_CHARS: usize = 400;

/// Conservative character budget for an embed call.
/// `nomic-embed-text` has a 2048-token context and the
/// modern Ollama runner returns HTTP 400 (`"the input
/// length exceeds the context length"`) on overflow
/// rather than silently truncating. Observed on a 9121-
/// char weather-page tool result in session
/// `270aa8e2`: empirical char/token ratios at 3000 chars
/// were 1.87, putting 4000 chars over the 2048-token
/// ceiling for that content shape. Structured / numeric
/// content (markdown tables, JSON, code) tokenizes
/// denser than English prose, so the safe cap is
/// well below a naive `2048 × 4` estimate. 3000 chars
/// leaves ~400 tokens of headroom against the densest
/// real-world content the harness has actually fed the
/// embedder. We lose the tail of long docs but the
/// embedder still produces a useful vector for the head,
/// which is typically the most topical part of a web
/// page or a tool result. Tradeoff: under-truncating
/// loses the entry to a 4xx (worse — no signal at all);
/// over-truncating loses tail context (better — partial
/// signal beats none).
const OLLAMA_INPUT_CHAR_CAP: usize = 3000;

/// Total per-request budget for an embed call. RC2 of
/// `docs/code_review_2026-06-11.md`: the client used to be
/// built with no timeout at all, so a busy or wedged Ollama
/// stalled the caller indefinitely. Embeds are best-effort
/// (every caller treats failure as "fall back to keyword
/// overlap"), so a bounded failure is strictly better than
/// an unbounded wait.
const EMBED_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// TCP connect budget for an embed call. Separate from the
/// total budget so an unreachable host fails fast rather
/// than consuming the whole request timeout.
const EMBED_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Truncate a string to `max_chars` graphemes-ish (chars,
/// good enough for log diagnostics) and append an ellipsis
/// when clipped.
fn preview(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Strategy for embedding text into a fixed-dimension
/// vector. Implementations are typically HTTP-backed
/// (Ollama, OpenAI) or local (fastembed, ONNX).
#[async_trait]
pub(crate) trait Embedder: Send + Sync {
    /// Embed a single text. Returns the embedding vector
    /// or a string error. The reranker treats errors as a
    /// fallback signal — it drops to keyword overlap for
    /// that candidate rather than failing the model turn.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, String>;
}

/// Cosine similarity between two equal-length vectors.
/// Returns 0.0 if either vector is the zero vector
/// (avoids NaN). Range: [-1.0, 1.0].
pub(crate) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Ollama-backed embedder. Issues one HTTP `POST` per
/// `embed` call against `<base_url>/api/embed`. The
/// expected response shape is the post-2024 Ollama
/// format: `{ "embeddings": [[...]] }` with one nested
/// vector per input. We only ever send one input per
/// call so we read `embeddings[0]`.
///
/// Construct with `OllamaEmbedder::new(base_url, model)`.
/// The `model` field is the Ollama model name (e.g.
/// `nomic-embed-text`); it must already be pulled.
pub(crate) struct OllamaEmbedder {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

impl OllamaEmbedder {
    /// Build a new embedder.
    pub(crate) fn new(base_url: String, model: String) -> Self {
        Self::with_timeouts(base_url, model, EMBED_HTTP_TIMEOUT, EMBED_CONNECT_TIMEOUT)
    }

    /// Like [`OllamaEmbedder::new`] but with explicit
    /// timeouts. Split out so tests can use sub-second
    /// bounds against unroutable hosts.
    fn with_timeouts(
        base_url: String,
        model: String,
        timeout: Duration,
        connect_timeout: Duration,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(connect_timeout)
            .build()
            // Only fails when the TLS backend can't
            // initialize. Embeds are best-effort, so fall
            // back to the default (timeout-less) client
            // rather than failing construction.
            .unwrap_or_else(|error| {
                warn!(
                    target: "anie_cli::embedder",
                    %error,
                    "failed to build embed http client with timeouts; using default client"
                );
                reqwest::Client::new()
            });
        Self {
            client,
            base_url,
            model,
        }
    }
}

#[async_trait]
impl Embedder for OllamaEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
        // Trim leading/trailing whitespace; Ollama can
        // reject empty inputs.
        let text = text.trim();
        if text.is_empty() {
            return Err("embed: empty input".to_string());
        }
        // Truncate inputs that would exceed the embedding
        // model's context length. The modern Ollama runner
        // returns HTTP 400 (rather than silently
        // truncating) when input length exceeds the model
        // context, so the harness has to clip first or
        // lose the entry to a 4xx. UTF-8-safe truncation
        // via `.chars().take(N).collect()`.
        let original_chars = text.chars().count();
        let send_text: Cow<'_, str> = if original_chars > OLLAMA_INPUT_CHAR_CAP {
            let truncated: String = text.chars().take(OLLAMA_INPUT_CHAR_CAP).collect();
            tracing::debug!(
                target: "anie_cli::embedder",
                model = %self.model,
                original_chars,
                cap = OLLAMA_INPUT_CHAR_CAP,
                "truncating embed input to stay under model context"
            );
            Cow::Owned(truncated)
        } else {
            Cow::Borrowed(text)
        };
        let url = format!("{}/api/embed", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "input": send_text.as_ref(),
        });
        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("embed http: {e}"))?;
        if !response.status().is_success() {
            // On non-2xx, capture the response body and
            // log it alongside the input shape so we can
            // diagnose what's actually failing — Ollama's
            // `/api/embed` returns useful error JSON
            // (`{"error":"…"}`) that the caller's plain
            // status string would otherwise discard.
            let status = response.status();
            let body_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
            let sent_chars = send_text.chars().count();
            let input_preview = preview(send_text.as_ref(), FAIL_LOG_INPUT_PREVIEW_CHARS);
            let body_preview = preview(body_text.trim(), FAIL_LOG_BODY_PREVIEW_CHARS);
            warn!(
                target: "anie_cli::embedder",
                model = %self.model,
                status = %status,
                original_chars,
                sent_chars,
                input_preview = %input_preview,
                body_preview = %body_preview,
                "ollama embed call failed"
            );
            return Err(format!("embed http status: {status}; body: {body_preview}"));
        }
        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| format!("embed parse: {e}"))?;
        // Post-2024 Ollama format: {"embeddings": [[...]]}.
        let embeddings = json
            .get("embeddings")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "embed parse: missing `embeddings` array".to_string())?;
        let first = embeddings
            .first()
            .and_then(|v| v.as_array())
            .ok_or_else(|| "embed parse: empty `embeddings` array".to_string())?;
        let vec: Vec<f32> = first
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();
        if vec.is_empty() {
            return Err("embed parse: vector is empty".to_string());
        }
        Ok(vec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::MockServer;

    /// Test-only embedder that returns deterministic
    /// vectors keyed off input text. Used by reranker
    /// tests in PR 08.3.
    pub(crate) struct FixedEmbedder {
        pub mappings: std::collections::HashMap<String, Vec<f32>>,
    }

    #[async_trait]
    impl Embedder for FixedEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
            self.mappings
                .get(text)
                .cloned()
                .ok_or_else(|| format!("FixedEmbedder: no mapping for {text:?}"))
        }
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors_score_zero() {
        let a = [1.0_f32, 0.0, 0.0];
        let b = [0.0_f32, 1.0, 0.0];
        // Orthogonal — perfectly uncorrelated, cosine = 0.
        assert!((cosine_similarity(&a, &b) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_identical_vectors_score_one() {
        let a = [1.0_f32, 2.0, 3.0];
        // Identical — perfectly correlated, cosine = 1.
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_handles_zero_vector_returns_zero() {
        let zero = [0.0_f32, 0.0, 0.0];
        let other = [1.0_f32, 1.0, 1.0];
        // Zero vector has no direction; treat as
        // uncorrelated rather than NaN.
        assert_eq!(cosine_similarity(&zero, &other), 0.0);
        assert_eq!(cosine_similarity(&other, &zero), 0.0);
    }

    #[test]
    fn cosine_similarity_handles_mismatched_lengths_returns_zero() {
        let a = [1.0_f32, 2.0];
        let b = [1.0_f32, 2.0, 3.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_similarity_anti_aligned_vectors_score_negative_one() {
        let a = [1.0_f32, 0.0];
        let b = [-1.0_f32, 0.0];
        assert!((cosine_similarity(&a, &b) - -1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn fixed_embedder_returns_deterministic_vectors() {
        let mut mappings = std::collections::HashMap::new();
        mappings.insert("hello".to_string(), vec![1.0, 0.0, 0.0]);
        mappings.insert("world".to_string(), vec![0.0, 1.0, 0.0]);
        let e = FixedEmbedder { mappings };
        assert_eq!(e.embed("hello").await.unwrap(), vec![1.0, 0.0, 0.0]);
        assert_eq!(e.embed("world").await.unwrap(), vec![0.0, 1.0, 0.0]);
        assert!(e.embed("missing").await.is_err());
    }

    #[tokio::test]
    async fn ollama_embedder_parses_embed_response() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST).path("/api/embed");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(serde_json::json!({
                        "embeddings": [[0.1, 0.2, 0.3, 0.4]]
                    }));
            })
            .await;
        let embedder = OllamaEmbedder::new(server.base_url(), "nomic-embed-text".into());
        let vec = embedder.embed("hello").await.expect("ok");
        assert_eq!(vec.len(), 4);
        assert!((vec[0] - 0.1).abs() < 1e-6);
        assert!((vec[3] - 0.4).abs() < 1e-6);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn ollama_embedder_propagates_http_error() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST).path("/api/embed");
                then.status(500).body("oops");
            })
            .await;
        let embedder = OllamaEmbedder::new(server.base_url(), "nomic-embed-text".into());
        let err = embedder.embed("hello").await.expect_err("should error");
        assert!(
            err.contains("500"),
            "error should surface the status code: {err}"
        );
    }

    /// Body-length matcher used by the truncation
    /// regression test: a request body strictly shorter
    /// than 2× the cap means the input field had to have
    /// been clipped before send. Un-truncated, an
    /// 18000-char input would push the body well past
    /// that bound (huge × 3 + JSON overhead).
    fn request_body_under_cap_envelope(req: &httpmock::prelude::HttpMockRequest) -> bool {
        req.body
            .as_ref()
            .map(|b| b.len() < OLLAMA_INPUT_CHAR_CAP * 2)
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn ollama_embedder_truncates_oversized_input_before_send() {
        // Regression test for the 400 observed on a 9121-
        // char tool result in session 270aa8e2:
        // `nomic-embed-text` rejects inputs that exceed
        // its 2048-token context. The embedder must
        // truncate to OLLAMA_INPUT_CHAR_CAP first so we
        // get a usable embedding rather than losing the
        // entry to a 4xx.
        let server = MockServer::start_async().await;
        let huge: String = "x".repeat(OLLAMA_INPUT_CHAR_CAP * 3);
        // The success mock requires a body shorter than
        // 2× the cap — only matches when the input was
        // truncated. Without truncation the request goes
        // unmatched (httpmock 404s), the embed call
        // errors, and the test fails with a clear signal.
        let success = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/api/embed")
                    .matches(request_body_under_cap_envelope);
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(serde_json::json!({"embeddings": [[0.1, 0.2, 0.3]]}));
            })
            .await;
        let embedder = OllamaEmbedder::new(server.base_url(), "nomic-embed-text".into());
        let vec = embedder
            .embed(&huge)
            .await
            .expect("should succeed after truncation");
        assert_eq!(vec.len(), 3);
        success.assert_async().await;
    }

    #[tokio::test]
    async fn ollama_embedder_includes_response_body_in_error() {
        // Regression test for the diagnostic gap that left
        // 400-class failures opaque (`embed http status:
        // 400 Bad Request` with no further detail). Ollama
        // returns the actual cause as a JSON body
        // (`{"error":"…"}`); the embedder should surface
        // that body in the returned error string so the bg
        // worker's WARN log carries it.
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST).path("/api/embed");
                then.status(400)
                    .header("content-type", "application/json")
                    .body(r#"{"error":"input length exceeds maximum context length"}"#);
            })
            .await;
        let embedder = OllamaEmbedder::new(server.base_url(), "nomic-embed-text".into());
        let err = embedder.embed("hello").await.expect_err("should error");
        assert!(err.contains("400"), "should include status: {err}");
        assert!(
            err.contains("input length exceeds maximum context length"),
            "should include response body so failures are debuggable: {err}"
        );
    }

    #[tokio::test]
    async fn embed_against_unroutable_host_fails_bounded_instead_of_hanging() {
        // RC2 regression (docs/code_review_2026-06-11.md):
        // the client used to be built with no timeout, so a
        // wedged Ollama stalled the caller indefinitely —
        // and the prompt embed sat on the model-turn start
        // path. 10.255.255.1 is in private space with no
        // route from CI/dev machines; the connect must hit
        // the configured timeout, not hang.
        let embedder = OllamaEmbedder::with_timeouts(
            "http://10.255.255.1:11434".into(),
            "nomic-embed-text".into(),
            Duration::from_millis(400),
            Duration::from_millis(250),
        );
        let result =
            tokio::time::timeout(Duration::from_secs(5), embedder.embed("hello")).await;
        let inner = result.expect("embed must fail within the configured timeout, not hang");
        assert!(inner.is_err(), "unroutable host should surface an error");
    }

    #[tokio::test]
    async fn ollama_embedder_rejects_empty_input() {
        let embedder = OllamaEmbedder::new("http://localhost:1".into(), "x".into());
        let err = embedder.embed("   ").await.expect_err("should error");
        assert!(err.contains("empty"));
    }

    #[tokio::test]
    async fn ollama_embedder_rejects_malformed_response() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST).path("/api/embed");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(serde_json::json!({"unexpected": "shape"}));
            })
            .await;
        let embedder = OllamaEmbedder::new(server.base_url(), "x".into());
        let err = embedder.embed("hi").await.expect_err("should error");
        assert!(err.contains("missing `embeddings`"));
    }
}
