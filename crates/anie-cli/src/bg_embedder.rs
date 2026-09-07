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
//! 1. Controller spawns the worker once per rlm session
//!    (`RlmSessionState`) when `ANIE_EMBEDDING_MODEL`
//!    names an Ollama embedding model. Embeddings are
//!    opt-in — RC2 of `docs/code_review_2026-06-11.md`:
//!    a default-on embedder forced the user's Ollama to
//!    keep a second model resident alongside the chat
//!    model. The worker gets `Arc<dyn Embedder>` +
//!    `Arc<RwLock<ExternalContext>>` + a
//!    `CancellationToken`.
//! 2. Policy enqueues `EmbedRequest { id, text }` after
//!    archive (only for messages above a size threshold).
//! 3. Worker pulls, embeds, writes back via
//!    `set_embedding`.
//! 4. Worker exits when the cancellation token fires
//!    (session-state drop) or every sender is dropped.
//!
//! Bounded mpsc (capacity 64). When the worker falls
//! behind, the policy's `try_send` returns Full and the
//! request is dropped — the entry stays unembedded and
//! the reranker falls back to keyword overlap for that
//! candidate.

use std::sync::Arc;

use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::embedder::Embedder;
use crate::external_context::{ExternalContext, MessageId};

/// Token-cost threshold below which messages aren't worth
/// embedding. Mirrors `bg_summarizer::SUMMARIZE_MIN_TOKENS`
/// — short messages don't carry enough signal to score
/// reliably. Keyword overlap handles them adequately.
pub(crate) const EMBED_MIN_TOKENS: u64 = 200;

/// Bounded queue capacity — same as bg_summarizer. Keeps
/// memory pressure predictable; full-channel drops are a
/// graceful degradation rather than a failure.
const EMBED_CHANNEL_CAPACITY: usize = 64;

/// Request sent to the background embed worker.
#[derive(Debug, Clone)]
pub(crate) struct EmbedRequest {
    /// `ExternalContext` ID to attach the embedding to.
    pub id: MessageId,
    /// The text to embed. The policy extracts this from
    /// the message before enqueuing so the worker doesn't
    /// need to take a read lock on the store.
    pub text: String,
}

/// Spawn the background worker. Returns the sender end of
/// the request channel plus the worker's `JoinHandle`; the
/// caller (the rlm session state) stashes the sender on the
/// per-run policy so eviction/archive can enqueue, and
/// keeps the handle so it can abort the worker on teardown.
///
/// Worker exits when `cancel` fires or the sender is closed
/// (every Sender clone dropped). RC3 of
/// `docs/code_review_2026-06-11.md`: per-run policies hold
/// sender clones that can outlive the session state, so the
/// token — not sender-drop — is the authoritative teardown
/// signal.
pub(crate) fn spawn_embed_worker(
    embedder: Arc<dyn Embedder>,
    external: Arc<RwLock<ExternalContext>>,
    cancel: CancellationToken,
) -> (mpsc::Sender<EmbedRequest>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<EmbedRequest>(EMBED_CHANNEL_CAPACITY);
    let handle = tokio::spawn(async move {
        loop {
            let EmbedRequest { id, text } = tokio::select! {
                () = cancel.cancelled() => break,
                request = rx.recv() => match request {
                    Some(request) => request,
                    None => break,
                },
            };
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
    (tx, handle)
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
        let (tx, _handle) = spawn_embed_worker(
            Arc::clone(&embedder),
            Arc::clone(&store),
            CancellationToken::new(),
        );

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
        let (tx, _handle) = spawn_embed_worker(
            Arc::clone(&embedder),
            Arc::clone(&store),
            CancellationToken::new(),
        );

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

    /// RC3 regression (docs/code_review_2026-06-11.md):
    /// the token — not sender-drop — is the authoritative
    /// teardown signal. Cancelling must stop the worker
    /// even while a sender clone is still held.
    #[tokio::test]
    async fn cancelled_embed_worker_exits_while_sender_still_held() {
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(HashMap::new()));
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let cancel = CancellationToken::new();
        let (tx, handle) = spawn_embed_worker(embedder, store, cancel.clone());
        cancel.cancel();
        tokio::time::timeout(tokio::time::Duration::from_secs(2), handle)
            .await
            .expect("worker must exit on cancellation, not wait for sender drop")
            .expect("worker task joins cleanly");
        drop(tx);
    }
}
