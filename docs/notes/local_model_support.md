# Local Model Support

## Summary

Improve support for local/self-hosted models: context length detection,
thinking level integration, and automatic compaction.

## Current State

- Local models are detected via Ollama/LM Studio probing on startup
- Reasoning capabilities are inferred by model family heuristics
- Context window defaults to 32,768 tokens for discovered models
- Thinking levels affect prompt steering, native reasoning fields
  (when supported), and token headroom
- Compaction exists but is not automatic

## What Was Fixed (reasoning_fix_plan.md)

- Thinking-only completions are now rejected as errors
- Thinking blocks are stripped from OpenAI-compatible replay
- Provider-aware replay policy is explicit
- `ThinkingRequestMode` provides declarative request-shape control

## Remaining Action Items

### 1. Context length detection
Local models have varying context lengths. Improve detection:
- Query Ollama `/api/show` for model metadata
- Query vLLM `/v1/models` for `max_model_len`
- Allow manual override in config
- Fall back to conservative defaults if detection fails
- Store discovered lengths to avoid repeated queries
- Surface the active context length in the status bar

### 2. Automatic context compaction
Trigger compaction automatically when approaching the context limit:
- Configurable threshold (e.g., 80-90% of context used)
- Summarize older messages while preserving recent context
- Keep configurable amount of recent tokens verbatim
- Reserve tokens for the model's response
- Visual indicator in TUI when compaction occurs
- Manual trigger via `/compact` command

Compaction settings already exist in config (`compaction.enabled`,
`reserve_tokens`, `keep_recent_tokens`). The missing piece is the
automatic trigger based on context usage.

### 3. Parallel tool calling
Investigate and potentially enable parallel tool execution:
- Multiple tool calls from one assistant message execute concurrently
- File-mutating tools use per-file queues to avoid race conditions
- TUI surfaces concurrent execution clearly
- Handle partial failures (one tool errors, others succeed)

Anie's agent loop already supports parallel tool calling in the code
(`parallel_tool_calls_execute_concurrently` test exists). The question
is whether the TUI and error handling are ready for it in practice.

## Priority

- Context length detection: Medium — affects model usability
- Automatic compaction: Medium — prevents context overflow errors
- Parallel tool calling: Low — performance optimization
