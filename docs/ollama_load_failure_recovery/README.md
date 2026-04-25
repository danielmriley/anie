# Ollama load-failure recovery

**When Ollama refuses to load a model because the requested
`num_ctx` exceeds available memory, classify the error
distinctly, surface an actionable message that points the user
at `/context-length`, and retry once with a halved `num_ctx`
before giving up.**

## Context

After
[`docs/ollama_native_chat_api/README.md`](../ollama_native_chat_api/README.md)
shipped, anie sends `options.num_ctx = model.context_window` on
every `/api/chat` request, and `Model.context_window` reflects
the architectural max from `/api/show` (PR 6 of that plan plus
the `local-probe` follow-up at commit `8714b33`). For most
models on most hardware this is correct: Ollama allocates the
KV cache buffer up to `num_ctx` at model load time, spills to
system RAM when VRAM is tight, and the request proceeds.

For some `(model, num_ctx, hardware)` combinations Ollama
cannot fit the buffer at all and rejects the load. Today
anie's response classifier
([`crates/anie-providers-builtin/src/ollama_chat/mod.rs:159`](../../crates/anie-providers-builtin/src/ollama_chat/mod.rs))
covers `Auth (401/403)`, `RateLimited (429)`, and the
think/thinking patterns from
[`ollama_caps/PR2`](../ollama_capability_discovery/README.md).
Load-failure bodies fall through to the generic
`classify_http_error` and surface as
`ProviderError::Http { status: 500, .. }` — terminal in
[`retry_policy.rs`](../../crates/anie-cli/src/retry_policy.rs)'s
`decide` arm for `Http`. The user sees a raw 500 and has no
direct path to the fix (`/context-length` slash command).

This plan closes that gap.

### What KV-cache allocation actually looks like

Worth stating precisely because the failure mode depends on
this:

- **The KV cache buffer is allocated up front at model load
  time**, sized to `num_ctx × num_layers × 2 (k+v) × head_dim ×
  num_heads × 2 bytes (f16)`. It is NOT lazy-allocated as
  tokens fill the context.
- **Ollama spills to system RAM** when GPU VRAM is insufficient,
  so OOM only manifests when total available memory (VRAM + RAM)
  cannot fit `weights + KV cache + overhead`.
- For small models (`qwen3.5:0.8b` at ~870 MB weights) even
  `num_ctx = 262_144` fits in a few GB and runs fine on most
  systems. We verified this empirically on the user's setup
  during the local-probe smoke.
- For large models on constrained hardware (`qwen3.5:35b` at
  ~22 GB weights × `num_ctx = 262_144` × the per-layer KV
  arithmetic above ≈ 50 GB total demand) the load fails on a
  16 GB / 32 GB Mac.

So the affected population is "users running larger models on
constrained hardware." The slash command is the fix; this plan
makes the fix discoverable.

### What a load failure looks like on the wire

**Empirical verification required before PR 1 lands.** The
implementer must produce a real load failure (e.g., set
`/context-length 1048576` on a model that doesn't fit, or
deliberately probe a model that exceeds available memory) and
capture:

```bash
# Trigger a deliberately oversized load. The response body
# carries the actual error wording — copy it verbatim into
# tests, do not paraphrase.
curl -s -X POST http://localhost:11434/api/chat \
  -H 'Content-Type: application/json' \
  -d '{"model":"qwen3:32b","messages":[{"role":"user","content":"hi"}],
       "stream":false,"options":{"num_ctx":1048576}}'
```

Anticipated body shapes (to confirm against live Ollama; do
NOT hard-code from this plan):

- `{"error":"model requires more system memory (...) than is available"}`
- `{"error":"failed to load model: context size exceeds available memory"}`
- `{"error":"requested ... but only ... bytes available"}`
- HTTP 500 status code with the body inline.

The same evidence-first discipline as
[`docs/ollama_native_chat_api/README.md`](../ollama_native_chat_api/README.md)
PR 3: paste the raw body into the PR description so the test
fixtures match what a real Ollama actually returns.

## Design

Three layers, each independently useful:

1. **Recognize the error** — extend
   `classify_ollama_error_body` with load-failure body patterns
   and a new typed variant.
2. **Retry once with halved `num_ctx`** — same one-strategy
   retry pattern as PR 4 of native_chat (drop-`think`-and-retry).
   If the halved request also fails, surface the typed error.
3. **Make the error message actionable** — include the
   suggested `/context-length` value in the user-facing
   message, so the user sees "try `/context-length 32768`"
   not just "the model failed to load."

### New typed variant

Add `ProviderError::ModelLoadResources` to
[`crates/anie-provider/src/error.rs`](../../crates/anie-provider/src/error.rs):

```rust
/// The provider rejected a request because the requested
/// resources (typically `num_ctx` for Ollama) exceed available
/// memory. Carries a suggested smaller value the caller can
/// retry with, or surface to the user as recovery guidance.
///
/// anie-specific (not in pi): pi does not have a native Ollama
/// codepath, so this failure mode doesn't reach pi's error
/// taxonomy. Mark this variant accordingly per CLAUDE.md §3.
ModelLoadResources {
    /// Original wire body from the provider, unmodified.
    body: String,
    /// Suggested `num_ctx` for a retry — half of what the
    /// failed request used, rounded down to the nearest 1 KiB
    /// for tidy logging.
    suggested_num_ctx: u64,
},
```

Routed in `retry_policy::decide` to `RetryDecision::GiveUp` at
the outer (controller) layer, since the inner-strategy retry
in `OllamaChatProvider::stream` handles the recoverable case.
The outer give-up is what surfaces the actionable message to
the user — the provider has already tried halving once.

### Body-pattern recognition

In `classify_ollama_error_body`, add a check before the
generic fall-through:

```rust
fn looks_like_load_resource_failure(body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    // Substrings collected from the empirical-verification
    // checklist above. Add each one to a dedicated test
    // fixture; do not over-broaden the match — false
    // positives would convert unrelated 500s into
    // num_ctx retries.
    body.contains("requires more system memory")
        || body.contains("more system memory")
        || body.contains("exceeds available memory")
        || (body.contains("memory") && body.contains("available"))
        || body.contains("failed to load model")
}
```

Same conservative-classification stance as
`looks_like_native_reasoning_compat_body` at
[`reasoning_strategy.rs:201-220`](../../crates/anie-providers-builtin/src/openai/reasoning_strategy.rs).
Add a NEGATIVE test for each unrelated 500 we don't want
upgraded.

### Retry strategy

In `OllamaChatProvider::stream`
([`mod.rs`](../../crates/anie-providers-builtin/src/ollama_chat/mod.rs)),
mirror the existing PR-4 retry-without-`think` pattern:

```rust
let primary_attempt = self.send_once(&model, &context, &options).await;
match primary_attempt {
    Err(ProviderError::ModelLoadResources { suggested_num_ctx, .. }) => {
        let mut halved_options = options.clone();
        halved_options.num_ctx_override = Some(suggested_num_ctx);
        self.send_once(&model, &context, &halved_options).await
        // If this also returns ModelLoadResources, propagate
        // the halved-attempt's error so the user sees the
        // smaller number in the message.
    }
    other => other,
}
```

Notes:
- `suggested_num_ctx` is `request_num_ctx / 2`, floored to a
  clean number. The caller doesn't see this value unless the
  retry also fails.
- The halved retry happens **once**. No exponential decay loop
  — Ollama's reload cost is multi-second per attempt, and a
  user with severely undersized hardware will hit the same
  failure at quarter-size anyway.
- If the user has a runtime override active, the suggested
  value still halves the override. The user can reset via
  `/context-length reset` to fall back to the catalog value.

### User-facing message

When the give-up surfaces, the message that lands in the TUI
should read approximately:

> Model `qwen3.5:35b` could not be loaded with `num_ctx =
> 262144`. Tried `131072`, also failed. Try a smaller value
> with `/context-length 32768` (or whatever fits your
> hardware), then resend.

Format mirrors the existing user-facing error wording in
[`anie-cli/src/user_error.rs`](../../crates/anie-cli/src/user_error.rs).

## Files to touch

| File | PR | What |
|------|----|------|
| `crates/anie-provider/src/error.rs` | 1 | Add `ModelLoadResources { body, suggested_num_ctx }` variant; serde round-trip test |
| `crates/anie-providers-builtin/src/ollama_chat/mod.rs` | 1 | Add `looks_like_load_resource_failure` helper; route in `classify_ollama_error_body` to `ModelLoadResources`; tests against captured real bodies |
| `crates/anie-cli/src/retry_policy.rs` | 2 | Route `ModelLoadResources` to `GiveUp::Terminal` at the outer layer (the inner retry is the recovery path) |
| `crates/anie-providers-builtin/src/ollama_chat/mod.rs` | 2 | Add the one-strategy retry: on `ModelLoadResources`, retry once with `suggested_num_ctx`; if still fails, propagate |
| `crates/anie-cli/src/user_error.rs` | 3 | Render `ModelLoadResources` with the actionable `/context-length <N>` hint |

## Phased PRs

### PR 1 — Recognize Ollama load-failure bodies, route to typed variant

**Why first:** smallest, recognition-only. After this PR a
load failure surfaces as a clean typed error in logs and the
user-facing path, but no retry behavior changes yet. Gives the
implementer a chance to land the body-pattern recognition
without coupling it to the retry semantics.

**Empirical verification checklist** (must run before writing
the body-pattern matcher):

```bash
# From a machine with a local Ollama and at least one model
# pulled, deliberately overload num_ctx.

# 1. Massive num_ctx on a small model — body shape on a
#    successful refusal:
curl -s -X POST http://localhost:11434/api/chat \
  -d '{"model":"qwen3:8b","messages":[{"role":"user","content":"hi"}],
       "stream":false,"options":{"num_ctx":33554432}}'

# 2. num_ctx larger than the model's architectural max (Ollama
#    may clamp silently or return an error — capture which):
curl -s -X POST http://localhost:11434/api/chat \
  -d '{"model":"qwen3:8b","messages":[{"role":"user","content":"hi"}],
       "stream":false,"options":{"num_ctx":1048576}}'

# 3. Repeat against a larger model where loading fits but
#    num_ctx is the binding constraint, e.g., qwen3:32b at
#    several million:
curl -s -X POST http://localhost:11434/api/chat \
  -d '{"model":"qwen3:32b","messages":[{"role":"user","content":"hi"}],
       "stream":false,"options":{"num_ctx":4194304}}'

# 4. HTTP status code: confirm whether Ollama returns 500, 503,
#    400, or something else. The classifier must match on body
#    pattern, not status alone.
```

Paste raw outputs into the PR description.

**Scope:**

- `error.rs`: add the `ModelLoadResources` variant. Implement
  `Display` with the suggested-num_ctx value. Round-trip test.
- `ollama_chat/mod.rs`:
  - Add `looks_like_load_resource_failure` private helper.
  - Add `parse_request_num_ctx_for_suggestion` — read the
    just-sent body or pass it through. Cleanest: capture the
    request's `num_ctx` when building the body and thread it
    into `classify_ollama_error_body` as a parameter.
  - Wire the recognition into `classify_ollama_error_body`,
    returning `ModelLoadResources { body, suggested_num_ctx:
    request_num_ctx / 2 }`.
- Tests (each must use a body string captured from the
  empirical session above, not invented):
  - `classify_ollama_error_body_recognizes_requires_more_system_memory`
  - `classify_ollama_error_body_recognizes_failed_to_load_model`
  - `classify_ollama_error_body_recognizes_exceeds_available_memory`
  - **Negative**:
    `classify_ollama_error_body_does_not_misclassify_unrelated_500`
    (a body about a missing field stays `Http { 500 }`).
  - `model_load_resources_suggested_num_ctx_is_half_of_requested`
  - `model_load_resources_suggested_num_ctx_floors_below_2048_at_2048`
    (lower bound — never suggest a value below the
    `/context-length` minimum from
    [`ollama_context_length_override`](../ollama_context_length_override/README.md)).

**Exit criteria:**

- A real Ollama load failure produces
  `ProviderError::ModelLoadResources` instead of
  `Http { 500, .. }`.
- The variant carries the original body verbatim plus a
  suggested halved `num_ctx`.
- No retry behavior change yet.

### PR 2 — Halved-`num_ctx` retry in `OllamaChatProvider::stream`

**Why second:** independent of PR 3, depends on PR 1's typed
error. After this PR, a load-failure that's recoverable via a
smaller `num_ctx` recovers automatically; only persistent
failures (halved size still doesn't fit) surface to the user.

**Scope:**

- `ollama_chat/mod.rs`:
  - Refactor `stream()` so the request-build + send is in a
    private helper `send_once(&self, model, context, options)`.
  - Wrap the helper call in the
    `ProviderError::ModelLoadResources` retry: on first failure
    of that variant, build a fresh `StreamOptions` with
    `num_ctx_override = Some(suggested_num_ctx)`, retry once.
  - **Idempotency:** if the second attempt also returns
    `ModelLoadResources`, propagate the second error (so the
    user sees the smaller number in the message).
- `retry_policy.rs::decide`: route `ModelLoadResources` to
  `GiveUp { reason: Terminal }`. The inner retry is the
  recovery surface — the outer policy should not retry again.
- Tests (use a mock Ollama via the existing test harness):
  - `ollama_load_failure_triggers_one_halved_retry_then_surfaces`
  - `ollama_load_failure_recovered_by_halved_retry_streams_normally`
    (mock returns load-failure on `num_ctx = N`, success on
    `num_ctx = N/2`; assert the stream completes).
  - `retry_policy_decide_classifies_model_load_resources_as_terminal`

**Exit criteria:**

- A load failure recoverable at half-size proceeds
  transparently from the user's perspective (one extra
  multi-second wait while Ollama reloads).
- A load failure that's not recoverable surfaces as a single
  clean error, not a retry storm.

### PR 3 — Actionable user-facing message via `user_error.rs`

**Why third:** UX polish on top of the typed error and retry.
Lets the user act on the failure without consulting docs.

**Scope:**

- `anie-cli/src/user_error.rs`:
  - Match `ProviderError::ModelLoadResources` and produce a
    multi-line message:

    ```
    Model 'qwen3.5:35b' could not be loaded with the requested
    context window (262144 tokens). Tried 131072, also failed.

    Try a smaller value: /context-length 32768
    ```

  - Use the suggested-num_ctx value as the second-attempt
    number; mention "and tried halved" only when the inner
    retry actually ran.
- Tests:
  - `user_error_for_model_load_resources_includes_context_length_hint`
  - `user_error_message_mentions_attempted_halved_value`
  - `user_error_renders_provider_body_excerpt`
    (the raw body fragment is preserved for debugging, but
    truncated at ~200 chars to avoid wall-of-text in the TUI).

**Exit criteria:**

- Users hitting the failure see an actionable next step.
- The message includes both the originally-attempted value
  and the smaller value the inner retry tried.

## Test plan

Per-PR tests above. Cross-cutting:

| # | Test | Where |
|---|------|-------|
| Manual | qwen3:32b on local Ollama with `/context-length 1048576`. Verify either the inner retry recovers (Ollama loads with 524288) or the user sees the actionable message. | smoke |
| Manual | qwen3:8b on local Ollama with default num_ctx — must continue to work unchanged (no false-positive classification of normal responses). | regression |
| Manual | qwen3:8b on local Ollama with `/context-length 32768` and a model whose architectural max is 40960 — verify nothing is mis-classified as a load failure. | regression |
| Auto | `cargo test --workspace --no-fail-fast` green. | CI |
| Auto | `cargo clippy --workspace --all-targets -- -D warnings` clean. | CI |

## Risks

- **False-positive classification.** The body-pattern matcher
  must be conservative. A normal model output that happens to
  contain "memory" and "available" should NEVER hit the
  classifier — but normal output is in the streaming response,
  not the error body, so this is contained. Negative tests in
  PR 1 guard the boundary.
- **Two reloads on persistent failure.** A halved retry that
  also fails costs a second model load attempt — multi-second
  latency. Worth the UX of clean recovery; document in the PR
  body so future maintainers don't try to "optimize" this away.
- **Suggested value below the override minimum.** The
  `/context-length` plan sets a 2048 minimum. The halving
  arithmetic must floor at 2048 — see PR 1 test
  `model_load_resources_suggested_num_ctx_floors_below_2048_at_2048`.
- **Empirical body shapes can drift.** Ollama's error wording
  has changed between releases. The body-pattern matcher uses
  multiple substring alternatives so a single rewording
  upstream doesn't break recognition. Document the captured
  bodies + Ollama version in the PR description.
- **Concurrent runtime override and retry.** If the user
  changes `/context-length` while a halved retry is in flight,
  the next request uses the new value; the in-flight retry
  uses the captured `suggested_num_ctx`. Acceptable;
  document.

## Exit criteria

- [ ] PR 1 merged: load-failure bodies are recognized and
      surface as `ProviderError::ModelLoadResources`.
- [ ] PR 2 merged: a single halved-`num_ctx` retry happens
      automatically before the error reaches the user.
- [ ] PR 3 merged: the user-facing message includes a
      concrete `/context-length <N>` next-step.
- [ ] Cross-cutting smoke (manual + regression) passes.
- [ ] `cargo test --workspace --no-fail-fast` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] No anie-specific deviation from pi unflagged: the entire
      `ModelLoadResources` variant is one such deviation
      (commented at variant definition per CLAUDE.md §3).

## Deferred

- **VRAM probing at discovery time.** anie could call
  `/api/version` or query system memory (`nvidia-smi`,
  `sysctl hw.memsize`) at discovery time to clamp
  `Model.context_window` proactively. Adds platform-specific
  code, multiple system probes, and is preempted by the
  reactive recovery path above. Defer until a user reports
  the actionable-error UX is insufficient.
- **`/api/ps`-based suggestions after success.** After a
  successful load, anie could query `/api/ps` to see what
  Ollama actually allocated. If Ollama's runtime reduced the
  effective `num_ctx` below what we requested (a feature being
  added in newer Ollama versions), we could log a one-time
  hint. Informational only, no behavior change.
- **Conservative default cap on first-time setup.** A
  separate plan covers this:
  [`../ollama_default_num_ctx_cap/README.md`](../ollama_default_num_ctx_cap/README.md).
  Distinct concern: this plan is reactive (handle the failure
  cleanly); the cap plan is preventive (avoid the failure on
  known-constrained hardware).

## Reference

### Ollama docs

- `/api/chat` reference:
  <https://github.com/ollama/ollama/blob/main/docs/api.md#generate-a-chat-completion>
- KV cache and memory behavior — Ollama issues + llama.cpp
  source. Cite specific commits in PR 1's body when capturing
  empirical bodies.

### anie sites

- Native `/api/chat` codepath: `docs/ollama_native_chat_api/README.md`
  — provides `OllamaChatProvider`.
- `/context-length` slash command:
  `docs/ollama_context_length_override/README.md` — the
  recovery surface for the user-facing message.
- Existing classifier shape:
  `crates/anie-providers-builtin/src/ollama_chat/mod.rs:159`
  (`classify_ollama_error_body`).
- One-strategy retry precedent (drop-`think`):
  `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs:201-220`
  (`looks_like_native_reasoning_compat_body`) and
  `crates/anie-providers-builtin/src/openai/mod.rs::send_stream_request`
  loop.
