//! Indexed external-context store.
//!
//! Phase B of `docs/rlm_2026-04-29/06_phased_implementation.md`.
//!
//! Holds messages addressable by stable ID, with secondary
//! indexes by message kind and by tool name. The recurse
//! tool's [`crate::recurse_provider::ControllerContextProvider`]
//! reads from this store instead of scanning a raw
//! `Vec<Message>`. Phase C will start *moving* messages out
//! of the active context into the store (eviction); Phase B
//! lays the substrate without changing eviction semantics —
//! the store starts as a mirror of the parent run's context
//! and stays that way until the Phase C policy lands.
//!
//! Why a separate type instead of a `Vec<Message>` plus
//! per-call linear scans:
//!
//! - **Addressable reads.** `get_by_id` is O(1); the
//!   recurse tool's `ToolResult` scope no longer has to
//!   walk the whole context to find one message by
//!   `tool_call_id`.
//! - **Kind / tool indexes.** Future scope kinds (e.g.,
//!   "every web_read result this run") become cheap.
//! - **Stable IDs across compaction.** The active context
//!   can shrink (compaction) while the store retains the
//!   full history under stable IDs the recurse tool can
//!   reference.
//!
//! Persistence: in-memory for v1 (this commit). The session
//! log on disk gives us free durability when we want it
//! later — every message anie has ever seen is already
//! JSONL-persisted under `~/.anie/sessions/`.

use std::collections::HashMap;

use anie_protocol::Message;

/// Stable identifier for a message in the [`ExternalContext`].
/// Position-based (insertion order); IDs do not move when
/// later messages are appended.
pub(crate) type MessageId = usize;

/// Label for a message's role kind, used as a key into the
/// `by_kind` index. We don't hash `Message` itself (clones,
/// signature mismatches across protocol changes); a small
/// stable enum is the durable shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MessageKindLabel {
    User,
    Assistant,
    ToolResult,
    Custom,
}

impl MessageKindLabel {
    fn from_message(m: &Message) -> Self {
        match m {
            Message::User(_) => Self::User,
            Message::Assistant(_) => Self::Assistant,
            Message::ToolResult(_) => Self::ToolResult,
            Message::Custom(_) => Self::Custom,
        }
    }
}

/// A message plus its assigned ID. Stored in insertion
/// order; the `Vec`'s position equals `id`. The `id` field
/// is redundant with position today but kept around so
/// future eviction code (Phase C) can reorder the underlying
/// storage without losing the original assignment — IDs the
/// model has cached via earlier `recurse` calls must remain
/// stable.
#[derive(Debug, Clone)]
pub(crate) struct StoredMessage {
    /// Stable identifier; equals the message's position at
    /// insertion time. Phase C may decouple position from id
    /// when eviction starts moving messages around.
    #[allow(dead_code)]
    pub id: MessageId,
    pub message: Message,
    /// Optional Phase-F summary, written by the background
    /// summarizer worker after archive. The recurse provider
    /// can return this in place of the full message body
    /// when the model only needs an outline; the relevance
    /// reranker can page summaries in cheaply when the full
    /// content wouldn't fit under the budget.
    pub summary: Option<String>,
}

/// Indexed store of messages. Owns its messages; clones
/// references when callers ask for ranges or filtered
/// subsets.
#[derive(Debug, Default)]
pub(crate) struct ExternalContext {
    /// All stored messages in insertion order. `messages[id]`
    /// is the message with that ID, by construction.
    messages: Vec<StoredMessage>,
    /// Index: message kind → IDs. Maintained on every
    /// `push`.
    by_kind: HashMap<MessageKindLabel, Vec<MessageId>>,
    /// Index: tool name → IDs of `ToolResult` messages with
    /// that name. Updated on `push` for `ToolResult` kinds.
    /// Other kinds don't appear in this index.
    by_tool: HashMap<String, Vec<MessageId>>,
    /// Index: `tool_call_id` → ID. One-to-one because every
    /// tool call has a distinct id within a run.
    /// `find_by_tool_call_id` is the load-bearing path for
    /// the recurse tool's `ToolResult` scope.
    by_tool_call_id: HashMap<String, MessageId>,
}

impl ExternalContext {
    /// Create an empty store.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Build a store pre-populated with `messages`. Each
    /// message is appended in order, receiving the IDs
    /// `0..messages.len()`.
    #[must_use]
    pub(crate) fn from_messages(messages: Vec<Message>) -> Self {
        let mut store = Self::new();
        for message in messages {
            store.push(message);
        }
        store
    }

    /// Append a message and return its assigned ID. The ID
    /// is the message's position in insertion order; later
    /// pushes do not perturb it.
    pub(crate) fn push(&mut self, message: Message) -> MessageId {
        let id = self.messages.len();
        self.by_kind
            .entry(MessageKindLabel::from_message(&message))
            .or_default()
            .push(id);
        if let Message::ToolResult(t) = &message {
            self.by_tool
                .entry(t.tool_name.clone())
                .or_default()
                .push(id);
            self.by_tool_call_id.insert(t.tool_call_id.clone(), id);
        }
        self.messages.push(StoredMessage {
            id,
            message,
            summary: None,
        });
        id
    }

    /// Attach a summary to an existing stored message.
    /// Idempotent — replaces the previous summary if any.
    /// Used by the Phase-F background summarizer worker
    /// after it produces a summary for a recently-archived
    /// message. No-op when `id` is out of bounds.
    pub(crate) fn set_summary(&mut self, id: MessageId, summary: String) {
        if let Some(stored) = self.messages.get_mut(id) {
            stored.summary = Some(summary);
        }
    }

    /// Borrow the optional summary attached to `id`. Used by
    /// the recurse tool when the caller asks for the
    /// summarized form rather than the full body.
    /// `#[allow(dead_code)]` — Phase F's recurse-side
    /// integration lands in a follow-up commit.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn get_summary(&self, id: MessageId) -> Option<&str> {
        self.messages.get(id).and_then(|s| s.summary.as_deref())
    }

    /// Number of stored messages that currently have a
    /// summary attached. Used by the ledger to surface
    /// "N summaries available" so the model knows the
    /// summarizer has caught up.
    #[must_use]
    pub(crate) fn summary_count(&self) -> usize {
        self.messages.iter().filter(|s| s.summary.is_some()).count()
    }

    /// Number of stored messages. `#[allow(dead_code)]` —
    /// Phase C will read this to decide when the active
    /// context approaches the configured ceiling.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.messages.len()
    }

    /// True iff no messages have been stored.
    /// `#[allow(dead_code)]` — Phase C consumer.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Get the message with `id`. None if `id` is out of
    /// bounds.
    #[must_use]
    pub(crate) fn get_by_id(&self, id: MessageId) -> Option<&Message> {
        self.messages.get(id).map(|s| &s.message)
    }

    /// Half-open range `[start, end)` of stored messages,
    /// returned as cloned `Message` values for the caller's
    /// use. Returns an error string when `start > end` or
    /// `end > len`. Mirrors `RecurseScope::MessageRange`'s
    /// validation contract.
    pub(crate) fn range(&self, start: usize, end: usize) -> Result<Vec<Message>, String> {
        if start > end {
            return Err(format!("range start ({start}) > end ({end})"));
        }
        if end > self.messages.len() {
            return Err(format!(
                "range end ({end}) exceeds context length ({})",
                self.messages.len()
            ));
        }
        Ok(self.messages[start..end]
            .iter()
            .map(|s| s.message.clone())
            .collect())
    }

    /// Iterate over every stored message in insertion order.
    /// Used by `RecurseScope::MessageGrep`'s regex scan and
    /// by future keyword-search code paths.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Message> {
        self.messages.iter().map(|s| &s.message)
    }

    /// Like `iter` but also exposes each entry's stable id
    /// and optional summary. Used by the relevance reranker
    /// to substitute a summary for the full body when the
    /// full body wouldn't fit under the budget.
    pub(crate) fn iter_with_meta(
        &self,
    ) -> impl Iterator<Item = (MessageId, &Message, Option<&str>)> {
        self.messages
            .iter()
            .map(|s| (s.id, &s.message, s.summary.as_deref()))
    }

    /// Look up the message ID for a `tool_call_id`. None
    /// when the id isn't known to the store. Used by
    /// `RecurseScope::ToolResult`.
    #[must_use]
    pub(crate) fn find_by_tool_call_id(&self, tool_call_id: &str) -> Option<MessageId> {
        self.by_tool_call_id.get(tool_call_id).copied()
    }

    /// IDs of every message of the given kind, in insertion
    /// order. Returns an empty slice (not None) when no
    /// messages of that kind have been stored.
    /// `#[allow(dead_code)]` — Phase C consumer (eviction
    /// walks by kind so it can pin system + recent user
    /// messages and evict older bulk content first).
    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn ids_by_kind(&self, kind: MessageKindLabel) -> &[MessageId] {
        self.by_kind.get(&kind).map_or(&[], Vec::as_slice)
    }

    /// IDs of every `ToolResult` message produced by the
    /// named tool. Returns an empty slice when the tool
    /// hasn't been invoked or hasn't returned in this run.
    /// `#[allow(dead_code)]` — Phase E consumer (relevance
    /// reranking can preferentially page in tool results
    /// matching the current request).
    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn ids_by_tool(&self, tool_name: &str) -> &[MessageId] {
        self.by_tool.get(tool_name).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_protocol::{
        AssistantMessage, ContentBlock, StopReason, ToolResultMessage, Usage, UserMessage,
        now_millis,
    };

    fn user(text: &str) -> Message {
        Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: now_millis(),
        })
    }

    fn assistant(text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "test".into(),
            model: "test".into(),
            timestamp: now_millis(),
            reasoning_details: None,
        })
    }

    fn tool_result(call_id: &str, tool_name: &str, body: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: call_id.into(),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::Text { text: body.into() }],
            details: serde_json::Value::Null,
            is_error: false,
            timestamp: now_millis(),
        })
    }

    /// Push assigns IDs `0, 1, 2, …` in insertion order.
    #[test]
    fn push_returns_monotonic_ids() {
        let mut store = ExternalContext::new();
        assert_eq!(store.push(user("a")), 0);
        assert_eq!(store.push(user("b")), 1);
        assert_eq!(store.push(user("c")), 2);
        assert_eq!(store.len(), 3);
    }

    /// `from_messages` is equivalent to constructing empty +
    /// pushing each in order.
    #[test]
    fn from_messages_assigns_ids_in_order() {
        let store = ExternalContext::from_messages(vec![user("a"), user("b"), user("c")]);
        assert_eq!(store.len(), 3);
        for i in 0..3 {
            assert!(store.get_by_id(i).is_some());
        }
        assert!(store.get_by_id(3).is_none());
    }

    /// `get_by_id` is O(1) and returns the right message.
    #[test]
    fn get_by_id_returns_correct_message() {
        let store = ExternalContext::from_messages(vec![user("zero"), user("one"), user("two")]);
        let Message::User(u) = store.get_by_id(1).expect("present") else {
            panic!("expected user");
        };
        let ContentBlock::Text { text } = &u.content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "one");
    }

    /// `range` returns the half-open subset and validates
    /// bounds.
    #[test]
    fn range_returns_slice() {
        let store =
            ExternalContext::from_messages(vec![user("a"), user("b"), user("c"), user("d")]);
        let r = store.range(1, 3).expect("ok");
        assert_eq!(r.len(), 2);
        // start > end errors.
        assert!(store.range(3, 1).is_err());
        // end > len errors.
        assert!(store.range(0, 99).is_err());
        // empty range is ok.
        assert!(store.range(2, 2).expect("empty ok").is_empty());
    }

    /// `ids_by_kind` returns IDs grouped by message kind.
    /// User and Assistant both index correctly when mixed.
    #[test]
    fn by_kind_groups_messages_correctly() {
        let store = ExternalContext::from_messages(vec![
            user("u0"),
            assistant("a1"),
            user("u2"),
            assistant("a3"),
            user("u4"),
        ]);
        assert_eq!(store.ids_by_kind(MessageKindLabel::User), &[0, 2, 4]);
        assert_eq!(store.ids_by_kind(MessageKindLabel::Assistant), &[1, 3]);
        // Kinds with no entries return an empty slice, not None.
        assert!(store.ids_by_kind(MessageKindLabel::ToolResult).is_empty());
        assert!(store.ids_by_kind(MessageKindLabel::Custom).is_empty());
    }

    /// `ids_by_tool` only contains `ToolResult` messages, and
    /// only the ones with the matching tool name.
    #[test]
    fn by_tool_returns_only_matching_tool_results() {
        let store = ExternalContext::from_messages(vec![
            user("u0"),
            tool_result("c1", "bash", "ls output"),
            tool_result("c2", "read", "file contents"),
            tool_result("c3", "bash", "echo output"),
            assistant("a4"),
        ]);
        assert_eq!(store.ids_by_tool("bash"), &[1, 3]);
        assert_eq!(store.ids_by_tool("read"), &[2]);
        // Unknown tool: empty slice.
        assert!(store.ids_by_tool("nonexistent").is_empty());
    }

    /// `find_by_tool_call_id` is O(1) and returns the right
    /// message ID.
    #[test]
    fn find_by_tool_call_id_returns_correct_id() {
        let store = ExternalContext::from_messages(vec![
            tool_result("call_a", "bash", "first"),
            tool_result("call_b", "read", "second"),
            tool_result("call_c", "bash", "third"),
        ]);
        assert_eq!(store.find_by_tool_call_id("call_a"), Some(0));
        assert_eq!(store.find_by_tool_call_id("call_b"), Some(1));
        assert_eq!(store.find_by_tool_call_id("call_c"), Some(2));
        assert_eq!(store.find_by_tool_call_id("call_unknown"), None);
    }

    /// `iter` walks every message in insertion order.
    #[test]
    fn iter_walks_in_insertion_order() {
        let store =
            ExternalContext::from_messages(vec![user("first"), assistant("second"), user("third")]);
        let texts: Vec<String> = store
            .iter()
            .filter_map(|m| match m {
                Message::User(u) => match &u.content[0] {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                },
                Message::Assistant(a) => match a.content.first() {
                    Some(ContentBlock::Text { text }) => Some(text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["first", "second", "third"]);
    }

    /// `is_empty` matches `len == 0`.
    #[test]
    fn empty_store_predicates() {
        let mut store = ExternalContext::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        store.push(user("x"));
        assert!(!store.is_empty());
        assert_eq!(store.len(), 1);
    }
}
