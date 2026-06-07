# Eval results

Generated comparison reports land here (`<date>.json` + `<date>.md`).

A checked-in 3-mode (`baseline`/`current`/`rlm`) report requires a
configured model (local via Ollama/LM Studio, or an API key). Generate
one with:

```
cargo build --release
target/release/evals \
  --scenarios crates/anie-evals/scenarios/*.toml \
  --modes baseline,current,rlm \
  --model <your-model> \
  --repo-root . \
  --out crates/anie-evals/results/$(date +%F)
```

CI safety is provided by `tests/runner_mock.rs` (a fake-binary golden
test that needs no model); a live-model report is an operator step.
