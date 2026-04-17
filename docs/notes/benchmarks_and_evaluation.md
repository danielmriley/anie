# Benchmarks and Evaluation

## Summary

Build a benchmark suite to measure and track agent quality over time,
and investigate compatibility with external benchmark suites.

## Current State

Anie has integration tests that verify tool execution, TUI rendering,
session persistence, and config wiring. There are no benchmarks that
measure end-to-end agent quality on realistic tasks.

## Action Items

### 1. Internal benchmark suite
Build a set of benchmark tasks that exercise anie end-to-end:
- Run against real-world-scale codebases
- Each task takes roughly 30 minutes
- Requires many tool calls (read, write, edit, bash)
- Covers: refactoring, feature implementation, debugging, documentation

Scoring dimensions:
- Task completion (binary: did it finish?)
- Correctness (does the result work?)
- Efficiency (number of tool calls, token usage, cost)
- Time to completion

Should be reproducible and runnable on-demand (and eventually in CI).

### 2. Investigate TerminalBench
Look into TerminalBench as an external benchmark suite:
- What does it test?
- What interface does it expect (CLI, API, stdin/stdout, RPC)?
- Can anie be adapted to satisfy that interface?
- Does it complement or overlap with internal benchmarks?

### 3. Scoring and tracking
- Store benchmark results in a structured format (JSON/JSONL)
- Track results over time to detect regressions
- Compare across models and providers
- Eventually publish results for transparency

## Priority

Low — valuable for long-term quality tracking but not blocking any
near-term work. The internal benchmark suite should come first.
