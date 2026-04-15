# Phase 4: Sessions and Compaction (Weeks 7–8)

**Goal:** Add session persistence and context compaction. By the end of Phase 4, conversations survive restarts (`--resume`), `--resume <session_id>` reopens the most recently appended leaf in that session file, and the agent automatically summarizes old context when approaching the context window limit. Branch selection beyond the active leaf remains future work.

---

## Sub-phase 4.1: JSONL Session File Format

**Duration:** Days 1–3

### File Structure

Each session is a single `.jsonl` file in `~/.anie/sessions/`. Filename: `{session_id}.jsonl`.

**First line (header):**
```json
{"type":"session","version":1,"id":"a1b2c3d4","timestamp":"2026-04-13T10:00:00Z","cwd":"/home/user/project"}
```

**Subsequent lines (entries):**
```json
{"type":"message","id":"e5f6a7b8","parentId":null,"timestamp":"2026-04-13T10:00:01Z","message":{"role":"user","content":[{"type":"text","text":"Hello"}],"timestamp":1744531201000}}
{"type":"message","id":"c9d0e1f2","parentId":"e5f6a7b8","timestamp":"2026-04-13T10:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"Hi there!"}],"usage":{"input_tokens":100,"output_tokens":10},"stop_reason":"Stop","provider":"anthropic","model":"claude-sonnet-4-6","timestamp":1744531202000}}
```

### Entry Types

```rust
// crates/anie-session/src/entry.rs

/// The file header. Always the first line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub entry_type: String, // always "session"
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    pub parent_session: Option<String>,
}

/// Base fields shared by all entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryBase {
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: Option<String>,
    pub timestamp: String,
}

/// All entry types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionEntry {
    #[serde(rename = "message")]
    Message {
        #[serde(flatten)]
        base: EntryBase,
        message: Message,
    },
    #[serde(rename = "compaction")]
    Compaction {
        #[serde(flatten)]
        base: EntryBase,
        summary: String,
        tokens_before: u64,
        #[serde(rename = "firstKeptEntryId")]
        first_kept_entry_id: String,
    },
    #[serde(rename = "model_change")]
    ModelChange {
        #[serde(flatten)]
        base: EntryBase,
        model: String,
        provider: String,
    },
    #[serde(rename = "thinking_change")]
    ThinkingChange {
        #[serde(flatten)]
        base: EntryBase,
        level: ThinkingLevel,
    },
    #[serde(rename = "label")]
    Label {
        #[serde(flatten)]
        base: EntryBase,
        target_id: String,
        label: Option<String>,
    },
}

/// Union of header and entry for file parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FileEntry {
    Header(SessionHeader),
    Entry(SessionEntry),
}
```

### ID Generation

Use short 8-character hex IDs (truncated UUID) for readability in JSONL, with collision detection:

```rust
use uuid::Uuid;
use std::collections::HashSet;

pub fn generate_id(existing: &HashSet<String>) -> String {
    for _ in 0..100 {
        let id = Uuid::new_v4().to_string()[..8].to_string();
        if !existing.contains(&id) {
            return id;
        }
    }
    // Fallback to full UUID
    Uuid::new_v4().to_string()
}
```

### File Parsing

```rust
pub fn parse_session_file(content: &str) -> Result<(SessionHeader, Vec<SessionEntry>)> {
    let mut lines = content.lines();

    // First line must be the header
    let header_line = lines.next()
        .ok_or_else(|| anyhow!("Empty session file"))?;
    let header: SessionHeader = serde_json::from_str(header_line)?;

    let mut entries = Vec::new();
    for (line_num, line) in lines.enumerate() {
        let line = line.trim();
        if line.is_empty() { continue; }
        match serde_json::from_str::<SessionEntry>(line) {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                tracing::warn!("Skipping malformed line {} in session: {}", line_num + 2, e);
                // Continue — don't fail the entire session for one bad line
            }
        }
    }

    Ok((header, entries))
}
```

### Tests

1. **Roundtrip:** Write entries to JSONL, read back, verify equality.
2. **Malformed line resilience:** Insert a garbage line, verify other entries parse.
3. **All entry types:** Serialize and deserialize each variant.
4. **Edge cases:** Empty content arrays, Unicode text, very long messages.

### Acceptance Criteria

- All entry types serialize/deserialize correctly to JSONL.
- Parser is resilient to malformed lines.
- IDs are unique and short.

---

## Sub-phase 4.2: SessionManager

**Duration:** Days 3–5

### Core API

```rust
pub struct SessionManager {
    sessions_dir: PathBuf,
    header: SessionHeader,
    entries: Vec<SessionEntry>,
    by_id: HashMap<String, usize>,  // id → index in entries
    id_set: HashSet<String>,
    leaf_id: Option<String>,        // current tip of the active branch
    file_handle: std::fs::File,     // open for append
}

impl SessionManager {
    /// Create a new session.
    pub fn new_session(sessions_dir: &Path, cwd: &Path) -> Result<Self> { ... }

    /// Open an existing session.
    pub fn open_session(path: &Path) -> Result<Self> { ... }

    /// Fork from a specific entry, creating a new branch.
    pub fn fork(&mut self, from_entry_id: &str) -> Result<()> { ... }

    /// Append entries to the current branch.
    pub fn add_entries(&mut self, entries: Vec<SessionEntry>) -> Result<()> { ... }

    /// Convenience: append a single message at the current leaf.
    pub fn append_message(&mut self, message: &Message) -> Result<()> { ... }

    /// Convenience: append multiple messages in order at the current leaf.
    pub fn append_messages(&mut self, messages: &[Message]) -> Result<()> { ... }

    /// Get the current leaf entry ID.
    pub fn leaf_id(&self) -> Option<&str> { ... }

    /// Get a specific entry by ID.
    pub fn get_entry(&self, id: &str) -> Option<&SessionEntry> { ... }

    /// Walk from leaf to root, collecting the active branch.
    pub fn get_branch(&self, leaf_id: &str) -> Vec<&SessionEntry> { ... }

    /// Get all entries (for tree visualization).
    pub fn get_entries(&self) -> &[SessionEntry] { ... }

    /// List all sessions with metadata.
    pub fn list_sessions(sessions_dir: &Path) -> Result<Vec<SessionInfo>> { ... }

    /// Get the session header.
    pub fn header(&self) -> &SessionHeader { ... }

    /// Build the context (messages) from the current branch.
    pub fn build_context(&self) -> SessionContext { ... }
}
```

**Resume semantics for v1.0:** `open_session()` sets `leaf_id` to the most recently appended entry in the file. `--resume <session_id>` resumes that leaf. Explicit branch selection by leaf ID can land after v1.0 without changing the file format.

### Append-Only Writes

Entries are appended to the file immediately, never rewritten:

```rust
impl SessionManager {
    pub fn add_entries(&mut self, entries: Vec<SessionEntry>) -> Result<()> {
        for entry in &entries {
            // Validate parent_id exists (if not null)
            if let Some(ref parent_id) = entry.base().parent_id {
                if !self.id_set.contains(parent_id) {
                    return Err(anyhow!("Parent ID {} not found", parent_id));
                }
            }

            // Serialize and append
            let line = serde_json::to_string(entry)?;
            use std::io::Write;
            writeln!(self.file_handle, "{}", line)?;
            self.file_handle.flush()?;

            // Update in-memory index
            let idx = self.entries.len();
            self.by_id.insert(entry.base().id.clone(), idx);
            self.id_set.insert(entry.base().id.clone());
            self.entries.push(entry.clone());

            // Update leaf
            self.leaf_id = Some(entry.base().id.clone());
        }
        Ok(())
    }
}
```

### Tree Traversal

The branch is reconstructed by walking `parent_id` pointers from leaf to root:

```rust
impl SessionManager {
    pub fn get_branch(&self, leaf_id: &str) -> Vec<&SessionEntry> {
        let mut branch = Vec::new();
        let mut current_id = Some(leaf_id.to_string());

        while let Some(ref id) = current_id {
            if let Some(&idx) = self.by_id.get(id) {
                let entry = &self.entries[idx];
                branch.push(entry);
                current_id = entry.base().parent_id.clone();
            } else {
                break;
            }
        }

        branch.reverse(); // Root to leaf order
        branch
    }
}
```

### Build Context from Branch

```rust
pub struct SessionContextMessage {
    pub entry_id: String,
    pub message: Message,
}

pub struct SessionContext {
    pub messages: Vec<SessionContextMessage>,
    pub thinking_level: Option<ThinkingLevel>,
    pub model: Option<(String, String)>,  // (provider, model_id)
}

impl SessionManager {
    pub fn build_context(&self) -> SessionContext {
        let leaf = match &self.leaf_id {
            Some(id) => id.clone(),
            None => return SessionContext::empty(),
        };

        let branch = self.get_branch(&leaf);
        let mut messages = Vec::new();
        let mut thinking_level = None;
        let mut model = None;
        let mut compaction_boundary: Option<&str> = None;

        // Find the latest compaction entry in the branch
        for entry in branch.iter().rev() {
            if let SessionEntry::Compaction { first_kept_entry_id, summary, .. } = entry {
                compaction_boundary = Some(first_kept_entry_id.as_str());
                messages.push(SessionContextMessage {
                    entry_id: entry.base().id.clone(),
                    message: Message::User(UserMessage {
                        content: vec![ContentBlock::Text {
                            text: format!("[Previous conversation summary]\n\n{}", summary),
                        }],
                        timestamp: 0,
                    }),
                });
                break;
            }
        }

        // Collect messages from the compaction boundary (or beginning) to leaf
        let mut collecting = compaction_boundary.is_none();
        for entry in &branch {
            if !collecting {
                if entry.base().id == compaction_boundary.unwrap() {
                    collecting = true;
                }
                continue;
            }

            match entry {
                SessionEntry::Message { base, message } => {
                    messages.push(SessionContextMessage {
                        entry_id: base.id.clone(),
                        message: message.clone(),
                    });
                }
                SessionEntry::ThinkingChange { level, .. } => {
                    thinking_level = Some(*level);
                }
                SessionEntry::ModelChange { provider, model: model_id, .. } => {
                    model = Some((provider.clone(), model_id.clone()));
                }
                _ => {}
            }
        }

        SessionContext { messages, thinking_level, model }
    }
}
```

Returning `entry_id` alongside each message avoids brittle pointer or timestamp matching later when compaction needs to identify `first_kept_entry_id`.

### Session Listing

```rust
pub fn list_sessions(sessions_dir: &Path) -> Result<Vec<SessionInfo>> {
    let mut sessions = Vec::new();

    for entry in std::fs::read_dir(sessions_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map_or(true, |e| e != "jsonl") { continue; }

        // Read only the first few lines for metadata
        let content = std::fs::read_to_string(&path)?;
        let mut lines = content.lines();

        if let Some(header_line) = lines.next() {
            if let Ok(header) = serde_json::from_str::<SessionHeader>(header_line) {
                // Count messages and get first user message
                let mut message_count = 0u32;
                let mut first_message = String::new();
                for line in lines {
                    if let Ok(entry) = serde_json::from_str::<SessionEntry>(line) {
                        if let SessionEntry::Message { ref message, .. } = entry {
                            message_count += 1;
                            if first_message.is_empty() {
                                if let Message::User(um) = message {
                                    first_message = um.content.iter()
                                        .filter_map(|c| match c {
                                            ContentBlock::Text { text } => Some(text.as_str()),
                                            _ => None,
                                        })
                                        .collect::<Vec<_>>()
                                        .join(" ");
                                }
                            }
                        }
                    }
                }

                let metadata = entry.metadata()?;
                sessions.push(SessionInfo {
                    path: path.clone(),
                    id: header.id,
                    cwd: header.cwd,
                    created: metadata.created()?.into(),
                    modified: metadata.modified()?.into(),
                    message_count,
                    first_message,
                });
            }
        }
    }

    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(sessions)
}
```

### Tests

1. **New session:** Create, add entries, verify file content.
2. **Open session:** Write file, open, verify entries loaded.
3. **Tree structure:** Add entries with parent_id chain, verify `get_branch()`.
4. **Fork:** Fork from mid-branch, add entries on new branch, verify both branches.
5. **Build context:** Verify messages are reconstructed correctly.
6. **Build context with compaction:** Compaction entry replaces old messages.
7. **List sessions:** Create multiple sessions, verify listing.
8. **Malformed entry resilience:** Verify parser skips bad lines.

### Acceptance Criteria

- Sessions persist to `~/.anie/sessions/*.jsonl`.
- Entries are append-only (never rewrite).
- Tree traversal reconstructs branches correctly.
- Context building handles compaction entries.
- Session listing returns sorted results.

---

## Sub-phase 4.3: Session Integration with the Interactive Controller

**Duration:** Days 5–6

### Wire Session into Interactive Startup

```rust
pub async fn start_interactive(cli_args: CliArgs) -> Result<()> {
    // ... (existing setup from Phase 3) ...

    // Session setup
    let sessions_dir = dirs::home_dir().unwrap().join(".anie/sessions");
    std::fs::create_dir_all(&sessions_dir)?;

    let session = if let Some(ref session_id) = cli_args.resume {
        // Resume existing session
        let path = sessions_dir.join(format!("{}.jsonl", session_id));
        SessionManager::open_session(&path)?
    } else {
        SessionManager::new_session(&sessions_dir, &cwd)?
    };

    // Build initial context from session
    let session_ctx = session.build_context();
    let mut context = session_ctx.messages;

    // Apply session model/thinking overrides
    if let Some((provider, model_id)) = session_ctx.model {
        // Override model from session
    }
    if let Some(level) = session_ctx.thinking_level {
        // Override thinking from session
    }

    // ... (pass session + restored context into the interactive controller) ...
}
```

### Persist Run Results to Session

Do **not** persist session state from TUI render events. The interactive controller owns the canonical context and writes prompts/results explicitly:

```rust
async fn handle_submit_prompt(&mut self, text: String) -> Result<()> {
    let user_msg = Message::User(UserMessage {
        content: vec![ContentBlock::Text { text }],
        timestamp: now_millis(),
    });

    self.session.append_message(&user_msg)?; // persist immediately for crash resilience

    let run_result = self.agent.run(
        vec![user_msg],
        self.context.iter().map(|m| m.message.clone()).collect(),
        self.event_tx.clone(),
        self.cancel.child_token(),
    ).await;

    self.session.append_messages(&run_result.generated_messages)?;
    self.context = self.session.build_context().messages;
    Ok(())
}
```

This keeps persistence and replay tied to the controller's source of truth instead of assuming that every render event maps 1:1 to a session entry.

### Model/Thinking Change Persistence

When the user changes the model or thinking level via slash command:

```rust
fn handle_model_command(&mut self, arg: Option<&str>) {
    if let Some(model) = new_model {
        // Persist the change
        let entry = SessionEntry::ModelChange {
            base: EntryBase {
                id: self.session.generate_id(),
                parent_id: self.session.leaf_id().map(|s| s.to_string()),
                timestamp: now_iso8601(),
            },
            model: model.id.clone(),
            provider: model.provider.clone(),
        };
        self.session.add_entries(vec![entry]).ok();
    }
}
```

### Acceptance Criteria

- User prompts are persisted immediately and generated assistant/tool-result messages are persisted from `AgentRunResult`.
- `--resume <id>` loads a previous session and resumes the most recently appended leaf.
- Model and thinking changes are persisted and restored.

---

## Sub-phase 4.4: Context Compaction

**Duration:** Days 6–10

This is the most algorithmically complex part of the project. Follow pi's algorithm closely — it's well-proven.

### Token Estimation

```rust
/// Estimate token count for a message.
/// Heuristic: chars / 4 for text, 1200 for images.
pub fn estimate_tokens(message: &Message) -> u64 {
    match message {
        Message::User(um) | Message::Assistant(AssistantMessage { content, .. })
        | Message::ToolResult(ToolResultMessage { content, .. }) => {
            content_tokens(match message {
                Message::User(um) => &um.content,
                Message::Assistant(am) => &am.content,
                Message::ToolResult(tr) => &tr.content,
                _ => unreachable!(),
            })
        }
        Message::Custom(_) => 100, // Estimate
    }
}

fn content_tokens(blocks: &[ContentBlock]) -> u64 {
    blocks.iter().map(|b| match b {
        ContentBlock::Text { text } => (text.len() as u64) / 4,
        ContentBlock::Image { .. } => 1200,
        ContentBlock::Thinking { thinking } => (thinking.len() as u64) / 4,
        ContentBlock::ToolCall(tc) => {
            let args_len = serde_json::to_string(&tc.arguments)
                .map(|s| s.len())
                .unwrap_or(0);
            (tc.name.len() as u64 + args_len as u64) / 4
        }
    }).sum()
}
```

### Compaction Algorithm

```rust
pub struct CompactionConfig {
    pub context_window: u64,
    pub reserve_tokens: u64,       // default 16384
    pub keep_recent_tokens: u64,   // default 20000
}

pub struct CompactionResult {
    pub summary: String,
    pub tokens_before: u64,
    pub first_kept_entry_id: String,
    pub messages_discarded: usize,
}

impl SessionManager {
    /// Check if compaction is needed and perform it if so.
    pub async fn auto_compact(
        &mut self,
        config: &CompactionConfig,
        model: &Model,
        request_options_resolver: &dyn RequestOptionsResolver,
        provider_registry: &ProviderRegistry,
    ) -> Result<Option<CompactionResult>> {
        let context = self.build_context();
        let total_tokens: u64 = context.messages.iter()
            .map(|m| estimate_tokens(&m.message))
            .sum();

        // Check threshold
        if total_tokens <= config.context_window - config.reserve_tokens {
            return Ok(None); // No compaction needed
        }

        tracing::info!(
            "Context tokens ({}) exceed threshold ({}). Compacting...",
            total_tokens,
            config.context_window - config.reserve_tokens,
        );

        // Find the cut point
        let (discard, keep, cut_entry_id) = self.find_cut_point(
            &context.messages,
            config.keep_recent_tokens,
        )?;

        // Summarize the discarded portion
        let summary = self.summarize_messages(
            &discard,
            model,
            request_options_resolver,
            provider_registry,
        ).await?;

        // Persist the compaction entry
        let compaction_entry = SessionEntry::Compaction {
            base: EntryBase {
                id: self.generate_id(),
                parent_id: self.leaf_id().map(|s| s.to_string()),
                timestamp: now_iso8601(),
            },
            summary: summary.clone(),
            tokens_before: total_tokens,
            first_kept_entry_id: cut_entry_id.clone(),
        };
        self.add_entries(vec![compaction_entry])?;

        Ok(Some(CompactionResult {
            summary,
            tokens_before: total_tokens,
            first_kept_entry_id: cut_entry_id,
            messages_discarded: discard.len(),
        }))
    }
}
```

### Finding the Cut Point

Walk backwards from the newest message, accumulating token estimates. Cut at `keep_recent_tokens`:

```rust
fn find_cut_point(
    &self,
    messages: &[SessionContextMessage],
    keep_recent_tokens: u64,
) -> Result<(Vec<SessionContextMessage>, Vec<SessionContextMessage>, String)> {
    let mut accumulated = 0u64;
    let mut cut_index = messages.len();

    for (i, msg) in messages.iter().enumerate().rev() {
        accumulated += estimate_tokens(&msg.message);
        if accumulated >= keep_recent_tokens {
            cut_index = i + 1; // Keep from this index onward
            break;
        }
    }

    // Adjust cut to avoid splitting a turn (assistant + tool results)
    while cut_index < messages.len() {
        match &messages[cut_index].message {
            Message::ToolResult(_) => cut_index += 1,
            _ => break,
        }
    }

    if cut_index == 0 || cut_index >= messages.len() {
        return Err(anyhow!("Cannot compact: not enough messages to discard"));
    }

    let discard = messages[..cut_index].to_vec();
    let keep = messages[cut_index..].to_vec();
    let first_kept_entry_id = keep[0].entry_id.clone();

    Ok((discard, keep, first_kept_entry_id))
}
```

Because `build_context()` now preserves `entry_id`, compaction never has to guess which session entry corresponds to the first kept message.

### Summarization Prompt

Follow pi's structured summary format:

```rust
fn build_compaction_prompt(
    messages_to_summarize: &[Message],
    existing_summary: Option<&str>,
) -> String {
    let mut prompt = String::new();

    if let Some(prev_summary) = existing_summary {
        prompt.push_str("Below is an existing conversation summary followed by new messages. \
            Update the summary to incorporate the new information. \
            Merge rather than replace — preserve important details from the existing summary.\n\n");
        prompt.push_str("## Existing Summary\n\n");
        prompt.push_str(prev_summary);
        prompt.push_str("\n\n## New Messages to Incorporate\n\n");
    } else {
        prompt.push_str("Summarize the following conversation for context continuity. \
            The summary will be used to maintain context in a coding assistant session.\n\n");
        prompt.push_str("## Messages\n\n");
    }

    for msg in messages_to_summarize {
        match msg {
            Message::User(um) => {
                prompt.push_str("User: ");
                for block in &um.content {
                    if let ContentBlock::Text { text } = block {
                        prompt.push_str(text);
                    }
                }
                prompt.push_str("\n\n");
            }
            Message::Assistant(am) => {
                prompt.push_str("Assistant: ");
                for block in &am.content {
                    match block {
                        ContentBlock::Text { text } => prompt.push_str(text),
                        ContentBlock::ToolCall(tc) => {
                            prompt.push_str(&format!("[Called tool: {}]", tc.name));
                        }
                        _ => {}
                    }
                }
                prompt.push_str("\n\n");
            }
            Message::ToolResult(tr) => {
                prompt.push_str(&format!("Tool result ({}): ", tr.tool_name));
                for block in &tr.content {
                    if let ContentBlock::Text { text } = block {
                        // Truncate long tool results for the summary
                        if text.len() > 500 {
                            prompt.push_str(&text[..500]);
                            prompt.push_str("...[truncated]");
                        } else {
                            prompt.push_str(text);
                        }
                    }
                }
                prompt.push_str("\n\n");
            }
            _ => {}
        }
    }

    prompt.push_str("\n\nProvide a structured summary with these sections:\n\
        1. **Goal**: What the user is trying to accomplish\n\
        2. **Progress**: What has been done so far (completed tasks, key decisions)\n\
        3. **Key Decisions**: Important choices made and their rationale\n\
        4. **Files Modified**: List of files that were read or modified\n\
        5. **Next Steps**: What remains to be done, if apparent\n\
        6. **Critical Context**: Any constraints, preferences, or important details to preserve\n\n\
        Keep the summary concise but comprehensive. Focus on information needed to continue the work.");

    prompt
}
```

### Calling the LLM for Summarization

```rust
async fn summarize_messages(
    &self,
    messages: &[SessionContextMessage],
    model: &Model,
    request_options_resolver: &dyn RequestOptionsResolver,
    provider_registry: &ProviderRegistry,
) -> Result<String> {
    let existing_summary = self.get_latest_compaction_summary();
    let prompt = build_compaction_prompt(
        &messages.iter().map(|m| m.message.clone()).collect::<Vec<_>>(),
        existing_summary.as_deref(),
    );

    let request = request_options_resolver
        .resolve(model, &messages.iter().map(|m| m.message.clone()).collect::<Vec<_>>())
        .await?;

    let provider = provider_registry.get(&model.api)
        .ok_or_else(|| anyhow!("No provider for {:?}", model.api))?;

    let mut resolved_model = model.clone();
    if let Some(base_url) = request.base_url_override {
        resolved_model.base_url = base_url;
    }

    let llm_context = LlmContext {
        system_prompt: "You are a conversation summarizer. Produce concise, structured summaries.".into(),
        messages: vec![LlmMessage {
            role: "user".into(),
            content: serde_json::Value::String(prompt),
        }],
        tools: vec![],
    };

    let options = StreamOptions {
        api_key: request.api_key,
        temperature: Some(0.3),
        max_tokens: Some(4096),
        thinking: ThinkingLevel::Off,
        headers: request.headers,
    };

    let stream = provider.stream(&resolved_model, llm_context, options)?;
    tokio::pin!(stream);

    let mut summary = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(ProviderEvent::TextDelta(text)) => summary.push_str(&text),
            Ok(ProviderEvent::Done(_)) => break,
            Err(error) => return Err(anyhow!("Summarization failed: {}", error)),
            _ => {}
        }
    }

    Ok(summary)
}
```

### Auto-Compaction Trigger

Integrate into the agent orchestration (in `anie-tui` or `anie-cli`):

```rust
// Before each agent loop run, check compaction
async fn run_with_compaction(
    &mut self,
    prompts: Vec<Message>,
    context: &mut Vec<Message>,
    event_tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
) -> AgentRunResult {
    // Check if compaction is needed
    let total_tokens: u64 = context.iter().map(estimate_tokens).sum();
    let threshold = self.config.model.context_window - self.compaction_config.reserve_tokens;

    if total_tokens > threshold {
        match self.session.auto_compact(
            &self.compaction_config,
            &self.config.model,
            self.request_options_resolver.as_ref(),
            &self.provider_registry,
        ).await {
            Ok(Some(result)) => {
                tracing::info!(
                    "Compacted: {} messages discarded, {} tokens before",
                    result.messages_discarded,
                    result.tokens_before,
                );
                let session_ctx = self.session.build_context();
                *context = session_ctx.messages
                    .into_iter()
                    .map(|m| m.message)
                    .collect();
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("Compaction failed: {}", e);
                // Continue without compaction
            }
        }
    }

    self.agent.run(prompts, context.clone(), event_tx, cancel).await
}
```

### Context Overflow Recovery

If the LLM returns a context overflow error, compact and retry:

```rust
// In the agent loop / controller error handling
match &provider_error {
    ProviderError::ContextOverflow(_) => {
        tracing::warn!("Context overflow detected. Forcing compaction and retrying.");
        let aggressive_config = CompactionConfig {
            keep_recent_tokens: self.compaction_config.keep_recent_tokens / 2,
            ..self.compaction_config.clone()
        };
        if let Ok(Some(_)) = self.session.auto_compact(
            &aggressive_config,
            &self.config.model,
            self.request_options_resolver.as_ref(),
            &self.provider_registry,
        ).await {
            let session_ctx = self.session.build_context();
            *context = session_ctx.messages
                .into_iter()
                .map(|m| m.message)
                .collect();
            return self.run(prompts, context.clone(), event_tx, cancel).await;
        }
    }
    _ => {}
}
```

### Compaction Display in TUI

Show compaction events in the output pane:

```rust
AgentEvent::CompactionStart => {
    self.output_pane.add_system_message("⟳ Compacting context...");
}
AgentEvent::CompactionEnd { result } => {
    self.output_pane.add_system_message(&format!(
        "✓ Compacted: {} messages summarized, {} → {} tokens",
        result.messages_discarded,
        result.tokens_before,
        estimate_current_tokens(&self.context),
    ));
}
```

**Note:** `CompactionStart` and `CompactionEnd` are not yet in the `AgentEvent` enum. Add them:

```rust
pub enum AgentEvent {
    // ... existing variants ...
    CompactionStart,
    CompactionEnd { summary: String, tokens_before: u64, tokens_after: u64 },
}
```

### Tests

1. **Token estimation:** Verify text and image estimates.
2. **Cut point finding:** Messages split at correct boundary, no orphaned tool results.
3. **Summary generation (mock):** Use mock provider to test the summarization flow.
4. **Compaction persistence:** Compaction entry in JSONL, context rebuilt correctly.
5. **Iterative compaction:** Two compactions, verify summary merging.
6. **Overflow recovery:** Simulate context overflow, verify retry after compaction.

### Acceptance Criteria

- Auto-compaction triggers when context exceeds threshold.
- Summary captures goal, progress, decisions, files, next steps.
- Context rebuilt correctly from compaction entry + recent messages.
- Iterative compaction merges with existing summary.
- TUI shows compaction status.

---

## Sub-phase 4.5: Session Forking and `/session` Commands

**Duration:** Days 8–10

### Fork Operation

The storage layer still supports branch semantics within one JSONL file via `parentId`, but the **interactive `/fork` UX for v1.0 should create a new child session file** seeded with the current active branch. This avoids requiring branch-selection UI before continuing work.

```rust
impl SessionManager {
    pub fn fork_to_child_session(&self, sessions_dir: &Path) -> Result<SessionManager> {
        let mut child = SessionManager::new_session_with_parent(
            sessions_dir,
            Path::new(&self.header.cwd),
            Some(self.header.id.clone()),
        )?;

        if let Some(leaf_id) = self.leaf_id() {
            let branch_entries = self.get_branch(leaf_id)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            child.add_entries(branch_entries)?;
        }

        Ok(child)
    }
}
```

This preserves the active conversation context while giving the new fork its own session ID and file. The child session header stores `parent_session` so ancestry is still visible later.

For v1.0, keep resume semantics intentionally simple: `--resume <session_id>` reopens one session file and resumes its most recently appended leaf. Explicit branch-tip selection inside a file can still land later without changing the storage format.

### `/session` Slash Commands

```rust
fn handle_slash_command(&mut self, command: &str) {
    match cmd {
        "/session" => match arg {
            None => self.show_current_session(),
            Some("list") => self.show_session_list(),
            Some(id) => self.resume_session(id),
        },
        "/fork" => self.fork_session(),
        "/compact" => self.force_compact(),
        // ... existing commands ...
    }
}

fn show_session_list(&mut self) {
    let sessions_dir = dirs::home_dir().unwrap().join(".anie/sessions");
    match SessionManager::list_sessions(&sessions_dir) {
        Ok(sessions) => {
            let mut output = String::from("Sessions:\n");
            for (i, s) in sessions.iter().take(20).enumerate() {
                output.push_str(&format!(
                    "  {} {} — {} ({} messages)\n",
                    if i == 0 { "→" } else { " " },
                    s.id,
                    if s.first_message.len() > 60 {
                        format!("{}...", &s.first_message[..57])
                    } else {
                        s.first_message.clone()
                    },
                    s.message_count,
                ));
            }
            self.output_pane.add_system_message(&output);
        }
        Err(e) => {
            self.output_pane.add_system_message(&format!("Error listing sessions: {}", e));
        }
    }
}
```

### `/compact` Command

```rust
fn force_compact(&mut self) {
    self.output_pane.add_system_message("⟳ Forcing compaction...");
    // Spawn compaction on background task
    let session = self.session.clone(); // Arc<Mutex<SessionManager>>
    tokio::spawn(async move {
        // ... run compaction ...
    });
}
```

### Acceptance Criteria

- `/session list` shows recent sessions.
- `--resume <id>` loads a session and resumes the most recently appended leaf.
- `/fork` creates a new child session from the current point and switches to it.
- `/compact` forces compaction.

---

## Phase 4 Milestones Checklist

| # | Milestone | Verified By |
|---|---|---|
| 1 | JSONL format serializes/deserializes all entry types | Unit tests |
| 2 | SessionManager creates, appends, reads sessions | Unit tests |
| 3 | Tree traversal (get_branch) works with forks | Unit tests |
| 4 | Context building handles compaction entries | Unit tests |
| 5 | Auto-compaction triggers at threshold | Integration test |
| 6 | Summarization produces structured output | Mock provider test |
| 7 | Sessions persist across TUI restarts | Manual test |
| 8 | `--resume` loads previous session | Manual test |
| 9 | `/session list` shows sessions | Manual test |
| 10 | `/compact` forces compaction | Manual test |
