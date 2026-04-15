# anie-rs v1.0.1 Phased Plan

This folder expands `docs/IMPLEMENTATION_ORDER_V_1_0_1.md` into step-by-step planning documents.

Use this folder when preparing or reviewing implementation work for the current:
- OpenAI-compatible local-provider hotfixes
- TUI transcript-navigation fixes
- local reasoning expansion for local models

## Source order

The canonical sequence is still:
1. `docs/IMPLEMENTATION_ORDER_V_1_0_1.md`
2. the step documents in this folder
3. the design reference in `docs/local_model_thinking_plan.md`

If this folder and `docs/IMPLEMENTATION_ORDER_V_1_0_1.md` drift apart, update the step docs first and then update the order file.

---

## Steps in this folder

- `step_minus_1_openai_local_compatibility_hotfixes.md`
- `step_0_tui_transcript_scrolling_and_navigation.md`
- `step_1_openai_system_prompt_insertion_point.md`
- `step_2_local_defaults_and_prompt_steering_mvp.md`
- `step_3_tagged_reasoning_stream_parsing_mvp.md`
- `step_4_reasoning_capability_model_and_config.md`
- `step_5_native_reasoning_controls_for_modern_local_backends.md`
- `step_6_native_separated_reasoning_output.md`
- `step_7_backend_profiles_token_budgets_and_release_validation.md`

---

## Planning conventions used here

Each step document includes:
- why the step exists
- what code should change
- what should **not** change yet
- sub-steps in recommended implementation order
- required tests and manual validation
- exit criteria / gate conditions

These files are intended to reduce ambiguity before coding starts.

---

## Current sequencing summary

### Step -1
Fix current OpenAI-compatible local-provider correctness bugs first:
- system prompt forwarding
- reasoning-only delta parsing
- empty-stop protection
- regression coverage

### Step 0
Fix TUI transcript scrolling/navigation before long reasoning output becomes more common.

### Steps 1–7
Then land the structured local-reasoning work in increasing order of surface area and risk:
- stable provider-owned system-prompt insertion point
- local defaults and prompt steering MVP
- tagged parsing MVP
- capability model + config
- native reasoning controls
- native separated reasoning output
- heuristics, token budgets, and release validation

---

## Related docs

- `docs/IMPLEMENTATION_ORDER_V_1_0_1.md`
- `docs/local_model_thinking_plan.md`
