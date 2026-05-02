---
name: decompose-multi-constraint-task
description: When a task involves many interacting constraints (e.g., a C++ class with templates + iterators + raw memory + a driver + compile + run), break it into focused sub-problems and solve each separately. Don't try to hold all constraints in one generation.
---

# When this applies

You catch yourself in any of these patterns:

- Rewriting the same file 3+ times because each "fix" introduces a new bug.
- Touching code that has many interacting concerns at once: type system, memory management, iterator invariants, stdlib semantics, performance.
- Producing code where comments describe correct behavior but the implementation regresses (a sign your working memory is overloaded).
- Generating long files in one shot, then realizing later sections don't match earlier sections.

# Why decomposition helps

The model's working memory is finite. When a task has more interacting constraints than fit in your active attention, errors don't surface as "I don't know" — they surface as confidently-wrong code. Each individual piece looks fine in isolation; the interactions break.

Decomposition fights this by reducing the constraint count per generation. A focused sub-problem with 2-3 constraints is solvable; the same problem buried inside a 7-constraint blob isn't.

# What to do

## Step 1: identify the natural seams

Most multi-constraint tasks decompose along structural lines. For a C++ data structure:

1. **Skeleton**: class declaration, member fields, no methods yet.
2. **Iterator types**: forward iterator (with traits), reverse iterator. No interaction with the container's mutation methods yet.
3. **Mutation methods**: `push_back`, `push_front`, `insert`, `erase` — each as a focused chunk.
4. **Special members**: rule-of-five (see the `cpp-rule-of-five` skill).
5. **Driver**: a small `main.cpp` exercising the API.
6. **Build + run**: compile, run, verify output.

Each step has 2-3 constraints. None has all of them.

For other domains the seams differ:

- **API endpoint**: validation → business logic → persistence → response format → tests.
- **Data pipeline**: source connector → transform → sink → error handling → observability.
- **Refactor**: introduce new abstraction → migrate first call site → migrate remaining sites → remove old code → tests.

## Step 2: solve each sub-problem in isolation

Once decomposed, treat each piece as its own task. For each:

- Write only that piece.
- Verify it compiles / type-checks (unit verification, not full-suite).
- Move on.

Don't try to "while I'm here" multiple sub-problems. The scope creep that re-creates the original failure mode.

## Step 3: assemble + verify the whole

After all sub-problems are done individually, do the integration check: compile the full thing, run the full driver, run the full test suite. If integration surfaces an issue, the issue is at a SEAM between sub-problems — fix at the seam, not by re-doing whole pieces.

## Step 4: when to use `recurse` for a sub-problem

If the harness has the `recurse` tool available, you can spawn a focused sub-agent for any sub-problem that's still complex enough to benefit from its own working memory. The sub-agent sees only its own scope (the message range, file, or grep pattern you give it) plus its query — fewer constraints, sharper focus.

```json
{
  "query": "Implement just the move constructor and move assignment for /tmp/dll.hpp's DoublyLinkedList class. Do not touch other methods. Output only the two methods.",
  "scope": {
    "kind": "file",
    "path": "/tmp/dll.hpp"
  }
}
```

The sub-agent answers, you paste its answer in. Less coupling than rewriting the whole file.

# Anti-pattern

The pattern this skill exists to prevent:

```
[write /tmp/big_class.cpp]  <- 300 lines, all at once
[bash g++ ...]
[tool error]                 <- one of many constraints broken
[write /tmp/big_class.cpp]  <- 300 lines, "fixed" version
[bash g++ ...]
[tool error]                 <- different constraint broken now
[write /tmp/big_class.cpp]  <- repeat ad nauseam
```

After the second rewrite, STOP and decompose. You're not making progress; you're shuffling which constraint is broken at any given moment. Decomposition is the way out.

# Anti-anti-pattern

Don't decompose for the sake of decomposing. If a task has 2 constraints, write it in one shot. The cost of decomposition (extra tool calls, extra integration overhead) is real; only pay it when the constraint count actually warrants it.

Rule of thumb: if you can clearly enumerate every constraint in your head and they don't interact significantly, skip decomposition. If your inner monologue starts saying "and also" three times in a row about the same task, decompose.
