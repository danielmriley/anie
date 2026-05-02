---
name: verify-after-edit
description: After editing or writing a file under test, re-run the most recent build/test/run command before claiming the change works. The harness ledger lists prior bash commands; find the build/test command and re-execute.
---

# When this applies

You just used `edit` or `write` on a file that:
- Was previously compiled (you ran `g++`, `cargo build`, `tsc`, etc. earlier in this conversation).
- Was previously tested (you ran `pytest`, `cargo test`, `npm test`, etc.).
- Was previously executed (you ran the binary, ran a script, etc.).

The user is expecting your edit to be CORRECT, not just SYNTACTICALLY valid. The compiler can confirm syntax; only running the test or binary confirms behavior.

# Why this matters

The model's most damaging failure mode is claiming a change works without verifying it. Real failure modes you might be about to commit:

- **Syntactically valid, semantically broken.** The new code compiles but doesn't do what the change description claims.
- **Subtle regression.** You "improved" one thing but broke an unrelated test.
- **Cargo-cult fix.** You fixed the symptom (the error you were seeing) without fixing the cause; the next test surfaces the real bug.
- **Comment-vs-code drift.** You wrote correct intent in a comment but `= default`'ed the implementation. Comments don't run.

A re-run takes seconds. Catching the bug now beats catching it after the user reports it.

# What to do

## Step 1: find the most recent verification command

Scan the per-turn ledger for prior `bash` commands. Look for these patterns (in priority order):

- A test runner: `cargo test`, `pytest`, `npm test`, `go test`, `vitest`.
- A build + run: `g++ ... && ./binary`, `cargo run`, `python script.py`, `node app.js`.
- A linter / type-checker: `cargo clippy`, `mypy`, `tsc --noEmit`.
- A plain build: `cargo build`, `g++ -c`, `tsc`.

Pick the most relevant — usually the most recent test or build+run command for the same file.

If you can't recall the exact command and it's not in a ledger you can see, ASK the user before declaring success: "I'm about to claim the edit works. What's the right verification command for this file?"

## Step 2: re-run

Issue the bash command. Read the actual output, including:

- Exit code.
- Stdout / stderr — does the program produce expected output? Are there warnings?
- Any test failures, even unrelated ones (you may have broken something).

## Step 3: interpret carefully

A successful exit code is necessary but NOT sufficient. The output also has to match expectations.

- If `g++ -std=c++17 main.cpp -o bin && ./bin` exits 0 but produces empty output, that's not necessarily success — the binary might have segfaulted at the wrong moment.
- If `pytest` reports `passed` for the test you intended to add but `failed` for an unrelated one, you've regressed something.
- If `cargo test` reports new compile warnings, those are a yellow flag worth investigating.

## Step 4: only THEN claim success

If the verification command produced the output you expected, with no regressions, you can tell the user the change works.

If it didn't, treat it like any other failure — diagnose, fix, repeat verification. Don't tell the user it works just because you've already spent a few iterations.

# Anti-pattern

The failure mode this skill exists to prevent looks like:

```
[edit /path/to/file.cpp]
"I've fixed the issue. The change is now in place."
```

That's claiming success without verification. Don't do this.

The correct pattern:

```
[edit /path/to/file.cpp]
[bash g++ -std=c++17 file.cpp -o bin && ./bin]
"Compiled and ran successfully. Output: <actual output>. The change works."
```

# Edge cases

- **The verification command itself failed for an unrelated reason** (wrong cwd, missing binary, etc.). Fix the verification setup before judging the edit.
- **The change is to a file with no test suite.** Tell the user — "I edited X but I don't see a test for it; do you want me to write one, or do you have a way to verify manually?"
- **The change is large enough that verification takes a long time** (full integration test suite). Run a narrower subset first to fail fast, then the full suite if the narrow one passes.
