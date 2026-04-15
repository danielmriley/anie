# Post-Phase: Telegram Integration via teloxide

**Prerequisite:** Phases 1–6 complete (anie-rs is a working agent).
**Goal:** Connect anie-rs to Telegram so you can message it from your phone and have it execute tasks on your machine.

---

## Overview

This is a new crate (`anie-telegram`) in the workspace. It uses [teloxide](https://crates.io/crates/teloxide) (v0.17), the mature Rust Telegram bot framework. teloxide is async/tokio-native, supports long-polling and webhooks, has a dependency injection system (dptree), and handles all the Telegram Bot API details.

The architecture is identical to pi's `mom` (Slack bot): a thin messaging adapter that creates an `AgentLoop` per chat and forwards `AgentEvent`s as Telegram messages.

```
Telegram servers (Bot API)
        │
        ▼  (long polling)
┌──────────────────────────┐
│   anie-telegram          │
│   teloxide Dispatcher    │
│                          │
│   ┌─── ChatSession ───┐ │     One per Telegram chat
│   │  AgentLoop         │ │
│   │  ToolRegistry      │ │
│   │  SessionManager    │ │
│   │  event_rx listener │─┼──▶  Formats AgentEvents as Telegram messages
│   └────────────────────┘ │
└──────────────────────────┘
```

---

## Dependencies

```toml
# In workspace Cargo.toml
[workspace.dependencies]
teloxide = { version = "0.17", features = ["macros", "ctrlc_handler"] }

# crates/anie-telegram/Cargo.toml
[dependencies]
teloxide = { workspace = true }
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tracing = { workspace = true }
anyhow = { workspace = true }

anie-agent = { workspace = true }
anie-protocol = { workspace = true }
anie-provider = { workspace = true }
anie-providers-builtin = { workspace = true }
anie-tools = { workspace = true }
anie-session = { workspace = true }
anie-config = { workspace = true }
anie-auth = { workspace = true }
```

### About teloxide

teloxide is the de-facto standard Telegram bot framework for Rust:

- **Async-native:** Built on tokio, same runtime as anie-rs.
- **Dispatcher pattern:** Routes updates (messages, commands, callbacks) through a handler tree using `dptree` (dependency injection tree). Handlers receive typed, automatically-parsed parameters.
- **`Bot::from_env()`:** Reads `TELOXIDE_TOKEN` env var.
- **Long-polling by default:** No webhook server needed for development. Webhooks available via axum for production.
- **Rich types:** Full Telegram Bot API type coverage — messages, inline keyboards, file uploads, markdown formatting, chat actions ("typing..."), etc.
- **Ctrl+C handling:** Built-in graceful shutdown.
- **Actively maintained:** Regular releases tracking Telegram Bot API updates.

---

## Architecture

### Entry Point

```rust
// crates/anie-telegram/src/main.rs

use teloxide::prelude::*;
use std::sync::Arc;
use tokio::sync::Mutex;

mod chat_session;
mod event_handler;
mod formatting;
mod config;

use chat_session::{ChatSessionManager, SharedState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("anie_telegram=info,anie=info")
        .init();

    tracing::info!("Starting anie-telegram bot...");

    let bot = Bot::from_env();

    // Load anie config and set up providers
    let anie_config = anie_config::load_config(Default::default())?;
    let mut provider_registry = anie_provider::ProviderRegistry::new();
    anie_providers_builtin::register_builtin_providers(&mut provider_registry);

    let shared_state = SharedState {
        provider_registry: Arc::new(provider_registry),
        anie_config: Arc::new(anie_config),
        sessions: Arc::new(Mutex::new(ChatSessionManager::new())),
    };

    let handler = Update::filter_message()
        // Handle /start, /help, /model, /thinking, /clear, /compact commands
        .branch(
            dptree::entry()
                .filter_command::<BotCommand>()
                .endpoint(handle_command),
        )
        // Handle all other text messages as prompts
        .branch(
            Message::filter_text()
                .endpoint(handle_text_message),
        )
        // Handle photo messages (image + optional caption)
        .branch(
            Message::filter_photo()
                .endpoint(handle_photo_message),
        );

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![shared_state])
        .default_handler(|upd| async move {
            tracing::debug!("Unhandled update: {:?}", upd);
        })
        .error_handler(LoggingErrorHandler::with_custom_text(
            "Error in anie-telegram dispatcher",
        ))
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}
```

### Bot Commands

```rust
use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
enum BotCommand {
    /// Show help
    #[command(aliases = ["h"])]
    Help,
    /// Show or switch the active model
    Model(String),
    /// Set thinking level (off, low, medium, high)
    Thinking(String),
    /// Force context compaction
    Compact,
    /// Clear conversation history
    Clear,
    /// Show current status (model, context usage, session info)
    Status,
    /// Abort the current agent run
    Abort,
}
```

### Chat Session Manager

Each Telegram chat gets its own agent session, persisted independently:

```rust
// crates/anie-telegram/src/chat_session.rs

use std::collections::HashMap;
use std::path::PathBuf;

pub struct SharedState {
    pub provider_registry: Arc<ProviderRegistry>,
    pub anie_config: Arc<AnieConfig>,
    pub sessions: Arc<Mutex<ChatSessionManager>>,
}

impl Clone for SharedState {
    fn clone(&self) -> Self {
        Self {
            provider_registry: Arc::clone(&self.provider_registry),
            anie_config: Arc::clone(&self.anie_config),
            sessions: Arc::clone(&self.sessions),
        }
    }
}

pub struct ChatSessionManager {
    sessions: HashMap<ChatId, ChatSession>,
    sessions_dir: PathBuf,
}

pub struct ChatSession {
    pub chat_id: ChatId,
    pub agent_loop: Arc<AgentLoop>,
    pub context: Vec<Message>,
    pub session_manager: SessionManager,
    pub cancel: CancellationToken,
    pub is_running: bool,
}

impl ChatSessionManager {
    pub fn new() -> Self {
        let sessions_dir = dirs::home_dir()
            .unwrap()
            .join(".anie/telegram/sessions");
        std::fs::create_dir_all(&sessions_dir).ok();

        Self {
            sessions: HashMap::new(),
            sessions_dir,
        }
    }

    /// Get or create a session for a chat.
    pub fn get_or_create(
        &mut self,
        chat_id: ChatId,
        provider_registry: &Arc<ProviderRegistry>,
        config: &AnieConfig,
    ) -> &mut ChatSession {
        self.sessions.entry(chat_id).or_insert_with(|| {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            let session_file = self.sessions_dir
                .join(format!("chat_{}.jsonl", chat_id));

            // Create tool registry
            let mut tool_registry = ToolRegistry::new();
            tool_registry.register(Arc::new(ReadTool::new(cwd.to_str().unwrap())));
            tool_registry.register(Arc::new(WriteTool::new(
                cwd.clone(),
                Arc::new(FileMutationQueue::new()),
            )));
            tool_registry.register(Arc::new(EditTool::new(
                cwd.clone(),
                Arc::new(FileMutationQueue::new()),
            )));
            tool_registry.register(Arc::new(BashTool::new(cwd.to_str().unwrap())));

            // Resolve model and request options
            let model = resolve_model(config).unwrap();
            let request_options_resolver = Arc::new(AuthResolver {
                cli_api_key: None,
                config: config.clone(),
            });

            let system_prompt = build_telegram_system_prompt(&cwd, &tool_registry);

            let agent_loop = Arc::new(AgentLoop::new(
                Arc::clone(provider_registry),
                Arc::new(tool_registry),
                AgentLoopConfig {
                    model,
                    system_prompt,
                    thinking: config.model.thinking,
                    tool_execution: ToolExecutionMode::Parallel,
                    request_options_resolver,
                    get_steering_messages: None,
                    get_follow_up_messages: None,
                },
            ));

            // Open or create session
            let session_manager = if session_file.exists() {
                SessionManager::open_session(&session_file)
                    .unwrap_or_else(|_| SessionManager::new_session(
                        &self.sessions_dir, &cwd,
                    ).unwrap())
            } else {
                SessionManager::new_session(&self.sessions_dir, &cwd).unwrap()
            };

            // Load existing context
            let session_ctx = session_manager.build_context();

            ChatSession {
                chat_id,
                agent_loop,
                context: session_ctx.messages.into_iter().map(|m| m.message).collect(),
                session_manager,
                cancel: CancellationToken::new(),
                is_running: false,
            }
        })
    }
}
```

### Message Handler

The core handler that bridges Telegram messages to the agent loop:

```rust
// crates/anie-telegram/src/event_handler.rs

async fn handle_text_message(
    bot: Bot,
    msg: TelegramMessage,
    text: String,
    state: SharedState,
) -> ResponseResult<()> {
    let chat_id = msg.chat.id;
    let user_name = msg.from
        .as_ref()
        .and_then(|u| u.username.clone())
        .unwrap_or_else(|| "user".into());

    // Get or create chat session
    let mut sessions = state.sessions.lock().await;
    let session = sessions.get_or_create(
        chat_id,
        &state.provider_registry,
        &state.anie_config,
    );

    if session.is_running {
        bot.send_message(chat_id, "⏳ Still working on the previous request. Use /abort to cancel.")
            .await?;
        return Ok(());
    }

    session.is_running = true;
    let cancel = CancellationToken::new();
    session.cancel = cancel.clone();

    // Build user message
    let user_msg = Message::User(UserMessage {
        content: vec![ContentBlock::Text { text: text.clone() }],
        timestamp: now_millis(),
    });

    // Persist to session
    let entry = SessionEntry::Message {
        base: EntryBase {
            id: session.session_manager.generate_id(),
            parent_id: session.session_manager.leaf_id().map(|s| s.to_string()),
            timestamp: now_iso8601(),
        },
        message: user_msg.clone(),
    };
    session.session_manager.add_entries(vec![entry]).ok();

    // Set up channels
    let (event_tx, event_rx) = mpsc::channel(256);
    let agent = Arc::clone(&session.agent_loop);
    let mut context = session.context.clone();

    // Drop the lock before spawning the agent
    drop(sessions);

    // Spawn agent on background task
    let agent_handle = tokio::spawn(async move {
        let run_result = agent.run(vec![user_msg], context, event_tx, cancel).await;
        run_result.final_context
    });

    // Send "typing..." indicator
    bot.send_chat_action(chat_id, teloxide::types::ChatAction::Typing).await.ok();

    // Process events and send Telegram messages
    let final_context = process_agent_events(
        bot.clone(),
        chat_id,
        event_rx,
        agent_handle,
    ).await?;

    // Update session state
    let mut sessions = state.sessions.lock().await;
    if let Some(session) = sessions.sessions.get_mut(&chat_id) {
        session.context = final_context;
        session.is_running = false;

        // Persist new messages to session file
        // (The final context contains all new messages)
    }

    Ok(())
}
```

### Event Processing

Translate `AgentEvent`s into Telegram messages:

```rust
async fn process_agent_events(
    bot: Bot,
    chat_id: ChatId,
    mut event_rx: mpsc::Receiver<AgentEvent>,
    agent_handle: JoinHandle<Vec<Message>>,
) -> Result<Vec<Message>> {
    let mut response_text = String::new();
    let mut last_edit_msg_id: Option<MessageId> = None;
    let mut last_edit_time = Instant::now();

    // Telegram rate-limits message edits. Buffer streaming text
    // and edit the message at most every 2 seconds.
    let edit_interval = Duration::from_secs(2);

    while let Some(event) = event_rx.recv().await {
        match event {
            AgentEvent::MessageDelta { delta: StreamDelta::TextDelta(text) } => {
                response_text.push_str(&text);

                // Periodically edit the message to show streaming progress
                if last_edit_time.elapsed() >= edit_interval && !response_text.is_empty() {
                    let display_text = truncate_for_telegram(&response_text);
                    match last_edit_msg_id {
                        Some(msg_id) => {
                            bot.edit_message_text(chat_id, msg_id, &display_text)
                                .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                                .await.ok();
                        }
                        None => {
                            let sent = bot.send_message(chat_id, &display_text).await?;
                            last_edit_msg_id = Some(sent.id);
                        }
                    }
                    last_edit_time = Instant::now();
                }
            }

            AgentEvent::ToolExecStart { tool_name, args, .. } => {
                let tool_display = format_tool_start(&tool_name, &args);
                bot.send_message(chat_id, &tool_display)
                    .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                    .await.ok();

                // Refresh typing indicator
                bot.send_chat_action(chat_id, teloxide::types::ChatAction::Typing)
                    .await.ok();
            }

            AgentEvent::ToolExecEnd { tool_name, result, is_error, .. } => {
                let result_display = format_tool_result(&tool_name, &result, is_error);
                if !result_display.is_empty() {
                    bot.send_message(chat_id, &result_display)
                        .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                        .await.ok();
                }
            }

            AgentEvent::TurnStart => {
                // New turn (tool results fed back) — refresh typing
                bot.send_chat_action(chat_id, teloxide::types::ChatAction::Typing)
                    .await.ok();

                // Reset response text for new assistant message
                response_text.clear();
                last_edit_msg_id = None;
            }

            AgentEvent::AgentEnd { .. } => {
                // Send or edit the final response
                if !response_text.is_empty() {
                    let final_text = truncate_for_telegram(&response_text);
                    match last_edit_msg_id {
                        Some(msg_id) => {
                            bot.edit_message_text(chat_id, msg_id, &final_text)
                                .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                                .await.ok();
                        }
                        None => {
                            bot.send_message(chat_id, &final_text)
                                .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                                .await.ok();
                        }
                    }
                }
                break;
            }

            AgentEvent::RetryScheduled { attempt, max_retries, delay_ms, error } => {
                bot.send_message(
                    chat_id,
                    &format!("⟳ Retrying ({}/{}) in {:.0}s: {}", attempt, max_retries, delay_ms as f64 / 1000.0, error),
                ).await.ok();
            }

            _ => {}
        }
    }

    // Get final context from the agent task
    let context = agent_handle.await?;
    Ok(context)
}
```

### Telegram Message Formatting

Telegram has its own markdown variant (MarkdownV2) with different escaping rules. Build helpers:

```rust
// crates/anie-telegram/src/formatting.rs

/// Telegram MarkdownV2 requires escaping these characters:
/// _ * [ ] ( ) ~ ` > # + - = | { } . !
pub fn escape_markdown_v2(text: &str) -> String {
    let special = ['_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!'];
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if special.contains(&c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Telegram messages have a 4096 character limit.
/// Split long messages or truncate.
const TELEGRAM_MAX_LENGTH: usize = 4096;

pub fn truncate_for_telegram(text: &str) -> String {
    if text.len() <= TELEGRAM_MAX_LENGTH {
        return text.to_string();
    }
    format!(
        "{}\\.\\.\\. \\(truncated, {} chars total\\)",
        &text[..TELEGRAM_MAX_LENGTH - 50],
        text.len(),
    )
}

pub fn format_tool_start(tool_name: &str, args: &serde_json::Value) -> String {
    let display = match tool_name {
        "bash" => {
            let cmd = args.get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("...");
            format!("⚙ `$ {}`", escape_markdown_v2(cmd))
        }
        "read" => {
            let path = args.get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("...");
            format!("📖 Reading `{}`", escape_markdown_v2(path))
        }
        "edit" => {
            let path = args.get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("...");
            format!("✏️ Editing `{}`", escape_markdown_v2(path))
        }
        "write" => {
            let path = args.get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("...");
            format!("📝 Writing `{}`", escape_markdown_v2(path))
        }
        _ => format!("⚙ {}", escape_markdown_v2(tool_name)),
    };
    display
}

pub fn format_tool_result(
    tool_name: &str,
    result: &ToolResult,
    is_error: bool,
) -> String {
    let text = result.content.iter()
        .filter_map(|c| match c {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    if text.is_empty() {
        return String::new();
    }

    let prefix = if is_error { "❌" } else { "✓" };
    let truncated = if text.len() > 500 {
        format!("{}\\.\\.\\.", &text[..497])
    } else {
        text.clone()
    };

    format!(
        "{} `{}`\n```\n{}\n```",
        prefix,
        escape_markdown_v2(tool_name),
        truncated, // Code blocks don't need escaping in MarkdownV2
    )
}
```

### Telegram System Prompt

The system prompt for Telegram gets a few modifications:

```rust
fn build_telegram_system_prompt(cwd: &Path, tools: &ToolRegistry) -> String {
    let base = build_system_prompt(cwd, tools, &AnieConfig::default()).unwrap();

    format!(
        "{}\n\n\
        ## Telegram-Specific Guidelines\n\n\
        - You are communicating via Telegram. Keep responses concise.\n\
        - Use Telegram-compatible markdown: *bold*, _italic_, `code`, ```code blocks```.\n\
        - Do NOT use headers (#), tables, or other markdown features unsupported by Telegram.\n\
        - Long outputs should be summarized. The user is on a mobile device.\n\
        - When showing file contents or command output, show only the relevant portions.\n\
        - If a task will take many tool calls, provide brief progress updates between steps.",
        base,
    )
}
```

### Image Handling

teloxide makes it easy to receive photos. Forward them to the agent as images:

```rust
async fn handle_photo_message(
    bot: Bot,
    msg: TelegramMessage,
    state: SharedState,
) -> ResponseResult<()> {
    let chat_id = msg.chat.id;

    // Get the largest photo size
    let photos = msg.photo().unwrap();
    let photo = photos.last().unwrap(); // Largest size

    // Download the photo
    let file = bot.get_file(&photo.file.id).await?;
    let mut data = Vec::new();
    bot.download_file(&file.path, &mut data).await?;

    // Base64 encode
    let base64_data = base64::engine::general_purpose::STANDARD.encode(&data);

    // Build content blocks
    let mut content = vec![ContentBlock::Image {
        media_type: "image/jpeg".into(),
        data: base64_data,
    }];

    // Add caption as text if present
    if let Some(caption) = msg.caption() {
        content.push(ContentBlock::Text { text: caption.to_string() });
    }

    let user_msg = Message::User(UserMessage {
        content,
        timestamp: now_millis(),
    });

    // Route to the same agent handling as text messages
    // ... (same pattern as handle_text_message)

    Ok(())
}
```

### Authorization

Not everyone should be able to talk to your bot. Add an allowlist:

```rust
// Configuration in config.toml or environment
// ANIE_TELEGRAM_ALLOWED_USERS=123456,789012 (Telegram user IDs)

pub struct TelegramConfig {
    pub allowed_users: Vec<UserId>,
    pub allowed_chats: Vec<ChatId>,
}

fn is_authorized(config: &TelegramConfig, msg: &TelegramMessage) -> bool {
    if config.allowed_users.is_empty() && config.allowed_chats.is_empty() {
        return true; // No restrictions configured
    }

    if let Some(from) = &msg.from {
        if config.allowed_users.contains(&from.id) {
            return true;
        }
    }

    if config.allowed_chats.contains(&msg.chat.id) {
        return true;
    }

    false
}

// In the handler:
if !is_authorized(&telegram_config, &msg) {
    bot.send_message(msg.chat.id, "⛔ Unauthorized.").await?;
    return Ok(());
}
```

### Command Handlers

```rust
async fn handle_command(
    bot: Bot,
    msg: TelegramMessage,
    cmd: BotCommand,
    state: SharedState,
) -> ResponseResult<()> {
    let chat_id = msg.chat.id;

    match cmd {
        BotCommand::Help => {
            bot.send_message(chat_id, concat!(
                "🤖 *anie\\-rs Telegram Bot*\n\n",
                "Send me a message and I'll help with coding tasks\\.\n\n",
                "*Commands:*\n",
                "/help \\- Show this help\n",
                "/model \\<id\\> \\- Switch model\n",
                "/thinking \\<level\\> \\- Set thinking level\n",
                "/status \\- Show current status\n",
                "/compact \\- Compact context\n",
                "/clear \\- Clear conversation\n",
                "/abort \\- Cancel current run\n",
            ))
            .parse_mode(teloxide::types::ParseMode::MarkdownV2)
            .await?;
        }

        BotCommand::Abort => {
            let mut sessions = state.sessions.lock().await;
            if let Some(session) = sessions.sessions.get_mut(&chat_id) {
                if session.is_running {
                    session.cancel.cancel();
                    bot.send_message(chat_id, "⏹ Aborting...").await?;
                } else {
                    bot.send_message(chat_id, "Nothing running.").await?;
                }
            }
        }

        BotCommand::Status => {
            let sessions = state.sessions.lock().await;
            let status = if let Some(session) = sessions.sessions.get(&chat_id) {
                let tokens: u64 = session.context.iter()
                    .map(|m| estimate_tokens(m))
                    .sum();
                format!(
                    "Model: {}\nThinking: {:?}\nContext: {} tokens\nRunning: {}",
                    session.agent_loop.config.model.name,
                    session.agent_loop.config.thinking,
                    tokens,
                    session.is_running,
                )
            } else {
                "No active session.".to_string()
            };
            bot.send_message(chat_id, &status).await?;
        }

        BotCommand::Clear => {
            let mut sessions = state.sessions.lock().await;
            if let Some(session) = sessions.sessions.get_mut(&chat_id) {
                session.context.clear();
                bot.send_message(chat_id, "🗑 Conversation cleared.").await?;
            }
        }

        BotCommand::Model(model_id) => {
            if model_id.is_empty() {
                // Show current model
                let sessions = state.sessions.lock().await;
                if let Some(session) = sessions.sessions.get(&chat_id) {
                    bot.send_message(
                        chat_id,
                        &format!("Current model: {}", session.agent_loop.config.model.name),
                    ).await?;
                }
            } else {
                // Switch model
                bot.send_message(chat_id, &format!("Switched to: {}", model_id)).await?;
            }
        }

        BotCommand::Thinking(level) => {
            bot.send_message(chat_id, &format!("Thinking set to: {}", level)).await?;
        }

        BotCommand::Compact => {
            bot.send_message(chat_id, "⟳ Compacting context...").await?;
            // Trigger compaction...
            bot.send_message(chat_id, "✓ Context compacted.").await?;
        }
    }

    Ok(())
}
```

---

## Telegram-Specific Considerations

### Rate Limits

Telegram enforces rate limits on bots:
- **1 message per second per chat** (approximately)
- **30 messages per second globally**
- **20 messages per minute per group chat**

The event handler must buffer streaming text and edit messages at most every 2 seconds, not on every `TextDelta`. The implementation above handles this with `edit_interval`.

### Message Length

Telegram messages are capped at **4096 characters**. For long responses:
1. Truncate with a "see full output" note, or
2. Split into multiple messages, or
3. Send as a file attachment for very long outputs

```rust
fn send_long_text(bot: &Bot, chat_id: ChatId, text: &str) -> Vec<String> {
    if text.len() <= 4096 {
        return vec![text.to_string()];
    }

    // Split at paragraph boundaries
    let mut parts = Vec::new();
    let mut current = String::new();

    for paragraph in text.split("\n\n") {
        if current.len() + paragraph.len() + 2 > 4000 {
            parts.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(paragraph);
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts
}
```

### Typing Indicator

Telegram shows "typing..." for 5 seconds after `sendChatAction`. For long agent runs, periodically refresh it:

```rust
// Spawn a background task that sends typing every 4 seconds
let typing_task = tokio::spawn({
    let bot = bot.clone();
    async move {
        loop {
            bot.send_chat_action(chat_id, ChatAction::Typing).await.ok();
            tokio::time::sleep(Duration::from_secs(4)).await;
        }
    }
});

// Cancel when agent finishes
typing_task.abort();
```

### File Sharing

When the agent reads or creates files, optionally share them as Telegram documents:

```rust
// After a write or edit tool call, offer to share the file
async fn maybe_share_file(
    bot: &Bot,
    chat_id: ChatId,
    tool_name: &str,
    args: &serde_json::Value,
) {
    if tool_name == "write" || tool_name == "edit" {
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            let abs_path = std::path::Path::new(path);
            if abs_path.exists() {
                if let Ok(metadata) = std::fs::metadata(abs_path) {
                    // Only share small files (< 1 MB)
                    if metadata.len() < 1_000_000 {
                        let input_file = teloxide::types::InputFile::file(abs_path);
                        bot.send_document(chat_id, input_file).await.ok();
                    }
                }
            }
        }
    }
}
```

---

## Deployment

### Development (Long Polling)

```bash
# Set your bot token (from @BotFather on Telegram)
export TELOXIDE_TOKEN="123456:ABC-DEF..."

# Optional: restrict to your user ID
export ANIE_TELEGRAM_ALLOWED_USERS="your_telegram_user_id"

# Run
cargo run --package anie-telegram
```

### Production (Webhook via axum)

For always-on deployment, use webhooks instead of long polling. teloxide supports this with the `webhooks-axum` feature:

```toml
teloxide = { version = "0.17", features = ["macros", "ctrlc_handler", "webhooks-axum"] }
```

```rust
// Production: webhook mode
let url = "https://your-server.example.com/webhook".parse().unwrap();
let listener = teloxide::update_listeners::webhooks::axum(
    bot.clone(),
    teloxide::update_listeners::webhooks::Options::new(
        ([0, 0, 0, 0], 8443).into(),
        url,
    ),
).await?;

Dispatcher::builder(bot, handler)
    .dependencies(dptree::deps![shared_state])
    .build()
    .dispatch_with_listener(listener, LoggingErrorHandler::new())
    .await;
```

### Security

- **Run on the same machine** as the files you want the agent to access. The bot executes bash commands locally.
- **Use the allowlist.** Without it, anyone who finds your bot can run commands on your machine.
- **Consider a dedicated workspace directory** for Telegram sessions to limit file access scope.
- **Don't expose the bot token.** Treat it like an API key.

---

## Working Directory and Scoping

A critical design decision: what CWD does the Telegram agent use?

**Option A: Fixed CWD at startup** — The bot always works in the directory where it was launched. Simple but limiting.

**Option B: Per-chat CWD** — Each chat gets its own working directory. The user can set it with a `/cd` command. More flexible.

**Option C: Configurable workspace** — A configured workspace directory (e.g., `~/.anie/telegram/workspace/`) with per-chat subdirectories. Safest, prevents accidental modification of system files.

**Recommendation:** Start with Option A. Add Option C when you want to expose the bot to other users or run it as a persistent service.

---

## Future: Generalizing to Other Platforms

Once the Telegram adapter exists, the same pattern generalizes to Discord, Matrix, Slack, and others. The common interface is:

```rust
/// Trait for messaging platform adapters.
#[async_trait]
pub trait MessagingAdapter: Send + Sync {
    /// Receive the next incoming message.
    async fn recv_message(&mut self) -> Option<IncomingMessage>;

    /// Send a text message to a chat.
    async fn send_text(&self, chat_id: &str, text: &str) -> Result<MessageId>;

    /// Edit an existing message.
    async fn edit_text(&self, chat_id: &str, msg_id: &MessageId, text: &str) -> Result<()>;

    /// Send a typing indicator.
    async fn send_typing(&self, chat_id: &str) -> Result<()>;

    /// Send a file.
    async fn send_file(&self, chat_id: &str, path: &Path, caption: Option<&str>) -> Result<()>;
}
```

Each platform implements this trait. The agent orchestration and event processing is shared. But this abstraction is premature for v1 — build the Telegram adapter first, extract the pattern later.

---

## Implementation Checklist

| # | Task | Effort |
|---|---|---|
| 1 | Create `anie-telegram` crate with teloxide dependency | 1 hour |
| 2 | Basic dispatcher: receive messages, echo back | 2 hours |
| 3 | Wire in AgentLoop + tools per chat | 4 hours |
| 4 | Event → Telegram message formatting | 4 hours |
| 5 | Streaming with message editing (rate-limited) | 3 hours |
| 6 | Bot commands (/help, /model, /status, /abort, /clear) | 3 hours |
| 7 | Session persistence per chat | 2 hours |
| 8 | Photo/image handling | 2 hours |
| 9 | Authorization allowlist | 1 hour |
| 10 | Compaction integration | 2 hours |
| 11 | Typing indicator refresh loop | 1 hour |
| 12 | Long message splitting | 1 hour |
| **Total** | | **~3 days** |
