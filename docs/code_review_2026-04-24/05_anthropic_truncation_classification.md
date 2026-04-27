# 05 — Anthropic truncation classification

## Rationale

OpenAI-compatible streaming has a distinct `ProviderError::ResponseTruncated`
path for responses that exhaust output budget before producing a useful
answer. Anthropic streaming can hit a similar condition when the raw
stop reason is `"max_tokens"`, especially with reasoning-heavy models.
The review found that Anthropic's stream state can instead surface
`EmptyAssistantResponse` when no visible content was produced.

That gives the user the wrong guidance. "No visible content" suggests a
provider anomaly or unsupported content shape. `"max_tokens"` means the
request needs more output budget, less thinking, or less context.

## Design

Preserve Anthropic's raw stop reason far enough into stream finalization
to distinguish truncation from a genuinely empty response.

Options:

1. Add a protocol-level `StopReason::Length` or `StopReason::MaxTokens`.
2. Keep protocol `StopReason` unchanged and track a provider-internal
   raw Anthropic stop reason in the stream state.

Prefer option 2 unless a broader provider audit shows a common
cross-provider stop reason is needed. It avoids a session schema
question and keeps the behavior local to Anthropic's stream parser.

When finalizing:

- If raw stop reason is `"max_tokens"` and no visible assistant content
  exists, return `ProviderError::ResponseTruncated`.
- If raw stop reason is `"max_tokens"` and partial visible content
  exists, preserve the current partial response behavior unless the
  existing OpenAI path treats that as an error.
- Otherwise keep the current `EmptyAssistantResponse` behavior.

## Files to touch

- `crates/anie-providers-builtin/src/anthropic.rs`
  - Preserve raw stop reason in stream state.
  - Map max-token empty responses to `ProviderError::ResponseTruncated`.
- `crates/anie-provider/src/error.rs`
  - Only if the existing `ResponseTruncated` variant lacks the context
    needed for Anthropic guidance.
- Anthropic provider tests
  - Add SSE fixtures or state-machine tests for `"max_tokens"` with no
    visible content.

## Phased PRs

### PR A — Preserve raw stop reason in Anthropic stream state

**Change:**

- Add a stream-state field for the raw stop reason, or equivalent local
  enum.
- Populate it from `message_delta`.
- Keep existing mapped `StopReason` behavior for successful messages.

**Tests:**

- A normal stop reason still produces the same final assistant message.
- Raw `"max_tokens"` is retained through `message_stop`.

**Exit criteria:**

- The finalizer can tell truncation from generic empty output.

### PR B — Return truncation errors with actionable guidance

**Change:**

- On raw `"max_tokens"` + no visible content, return
  `ProviderError::ResponseTruncated`.
- Ensure retry policy and user-facing messages treat it like the
  existing OpenAI truncation path.

**Tests:**

- Anthropic max-token/no-visible-content fixture returns
  `ResponseTruncated`.
- Empty response without max-token stop reason still returns
  `EmptyAssistantResponse`.

**Exit criteria:**

- Users receive truncation guidance for Anthropic output-budget
  exhaustion.

## Test plan

- `cargo test -p anie-providers-builtin anthropic`
- `cargo test -p anie-cli retry`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`

## Risks

- Do not classify every `"max_tokens"` as fatal if useful visible
  content exists and current behavior intentionally returns the partial
  answer.
- If `StopReason` changes, check session serialization and schema
  compatibility before landing.

## Exit criteria

- Anthropic no-visible-content truncation is classified distinctly from
  generic empty responses.
- User guidance matches the recovery action.

