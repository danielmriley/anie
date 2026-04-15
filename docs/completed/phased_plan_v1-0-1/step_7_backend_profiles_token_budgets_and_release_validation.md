# Step 7 — Backend Profiles, Token Budgets, and Release Validation

This step turns the earlier implementation work into a stable, shippable v1.0.1 result.

## Why this step exists

By the time we reach this step, the code should already support:
- OpenAI-compatible local-provider correctness hotfixes
- robust transcript navigation
- provider-owned prompt steering
- tagged reasoning parsing
- reasoning capability metadata/config
- native reasoning request controls
- native separated reasoning output

What remains is to make the resulting behavior predictable, conservative, and release-ready across real local backends.

---

## Primary outcomes required from this step

By the end of this step:
- backend/model-family defaults are explainable and conservative
- verbose local reasoning does not obviously break output budgeting
- session persistence, replay, and compaction remain stable under reasoning-heavy use
- we have a real validation matrix rather than a purely theoretical plan

---

## Files expected to change

Primary:
- `crates/anie-providers-builtin/src/local.rs`
- `crates/anie-providers-builtin/src/openai.rs`
- `crates/anie-config/src/lib.rs`

Possible validation/test touches:
- `crates/anie-session/src/lib.rs`
- `crates/anie-cli/src/controller.rs` tests if runtime behavior needs stronger coverage
- docs/manual validation notes as needed

---

## Constraints

1. Explicit config must always beat heuristics.
2. Heuristics should remain narrow and explainable.
3. Do not add user-facing controls unless the earlier steps prove them necessary.
4. Token policy should be conservative and easy to revise later.

---

## Recommended implementation order inside this step

### Sub-step A — add conservative backend defaults

Use server identity and narrow model-family signals to choose a default reasoning profile when no explicit config override is present.

Recommended starting defaults:
- **Ollama recent reasoning-capable families**
  - native control
  - native output preferred, tag fallback still tolerated
- **LM Studio recent**
  - native control
  - native output preferred, tag fallback still tolerated because the user toggle may be off
- **vLLM recent with reasoning-capable family**
  - native control
  - native output preferred
- **unknown local OpenAI-compatible model**
  - prompt-only fallback by default

These are defaults, not guarantees.

### Sub-step B — keep heuristics narrow

Avoid broad fuzzy matching.

Prefer:
- server identity first
- a short allowlist of known reasoning-capable families second
- explicit config override above all heuristic guesses

This step should optimize predictability over coverage.

### Sub-step C — add token-headroom policy

Reasoning-heavy local models can consume a large amount of completion budget.

Add a simple policy tied to:
- `ThinkingLevel`
- effective reasoning profile
- possibly model max-token defaults

Examples of acceptable first-version behavior:
- reserve more headroom for `Medium` and `High`
- reserve more headroom when visible reasoning output is likely
- keep the policy simple enough to reason about in tests

Do **not** copy Anthropic’s budget model mechanically.

### Sub-step D — validate session/replay/compaction under reasoning-heavy transcripts

Because reasoning blocks can materially enlarge transcripts, explicitly validate:
- session persistence of thinking blocks
- resumed transcript reconstruction
- compaction behavior when many thinking blocks are present
- transcript replacement after compaction

If coverage gaps appear, add tests where the failure risk is greatest.

### Sub-step E — complete the validation matrix

Use the actual backend classes we care about rather than a generic “OpenAI-compatible” bucket.

Minimum manual validation targets:
- Ollama recent reasoner
- LM Studio with separation toggle on
- LM Studio with separation toggle off
- vLLM with reasoning parser enabled
- unknown local model using fallback path
- explicitly configured model with custom tags

### Sub-step F — define final ship gate for v1.0.1

Treat this step as where the feature becomes release-grade.

The final check is not just whether individual parser/request tests pass.
It is whether the whole local reasoning story is:
- understandable
- stable
- backward-compatible
- usable in the TUI

---

## Detailed code touchpoints

### `crates/anie-providers-builtin/src/local.rs`

Likely updates:
- narrow backend/model-family defaults
- server-identity-aware profile selection

### `crates/anie-providers-builtin/src/openai.rs`

Likely updates:
- token-headroom logic
- effective reasoning-profile integration refinement
- final fallback ordering polish

### `crates/anie-config/src/lib.rs`

Likely updates:
- optional config hooks for profile/tuning overrides if needed
- preserving explicit override precedence

### `crates/anie-session/src/lib.rs`

Likely test or validation-only changes:
- ensure reasoning-heavy transcripts behave sensibly under persistence/replay/compaction

---

## Test plan

### Required automated tests

1. **backend defaults resolve as intended**
   - Ollama
   - LM Studio
   - vLLM
   - unknown local

2. **explicit config overrides backend defaults**

3. **token-headroom behavior changes predictably with thinking level/profile**

4. **reasoning-heavy session replay remains stable**

5. **compaction remains stable when thinking blocks are abundant**

6. **hosted-provider behavior remains unchanged**

### Required manual validation matrix

At minimum validate one example from each category:

| Backend class | Example expected behavior |
|---|---|
| Ollama recent | native controls + native or tag fallback output |
| LM Studio with separation ON | native controls + native separated reasoning |
| LM Studio with separation OFF | native controls + tag fallback output |
| vLLM recent | native controls + native separated reasoning |
| Unknown local model | prompt-only fallback |
| Explicitly configured model | custom tag / output override |

### Release-surface checks

Also confirm:
- TUI transcript navigation still works well with long reasoning output
- print mode remains sane when reasoning is verbose
- RPC mode still behaves predictably if reasoning appears in streamed assistant content

---

## Manual validation plan

1. Exercise each backend class in the validation matrix.
2. Verify at least one reasoning-heavy session can be resumed successfully.
3. Force or trigger compaction on a reasoning-heavy transcript and verify replay remains sane.
4. Confirm the TUI remains usable with long reasoning blocks.
5. Confirm hosted-provider behavior did not regress while local support expanded.

---

## Risks to watch

1. **heuristic overreach**
   - overfitting to a few model names will make unknown models worse
2. **budget policy surprises**
   - overly aggressive headroom changes can reduce useful answer budget or increase truncation unexpectedly
3. **session-size regressions**
   - visible reasoning can materially increase transcript size and compaction pressure
4. **release ambiguity**
   - without a strict validation matrix, the feature may seem “implemented” but still feel inconsistent in practice

---

## Exit criteria

This step is complete only when all of the following are true:
- backend defaults are conservative and test-covered
- explicit config always wins over heuristics
- token-headroom behavior is predictable enough for a first release
- session persistence/replay/compaction are stable under reasoning-heavy use
- the manual validation matrix has been exercised successfully

---

## End state

When this step is complete, v1.0.1 should be ready to present local reasoning as a first-class capability rather than a hosted-only or fragile fallback experience.
