use anie_integration_tests::helpers::*;
use anie_protocol::{AssistantMessage, ContentBlock, Message, StopReason, Usage};
use anie_provider::mock::MockStreamScript;
use anie_session::{EntryBase, SessionEntry, SessionManager};

#[tokio::test]
async fn resumed_session_context_drives_new_agent_run() {
    let (dir, mut session) = create_temp_session();
    let _ = dir; // keep alive

    let prompt = user_prompt("Read the file.");
    session.append_message(&prompt).expect("persist prompt");
    session
        .append_messages(&[Message::Assistant(final_assistant(
            "I read the file. It contains hello.",
        ))])
        .expect("persist assistant");

    let session_path = session.path().to_path_buf();
    drop(session);

    let reopened = SessionManager::open_session(&session_path).expect("reopen");
    let prior_context: Vec<Message> = reopened
        .build_context()
        .messages
        .into_iter()
        .map(|m| m.message)
        .collect();
    assert_eq!(prior_context.len(), 2);

    let provider = anie_provider::mock::MockProvider::new(vec![MockStreamScript::from_message(
        final_assistant("Yes, I remember the file."),
    )]);
    let agent = build_agent(
        provider,
        std::sync::Arc::new(anie_agent::ToolRegistry::new()),
    );
    let new_prompt = user_prompt("Do you remember what the file contained?");
    let (result, _events) =
        run_agent_collecting_events(agent, vec![new_prompt], prior_context).await;

    assert!(result.terminal_error.is_none());
    assert_eq!(result.generated_messages.len(), 1);
    assert_eq!(result.final_context.len(), 4);
}

#[tokio::test]
async fn thinking_blocks_survive_session_roundtrip_into_agent_context() {
    let (dir, mut session) = create_temp_session();
    let _ = dir;

    let prompt = user_prompt("Think about this.");
    session.append_message(&prompt).expect("persist prompt");

    let assistant = Message::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::Thinking {
                thinking: "Let me reason about this.".into(),
            },
            ContentBlock::Text {
                text: "The answer is 42.".into(),
            },
        ],
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp: 1,
    });
    session
        .append_messages(&[assistant])
        .expect("persist assistant");

    let session_path = session.path().to_path_buf();
    drop(session);

    let reopened = SessionManager::open_session(&session_path).expect("reopen");
    let context = reopened.build_context();
    assert_eq!(context.messages.len(), 2);

    let assistant_msg = match &context.messages[1].message {
        Message::Assistant(a) => a,
        other => panic!("expected Assistant, got {other:?}"),
    };
    assert_eq!(assistant_msg.content.len(), 2);
    assert!(matches!(
        &assistant_msg.content[0],
        ContentBlock::Thinking { thinking } if thinking == "Let me reason about this."
    ));
    assert!(matches!(
        &assistant_msg.content[1],
        ContentBlock::Text { text } if text == "The answer is 42."
    ));
}

#[tokio::test]
async fn compacted_session_context_drives_agent_run() {
    let (dir, mut session) = create_temp_session();
    let _ = dir;

    // Persist 4 question/answer exchanges with incrementing timestamps.
    let exchanges = 4;
    let mut entry_ids = Vec::new();
    for i in 0..exchanges {
        let ts = (i as u64 + 1) * 100;
        let id = session
            .append_message(&user_prompt_at(&format!("question {i}"), ts))
            .expect("persist prompt");
        entry_ids.push(id);
        let ids = session
            .append_messages(&[Message::Assistant(final_assistant_at(
                &format!("answer {i}"),
                ts + 50,
            ))])
            .expect("persist assistant");
        entry_ids.extend(ids);
    }

    // Each exchange produces 2 entry IDs (question + answer).  We want to
    // keep from the last exchange onward (exchange index 3, i.e. "question 3").
    let kept_exchange = 3;
    let first_kept_id = entry_ids[kept_exchange * 2].clone();
    session
        .add_entries(vec![SessionEntry::Compaction {
            base: EntryBase {
                id: format!("compact-{}", entry_ids.len()),
                parent_id: session.leaf_id().map(str::to_string),
                timestamp: "500".into(),
            },
            summary: "Prior discussion covered questions 0 through 2.".into(),
            first_kept_entry_id: first_kept_id,
            tokens_before: 5000,
        }])
        .expect("add compaction");

    let session_path = session.path().to_path_buf();
    drop(session);

    let reopened = SessionManager::open_session(&session_path).expect("reopen");
    let context = reopened.build_context();

    // Context should have: compaction summary + remaining exchange (question 3 + answer 3).
    assert!(
        context.messages.len() <= 4,
        "expected compacted context, got {} messages",
        context.messages.len()
    );
    // The first message should be the compaction summary (a user-shaped message).
    let first_text = match &context.messages[0].message {
        Message::User(u) => u
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or(""),
        Message::Assistant(a) => a
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or(""),
        _ => "",
    };
    assert!(
        first_text.contains("Prior discussion"),
        "first message was: {first_text}"
    );

    // Drive a new agent run with the compacted context.
    let prior: Vec<Message> = context.messages.into_iter().map(|m| m.message).collect();
    let provider = anie_provider::mock::MockProvider::new(vec![MockStreamScript::from_message(
        final_assistant("Continuing from compacted context."),
    )]);
    let agent = build_agent(
        provider,
        std::sync::Arc::new(anie_agent::ToolRegistry::new()),
    );
    let (result, _events) =
        run_agent_collecting_events(agent, vec![user_prompt("Continue.")], prior).await;

    assert!(result.terminal_error.is_none());
    assert_eq!(result.generated_messages.len(), 1);
}
