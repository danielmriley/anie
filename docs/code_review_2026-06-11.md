# Comprehensive review — 2026-06-11

Multi-agent review (5 perf lenses + 4 trim reviewers, every finding
adversarially verified; 61 agents). Motivating symptom: during a manual
interactive test with a **small** Ollama model, the laptop became
system-unresponsive for several seconds. 31 perf findings and 16 trim
findings confirmed; 5 findings refuted by verification and excluded.

## The freeze, root-caused

The laptop freeze is **not one bug** — it's `--harness-mode=rlm`
stacking three additional LLM workloads onto the same Ollama instance
that is serving the interactive turn, plus a TUI render loop whose
streaming cost is quadratic. All confirmed at high confidence with the
full trigger→rate→cost chain verified.

### RC1 (CRITICAL): background summarizer runs full chat generations concurrently with the live turn

`build_rlm_extras` unconditionally builds `LlmSummarizer` with the
**same chat model** and spawns its worker (`controller.rs:3124-3156`) —
there is **no enable flag and no kill switch** (only
`ANIE_SUMMARIZER_TIMEOUT_SECS` exists). The rlm default ceiling is
finite (16_384, `controller.rs:2995`), so the policy is hot by default.
`before_model` fires before **every** ModelTurn step
(`agent_loop.rs:1145-1149`); archiving is **unconditional** (not gated
on ceiling pressure, `context_virt.rs:501-523`); every archived message
≥ ~800 chars enqueues a summary (`context_virt.rs:535-545`). Each
summary is a **full streaming chat completion** with the entire message
body as prompt (`bg_summarizer.rs:249,293`), 180s timeout, up to 64
queued. Net effect on a laptop: two simultaneous LLM generations (or a
saturated Ollama queue) — the observed freeze.

**Fix**: default to `HeadTruncationSummarizer` (free) when the parent
is local Ollama; make LLM summaries opt-in; and/or pause the worker
while a foreground stream is active (drain only between turns).

### RC2 (HIGH): embedder default-on loads a second model and blocks the turn path

With `ANIE_EMBEDDING_MODEL` unset, rlm + Ollama parents default the
embedder **on** with `nomic-embed-text` against the same base_url
(`controller.rs:3061,3082-3105`). Alternating generate/embed traffic
forces Ollama to keep or swap **two models** resident — on a
RAM-constrained laptop each swap is hundreds of MB of IO. Worse, the
**prompt embed is awaited inline inside `before_model`**
(`context_virt.rs:981,1012`) — before the empty-candidates check, so it
fires even with nothing to rerank — and `OllamaEmbedder` uses
`reqwest::Client::new()` with **no timeout** (`embedder.rs:148`): a
busy/wedged Ollama stalls the start of every model turn indefinitely.

**Fix**: default embedder OFF for local Ollama parents; add a client
timeout; never await the embed inline (background it, fall back to
keyword overlap for the current fire).

### RC3 (HIGH): per-run rebuild/respawn; orphaned workers; discarded work

`start_prompt_run` / `start_continuation_run` rebuild the
`ExternalContext` from a full context clone and spawn a **fresh**
summarizer + embedder worker pair on **every prompt and every
continuation** (`controller.rs:1904,1960,3139,3156`). JoinHandles are
discarded — `tokio::spawn` detaches; **no abort path exists** — so old
workers keep draining up to 64 queued LLM calls against a dead store
after the run ends or is cancelled, and all summaries/embeddings paid
for in prior runs are thrown away.

**Fix**: hoist `ExternalContext` + the worker pair to session scope
(create once, append deltas); abort or `select!` on a cancel token.

### RC4 (HIGH): TUI streaming render is O(n²) at up to 125 fps

- Every text delta nulls the streaming render cache; every frame does a
  **full markdown re-parse + syntect re-highlight of the entire
  accumulated answer** (`output.rs:176-184,202`). Quadratic over a
  streaming turn; empirically confirmed.
- Streaming **thinking** text has no cache at all: full String copy +
  full re-wrap per frame (`output.rs:1702-1747`), and ticks force
  redraws whenever the agent is non-idle.
- The flat line list is rebuilt every frame whenever any animated block
  exists: O(total transcript lines) walk + Arc/link-map clones
  (`output.rs:999-1005`).
- All multiplied by `FRAME_BUDGET = 8ms` → up to 125 full renders/sec
  for agent-event frames (`app.rs:2359,2429`).

**Fix (ordered)**: two-tier frame budget (8ms for input, ~33-50ms for
streaming frames — caps everything 4-6× for free); per-code-block
syntect cache + re-parse only the unterminated tail; single-slot
thinking cache; prefix-stable flat-cache splice.

## Remaining perf findings (verified)

**Medium:**
- `context_virt.rs:469` — `before_model` does O(full context + archive)
  clone/scan per ModelTurn step. Cache token sets at push time; archive
  by `Arc<Message>`.
- `context_virt.rs:1033` — `page_in_relevant` deep-clones nearly the
  whole archive per turn once embeddings are active. Score by
  reference, clone only selected.
- `grep.rs:258` — grep reads/scans **binary files in full** (no
  `BinaryDetection`) and descends into `.git/`. Add
  `BinaryDetection::quit(b'\x00')`, skip `.git`, `max_filesize`.
- `embedder.rs:148` — no HTTP timeout (also part of RC2).

**Low (grouped):**
- Checkpoint store: per-turn synchronous full-read + SHA256 of every
  agent-modified file on the controller task; pretty-JSON manifest
  rewritten per capture (quadratic growth); blobs uncompressed and
  never pruned; store reopened per capture
  (`session_handle.rs:249`, `checkpoint.rs:166,363`,
  `controller.rs:1875`). Fix: keep store open, skip unchanged
  (mtime,size), `spawn_blocking`, compact/append manifest, zstd + GC.
- `find.rs:167` — find descends into `.git/` (hidden filter disabled).
- `anie-session/src/lib.rs:1185` — `list_sessions` fully parses every
  line of every session file synchronously on the controller task.
- Event payload clones: `ToolExecEnd` deep-clones full tool results
  (`agent_loop.rs:1862`); `TranscriptReplace` carries a full transcript
  clone (`agent_loop.rs:1249`). Use `Arc` payloads / truncated bodies.
- `agent_loop.rs:1735` — tool-call repair adds ≤2 extra local
  generations per invalid call (by design, but document
  `ANIE_DISABLE_TOOL_REPAIR=1` as a relief valve; consider a per-turn
  budget).

## Trim findings (verified)

**Dead code / stale annotations (~50 LOC):**
- 6+ stale `#[allow(dead_code)] // wired up in PR 08.x` shims —
  features shipped (`embedder.rs:85,103`, `bg_embedder.rs:45-74`,
  `external_context.rs`).
- Module-wide `allow(dead_code)` on `failure_loop.rs` / `recurse_depth.rs`
  (shipped); broken `Self::disabled` doc link (`failure_loop.rs:57`).
- `SelectList`: five dead methods from an abandoned migration
  (`select_list.rs:51`, ~35 LOC).
- Four stale `allow(dead_code)` on live `AutocompletePopup` methods;
  one stale annotation in `model_discovery.rs:661`.

**Duplication (~100 LOC):**
- The rebuild-ToolRegistry-by-copying pattern ×3
  (`controller.rs:2748`, bootstrap, build_agent) → add
  `ToolRegistry::with_added(...)`.
- `discovery_model_api`/`discovery_model_base_url` byte-identical in 3
  files (`anie-tui/src/app.rs:2723` + 2 others).
- OAuth token-POST helper ×3 + `GoogleTokenResponse` ×2 in anie-auth.
- `RunMetricsView` hand-mirror in anie-evals — at minimum add a
  round-trip drift test.

**Over-engineering (~130 LOC):**
- Tool-execution hook system (`hooks.rs`) — speculative extension
  point, no production consumer (~90 LOC + hot-path branches).
- `ArgumentSource` trait machinery in autocomplete — exercised only by
  its own test (~40 LOC).

**Simplification:**
- `controller.rs`: `impl InteractiveController` ~1625 lines;
  `try_handle_action` a ~520-line match. Extract `loop_command.rs` /
  `goal_command.rs` + named `handle_*` methods.
- `agent_loop.rs` (3313 lines): clean seam — extract the
  message-builder cluster to `stream_builder.rs`.
- `StatusRenderCache` (`app.rs:402`): duplicates 10 fields and its
  staleness check **omits todo/cost — a real stale-paint bug**, not
  just cleanup.

## Refuted by verification (excluded)

`SessionHandle::sessions_dir` (live), several "dead" accessors
(test-used or live), `Message::Custom` (load-bearing for
deserialization compat), placeholder overlays + `OverlayOutcome`
variants (documented roadmap stubs).

## Recommended fix order

1. **Stop the freeze** (RC1+RC2): summarizer → head-truncation default
   on local Ollama; embedder → opt-in for local Ollama; embed off the
   turn path; client timeout. *Until then: avoid rlm mode on the
   laptop, or set `ANIE_EMBEDDING_MODEL=""` and
   `ANIE_ACTIVE_CEILING_TOKENS=18446744073709551615` (no summarizer
   kill switch exists — itself a finding).*
2. **Worker lifecycle** (RC3): session-scoped store + workers, abort on
   cancel.
3. **TUI budget + caches** (RC4): two-tier frame budget first (one
   constant, 4-6× relief), then the streaming caches.
4. Tool walkers (`grep`/`find` `.git` + binary), checkpoint IO batch.
5. Trim batch: ~250-300 LOC removable across dead code, duplication,
   and the two speculative abstractions; plus the StatusRenderCache
   stale-paint fix.
