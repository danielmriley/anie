# Eval scenarios

Each TOML is one single-turn scenario scored by deterministic automated
checks (`contains`, `contains_any`, `must_call_tool`, `max_tokens`,
`max_wall_clock_ms`) — no LLM-as-judge in this first cut. `contains` is
all-of (every substring must appear); `contains_any` is any-of (at least
one), for facts with multiple valid phrasings. Navigation scenarios drop
incidental `must_call_tool` checks and grade the answer instead, so every
`repo_navigation` scenario must carry a `contains`/`contains_any`
assertion (enforced by `tests/corpus.rs`). Families: `repo_navigation`,
`tool_use`, `verification`. Run against the anie repo as the fixture
(`--repo-root .`):

```
cargo build --release
target/release/evals \
  --scenarios crates/anie-evals/scenarios/*.toml \
  --modes baseline,current,rlm \
  --model <a-configured-model> \
  --repo-root . \
  --out crates/anie-evals/results/$(date +%F)
```

The committed scenarios reference real anie symbols so they exercise
genuine repo navigation. Every file is validated (parse + non-empty
prompt/expect) by `tests/corpus.rs`.
