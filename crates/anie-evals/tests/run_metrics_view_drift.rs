//! Drift guard: `RunMetricsView` hand-mirrors anie-cli's `RunMetrics`
//! across the `--metrics-out` JSON boundary (the view deliberately
//! avoids depending on anie-cli — see `RunMetricsView`'s doc comment).
//! Because the two shapes are coupled only by field names, a cli-side
//! rename (e.g. `total_tokens` -> `total`) would silently default the
//! view's field to zero and quietly zero out eval metrics rather than
//! erroring.
//!
//! This test feeds a fully-populated JSON literal matching anie-cli's
//! *current* serialization (schema v5, including the `cost`,
//! `compaction`, `tool_repair`, `recovery`, `prompt`, and `context`
//! blocks and the full `tokens` shape) through `RunMetricsView` and
//! asserts every modelled field survives with its non-default value.
//! If anie-cli renames a field the view reads, the matching assertion
//! fails — turning a silent data-loss regression into a loud test
//! failure.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use anie_evals::RunMetricsView;

/// Mirror of anie-cli's `RunMetrics` serialization as of schema v5
/// (`crates/anie-cli/src/run_metrics.rs`). Every field carries a
/// distinct non-default value so the round-trip can prove each one
/// landed in the right place rather than defaulting.
fn fully_populated_run_metrics_json() -> serde_json::Value {
    serde_json::json!({
        "schema_version": 5,
        "harness_mode": "augmented",
        "model": "qwen2.5-coder:7b",
        "provider": "ollama",
        "wall_clock_ms": 12_345_u64,
        "turns": 7_u32,
        "tokens": {
            "input_tokens": 1_000_u64,
            "output_tokens": 2_000_u64,
            "cache_read_tokens": 300_u64,
            "cache_write_tokens": 400_u64,
            "total_tokens": 3_700_u64
        },
        "cost": {
            "input_cost": 0.0,
            "output_cost": 0.0,
            "total_cost": 0.0
        },
        "tools": {
            "calls": 11_u32,
            "failures": 2_u32,
            "by_tool": {
                "read_file": { "calls": 6_u32, "failures": 0_u32 },
                "write_file": { "calls": 5_u32, "failures": 2_u32 }
            }
        },
        "compaction": {
            "pre_prompt": 1_u32,
            "mid_turn": 2_u32,
            "reactive_overflow": 3_u32,
            "total": 6_u32
        },
        "tool_repair": {
            "coerced": 4_u32,
            "repaired": 3_u32,
            "failed_after_repair": 1_u32,
            "repaired_then_failed": 2_u32
        },
        "recovery": {
            "verify_runs": 5_u32,
            "verify_failures": 2_u32,
            "loop_perturbations": 1_u32,
            "grounded_edit_failures": 3_u32
        },
        "prompt": {
            "system_prompt_tokens": 2_345_u64
        },
        "context": {
            "evictions": 12_u64,
            "evicted_tokens": 8_400_u64,
            "page_ins": 5_u64,
            "page_in_tokens": 2_100_u64,
            "ledger_tokens_total": 640_u64,
            "prefill_tokens_total": 91_000_u64,
            "truncation_suspected": 3_u64
        }
    })
}

#[test]
fn run_metrics_view_round_trips_every_modelled_field_from_cli_schema_v5() {
    let json = fully_populated_run_metrics_json();
    let view: RunMetricsView =
        serde_json::from_value(json).expect("anie-cli schema-v5 JSON must deserialize into the view");

    assert_eq!(view.schema_version, 5, "schema_version");
    assert_eq!(view.harness_mode, "augmented", "harness_mode");
    assert_eq!(view.wall_clock_ms, 12_345, "wall_clock_ms");
    assert_eq!(view.turns, 7, "turns");

    // A field rename on the cli side would default this to zero.
    assert_eq!(view.tokens.total_tokens, 3_700, "tokens.total_tokens");

    assert_eq!(view.tools.calls, 11, "tools.calls");
    assert_eq!(view.tools.failures, 2, "tools.failures");
    assert_eq!(view.tools.by_tool.len(), 2, "tools.by_tool entry count");
    let read_file = view
        .tools
        .by_tool
        .get("read_file")
        .expect("tools.by_tool must keep the read_file key");
    assert_eq!(read_file.calls, 6, "by_tool[read_file].calls");
    assert_eq!(read_file.failures, 0, "by_tool[read_file].failures");
    let write_file = view
        .tools
        .by_tool
        .get("write_file")
        .expect("tools.by_tool must keep the write_file key");
    assert_eq!(write_file.calls, 5, "by_tool[write_file].calls");
    assert_eq!(write_file.failures, 2, "by_tool[write_file].failures");

    // Rescue counters (schema v2, extended in v3) — the fields this
    // guard most wants to protect, since the runner scores
    // tool-reliability off them.
    assert_eq!(view.tool_repair.coerced, 4, "tool_repair.coerced");
    assert_eq!(view.tool_repair.repaired, 3, "tool_repair.repaired");
    assert_eq!(
        view.tool_repair.failed_after_repair, 1,
        "tool_repair.failed_after_repair"
    );
    assert_eq!(
        view.tool_repair.repaired_then_failed, 2,
        "tool_repair.repaired_then_failed"
    );

    // Recovery + prompt blocks (schema v4, plans 03/04 of
    // `docs/local_model_augmentation/`).
    assert_eq!(view.recovery.verify_runs, 5, "recovery.verify_runs");
    assert_eq!(view.recovery.verify_failures, 2, "recovery.verify_failures");
    assert_eq!(
        view.recovery.loop_perturbations, 1,
        "recovery.loop_perturbations"
    );
    assert_eq!(
        view.recovery.grounded_edit_failures, 3,
        "recovery.grounded_edit_failures"
    );
    assert_eq!(
        view.prompt.system_prompt_tokens, 2_345,
        "prompt.system_prompt_tokens"
    );

    // Context-virtualization telemetry (schema v5, rlm2/PR1 of
    // `docs/rlm_context_v2/`). The runner scores the navigation-tax
    // win off these, so a cli-side rename must be loud.
    assert_eq!(view.context.evictions, 12, "context.evictions");
    assert_eq!(view.context.evicted_tokens, 8_400, "context.evicted_tokens");
    assert_eq!(view.context.page_ins, 5, "context.page_ins");
    assert_eq!(view.context.page_in_tokens, 2_100, "context.page_in_tokens");
    assert_eq!(
        view.context.ledger_tokens_total, 640,
        "context.ledger_tokens_total"
    );
    assert_eq!(
        view.context.prefill_tokens_total, 91_000,
        "context.prefill_tokens_total"
    );
    assert_eq!(
        view.context.truncation_suspected, 3,
        "context.truncation_suspected"
    );
}

#[test]
fn v2_tool_repair_block_loads_with_repaired_then_failed_defaulted() {
    // Forward-compat: a v2 artifact's `tool_repair` block predates
    // `repaired_then_failed`; the view must default it, not error.
    let mut json = fully_populated_run_metrics_json();
    json["schema_version"] = serde_json::json!(2);
    json["tool_repair"] = serde_json::json!({
        "coerced": 4_u32,
        "repaired": 3_u32,
        "failed_after_repair": 1_u32
    });
    json.as_object_mut().unwrap().remove("recovery");
    json.as_object_mut().unwrap().remove("prompt");
    json.as_object_mut().unwrap().remove("context");
    let view: RunMetricsView = serde_json::from_value(json).expect("v2 JSON loads");
    assert_eq!(view.tool_repair.repaired, 3);
    assert_eq!(view.tool_repair.repaired_then_failed, 0);
}

#[test]
fn v3_artifact_loads_with_recovery_and_prompt_defaulted() {
    // Forward-compat: a v3 artifact predates the `recovery` and
    // `prompt` blocks; the view must default both, not error.
    let mut json = fully_populated_run_metrics_json();
    json["schema_version"] = serde_json::json!(3);
    json.as_object_mut().unwrap().remove("recovery");
    json.as_object_mut().unwrap().remove("prompt");
    json.as_object_mut().unwrap().remove("context");
    let view: RunMetricsView = serde_json::from_value(json).expect("v3 JSON loads");
    assert_eq!(view.recovery, anie_evals::RecoveryView::default());
    assert_eq!(view.prompt, anie_evals::PromptView::default());
}

#[test]
fn v4_artifact_loads_with_context_defaulted() {
    // Forward-compat: a v4 artifact predates the `context` block;
    // the view must default it, not error.
    let mut json = fully_populated_run_metrics_json();
    json["schema_version"] = serde_json::json!(4);
    json.as_object_mut().unwrap().remove("context");
    let view: RunMetricsView = serde_json::from_value(json).expect("v4 JSON loads");
    assert_eq!(view.context, anie_evals::ContextView::default());
}

#[test]
fn run_metrics_view_matches_cli_expected_schema_version() {
    // The view's expected schema version must track anie-cli's
    // `RUN_METRICS_SCHEMA_VERSION`; the literal above is built for it.
    assert_eq!(anie_evals::EXPECTED_RUN_METRICS_SCHEMA_VERSION, 5);
}
