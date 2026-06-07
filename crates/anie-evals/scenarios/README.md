# Eval scenarios

Each TOML is one single-turn scenario scored by deterministic automated
checks (`contains`, `must_call_tool`, `max_tokens`, `max_wall_clock_ms`)
— no LLM-as-judge in this first cut. Families: `repo_navigation`,
`tool_use`. Run against the anie repo as the fixture (`--repo-root .`):

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
