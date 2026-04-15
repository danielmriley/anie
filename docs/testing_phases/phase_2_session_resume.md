# Phase 2 — Session Resume and Context Continuity

## Why this phase exists

Session persistence is only useful if the persisted data can be reopened and used to continue work. This phase verifies that the session → context rebuild → agent loop chain produces valid, continuous behavior.

These tests are distinct from Phase 1. Phase 1 tests verify that data gets **into** a session correctly. Phase 2 tests verify that data comes **out** of a session correctly and can drive a new agent run.

---

## File to create

### `crates/anie-integration-tests/tests/session_resume.rs`

---

## Test cases

### Test 6: `resumed_session_context_drives_new_agent_run`

**Scenario:** A session with prior conversation is reopened. The rebuilt context is used as input to a new agent loop run, which completes successfully.

**Setup:**
- Create a session in a temp directory.
- Persist a user prompt: `"Read the file."`.
- Persist an assistant message: `"I read the file. It contains hello."`.
- Reopen the session and build context.

**Execution:**
- Extract the messages from `build_context()`.
- Create a new `MockProvider` scripted with `final_assistant("Yes, I remember the file.")`.
- Create an agent loop with an empty tool registry.
- Run the agent with a new prompt `"Do you remember what the file contained?"` and the rebuilt context.

**Assertions:**
- The agent run completes without error.
- `result.terminal_error` is `None`.
- `result.generated_messages.len() == 1` (the new assistant response).
- `result.final_context.len() == 4` (prior user + prior assistant + new user + new assistant).

---

### Test 7: `thinking_blocks_survive_session_roundtrip_into_agent_context`

**Scenario:** An assistant message with `ContentBlock::Thinking` is persisted and then rebuilt into context. The thinking block is preserved through the roundtrip.

**Setup:**
- Create a session.
- Persist a user prompt.
- Persist an assistant message with both a `Thinking` block and a `Text` block:
  ```rust
  AssistantMessage {
      content: vec![
          ContentBlock::Thinking { thinking: "Let me reason about this.".into() },
          ContentBlock::Text { text: "The answer is 42.".into() },
      ],
      ...
  }
  ```

**Execution:**
- Reopen the session.
- Build context.

**Assertions:**
- The rebuilt context has 2 messages.
- The assistant message in context has 2 content blocks.
- The first content block is `ContentBlock::Thinking` with the original thinking text.
- The second content block is `ContentBlock::Text` with the original answer text.

---

### Test 8: `compacted_session_context_drives_agent_run`

**Scenario:** A session is compacted (simulating long conversation history being summarized). The compacted context is used to drive a new agent run.

**Setup:**
- Create a session.
- Persist several user/assistant message pairs (at least 4 exchanges) to simulate a conversation.
- Manually add a compaction entry to the session with a summary like `"Prior discussion covered file reading and editing."`.
  - Use `session.add_entries(...)` with a `SessionEntry::Compaction` entry.

**Execution:**
- Reopen the session and build context.
- The rebuilt context should start from the compaction summary, not from the original first message.
- Create a `MockProvider` scripted with `final_assistant("Continuing from compacted context.")`.
- Run the agent with the rebuilt context and a new prompt.

**Assertions:**
- `result.terminal_error` is `None`.
- The agent run completes successfully.
- The rebuilt context includes the compaction summary message.
- The rebuilt context does **not** include the messages that were compacted away.
- `result.final_context` includes the compaction summary, the recent (non-compacted) messages, the new prompt, and the new assistant response.

---

## Key implementation notes

### Building context from a session

The helper chain is:
```rust
let context: Vec<Message> = session
    .build_context()
    .messages
    .into_iter()
    .map(|m| m.message)
    .collect();
```

This strips `entry_id` from `SessionContextMessage` and produces a plain `Vec<Message>` suitable for the agent loop.

### Compaction entries

A compaction entry can be added with:
```rust
session.add_entries(vec![SessionEntry::Compaction {
    base: SessionEntryBase {
        id: session.generate_id(),
        parent_id: session.leaf_id().map(str::to_string),
        timestamp: 1,
    },
    summary: "Summary of earlier conversation.".into(),
    discarded_entry_ids: vec![...],
    tokens_before: 5000,
}])?;
```

The `discarded_entry_ids` should reference the IDs of earlier entries that are being "compacted away". Use the IDs returned by `append_message` to populate this list.

### What NOT to test here

- Compaction **trigger logic** (threshold checks, auto-compaction) — that belongs in `anie-session` unit tests.
- Compaction **summary generation** (LLM summarization call) — use a hand-written summary instead.
- Provider-level behavior during the resumed run — the mock provider handles that.

---

## Exit criteria

- [ ] `crates/anie-integration-tests/tests/session_resume.rs` exists with 3 passing tests
- [ ] at least one test verifies that a rebuilt context drives a successful agent run
- [ ] at least one test verifies that thinking blocks survive the full session roundtrip
- [ ] at least one test verifies that compacted context omits old messages and includes the summary
- [ ] `cargo test -p anie-integration-tests` passes
- [ ] `cargo test --workspace` passes
