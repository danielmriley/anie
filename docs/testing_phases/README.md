# Integration Testing Phases

This folder contains step-by-step implementation plans for the integration test suite described in `docs/integration_testing_plan.md`.

## Phase order

0. `phase_0_test_crate_and_infrastructure.md` — create the test crate, shared helpers, and verify the harness compiles
1. `phase_1_agent_tools_session.md` — agent loop → real tools → session persistence (Category 1)
2. `phase_2_session_resume.md` — session resume and context continuity (Category 2)
3. `phase_3_agent_tui.md` — agent events → TUI rendering consistency (Category 3)
4. `phase_4_config_wiring.md` — config → provider registry wiring (Category 4)

## Conventions

Each phase document includes:
- why the phase exists
- files to create or modify
- shared helpers introduced
- detailed test cases with expected assertions
- exit criteria

Phases are independent after Phase 0. They can be implemented in any order, though the numbered order reflects priority.
