# Phase 3 — Agent Events → TUI Rendering Consistency

## Why this phase exists

The TUI renders based on the `AgentEvent` stream emitted by the agent loop. The agent unit tests verify that the correct events are emitted. The TUI unit tests verify that individual events render correctly. But no existing test connects these two sides: running the agent loop, collecting the events it actually emits, and verifying that replaying them into the TUI produces the expected visual output.

This phase tests the **contract** between the agent loop and the TUI. If either side changes its event shapes or rendering assumptions, these tests will catch the drift.

---

## File to create

### `crates/anie-integration-tests/tests/agent_tui.rs`

---

## Test cases

### Test 9: `agent_events_render_prompt_assistant_and_tool_blocks`

**Scenario:** A complete agent run with a tool call produces events that render correctly in the TUI: the user prompt, the assistant text, the tool execution block, and the final assistant response are all visible.

**Setup:**
- Write a file `example.txt` containing `hello from disk` to a temp directory.
- `MockProvider` scripted with:
  1. Assistant requesting `read` tool call for `example.txt`.
  2. `final_assistant("I read the file successfully.")`.
- Real tool registry with `ReadTool`.

**Execution:**
- Run the agent loop and collect all `AgentEvent`s.
- Create a TUI `App` instance.
- Replay every collected event into the app via `app.handle_agent_event(event)`.
- Render the app to a `TestBackend` buffer.
- Extract the screen text.

**Assertions:**
- The screen contains the user prompt text.
- The screen contains `I read the file successfully.` (final assistant text).
- The screen contains `read example.txt` (tool block title).
- The screen contains `hello from disk` (tool result body).

---

### Test 10: `agent_thinking_events_render_thinking_section_above_answer`

**Scenario:** The agent emits thinking deltas followed by text deltas. The TUI renders the thinking section above the answer with the expected visual structure.

**Setup:**
- `MockProvider` scripted with a stream that emits:
  1. `ProviderEvent::Start`
  2. `ProviderEvent::ThinkingDelta("Let me reason about this.")` 
  3. `ProviderEvent::TextDelta("The final answer is 42.")`
  4. `ProviderEvent::Done(assistant_message)` — where the assistant message has both a `Thinking` block and a `Text` block.
- Empty tool registry.

**Execution:**
- Run the agent loop and collect events.
- Replay into the TUI app.
- Render to a `TestBackend`.

**Assertions:**
- The screen contains the `thinking` heading.
- The screen contains `│ Let me reason about this.` (guttered thinking body).
- The screen contains `The final answer is 42.` (answer text).
- The thinking section appears **before** the answer text in the screen string (index comparison).

---

### Test 11: `agent_stream_error_renders_error_in_tui`

**Scenario:** The provider emits a stream error partway through. The TUI displays the error.

**Setup:**
- `MockProvider` scripted with:
  1. `Ok(ProviderEvent::Start)`
  2. `Err(ProviderError::Stream("connection lost"))`
- Empty tool registry.

**Execution:**
- Run the agent loop and collect events.
- Replay into the TUI app.
- Render to a `TestBackend`.

**Assertions:**
- The screen contains `connection lost` (the error message).
- The agent run result has `terminal_error == Some(ProviderError::Stream("connection lost"))`.

---

## TUI rendering helper

The replay-and-render pattern is the same for all three tests. Extract it as a helper in the test file or in `helpers.rs`:

```rust
fn replay_events_and_render(
    events: &[AgentEvent],
    width: u16,
    height: u16,
) -> String {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::channel(8);
    let mut app = App::new(event_rx, action_tx);

    for event in events {
        app.handle_agent_event(event.clone()).expect("handle event");
    }

    let mut terminal = Terminal::new(TestBackend::new(width, height))
        .expect("test terminal");
    terminal.draw(|frame| app.render(frame)).expect("draw");

    let buffer = terminal.backend().buffer();
    let area = buffer.area;
    let mut rows = Vec::new();
    for y in 0..area.height {
        let mut row = String::new();
        for x in 0..area.width {
            row.push_str(&buffer[(x, y)].symbol());
        }
        rows.push(row.trim_end().to_string());
    }
    rows.join("\n")
}
```

The `App::new` constructor expects `mpsc::Receiver<AgentEvent>` and `mpsc::Sender<UiAction>`. We create throwaway channels since we're only replaying events, not driving the real event loop. The receiver channel is never read — events are injected directly via `handle_agent_event`.

---

## Key implementation notes

### MockProvider stream scripts for streaming events

For Test 10, the mock provider needs to emit individual streaming events (not just a final message). Use `MockStreamScript::new(vec![...])` with explicit `ProviderEvent::Start`, `ProviderEvent::ThinkingDelta`, `ProviderEvent::TextDelta`, and `ProviderEvent::Done` items.

The agent loop translates these into `AgentEvent::MessageDelta` variants, which the TUI consumes.

### Screen size

Use a generous terminal size like `80x24` or `80x30` to avoid content being clipped. The TUI reserves 1 line for the status bar and 3 lines minimum for the input pane, leaving the rest for the output.

### What NOT to test here

- Scrolling behavior — covered by TUI unit tests.
- Exact styling/colors — not visible in `TestBackend` text extraction.
- Input handling — not relevant to agent event replay.
- Tool execution correctness — covered by Phase 1.

---

## Exit criteria

- [ ] `crates/anie-integration-tests/tests/agent_tui.rs` exists with 3 passing tests
- [ ] at least one test connects a real tool execution through the agent loop into a TUI render
- [ ] at least one test verifies the thinking section rendering from agent-emitted events
- [ ] at least one test verifies that a provider error surfaces in the TUI
- [ ] `cargo test -p anie-integration-tests` passes
- [ ] `cargo test --workspace` passes
