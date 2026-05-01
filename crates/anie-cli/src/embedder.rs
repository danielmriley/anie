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
//! `embed(text) -> Result<Vec<f32>, String>` plus a
//! synchronous `dim()` for sanity checks. Future
//! providers (Cohere, OpenAI, BAAI) plug into the same
//! trait without touching callers.
//!
//! Errors propagate as `String` rather than typed because
//! all callers (the reranker, the bg embed worker) treat
//! any failure as a fallback signal — they log and move
//! on. There's no value in distinguishing HTTP-level vs
//! parse-level failures at the trait boundary.

use async_trait::async_trait;

/// Strategy for embedding text into a fixed-dimension
/// vector. Implementations are typically HTTP-backed
/// (Ollama, OpenAI) or local (fastembed, ONNX).
#[allow(dead_code)] // wired up in PR 08.2 (cache) and 08.3 (reranker).
#[async_trait]
pub(crate) trait Embedder: Send + Sync {
    /// Embed a single text. Returns the embedding vector
    /// or a string error. The reranker treats errors as a
    /// fallback signal — it drops to keyword overlap for
    /// that candidate rather than failing the model turn.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, String>;

    /// Embedding dimensionality. Used as a sanity check —
    /// callers should error if a returned vector doesn't
    /// match this length.
    fn dim(&self) -> usize;
}

/// Cosine similarity between two equal-length vectors.
/// Returns 0.0 if either vector is the zero vector
/// (avoids NaN). Range: [-1.0, 1.0].
#[allow(dead_code)] // wired up in PR 08.3 (reranker).
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
#[allow(dead_code)] // wired up in PR 08.3 (controller spawn).
pub(crate) struct OllamaEmbedder {
    client: reqwest::Client,
    base_url: String,
    model: String,
    dim: usize,
}

impl OllamaEmbedder {
    /// Build a new embedder. `dim` is the vector size the
    /// model returns; for `nomic-embed-text` this is 768.
    /// Wrong values just hurt the sanity-check, they don't
    /// change behavior.
    #[allow(dead_code)] // wired up in PR 08.3 (controller spawn).
    pub(crate) fn new(base_url: String, model: String, dim: usize) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            model,
            dim,
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
        let url = format!("{}/api/embed", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "input": text,
        });
        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("embed http: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("embed http status: {}", response.status()));
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

    fn dim(&self) -> usize {
        self.dim
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
        pub dim: usize,
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
        fn dim(&self) -> usize {
            self.dim
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
        let e = FixedEmbedder { dim: 3, mappings };
        assert_eq!(e.embed("hello").await.unwrap(), vec![1.0, 0.0, 0.0]);
        assert_eq!(e.embed("world").await.unwrap(), vec![0.0, 1.0, 0.0]);
        assert!(e.embed("missing").await.is_err());
        assert_eq!(e.dim(), 3);
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
        let embedder = OllamaEmbedder::new(server.base_url(), "nomic-embed-text".into(), 4);
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
        let embedder = OllamaEmbedder::new(server.base_url(), "nomic-embed-text".into(), 768);
        let err = embedder.embed("hello").await.expect_err("should error");
        assert!(
            err.contains("500"),
            "error should surface the status code: {err}"
        );
    }

    #[tokio::test]
    async fn ollama_embedder_rejects_empty_input() {
        let embedder = OllamaEmbedder::new("http://localhost:1".into(), "x".into(), 1);
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
        let embedder = OllamaEmbedder::new(server.base_url(), "x".into(), 1);
        let err = embedder.embed("hi").await.expect_err("should error");
        assert!(err.contains("missing `embeddings`"));
    }
}
