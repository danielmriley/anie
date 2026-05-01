//! Background embedding worker.
//!
//! Plan 08 PR 08.2 of `docs/rlm_2026-04-29/`. Mirrors
//! the Phase-F summarizer worker shape — one mpsc channel,
//! one long-lived tokio task, the policy enqueues with
//! `try_send` so the model turn never blocks on slow
//! embedding calls.
//!
//! The worker pulls `EmbedRequest`s, calls
//! [`Embedder::embed`], and writes the resulting vector
//! back via `ExternalContext::set_embedding`. The
//! reranker (PR 08.3) reads from those cached vectors.
//!
//! Lifecycle:
//! 1. Controller spawns the worker on rlm-mode init when
//!    `ANIE_EMBEDDING_MODEL` is set, handing it
//!    `Arc<dyn Embedder>` + `Arc<RwLock<ExternalContext>>`.
//! 2. Policy enqueues `EmbedRequest { id, text }` after
//!    archive (only for messages above a size threshold).
//! 3. Worker pulls, embeds, writes back via
//!    `set_embedding`.
//! 4. Worker exits when the sender drops (controller
//!    teardown).
//!
//! Bounded mpsc (capacity 64). When the worker falls
//! behind, the policy's `try_send` returns Full and the
//! request is dropped — the entry stays unembedded and
//! the reranker falls back to keyword overlap for that
//! candidate.

use std::sync::Arc;

use tokio::sync::{RwLock, mpsc};

use crate::embedder::Embedder;
use crate::external_context::{ExternalContext, MessageId};

/// Token-cost threshold below which messages aren't worth
/// embedding. Mirrors `bg_summarizer::SUMMARIZE_MIN_TOKENS`
/// — short messages don't carry enough signal to score
/// reliably. Keyword overlap handles them adequately.
#[allow(dead_code)] // wired up in PR 08.3 (policy enqueue).
pub(crate) const EMBED_MIN_TOKENS: u64 = 200;

/// Bounded queue capacity — same as bg_summarizer. Keeps
/// memory pressure predictable; full-channel drops are a
/// graceful degradation rather than a failure.
#[allow(dead_code)] // wired up in PR 08.3 (policy + controller).
const EMBED_CHANNEL_CAPACITY: usize = 64;

/// Request sent to the background embed worker.
#[derive(Debug, Clone)]
#[allow(dead_code)] // wired up in PR 08.3 (policy enqueue).
pub(crate) struct EmbedRequest {
    /// `ExternalContext` ID to attach the embedding to.
    pub id: MessageId,
    /// The text to embed. The policy extracts this from
    /// the message before enqueuing so the worker doesn't
    /// need to take a read lock on the store.
    pub text: String,
}

/// Spawn the background worker. Returns the sender end of
/// the request channel; the caller stashes it on the policy
/// so eviction/archive can enqueue. The receiver moves
/// into the spawned task and drops on teardown.
///
/// Worker exits when the sender is closed (every Sender
/// clone dropped). On controller teardown, dropping the
/// sender drains the queue and the worker exits cleanly.
#[allow(dead_code)] // wired up in PR 08.3 (controller spawn).
pub(crate) fn spawn_embed_worker(
    embedder: Arc<dyn Embedder>,
    external: Arc<RwLock<ExternalContext>>,
) -> mpsc::Sender<EmbedRequest> {
    let (tx, mut rx) = mpsc::channel::<EmbedRequest>(EMBED_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        while let Some(EmbedRequest { id, text }) = rx.recv().await {
            match embedder.embed(&text).await {
                Ok(vec) => {
                    let mut store = external.write().await;
                    store.set_embedding(id, vec);
                }
                Err(error) => {
                    tracing::warn!(
                        target: "anie_cli::bg_embedder",
                        %error,
                        message_id = id,
                        "embedder failed; entry stays unembedded"
                    );
                }
            }
        }
    });
    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::Embedder;
    use anie_protocol::{ContentBlock, Message, UserMessage, now_millis};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Test stub: returns deterministic embeddings keyed
    /// off input text. Tracks call count for assertions.
    struct StubEmbedder {
        mappings: Mutex<HashMap<String, Vec<f32>>>,
        calls: Mutex<usize>,
    }

    impl StubEmbedder {
        fn new(mappings: HashMap<String, Vec<f32>>) -> Self {
            Self {
                mappings: Mutex::new(mappings),
                calls: Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Embedder for StubEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
            *self.calls.lock().unwrap() += 1;
            self.mappings
                .lock()
                .unwrap()
                .get(text)
                .cloned()
                .ok_or_else(|| format!("no mapping for {text:?}"))
        }
        fn dim(&self) -> usize {
            3
        }
    }

    fn user_msg(text: &str) -> Message {
        Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: now_millis(),
        })
    }

    /// `set_embedding` is idempotent — calling it twice
    /// replaces the prior vector with the new one.
    #[test]
    fn set_embedding_idempotent() {
        let mut store = ExternalContext::new();
        let id = store.push(user_msg("hello"));
        store.set_embedding(id, vec![1.0, 0.0, 0.0]);
        store.set_embedding(id, vec![0.0, 1.0, 0.0]);
        assert_eq!(store.get_embedding(id), Some(&[0.0_f32, 1.0, 0.0][..]));
    }

    /// `embedding_count` reflects how many entries have
    /// embeddings attached. Used by the ledger.
    #[test]
    fn embedding_count_reflects_state() {
        let mut store = ExternalContext::new();
        let id1 = store.push(user_msg("a"));
        let id2 = store.push(user_msg("b"));
        let _id3 = store.push(user_msg("c"));
        assert_eq!(store.embedding_count(), 0);
        store.set_embedding(id1, vec![1.0, 0.0, 0.0]);
        assert_eq!(store.embedding_count(), 1);
        store.set_embedding(id2, vec![0.0, 1.0, 0.0]);
        assert_eq!(store.embedding_count(), 2);
    }

    /// `get_embedding` returns None for entries that
    /// haven't been embedded — fallback path for the
    /// reranker when worker is behind.
    #[test]
    fn get_embedding_returns_none_for_unembedded() {
        let mut store = ExternalContext::new();
        let id = store.push(user_msg("hello"));
        assert_eq!(store.get_embedding(id), None);
    }

    /// `get_embedding` returns None for out-of-range ids.
    #[test]
    fn get_embedding_returns_none_for_bad_id() {
        let store = ExternalContext::new();
        assert_eq!(store.get_embedding(99), None);
    }

    /// Worker integration: enqueueing a request results
    /// in the store gaining an embedding for that ID.
    #[tokio::test]
    async fn embedder_cache_round_trips_via_worker() {
        let mut mappings = HashMap::new();
        mappings.insert("hello world".to_string(), vec![1.0, 2.0, 3.0]);
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(mappings));
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let id = store.write().await.push(user_msg("hello world"));
        let tx = spawn_embed_worker(Arc::clone(&embedder), Arc::clone(&store));

        tx.send(EmbedRequest {
            id,
            text: "hello world".to_string(),
        })
        .await
        .unwrap();
        // Drop the sender so the worker drains and exits.
        drop(tx);
        // Give the worker a moment.
        for _ in 0..50 {
            if store.read().await.get_embedding(id).is_some() {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(2)).await;
        }
        assert_eq!(
            store.read().await.get_embedding(id),
            Some(&[1.0_f32, 2.0, 3.0][..])
        );
    }

    /// Embedder failure logs + skips — the entry stays
    /// unembedded, no panic. The reranker handles this by
    /// falling back to keyword overlap.
    #[tokio::test]
    async fn worker_logs_and_skips_on_embedder_error() {
        // Empty mappings → embedder errors on every call.
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(HashMap::new()));
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let id = store.write().await.push(user_msg("anything"));
        let tx = spawn_embed_worker(Arc::clone(&embedder), Arc::clone(&store));

        tx.send(EmbedRequest {
            id,
            text: "anything".to_string(),
        })
        .await
        .unwrap();
        drop(tx);
        // Wait briefly; entry should never gain an
        // embedding.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        assert_eq!(store.read().await.get_embedding(id), None);
    }
}
