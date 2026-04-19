# 03c — `ReplayCapabilities` on `Model`

> Part of **plan 03** (round-trip fidelity audit). Read
> [03_roundtrip_fidelity_audit.md](03_roundtrip_fidelity_audit.md) for
> the full field inventory and motivation.
>
> **Dependencies:** 01c (introduced
> `Provider::requires_thinking_signature` as a provider-level method
> — this sub-plan moves that decision onto the `Model`).
> **Unblocks:** cleaner addition of future replay-related capabilities
> (encrypted reasoning, signed tool_use IDs, per-model redacted-
> thinking support, etc.).
> **Enforces principle:** 9 (one source of truth for "what provider
> supports what"; no ad-hoc `match provider.as_str()`).

## Goal

Replay-fidelity policy (does this model need signatures? does it
support redacted thinking? does it round-trip encrypted reasoning?)
lives on `Model` in a typed `ReplayCapabilities` struct, following the
pattern `ReasoningCapabilities` already uses. Sanitizer code and
provider code read the capability rather than asking the provider
trait.

This is a refactor — no behavior change relative to 01c. It sets up
02, future Responses API support, and any per-model variance that
might exist later (e.g., a future Claude variant that doesn't require
signatures, or an Opus variant that requires different replay state).

## Why this is a separate sub-plan

Plan 01c introduced `Provider::requires_thinking_signature() -> bool`
on the provider trait as the minimal change needed to fix the bug.
That placement is fine for one flag but ages poorly: each new
replay-related capability bolted onto the trait grows the trait and
spreads the decision across provider impls. Moving the capability
data onto `Model` centralizes the decision, matches the existing
`ReasoningCapabilities` pattern, and makes per-model variance
expressible without touching provider code.

## Files to change

| File | Change |
|------|--------|
| `crates/anie-provider/src/model.rs` | Add `ReplayCapabilities` struct, `replay_capabilities: Option<ReplayCapabilities>` on `Model` |
| `crates/anie-provider/src/lib.rs` | Re-export `ReplayCapabilities` |
| `crates/anie-cli/src/model_catalog.rs` | Populate `ReplayCapabilities` for the built-in Anthropic models |
| `crates/anie-provider/src/provider.rs` | Remove `requires_thinking_signature`; replace with a helper that reads from `Model` or a trait default that returns `false` |
| `crates/anie-providers-builtin/src/anthropic.rs` | Remove the `true` override |
| `crates/anie-agent/src/agent_loop.rs` | Route the sanitizer's `requires_thinking_signature` argument from `model.replay_capabilities()` rather than `provider.requires_thinking_signature()` |

## Sub-step A — The struct

In `crates/anie-provider/src/model.rs`:

```rust
/// Round-trip / replay requirements that vary per model (not per
/// provider). Populated in the model catalog for known models; `None`
/// on `Model` means "no special replay requirements" (the default
/// for OpenAI chat-completions, local models, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplayCapabilities {
    /// The provider requires every replayed thinking block to carry
    /// the cryptographic `signature` the API issued originally.
    /// Currently set on Anthropic Claude models with extended
    /// thinking support.
    pub requires_thinking_signature: bool,

    /// The provider can emit `redacted_thinking` blocks (opaque
    /// encrypted reasoning) that must be replayed verbatim. Used by
    /// plan 02.
    pub supports_redacted_thinking: bool,

    /// The provider's response contains an opaque
    /// `encrypted_content` that must be replayed to continue the
    /// reasoning chain. Reserved for future OpenAI Responses API
    /// support; currently false everywhere.
    pub supports_encrypted_reasoning: bool,
}
```

On `Model`:

```rust
pub struct Model {
    // ... existing fields ...
    pub replay_capabilities: Option<ReplayCapabilities>,
}
```

Add a helper:

```rust
impl Model {
    /// Return the effective replay capabilities for this model,
    /// falling back to `ReplayCapabilities::default()` (all false)
    /// when nothing is declared.
    #[must_use]
    pub fn effective_replay_capabilities(&self) -> ReplayCapabilities {
        self.replay_capabilities.clone().unwrap_or_default()
    }
}
```

## Sub-step B — Catalog population

In `crates/anie-cli/src/model_catalog.rs`, populate for every
Anthropic model with extended thinking support:

```rust
replay_capabilities: Some(ReplayCapabilities {
    requires_thinking_signature: true,
    supports_redacted_thinking: true,
    supports_encrypted_reasoning: false,
}),
```

For OpenAI chat-completions models and local models, leave `None`
(or set to explicit default); the sanitizer treats `None` the same as
all-false.

Double-check that every Anthropic model in the catalog gets the
struct — grep for `provider: "anthropic"` or equivalent.

## Sub-step C — Remove the provider-trait method

In `crates/anie-provider/src/provider.rs`, remove:

```rust
fn requires_thinking_signature(&self) -> bool {
    false
}
```

In `AnthropicProvider` (`crates/anie-providers-builtin/src/anthropic.rs`),
remove the `true` override.

## Sub-step D — Route through `Model`

In `crates/anie-agent/src/agent_loop.rs`, update the sanitizer call:

```rust
let replay = self.config.model.effective_replay_capabilities();
let sanitized_context = sanitize_context_for_request(
    &context,
    provider.includes_thinking_in_replay(),
    replay.requires_thinking_signature,
);
```

`includes_thinking_in_replay` stays on the provider for now — it's a
property of the wire format, not the model (though plan 02 might
revisit). If preferred, fold it into `ReplayCapabilities` as a
separate sub-plan.

## Sub-step E — Test updates

Any test that stubbed `Provider::requires_thinking_signature` must be
updated to construct a `Model` with the appropriate
`replay_capabilities` instead. Specifically:

- The sanitizer tests added in 01c.
- Any provider-trait mock in `anie-agent/src/tests.rs`.

Example test helper:

```rust
fn anthropic_model_with_signatures() -> Model {
    Model {
        // ... other fields ...
        replay_capabilities: Some(ReplayCapabilities {
            requires_thinking_signature: true,
            ..Default::default()
        }),
    }
}
```

## Verification

- [ ] `cargo check --workspace` compiles.
- [ ] `cargo test --workspace` passes — in particular, every sanitizer
      test from 01c still passes with the new routing.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] Behavior under a two-turn smoke test against Anthropic matches
      the post-01c baseline (same 400 fix, same sanitizer drops).

## Exit criteria

- [ ] `ReplayCapabilities` exists on `Model`.
- [ ] All Anthropic catalog entries declare signatures required.
- [ ] Sanitizer reads from `Model`, not from the provider trait.
- [ ] No `requires_thinking_signature` on `Provider` anymore.
- [ ] Existing 01c tests still pass.

## Out of scope

- Populating `supports_redacted_thinking` usage — that's plan **02**'s
  concern; 03c just reserves the field.
- Populating `supports_encrypted_reasoning` — future Responses API
  support.
- Reworking `includes_thinking_in_replay` — defer unless we find
  per-model variance for it.
