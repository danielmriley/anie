# 01e — Rollout and verification

> Part of **plan 01** (Anthropic thinking-signature replay). Read
> [01_anthropic_thinking_signatures.md](01_anthropic_thinking_signatures.md)
> for symptom and root-cause context.
>
> **Dependencies:** 01a, 01b, 01c, 01d all merged.
> **Unblocks:** production ship of the Anthropic signature fix.
> **Enforces principle:** 7 (the replay boundary is tested against
> real provider shapes before users see the change).

## Goal

Close the loop on plan 01 with the manual verifications that only a
human-driven smoke test can cover, and with an explicit go/no-go
checklist so the fix doesn't half-ship.

This sub-plan is not code. It's the release gate.

## Pre-merge automated checklist

Everything below is runnable from a clean checkout:

- [ ] `cargo check --workspace` compiles.
- [ ] `cargo test --workspace` passes — including:
  - [ ] The three new protocol roundtrip tests from **01a**.
  - [ ] The three new stream-capture tests from **01b**.
  - [ ] The serializer and sanitizer tests from **01c**.
  - [ ] The legacy-session integration test from **01d**.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] No `cache_control` markers exceed 4 on any generated request
      body (regression guard for the earlier fix).

## Pre-merge manual smoke checklist

These need a valid `ANTHROPIC_API_KEY`. Run from a built binary,
against the production Anthropic API, on each of the three main
Claude families — the original bug reproduced on at least one of
them, and each family has slightly different thinking-block behavior.

- [ ] **`claude-opus-4-7`** — thinking level High.
  1. Ask: "What's 7 × 13? Think through it."
  2. Wait for turn-1 response with visible thinking + final answer.
  3. Ask: "Now what's 7 × 14?"
  4. Turn-2 response arrives without HTTP 400. ✅

- [ ] **`claude-sonnet-4-6`** — thinking level Medium.
  1. Same two-turn flow.
  2. Turn-2 succeeds. ✅

- [ ] **`claude-haiku-4-5`** — thinking level Low.
  1. Same two-turn flow.
  2. Turn-2 succeeds. ✅

- [ ] **Tool-calling flow with thinking** (on any model).
  1. Thinking enabled, tools registered.
  2. Ask something that triggers a tool call.
  3. Tool result returns.
  4. Assistant replies with a second turn (interleaved thinking).
  5. Third turn after a user follow-up succeeds. ✅

- [ ] **Legacy session resume.**
  1. Using a pre-fix build (e.g., check out the pre-01 commit, run
     once, save the session file, return to head).
  2. Open the saved session with the post-fix build.
  3. Send a new user message.
  4. Request succeeds. No 400. INFO-log line about dropped thinking
     blocks appears in logs (if 01d sub-step D was implemented). ✅

## Rollback criteria

If any of the smoke tests fails, **do not merge**. Diagnose before
retrying:

- 400 with `thinking.signature` message → 01c sanitizer or serializer
  regressed.
- 400 with `redacted_thinking` message → unexpected; indicates a
  redacted block arrived that we silently dropped. Move plan **02**
  ahead of ship.
- 400 with any other field → escalate to plan **04** (replay error
  taxonomy) for classification before ship.
- Second-turn response is empty / missing text → unrelated to this
  plan; check plan **04** / local-mitigations doc.

## Post-merge verification

Within 24 hours of merge:

- [ ] Check logs for any `ReplayFidelity` errors (if plan 04 has
      landed) or raw `Http { status: 400 }` errors mentioning
      `signature` or `thinking`. Expected count: zero.
- [ ] Confirm at least one real user has completed a multi-turn
      thinking-enabled session against Anthropic.
- [ ] The original bug's `request_id`
      (`req_011CaCNC8FdNJLYZ2qp8qZsV`) was a specific failure; future
      requests in the same session shape should succeed.

## Exit criteria

- [ ] All automated and manual checks in this document pass.
- [ ] The branch is green in CI.
- [ ] Merge.
- [ ] Post-merge verification items complete within 24h.

## Out of scope

- Long-term monitoring / alerting infrastructure. If a regression
  happens later, the integration test from 01d should catch it pre-
  merge. If it somehow ships, plan 04's error taxonomy makes it
  visible in logs.
- User-facing changelog / release notes. Handled by whatever release
  process the project normally uses.
