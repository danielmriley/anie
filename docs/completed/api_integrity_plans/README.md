# API Integrity Plans

> **Scope.** This folder collects the plans required to make anie's
> outbound API/subscription calls robust — faithful on the wire, safe
> across retries, stable across multi-turn replay, and clearly
> diagnosable when something goes wrong. The immediate trigger was a
> production 400 from Anthropic (`messages.1.content.0.thinking.signature:
> Field required`), but the plans are scoped broader because the
> underlying class of bug — **provider-authoritative opaque state
> dropped on stream-in and invalid on replay** — almost certainly
> lurks elsewhere.

## Background: how we got here

Anie's provider abstraction treats a provider response as a stream of
`ProviderEvent`s that are collapsed into an `AssistantMessage` and then
replayed on every subsequent turn. The current stream state machines
(`crates/anie-providers-builtin/src/{anthropic.rs,openai/streaming.rs}`)
capture the *visible* parts of that stream (text, thinking text, tool
calls) and discard the rest.

That assumption — "the visible content is the full content" — is wrong
for providers that mint **opaque state** alongside visible content and
require that state back on subsequent requests. Anthropic's extended
thinking is the current failure; OpenAI's Responses API
(`encrypted_content`) is the analogous one we will eventually face; and
any provider-issued ID (tool-call IDs, citation IDs, cache tokens) is
in the same family.

The fix is not "patch Anthropic" — it is a consistent, provider-aware
architecture for preserving opaque round-trip state, plus tests that
exercise the two-turn replay path that current unit tests never hit.

## Plan index

Plans 01 and 03 have been split into fine-grained sub-plans, one per
phase, so each can ship as an independent PR. **Use the sub-plans for
implementation work.** The top-level `01_` and `03_` files are kept
as overview / reference only (symptom, root cause, field inventory)
and both carry a banner pointing at the sub-plans.

| # | File | Scope | Priority |
|---|------|-------|----------|
| 00 | [00_principles.md](00_principles.md) | Cross-cutting design principles that govern all following plans. Read first. | Read first |
| **01** | [01_anthropic_thinking_signatures.md](01_anthropic_thinking_signatures.md) | **Overview.** Current production bug. Capture `signature_delta` and replay it. | **P0** |
| 01a | [01a_protocol_field.md](01a_protocol_field.md) | Add `ContentBlock::Thinking.signature: Option<String>`. | P0 — first |
| 01b | [01b_stream_capture.md](01b_stream_capture.md) | Capture `signature_delta` events during Anthropic streaming. | P0 — after 01a |
| 01c | [01c_serializer_and_sanitizer.md](01c_serializer_and_sanitizer.md) | Emit signatures on wire; drop unsigned thinking. Fixes the 400. | P0 — after 01b |
| 01d | [01d_migration_test.md](01d_migration_test.md) | Legacy-session integration test. | P0 — after 01c |
| 01e | [01e_rollout.md](01e_rollout.md) | Pre-merge automated + manual smoke checklist. | P0 — gate |
| 02 | [02_anthropic_redacted_thinking.md](02_anthropic_redacted_thinking.md) | Handle `redacted_thinking` blocks — silently dropped today. | P1 |
| **03** | [03_roundtrip_fidelity_audit.md](03_roundtrip_fidelity_audit.md) | **Overview** and field inventory across providers. | P1 |
| 03a | [03a_stream_field_audit.md](03a_stream_field_audit.md) | Audit `_ => {}` arms + add round-trip contract doc blocks. | P1 |
| 03b | [03b_unsupported_block_rejection.md](03b_unsupported_block_rejection.md) | Explicit typed error for Anthropic server-feature blocks. | P1 |
| 03c | [03c_replay_capabilities.md](03c_replay_capabilities.md) | Move replay-fidelity flags onto `Model`, per the `ReasoningCapabilities` pattern. | P1 — after 01c |
| 03d | [03d_cross_provider_invariants.md](03d_cross_provider_invariants.md) | Table-driven invariant suite over all providers. | P1 — complements plan 06 |
| 04 | [04_replay_error_taxonomy.md](04_replay_error_taxonomy.md) | `ProviderError::ReplayFidelity` variant + UI rendering + retry policy. | P2 |
| 05 | [05_session_schema_migration.md](05_session_schema_migration.md) | Schema versioning + forward/backward compatibility for protocol types. | P2 |
| 06 | [06_integration_tests_multi_turn.md](06_integration_tests_multi_turn.md) | Replay test harness with per-scenario fixtures. | P1 (lands with 01) |

## Execution order

```
00 (principles) ──┐
                  ├──▶ 01a → 01b → 01c → 01d → 01e  (P0 fix chain, ship in that order)
                  ├──▶ 06                           (test harness; land alongside 01c–01d)
                  ├──▶ 02                           (redacted_thinking; follows 01)
                  ├──▶ 03a, 03b                     (audit + unsupported-block rejection)
                  ├──▶ 03c                          (ReplayCapabilities; follows 01c)
                  ├──▶ 03d                          (invariant suite; follows 03c + 06)
                  ├──▶ 04                           (error taxonomy polish)
                  └──▶ 05                           (schema migration; before any format change ships)
```

01a–01e is the P0 chain. Each sub-plan is its own PR; they must land
in the listed order because each depends on the previous. 06 lands
alongside 01c–01d so the fix is exercised by fixture tests before
merge.

## What this folder is *not*

- Not a refactor plan. Round-trip fidelity is a correctness issue, not
  an ergonomics one.
- Not a performance plan. Cache-control sizing is out of scope — that
  was handled by the earlier `convert_tools` fix and verified scalable.
- Not documentation for end users. These are implementation plans for
  the engineering work.

## Exit criteria for the whole folder

Every plan below is complete when:

1. The Anthropic + OpenAI providers pass a multi-turn replay integration
   test that exercises every field captured from a real provider stream.
2. The invariants in `00_principles.md` are encoded as `#[test]` or
   `debug_assert!` at call sites, not just described in prose.
3. The error taxonomy separates replay-fidelity failures from
   unknown-field failures so a UI layer can respond differently.
4. Sessions written by pre-fix binaries can still be loaded by post-fix
   binaries without panicking or sending invalid payloads.
