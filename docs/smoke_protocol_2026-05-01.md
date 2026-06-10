# Smoke-test protocol for harness changes (2026-05-01)

A repeatable multi-turn smoke test for validating
context-virtualization and small-model harness changes.
Re-run this test before merging any change that touches
the eviction policy, reranker, summarizer, recurse tool,
or system-prompt augment. Compare turn-by-turn behavior
against the baseline run captured here.

## Why this scenario

The 11-turn run on 2026-05-01 (session `ab03cc6f`,
qwen3.5:9b) caught real failure modes that a 2-3 turn
hello-world smoke would miss:

- **Real engineering coherence over many turns:** wrote
  buggy code in T3, parsed compiler errors, fixed and
  recompiled — the kind of recovery that distinguishes a
  capable harness from a chat wrapper.
- **Cargo-culted improvements:** T5 added a perfect-
  forwarding `insertHead(T&& value) {
  insertHead(std::forward<T>(value)); }` — infinite
  recursion. The model never caught it because it didn't
  re-verify after editing.
- **Hallucinated success on tool error:** T7 reported
  "compiled and ran successfully!" when the binary was
  segfaulting and `[tool error]` came back from bash.
- **Topic switch + tool reach:** T10 ("what should I
  wear Saturday?") tested whether the model
  autonomously reaches for `web_search`/`web_read` when
  the system prompt tells it to. It did not. T11
  ("actually check the forecast") confirmed it can
  follow through when prompted explicitly.
- **Cross-domain reranker isolation:** the wardrobe
  question paged in only 1 archived msg from the 52-msg
  C++ archive — the embedding reranker correctly judged
  the DLL conversation irrelevant.

## Setup

```bash
# from repo root
cargo build  # debug build is fine; smoke is qualitative

# fresh session
SESSION_ID=$(./target/debug/anie --harness-mode=rlm \
    --model qwen3.5:9b \
    --print "Hi, ready for a multi-turn test." \
  | tee /tmp/smoke_t0.txt \
  | grep -oE 'Session: [a-f0-9]+' | awk '{print $2}')

echo "Session: $SESSION_ID"
```

Per turn:

```bash
ANIE_EMBEDDING_MODEL=nomic-embed-text \
  ./target/debug/anie --harness-mode=rlm \
    --model qwen3.5:9b \
    --resume "$SESSION_ID" \
    --print "<prompt for turn N>" \
  > /tmp/smoke_tN.txt 2>&1
```

Use `nomic-embed-text` for the embedding reranker.
Default ceiling/keep-last unless a change calls for an
override. If overriding, document the env vars in the
run log.

## The 11-turn script

| Turn | Prompt |
|---|---|
| T1 | Implement a doubly linked list class in C++. Put it in `/tmp/dll_workspace/dll.hpp`. Make it generic (templated), header-only, with insert at head, insert at tail, insert at position, delete by value, size, and forward + backward iterators. |
| T2 | Now write a driver program in `/tmp/dll_workspace/main.cpp` that exercises the doubly linked list class. At minimum: insert several values, print forward and backward, delete one value, print again, and show the size at each step. |
| T3 | Compile `/tmp/dll_workspace/main.cpp` with g++ (use `-std=c++17`), then run the resulting binary. Show me the actual output. |
| T4 | Looking at the doubly linked list class we built, what are the top 3 improvements that would make it production-quality? Be specific. |
| T5 | Pick the top 2 improvements you just listed and actually implement them in `/tmp/dll_workspace/dll.hpp`. Update the file. |
| T6 | Now add a `reverse()` method to the DLLList class that reverses the list in place in O(n) time. Update `/tmp/dll_workspace/dll.hpp`. |
| T7 | Update `/tmp/dll_workspace/main.cpp` to also exercise the new `reverse()` method: build a list, print it, call `reverse()`, then print it again. Recompile and run, show me the actual output. |
| T8 | The binary just segfaulted (or didn't, depending on your run). Either way: re-read `dll.hpp` and `main.cpp`, then walk me through what the program should print step-by-step. If it crashes, tell me where and why. |
| T9 | Are there any other bugs you'd flag in the current `dll.hpp`? Cite line numbers. |
| T10 | Switching topics. What should I wear on Saturday? |
| T11 | I'm in `<your city>`. Can you actually check the weather forecast for Saturday and give me concrete advice based on it? |

(T8/T9 are the new turns added after the 2026-05-01
run. T8 is a forced-verification turn — it makes the
model re-read instead of trusting its own memory. T9
gives it explicit permission to find bugs that T7 may
have missed.)

## What to score per run

For each turn, log:

1. **Wall-clock duration** (Ollama can wedge on heavy
   context — `>3 min/turn` is a yellow flag, `>10 min`
   is the kill threshold).
2. **`rlm:` ledger line** (evicted N, paged in M,
   archive: K). Track how `paged_in` correlates with
   prompt relevance.
3. **Tool calls issued** vs. expected (read/edit/bash
   for code turns; web_search/web_read for T11; recurse
   for follow-ups that should hit the archive).
4. **`[tool error]` outcomes** — did the model recover,
   retry blindly, or hallucinate success?

Then comparing across runs:

| Signal | Baseline (2026-05-01) | Mitigations smoke (2026-05-01) | Skills + sub-agents (2026-05-02) |
|---|---|---|---|
| T3 self-debug succeeded | yes | yes | yes |
| T5 introduced infinite recursion | yes | yes (model still cargo-cults) | improved (cpp-rule-of-five skill loads when prompted; not always autonomous) |
| T7 hallucinated success | yes | **fixed** (PR 1 wrap) | fixed |
| T7 hung > 10 min | yes (killed) | mostly fixed | fixed |
| T10 autonomously fetched weather | no | regressed (PR 3 prompt then refused) → fixed (relocated) | **autonomous + grounded**: 7-step decompose plan → 6 web tool calls → 62°F/43°F/85% rain forecast |
| T11 web_search → web_read chain | yes | yes | yes (less needed — T10 now produces full answer) |
| Cross-domain reranker isolation (T10 paged_in count) | 1 / 52 | 1 / 52 | n/a (smoke 4 starts fresh, no archive) |
| Wall-clock to converge on T2 (DLL build + compile) | 43 min, never converged | 43 min, never converged | 6 min, valgrind clean (cpp-rule-of-five smoke; not full T1-T11 run) |

A "good" run improves at least one of these without
regressing the others.

### 2026-05-02 comprehensive smoke summary

After landing PRs through the sub-agents series + skills system + decompose, a comprehensive 5-test smoke confirmed:

- **Decompose plan visibility** works end-to-end (`[decompose plan]` block in transcript, model acknowledges).
- **Parallel-decompose dry-run** correctly renders single-round and multi-round structures (after a system-prompt fix encouraging dependency markers).
- **Skill autonomous-loading** worked on cpp-rule-of-five (Test 1 of skills smoke); less reliable when competing user-installed skills exist (Test 4 of skills smoke).
- **Topic switch (wardrobe)** dramatically improved: refusal → autonomous tool use + grounded forecast.
- **NO_PLAN_NEEDED sentinel** correctly skips the plan on trivial tasks (after system-prompt tightening).
- **Dependency markers** (`(depends on N)`) now produce real DAGs in the parallel-decompose renderer.

## Things this protocol catches that a 2-turn smoke
won't

- **Hallucinated success on tool errors.** Only shows
  up when there's enough conversation pressure that the
  model stops carefully re-reading.
- **Cargo-culted "improvements".** Any prompt that
  asks the model to *improve* its own code triggers
  pattern-matching on textbook patterns; the bugs
  surface only when a later turn exercises the changed
  code.
- **Topic-switch reranker quality.** T10 → T11 tests
  whether the embedding reranker keeps domains apart
  AND whether the model uses the tools the system
  prompt told it to.
- **Eviction loop wedging.** Turns 5+ have archive
  sizes that stress Ollama's prefill on every turn —
  this is where harness-side timeouts matter.

## Variants worth running

Once the baseline above is stable, run variants to
isolate what's driving any regression:

- **Ceiling sweep:** rerun T1-T11 at
  `ANIE_ACTIVE_CEILING_TOKENS={2k, 4k, 8k, 16k}`.
  Today's defaults assume small models can't handle
  more — verify that empirically per model.
- **Reranker off:** rerun with the embedding reranker
  disabled (no `ANIE_EMBEDDING_MODEL`) and confirm
  T10's cross-domain isolation degrades. If it doesn't,
  the reranker isn't earning its keep on this scenario.
- **Larger model via OpenRouter:** rerun the same
  prompts on `anthropic/claude-sonnet-4.6` or
  `openai/gpt-5.x` through OpenRouter to establish a
  capability ceiling. Anything the small model fails
  but the large model passes is a candidate for
  harness-side mitigation.

## Logged artifacts

For every run, keep:

- `/tmp/smoke_tN.txt` for N = 0..11 (the print outputs)
- `/tmp/dll_workspace/{dll.hpp, main.cpp, dll_demo}` at
  the end of T11
- The session jsonl (`~/.anie/sessions/<id>.jsonl`)
- The anie log for the day
  (`~/.anie/logs/anie.log.YYYY-MM-DD`)

Archive these somewhere durable when comparing across
harness changes. Otherwise the next "did this regress?"
question costs an hour to answer.

## Open questions / next iterations

- **Auto-watchdog on tool-error loops.** T7's 14-min
  hang was avoidable — if the same `[tool error]` fires
  3+ times in a row with no model-side adaptation,
  abort the turn rather than waiting for token budget
  to exhaust.
- **Forced re-verification step.** Add a system-prompt
  rule (or a pseudo-tool) that requires re-reading the
  file or re-running the binary before claiming "it
  works" after an edit. The hallucinated-success
  failure mode is too cheap to leave open.
- **Capability ceiling per model.** This protocol
  scores qwen3.5:9b. We need the same scored on
  qwen3-coder:30b, llama3.3:70b, and at least one
  large frontier model via OpenRouter, to know what
  "good" looks like and where the harness can
  realistically lift small-model output.
- **Concurrent execution of independent decompose
  rounds (PR 5.1).** PR 5 (dry-run) ships the parser
  + round renderer; PR 5.1 will add the
  `ControllerSubAgentFactory`-based executor that
  fans out independent rounds concurrently. Once
  landed, smoke should measure wall-clock reduction
  on the dependency-pipeline test.
- **"Don't relabel your own outputs" guard.** Smoke
  surfaced a small-model quirk where the prose
  summary swaps script/output labels even when the
  scripts themselves are correct. Could be a future
  skill or a structural reminder injected before the
  final assistant turn.
