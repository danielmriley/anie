# Baseline — gemma4:e4b, pre-campaign harness (lift-curve point 0)

Captured against commit `554b922` (before the PR13-18/plan-03/plan-02
campaign), same prompt as the qwen3.5:0.8b field session:
`--print --harness-mode=rlm "Good morning! Can you tell me the time?"`

## Numbers (metrics sidecar `/tmp/baseline_e4b_time.json`)

| Metric | gemma4:e4b (pre-campaign) | qwen3.5:0.8b field session |
|--------|---------------------------|----------------------------|
| turns | 3 | 23 |
| tool calls | 2 (web_search, web_read) | 22 |
| tool failures | 0 | 8 (36%) |
| coerced/repaired | 0/0 | 0/2 |
| input tokens (sum) | 35,342 (~11.8k/turn) | 11,291 → 20,181 |
| wall clock | 127s | — |
| answered the question | No — recommended time websites instead of running `date` | No |

The e4b already behaves qualitatively better than 0.8B (no hallucinated
tools, no loops, no errors) — but the ~11.8k/turn prompt weight
(plan 04's target) and the failure to just run `date` are both visible.

## New finding: hardware-blind `num_ctx` crashes llama-server

gemma4:e4b advertises `context length: 131072`. anie forwards
`num_ctx = model.context_window` verbatim
(`ollama_chat/convert.rs`: `options.num_ctx_override.unwrap_or(model.context_window)`),
and on this machine (6GB VRAM / 13GB RAM, partial offload) the request
kills llama-server outright:

```
GGML_ASSERT(n_inputs < GGML_SCHED_MAX_SPLIT_INPUTS) failed
```

Bisected via direct `/api/chat` calls: `think+tools` at
`num_ctx=16384` is fine; the identical request at `num_ctx=131072`
crashes (Ollama 0.30.7). So with NO overrides, a freshly-pulled
long-context model is **unusable in anie on consumer hardware** — and
even where it doesn't crash, a 131k KV-cache allocation on a laptop is
never what the user wants.

Workaround applied: `ollama_num_ctx_overrides` in
`~/.anie/state.json`, keyed `"{provider}:{model_id}"` (note: NOT the
bare model id) — `"ollama:gemma4:e4b": 16384`.

### Implication for plan 04 (PromptTier)

PromptTier derives from the *effective* window (post-override). With
no override, gemma4:e4b reads as a 131k window → **Full tier** on a
machine that can't even serve the request. Follow-up item:

**PR19 (proposed) — sane default `num_ctx` clamp for Ollama parents**:
`effective num_ctx = override || min(model.context_window,
ANIE_DEFAULT_NUM_CTX (default 32_768))`, with a startup line stating
the clamp and pointing at `/context-length`. This both prevents the
crash class and makes the tier boundary meaningful for freshly-pulled
long-context models. The existing `/context-length` override keeps
precedence.

## Still to capture (post-campaign)

- Same scenario, post-campaign binary, gemma4:e4b → prompt tokens
  (expect <4k turn-0 in Small tier... NOTE: without PR19 or an
  override, e4b is Full tier — capture with the 16384 override in
  place so Small tier engages).
- Same scenario, qwen3.5:0.8b post-campaign (expect PR16 to rescue the
  hallucinated-tool class; ledger v2 to kill the F2 syntax leak).
- Eval corpus `--modes current,rlm` on gemma4:e4b (first lift-curve
  matrix row).
