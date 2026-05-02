# Concurrency decisions for parallel sub-agents (2026-05-02)

This document captures the design decisions around concurrent
sub-agent execution that landed with PR 5.1, and the open
items deferred for future iterations.

## Default: Ollama runs sequentially

When the parent's model is served by Ollama (`ApiKind::OllamaChatApi`),
the parallel-decompose executor defaults to `max_concurrency = 1`
(sequential execution) regardless of `ANIE_PARALLEL_DECOMPOSE`.

`ANIE_PARALLEL_DECOMPOSE_FORCE=1` overrides the clamp for users
with hardware capable of running concurrent inference.

### Why

Local Ollama on a single GPU has three concrete problems with
concurrent inference:

1. **VRAM pressure.** Each concurrent request allocates its own
   KV cache. With qwen3.5:9b at ~16k context, two concurrent
   requests pull ~12-14 GB of VRAM combined. On a 16 GB
   consumer GPU you're at the edge; on CPU-only, concurrency
   thrashes RAM and the kernel's swap path. Net effect: slower
   per-request latency, often slower total wall-clock too.

2. **No KV-cache sharing across concurrent requests.** This is
   the big one. Sub-agents in a parallel-decompose round share
   a long prefix (system prompt + decompose plan + user
   message — typically 4-8k tokens) and only diverge on the
   per-sub-task suffix. With shared-prefix KV cache, the
   prefix is processed once and N suffixes share it. Without
   it, each request re-processes the entire prefix.

   API providers (Anthropic, OpenAI, OpenRouter pass-through)
   do this server-side automatically — concurrent requests
   with shared prefixes hit cache transparently.

   Ollama's HTTP API doesn't expose multi-completion-on-shared-
   context. The capability exists at the llama.cpp layer that
   Ollama is built on (slot-based KV reuse), but Ollama hasn't
   surfaced it as of this writing.

3. **Throughput vs. latency.** On a single GPU, two concurrent
   requests typically finish at ~50-60% the speed of one. Net
   throughput stays similar (you'd have run them sequentially
   in the same total time), but each individual request feels
   slower — so the user perceives a regression even when the
   wall-clock is comparable.

The pragmatic conclusion: for local Ollama in PR 5.1's first
iteration, force sequential execution. Document the limitation,
provide an opt-out for users who know their hardware, and
revisit when one of the future options (below) lands.

## Default: API providers can fan out

For non-Ollama providers (`ApiKind::OpenAICompletions`,
`ApiKind::Anthropic`, OpenRouter, etc.), the executor honors
`ANIE_PARALLEL_DECOMPOSE` directly, capped at 6 to stay well
below typical rate limits.

### Why this is safe

Concurrent API requests to a single account:
- Run on the provider's distributed infrastructure (no shared-
  GPU contention).
- Hit server-side prefix caching automatically — the shared
  decompose-plan prefix is paid for once per cache window,
  not N times.
- Account-level rate limits (typically 50-500 RPM, 10-50
  concurrent in flight) are an order of magnitude above what
  4-7 sub-tasks per decompose would demand.
- 429 rate-limit responses are handled by the existing retry
  layer, so a brief burst-cap bump just adds latency, not
  failures.

The cap of 6 is a safety margin; the typical decompose plan
has 3-7 sub-tasks, often with dependencies that force serial
sub-rounds, so concurrency rarely hits the cap in practice.

## Future: enable concurrent local-model inference properly

Two paths, in order of preference:

### Option A: `LlamaCppDirect` provider (recommended)

Add a new provider that talks to llama.cpp's HTTP server
directly (the upstream `server` binary in the llama.cpp
project, not Ollama).

Why llama.cpp directly:
- It exposes **slot-based KV reuse** via the `/slots` endpoint
  and `prompt_cache_all` / `cache_prompt` flags. This is the
  same primitive that vLLM's PagedAttention and SGLang's
  RadixAttention provide; llama.cpp has it too, just with a
  different API surface.
- It exposes **`n_parallel` config** for true concurrent slot
  processing.
- It exposes **shared-prefix tokens** via the API so we can
  fan out completions that share the prefix without
  reprocessing.

What we'd need to do:
1. Plan + implement the `LlamaCppDirectProvider` in
   `crates/anie-providers-builtin/`.
2. Add config knobs for `n_parallel`, `cache_prompt`, etc.
3. Add a sub-agent factory variant that detects this provider
   and uses the cache-prompt + parallel-slot path instead of
   issuing N independent requests.

This is its own plan series. Significant implementation
(weeks, not days), but it's the clean path to fast concurrent
local inference.

### Option B: Wait for Ollama to expose shared-prefix concurrency

Ollama could surface llama.cpp's slot/prompt-cache
capabilities through their HTTP API at any time. Watch for it
in their changelog. Cheaper to wait if it lands soon, more
expensive than option A if it never does.

In the meantime, the existing default (Ollama → sequential)
is the safe choice. We're not blocked on this — sub-agent
parallelism still benefits API users today, and local users
get sequential fallback that doesn't regress.

### Option C: Pre-warm + share via separate Ollama processes (rejected)

We considered running multiple ollama-runner processes with
the model pre-loaded in each, then routing different sub-
agents to different runners. Rejected because:
- Doubles VRAM usage instead of sharing it (defeats the point).
- Doesn't solve the prefix-reprocessing problem.
- Adds operational complexity (process management, health
  checking, port allocation).

## Reference

- llama.cpp server docs: [github.com/ggerganov/llama.cpp/tree/master/examples/server](https://github.com/ggerganov/llama.cpp/tree/master/examples/server)
- Ollama parallelism config: `OLLAMA_NUM_PARALLEL` (default 4),
  `OLLAMA_MAX_LOADED_MODELS` (default 1).
- Anthropic prompt caching: server-side, automatic, applied to
  prefixes of >=1024 tokens. Concurrent requests sharing a
  prefix all benefit from a single cache miss.
- OpenAI prompt caching: server-side, automatic, applies to
  GPT-4o and successors. Cache hits show up as
  `cached_input_tokens` in usage records.
