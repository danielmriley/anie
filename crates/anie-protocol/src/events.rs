use crate::{AssistantMessage, Message, StreamDelta, ToolResult, ToolResultMessage};

/// Why a compaction fired. Attached to `CompactionStart` /
/// `CompactionEnd` events so the UI and telemetry can
/// distinguish proactive (pre-prompt, mid-turn) compactions
/// from reactive overflow recovery without inspecting the
/// emitting call site. Plan
/// `docs/midturn_compaction_2026-04-27/06_compaction_telemetry.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionPhase {
    /// Triggered before sending a new user prompt
    /// (`InteractiveController::maybe_auto_compact`). Today's
    /// default behavior; corresponds to the "compacting" badge
    /// users have seen since pre-PR.
    PrePrompt,
    /// Triggered between sampling requests inside an active
    /// agent loop (`ControllerCompactionGate`). New as of
    /// PR 8.4 of the midturn-compaction plan.
    MidTurn,
    /// Triggered by `RetryDecision::Compact` after a provider
    /// `ContextOverflow` error
    /// (`InteractiveController::retry_after_overflow`). Always
    /// reactive — the failed sampling request is already on
    /// the wire.
    ReactiveOverflow,
}

/// In-process events emitted by the agent loop.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// The agent run has started.
    AgentStart,
    /// The agent run has ended.
    AgentEnd { messages: Vec<Message> },
    /// A turn has started.
    TurnStart,
    /// A turn has ended.
    TurnEnd {
        assistant: AssistantMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    /// A message has started.
    MessageStart { message: Message },
    /// A message delta was received.
    MessageDelta { delta: StreamDelta },
    /// A message has ended.
    MessageEnd { message: Message },
    /// A tool execution has started.
    ToolExecStart {
        call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    /// A tool execution emitted a partial update.
    ToolExecUpdate {
        call_id: String,
        partial: ToolResult,
    },
    /// A tool execution has finished.
    ToolExecEnd {
        call_id: String,
        result: ToolResult,
        is_error: bool,
    },
    /// Replace the rendered transcript with reconstructed history.
    TranscriptReplace { messages: Vec<Message> },
    /// A neutral controller-originated message for transcript display.
    SystemMessage { text: String },
    /// rlm policy fired and updated its external archive.
    /// The TUI listens for these to refresh just the
    /// `archive: N msgs` segment of the status bar without
    /// needing a full `StatusUpdate` (which the controller
    /// only emits at user-action boundaries).
    RlmStatsUpdate { archived_messages: u64 },
    /// Status-bar state changed outside provider-stream events.
    StatusUpdate {
        provider: String,
        model_name: String,
        thinking: String,
        estimated_context_tokens: u64,
        context_window: u64,
        cwd: String,
        session_id: String,
        /// Harness-mode label ("current" | "baseline" | "rlm").
        /// Surfaces in the TUI status bar so the user always
        /// knows which profile they're running.
        harness_mode: String,
        /// Total messages currently in the rlm external store
        /// (the archive the recurse tool reads from). Updated
        /// by the policy after every fire. `0` outside rlm
        /// mode. The TUI shows this so the user has an
        /// ambient signal that the virtualization is doing
        /// work (the count grows as the conversation
        /// progresses).
        rlm_archived_messages: u64,
    },
    /// Context compaction has started.
    CompactionStart {
        /// Why this compaction is firing. Plan 06 of
        /// `docs/midturn_compaction_2026-04-27/`.
        phase: CompactionPhase,
    },
    /// Context compaction completed successfully.
    CompactionEnd {
        /// Why this compaction fired. Mirrors the
        /// `CompactionStart::phase` value so consumers don't
        /// have to remember the most recent start to label
        /// the end.
        phase: CompactionPhase,
        summary: String,
        tokens_before: u64,
        tokens_after: u64,
    },
    /// A transient provider failure has been scheduled for retry.
    RetryScheduled {
        attempt: u32,
        max_retries: u32,
        delay_ms: u64,
        error: String,
    },
}
