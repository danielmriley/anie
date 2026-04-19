# Phase 1 — Agent Loop → Real Tools → Session Persistence

## Why this phase exists

This is the highest-value integration test category. It exercises the full vertical slice that matters most in production: a user prompt flows through the agent loop, triggers real tool execution against a temp directory, produces an `AgentRunResult`, and the generated messages are persisted to a session file.

No existing unit test crosses these boundaries together. Agent tests use `TestTool`. Tool tests don't touch sessions. Session tests don't run the agent loop.

---

## File to create

### `crates/anie-integration-tests/tests/agent_session.rs`

All tests in this file follow the same pattern:

1. Create a temp directory for tool operations and session storage.
2. Write any seed files needed for tool execution.
3. Create a `MockProvider` with scripted responses.
4. Create a real tool registry pointed at the temp directory.
5. Build and run the agent loop.
6. Persist the `AgentRunResult` to a `SessionManager`.
7. Assert on the generated messages, session contents, and (where relevant) file-system side effects.

---

## Test cases

### Test 1: `prompt_and_assistant_response_persist_to_session`

**Scenario:** The simplest end-to-end flow. A user prompt produces a text-only assistant response. Both are persisted.

**Setup:**
- `MockProvider` scripted with one `final_assistant("The answer is 42.")`.
- Empty tool registry (no tools needed).

**Execution:**
- Run the agent loop with `user_prompt("What is the answer?")`.
- Persist the user prompt to the session via `append_message`.
- Persist generated messages via `append_messages`.

**Assertions:**
- `result.generated_messages.len() == 1`.
- The generated message is `Message::Assistant` with text `"The answer is 42."`.
- Reopen the session from disk.
- `session.build_context().messages.len() == 2` (user + assistant).
- The first context message is the user prompt.
- The second context message is the assistant response.

---

### Test 2: `read_tool_call_persists_file_contents_in_session`

**Scenario:** The agent requests a file read. The real `ReadTool` executes. The tool result containing the file contents is persisted.

**Setup:**
- Write `src/main.rs` containing `fn main() { println!("hello"); }` to the temp directory.
- `MockProvider` scripted with:
  1. An assistant message requesting a `read` tool call with `{"path": "src/main.rs"}`.
  2. A `final_assistant("I've read the file.")`.
- Real tool registry with `ReadTool`.

**Execution:**
- Run the agent loop.
- Persist user prompt and generated messages to the session.

**Assertions:**
- `result.generated_messages.len() == 3` (assistant with tool call, tool result, final assistant).
- The tool result message contains `fn main()` in its content.
- The tool result's `details` contains `"path": "src/main.rs"`.
- Reopen the session and verify `build_context()` returns 4 messages in order: user, assistant, tool_result, assistant.

---

### Test 3: `edit_tool_modifies_file_and_persists_diff`

**Scenario:** The agent requests a file edit. The real `EditTool` modifies a file on disk and produces a diff. The diff is captured in the session.

**Setup:**
- Write `hello.txt` containing `Hello, world!` to the temp directory.
- `MockProvider` scripted with:
  1. An assistant message requesting an `edit` tool call with `{"path": "hello.txt", "edits": [{"oldText": "world", "newText": "anie"}]}`.
  2. A `final_assistant("Edit complete.")`.
- Real tool registry with `EditTool`.

**Execution:**
- Run the agent loop.
- Persist to session.

**Assertions:**
- The file `hello.txt` on disk now contains `Hello, anie!`.
- The tool result message has `is_error == false`.
- The tool result's `details` contains a `"diff"` key.
- The diff contains `- world` and `+ anie` (or equivalent diff output).
- Session roundtrip preserves the diff in the tool result details.

---

### Test 4: `bash_tool_captures_output_and_elapsed_time`

**Scenario:** The agent runs a shell command. The output and timing metadata are captured.

**Setup:**
- `MockProvider` scripted with:
  1. An assistant message requesting a `bash` tool call with `{"command": "echo integration-test-output"}`.
  2. A `final_assistant("Command complete.")`.
- Real tool registry with `BashTool`.

**Execution:**
- Run the agent loop.
- Persist to session.

**Assertions:**
- The tool result content contains `integration-test-output`.
- The tool result's `details` contains `"elapsed_ms"` as a positive integer.
- The tool result's `details` contains `"command": "echo integration-test-output"`.
- Session roundtrip preserves elapsed time in details.

---

### Test 5: `multi_turn_tool_loop_produces_correct_message_sequence`

**Scenario:** The agent makes two tool-call rounds before producing a final answer. The session contains the full message sequence in the correct order.

**Setup:**
- Write `data.txt` containing `alpha` to the temp directory.
- `MockProvider` scripted with:
  1. An assistant message requesting a `read` tool call for `data.txt`.
  2. An assistant message requesting a `bash` tool call for `echo done`.
  3. A `final_assistant("All tasks complete.")`.
- Real tool registry with `ReadTool` and `BashTool`.

**Execution:**
- Run the agent loop.
- Persist the user prompt and all generated messages.

**Assertions:**
- `result.generated_messages.len() == 5` (assistant+tool_result for each round, plus final assistant).
- Message type sequence: `Assistant, ToolResult, Assistant, ToolResult, Assistant`.
- The first tool result contains `alpha` (from the file read).
- The second tool result contains `done` (from the bash command).
- Reopen the session and verify `build_context()` returns 6 messages (user + 5 generated), in order.
- The rebuilt context message types match: `User, Assistant, ToolResult, Assistant, ToolResult, Assistant`.

---

## Shared setup pattern

Every test in this file should follow this structure:

```rust
#[tokio::test]
async fn test_name() {
    // 1. Temp directory
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path();

    // 2. Seed files (if needed)
    std::fs::write(cwd.join("file.txt"), "contents").expect("write seed");

    // 3. Mock provider
    let provider = MockProvider::new(vec![
        MockStreamScript::from_message(assistant_with_tool_calls(vec![...])),
        MockStreamScript::from_message(final_assistant("done")),
    ]);

    // 4. Tool registry
    let tools = real_tool_registry(cwd);

    // 5. Agent loop
    let agent = build_agent(Box::new(provider), tools);
    let (result, _events) = run_agent_collecting_events(
        agent,
        vec![user_prompt("go")],
        Vec::new(),
    ).await;

    // 6. Session persistence
    let sessions_dir = cwd.join("sessions");
    let mut session = SessionManager::new_session(&sessions_dir, cwd)
        .expect("new session");
    session.append_message(&user_prompt("go")).expect("persist prompt");
    session.append_messages(&result.generated_messages).expect("persist result");

    // 7. Assertions on result, session, and filesystem
    assert_eq!(result.generated_messages.len(), ...);

    let session_path = std::fs::read_dir(&sessions_dir)
        .expect("read sessions dir")
        .filter_map(Result::ok)
        .next()
        .expect("session file")
        .path();
    let reopened = SessionManager::open_session(&session_path).expect("reopen");
    let context = reopened.build_context();
    assert_eq!(context.messages.len(), ...);
}
```

---

## What should NOT be tested here

- Exhaustive tool edge cases (BOM, CRLF, fuzzy matching, image reads) — those belong in `anie-tools` unit tests.
- Provider SSE parsing — that belongs in `anie-providers-builtin` unit tests.
- TUI rendering — that belongs in Phase 3.
- Retry/backoff — the controller owns that; agent-level integration tests should focus on single successful runs.

---

## Exit criteria

- [ ] `crates/anie-integration-tests/tests/agent_session.rs` exists with 5 passing tests
- [ ] each test creates a temp directory, runs the agent loop, persists to a session, and reopens the session
- [ ] at least one test verifies a real file-system side effect (edit modifying a file)
- [ ] `cargo test -p anie-integration-tests` passes
- [ ] `cargo test --workspace` passes
