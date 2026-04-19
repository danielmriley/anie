# 04 — Replay error taxonomy

> **Priority: P2.** UX/diagnostics polish. Not a correctness fix, but
> the difference between "user sees an inscrutable 400 and abandons
> the session" and "user gets a clear error and starts a new
> conversation." Enforces principles 5, 10.

## Why this plan exists

Today, a 400 from Anthropic for a missing signature surfaces to the
user as:

```
HTTP error: 400 {"type":"error","error":{"type":"invalid_request_error",
"message":"messages.1.content.0.thinking.signature: Field required"},
"request_id":"req_011CaCNC8FdNJLYZ2qp8qZsV"}
```

That's accurate but useless to the user. After plan 01, this specific
400 shouldn't recur; but similar 400s will happen whenever a new
provider feature outpaces our capture logic, or when sessions are
shared between binaries with different schema support.

We need a taxonomy that:

1. Tells us (in code) when a 400 is a replay-fidelity issue vs. a
   genuine malformed request.
2. Gives the user an actionable message ("this session is incompatible
   with the current provider; start a new conversation").
3. Never retries a replay-fidelity failure (infinite loop protection).
4. Is testable without pinning brittle substring matches.

## Current state

`ProviderError` in `crates/anie-provider/src/error.rs` already separates:

- `Auth` (401/403)
- `RateLimited` (429 with Retry-After)
- `ContextOverflow` (triggers compaction)
- `Http { status, body }` (catch-all HTTP error)
- `NativeReasoningUnsupported` (400s that indicate a reasoning-field
  compat failure — body-pattern matched once in
  `classify_openai_http_error`)

The pattern for `NativeReasoningUnsupported` is exactly what we want
for replay fidelity: classify at the boundary, surface a typed
variant, drive retry/UX from that variant.

## Design

Add two variants and one classification helper, mirroring the pattern
in `openai/reasoning_strategy.rs:classify_openai_http_error`.

### New variants

```rust
/// A 400 whose body indicates that the request carried a message or
/// content block that's structurally invalid *for replay* (e.g.,
/// missing a required opaque field the provider minted on a prior
/// turn). Not retryable; session should be restarted.
#[error("Replay fidelity error ({provider_hint}): {detail}")]
ReplayFidelity {
    provider_hint: &'static str, // e.g., "anthropic", "openai"
    detail: String,               // trimmed body snippet, for logs
},

/// A 400 whose body indicates a feature our request referenced is not
/// supported by this deployment (model, region, account tier).
/// Not retryable; distinct from `NativeReasoningUnsupported`, which
/// has a specific fallback path.
#[error("Feature not supported by provider: {0}")]
FeatureUnsupported(String),
```

### Classification helper

In `crates/anie-providers-builtin/src/anthropic.rs`, introduce
`classify_anthropic_http_error` mirroring the OpenAI equivalent:

```rust
pub(crate) fn classify_anthropic_http_error(
    status: reqwest::StatusCode,
    body: &str,
    retry_after_ms: Option<u64>,
) -> ProviderError {
    if status.as_u16() == 400 && looks_like_replay_fidelity(body) {
        return ProviderError::ReplayFidelity {
            provider_hint: "anthropic",
            detail: body.chars().take(500).collect(),
        };
    }
    crate::classify_http_error(status, body, retry_after_ms)
}

fn looks_like_replay_fidelity(body: &str) -> bool {
    // Anthropic error messages for this category include phrases like:
    // - "thinking.signature: Field required"
    // - "redacted_thinking: Field required"
    // - "content.N.thinking.signature"
    let lower = body.to_ascii_lowercase();
    (lower.contains("thinking") && lower.contains("signature"))
        || lower.contains("redacted_thinking")
        || (lower.contains("messages.") && lower.contains(".thinking"))
}
```

Wire it in at `anthropic.rs:135-139` in place of the generic
`classify_http_error` call.

### Retry policy update

`RetryPolicy::decide` (`anie-cli/src/retry_policy.rs:64`) already
terminates on non-retryable errors. Add explicit arms:

```rust
ProviderError::ReplayFidelity { .. }
| ProviderError::FeatureUnsupported(_) => RetryDecision::GiveUp {
    reason: GiveUpReason::Terminal,
},
```

This is a no-op behaviorally (they'd fall through to `Http { .. }`
with a non-retry status anyway) — the explicit arm exists so the
intent is documented and future reviewers don't have to trace it.

### UI rendering

`crates/anie-tui/src/app.rs` and related assistant-error rendering
paths currently just show the `ProviderError::Display` output. Render
`ReplayFidelity` specially:

```
Session incompatible with provider
──────────────────────────────────
The assistant's prior response contained data this conversation can't
replay (the provider refused the request). Starting a new conversation
will fix this.

Provider: anthropic
Request ID: req_… (see logs for full error)
```

Offer a keybinding or affordance to reset the session without losing
the workspace (the `/new` command already exists — just suggest it).

## Phase 1 — Variants + classifier

**Files:**
- `crates/anie-provider/src/error.rs` — add variants.
- `crates/anie-providers-builtin/src/anthropic.rs` — classifier + wire-in.
- `crates/anie-providers-builtin/src/lib.rs` — re-export if needed.

**Verification:** unit tests in anthropic.rs for classifier:
- body with signature phrase → `ReplayFidelity`
- body with `redacted_thinking` → `ReplayFidelity`
- generic 400 → `Http`
- 401 → `Auth`

## Phase 2 — Retry policy arm

**Files:** `crates/anie-cli/src/retry_policy.rs`.

Trivial. Add arm, add test.

## Phase 3 — UI rendering

**Files:** `crates/anie-tui/src/app.rs`, likely also
`crates/anie-tui/src/widgets/panel.rs`.

Add a branch in the assistant-error renderer for `ReplayFidelity`.
Keep the full body accessible via a "show details" toggle for
debugging.

## Phase 4 — Structured logging

When a replay-fidelity error fires, emit a structured log line
containing:
- model id
- turn index
- `detail` snippet
- `request_id` (extracted from body)

This is the breadcrumb trail for post-incident debugging. Plain `eprintln!`
to stderr is fine — the project doesn't yet use a structured logger.

## Out of scope

- Automatic session recovery (restart conversation, replay last user
  turn). Tempting, but risks silent data loss. Keep the user in the
  loop.
- OpenAI-equivalent classifier. OpenAI chat-completions doesn't
  round-trip opaque reasoning state, so it doesn't have this failure
  mode today. Revisit when Responses API support lands.
