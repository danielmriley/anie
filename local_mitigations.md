# Local Mitigations for Empty Content Errors

## The Problem
When an LLM provider (like Ollama or OpenAI) generates a response that ultimately evaluates to an empty context block (no text, no thinking, and no tool calls), or if the generation is aborted with no content, `anie` stores an `Assistant` message with an empty `content` array:

```json
{"role":"assistant","content":[],"usage":{...},"stop_reason":"Stop","provider":"ollama"}
```

When this empty assistant message is subsequently included in the conversation history for the *next* turn, OpenAI-compatible APIs reject the payload with an HTTP 400 error:
```json
{"error":{"message":"invalid message content type: <nil>","type":"invalid_request_error","param":null,"code":null}}
```

This completely breaks the session and prevents the user from continuing.

## How `pi` Solves This
The `pi` project contains explicit mitigations for these issues within its provider abstraction layer (specifically `packages/ai/src/providers/openai-completions.ts` and `transform-messages.ts`).

The mitigations in `pi` include:
1. **Filtering Empty Assistant Messages:** In the `convert_messages` (or equivalent) step, `pi` explicitly drops `Assistant` messages from the context array if they have zero content and zero tool calls.
2. **Filtering Empty Blocks:** When parsing or converting a message, `pi` strips out text blocks or thinking blocks that consist only of whitespace or empty strings.
3. **Fallback Content:** For certain providers, if an assistant message *must* be sent (e.g., to satisfy alternating role requirements) but has no content, `pi` sends a default empty string `""` instead of `null` or `[]`.

## Plan for `anie`

After comparing the actual `pi` mitigation with `anie`'s architecture, the cleanest fix is **not** to patch every provider separately. The better one-place mitigation is to sanitize replayed transcript context in the agent loop **before** it reaches `provider.convert_messages(...)`.

That is now the preferred approach because:
1. it covers all providers at once,
2. it mirrors `pi`'s replay-time filtering behavior more closely,
3. it avoids inventing synthetic assistant text that would pollute future model context.

## What `anie` Already Does

A few protections already existed in the codebase:

- `crates/anie-providers-builtin/src/openai.rs` already drops totally empty assistant messages during OpenAI message conversion.
- The OpenAI stream state already treats a truly empty successful stop as a stream error (`"empty assistant response"`) rather than emitting a blank final assistant.

So the biggest remaining gap was **central replay sanitization** for all providers.

## Central Mitigation Applied in `crates/anie-agent/src/agent_loop.rs`

The agent loop now sanitizes the outbound context once, right before calling `provider.convert_messages(...)`.

### Sanitization rules

For replayed `assistant` messages:
- drop `StopReason::Error` and `StopReason::Aborted` messages entirely,
- remove whitespace-only `text` blocks,
- remove whitespace-only `thinking` blocks,
- if nothing remains after trimming, drop the whole assistant message.

This matches the spirit of `pi`:
- `pi/packages/ai/src/providers/transform-messages.ts` skips aborted/errored assistant turns during replay,
- `pi/packages/ai/src/providers/openai-completions.ts` filters empty text/thinking blocks and skips assistant messages with no content and no tool calls.

### Why this is better than injecting placeholder assistant text

A previous idea was to insert a fallback assistant string like:

```text
[Agent stopped generating without yielding any text or tool calls]
```

That does prevent `nil` payloads, but it is **not** what `pi` does, and it has a downside: it teaches the model a fake assistant utterance that never actually happened.

`pi`'s approach is better: **drop invalid/incomplete assistant turns from replay** rather than fabricate content.

## Current Implementation Shape

Conceptually, the fix is:

```rust
let sanitized_context = sanitize_context_for_request(&context);
let llm_context = LlmContext {
    system_prompt: self.config.system_prompt.clone(),
    messages: provider.convert_messages(&sanitized_context),
    tools: self.tool_registry.definitions(),
};
```

With helper behavior like:

```rust
fn sanitize_assistant_for_request(assistant: &AssistantMessage) -> Option<AssistantMessage> {
    if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) {
        return None;
    }

    let content = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if text.trim().is_empty() => None,
            ContentBlock::Thinking { thinking } if thinking.trim().is_empty() => None,
            _ => Some(block.clone()),
        })
        .collect::<Vec<_>>();

    if content.is_empty() {
        return None;
    }

    Some(AssistantMessage {
        content,
        ..assistant.clone()
    })
}
```

## Remaining Optional Improvement

One further hardening step is still possible:

- in the stream collection path, detect a provider-supplied `ProviderEvent::Done(...)` whose final assistant is still empty after normalization, and convert that into an explicit stream error.

That would improve the visible UX for blank turns.

However, for the original `invalid message content type: <nil>` problem, the central replay sanitization above is the key mitigation and is the closest match to the `pi` design.
