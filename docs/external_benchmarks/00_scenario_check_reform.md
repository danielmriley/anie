# bench/PR1 — scenario-check reform (outcome over strategy)

## Rationale
The 2026-06-12 matrix failed correct answers for tool choice:
`find_provider_trait` produced the right content in both modes and
failed only `must_call_tool = "grep"` + a cap. gemma4:e4b is
grep-averse but often right. Checks must grade outcomes.

## Design
- Navigation-family scenarios: drop `must_call_tool` (keep `contains`
  + budgets). Keep it ONLY where the tool IS the behavior under test:
  todo_tracked_survey (todo_write), verify_broken_fixture (implicit),
  repo_map scenarios as designed, paged_read_negative_control (read —
  it tests pagination tolerance).
- Recalibrate `max_tokens` from the v2 exit-gate run: cap =
  1.5 × the better mode's observed usage, rounded up to 4k.
- Add `expect.contains_any` (ANY-of list) for content with multiple
  valid phrasings; scenario corpus test updated.

## Tests
- corpus test: every navigation scenario asserts at least one
  `contains`/`contains_any`; parser round-trips contains_any;
  existing scenarios reparse.

## Exit criteria
- Matrix re-run shows no scenario failing on tool-choice alone.
