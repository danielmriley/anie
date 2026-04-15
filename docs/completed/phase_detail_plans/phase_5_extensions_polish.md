# Phase 5: EditTool, CLI, and Polish (Weeks 9–10)

**Goal:** Finish the remaining v1.0 product work: `EditTool`, the full CLI entry point, a minimal versioned RPC mode, onboarding, and the remaining slash commands. `WriteTool` should already exist from Phase 1; any extension system or memory features in this phase are explicitly optional / post-v1.0 polish.

---

## Sub-phase 5.1: WriteTool (Buffer / Polish)

**Duration:** Day 1

`WriteTool` should already be complete from Phase 1. Keep this slot only as schedule buffer or to add polish such as atomic temp-file writes, richer metadata, or additional tests if it slipped.

### Parameters

```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
    "content": { "type": "string", "description": "Content to write to the file" }
  },
  "required": ["path", "content"],
  "additionalProperties": false
}
```

### Implementation

```rust
pub struct WriteTool {
    cwd: PathBuf,
    mutation_queue: Arc<FileMutationQueue>,
}

#[async_trait]
impl Tool for WriteTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "write".into(),
            description: "Write content to a file. Creates the file if it doesn't exist, \
                overwrites if it does. Automatically creates parent directories.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write (relative or absolute)"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        _update_tx: Option<mpsc::Sender<ToolResult>>,
    ) -> Result<ToolResult, ToolError> {
        let path = args["path"].as_str()
            .ok_or_else(|| ToolError::ExecutionFailed("Missing 'path' argument".into()))?;
        let content = args["content"].as_str()
            .ok_or_else(|| ToolError::ExecutionFailed("Missing 'content' argument".into()))?;

        let abs_path = self.resolve_path(path);

        self.mutation_queue.with_lock(&abs_path, async {
            if cancel.is_cancelled() {
                return Err(ToolError::Aborted);
            }

            // Create parent directories
            if let Some(parent) = abs_path.parent() {
                tokio::fs::create_dir_all(parent).await
                    .map_err(|e| ToolError::ExecutionFailed(
                        format!("Failed to create directories: {}", e)
                    ))?;
            }

            // Write the file
            tokio::fs::write(&abs_path, content).await
                .map_err(|e| ToolError::ExecutionFailed(
                    format!("Failed to write {}: {}", path, e)
                ))?;

            let lines = content.lines().count();
            let bytes = content.len();

            Ok(ToolResult {
                content: vec![ContentBlock::Text {
                    text: format!("Successfully wrote {} ({} lines, {} bytes)", path, lines, bytes),
                }],
                details: serde_json::json!({
                    "path": path,
                    "lines": lines,
                    "bytes": bytes,
                }),
            })
        }).await
    }
}
```

### Tests

1. Write a new file, verify content.
2. Overwrite existing file.
3. Auto-create parent directories.
4. Cancellation.

---

## Sub-phase 5.2: EditTool

**Duration:** Days 2–4

The most complex tool. Follow pi's implementation closely.

### Parameters

```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path to the file to edit" },
    "edits": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "oldText": { "type": "string", "description": "Exact text to find and replace" },
          "newText": { "type": "string", "description": "Replacement text" }
        },
        "required": ["oldText", "newText"],
        "additionalProperties": false
      },
      "description": "One or more targeted replacements matched against the original file"
    }
  },
  "required": ["path", "edits"],
  "additionalProperties": false
}
```

### Core Edit Algorithm

Port pi's `applyEditsToNormalizedContent` logic.

**Important:** use fuzzy normalization only to *locate* match spans. Apply replacements back onto the original LF-normalized buffer (then restore BOM/line endings afterward). Do **not** write the fuzzy-normalized buffer back to disk, or you risk silently rewriting whitespace and Unicode characters unrelated to the requested edit.

Example structure:

```rust
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

pub fn apply_edits(
    content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<(String, String), EditError> {
    // 1. Normalize line endings to LF
    let normalized = normalize_to_lf(content);

    // 2. Normalize each edit's oldText/newText to LF
    let normalized_edits: Vec<Edit> = edits.iter().map(|e| Edit {
        old_text: normalize_to_lf(&e.old_text),
        new_text: normalize_to_lf(&e.new_text),
    }).collect();

    // 3. Validate no empty oldText
    for (i, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(EditError::EmptyOldText { index: i, path: path.into() });
        }
    }

    // 4. Try exact matching first
    let initial_matches: Vec<_> = normalized_edits.iter()
        .map(|e| fuzzy_find(&normalized, &e.old_text))
        .collect();

    // 5. If any need fuzzy matching, work in normalized space
    let base_content = if initial_matches.iter().any(|m| m.used_fuzzy) {
        normalize_for_fuzzy_match(&normalized)
    } else {
        normalized.clone()
    };

    // 6. Find all matches
    let mut matched_edits = Vec::new();
    for (i, edit) in normalized_edits.iter().enumerate() {
        let result = fuzzy_find(&base_content, &edit.old_text);
        if !result.found {
            return Err(EditError::NotFound {
                index: i,
                total: normalized_edits.len(),
                path: path.into(),
            });
        }

        // Check uniqueness
        let occurrences = count_occurrences(&base_content, &edit.old_text);
        if occurrences > 1 {
            return Err(EditError::Duplicate {
                index: i,
                total: normalized_edits.len(),
                occurrences,
                path: path.into(),
            });
        }

        matched_edits.push(MatchedEdit {
            edit_index: i,
            match_index: result.index,
            match_length: result.match_length,
            new_text: edit.new_text.clone(),
        });
    }

    // 7. Sort by position and check overlaps
    matched_edits.sort_by_key(|e| e.match_index);
    for i in 1..matched_edits.len() {
        let prev = &matched_edits[i - 1];
        let curr = &matched_edits[i];
        if prev.match_index + prev.match_length > curr.match_index {
            return Err(EditError::Overlap {
                first: prev.edit_index,
                second: curr.edit_index,
                path: path.into(),
            });
        }
    }

    // 8. Apply in reverse order (so offsets stay valid)
    let mut new_content = base_content.clone();
    for edit in matched_edits.iter().rev() {
        new_content = format!(
            "{}{}{}",
            &new_content[..edit.match_index],
            edit.new_text,
            &new_content[edit.match_index + edit.match_length..],
        );
    }

    // 9. Check for actual change
    if base_content == new_content {
        return Err(EditError::NoChange { path: path.into() });
    }

    Ok((base_content, new_content))
}
```

### Fuzzy Matching

Port pi's `normalizeForFuzzyMatch`:

```rust
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;

    text.nfkc()
        .collect::<String>()
        // Strip trailing whitespace per line
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        // Smart quotes → ASCII
        .replace(['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'], "'")
        .replace(['\u{201C}', '\u{201D}', '\u{201E}', '\u{201F}'], "\"")
        // Dashes → ASCII hyphen
        .replace(['\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}',
                  '\u{2014}', '\u{2015}', '\u{2212}'], "-")
        // Special spaces → regular space
        .replace(['\u{00A0}', '\u{202F}', '\u{205F}', '\u{3000}'], " ")
}

pub struct FuzzyFindResult {
    pub found: bool,
    pub index: usize,
    pub match_length: usize,
    pub used_fuzzy: bool,
}

pub fn fuzzy_find(content: &str, needle: &str) -> FuzzyFindResult {
    // Try exact first
    if let Some(index) = content.find(needle) {
        return FuzzyFindResult {
            found: true,
            index,
            match_length: needle.len(),
            used_fuzzy: false,
        };
    }

    // Try fuzzy
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_needle = normalize_for_fuzzy_match(needle);

    if let Some(index) = fuzzy_content.find(&fuzzy_needle) {
        return FuzzyFindResult {
            found: true,
            index,
            match_length: fuzzy_needle.len(),
            used_fuzzy: true,
        };
    }

    FuzzyFindResult {
        found: false,
        index: 0,
        match_length: 0,
        used_fuzzy: false,
    }
}
```

### BOM Handling

```rust
pub fn strip_bom(content: &str) -> (&str, &str) {
    if content.starts_with('\u{FEFF}') {
        ("\u{FEFF}", &content[3..]) // BOM is 3 bytes in UTF-8
    } else {
        ("", content)
    }
}
```

### Line Ending Handling

```rust
pub fn detect_line_ending(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}
```

### Diff Generation

Use the `similar` crate:

```rust
use similar::{ChangeTag, TextDiff};

pub fn generate_diff(old: &str, new: &str, context_lines: usize) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut output = Vec::new();

    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let max_line = old_lines.len().max(new_lines.len());
    let width = format!("{}", max_line).len();

    for group in diff.grouped_ops(context_lines) {
        for op in group {
            for change in diff.iter_changes(&op) {
                let line = change.value().trim_end_matches('\n');
                match change.tag() {
                    ChangeTag::Delete => {
                        let line_num = change.old_index().map(|i| i + 1).unwrap_or(0);
                        output.push(format!("-{:>width$} {}", line_num, line));
                    }
                    ChangeTag::Insert => {
                        let line_num = change.new_index().map(|i| i + 1).unwrap_or(0);
                        output.push(format!("+{:>width$} {}", line_num, line));
                    }
                    ChangeTag::Equal => {
                        let line_num = change.old_index().map(|i| i + 1).unwrap_or(0);
                        output.push(format!(" {:>width$} {}", line_num, line));
                    }
                }
            }
        }
    }

    output.join("\n")
}
```

### EditTool Implementation

```rust
pub struct EditTool {
    cwd: PathBuf,
    mutation_queue: Arc<FileMutationQueue>,
}

#[async_trait]
impl Tool for EditTool {
    fn definition(&self) -> ToolDef { ... }

    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        _update_tx: Option<mpsc::Sender<ToolResult>>,
    ) -> Result<ToolResult, ToolError> {
        let path = args["path"].as_str()
            .ok_or_else(|| ToolError::ExecutionFailed("Missing 'path'".into()))?;
        let edits_value = args["edits"].as_array()
            .ok_or_else(|| ToolError::ExecutionFailed("Missing 'edits'".into()))?;

        let edits: Vec<Edit> = edits_value.iter().map(|v| {
            Edit {
                old_text: v["oldText"].as_str().unwrap_or("").to_string(),
                new_text: v["newText"].as_str().unwrap_or("").to_string(),
            }
        }).collect();

        if edits.is_empty() {
            return Err(ToolError::ExecutionFailed("edits must not be empty".into()));
        }

        let abs_path = self.resolve_path(path);

        self.mutation_queue.with_lock(&abs_path, async {
            if cancel.is_cancelled() {
                return Err(ToolError::Aborted);
            }

            // Read file
            let raw = tokio::fs::read_to_string(&abs_path).await
                .map_err(|e| ToolError::ExecutionFailed(format!("File not found: {}", path)))?;

            // Strip BOM, detect line endings
            let (bom, content) = strip_bom(&raw);
            let original_ending = detect_line_ending(content);
            let normalized = normalize_to_lf(content);

            // Apply edits
            let (base, new_content) = apply_edits(&normalized, &edits, path)
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

            // Restore BOM and line endings
            let final_content = format!(
                "{}{}",
                bom,
                restore_line_endings(&new_content, original_ending),
            );

            // Write
            tokio::fs::write(&abs_path, &final_content).await
                .map_err(|e| ToolError::ExecutionFailed(format!("Failed to write: {}", e)))?;

            // Generate diff
            let diff = generate_diff(&base, &new_content, 4);

            Ok(ToolResult {
                content: vec![ContentBlock::Text {
                    text: format!("Successfully replaced {} block(s) in {}", edits.len(), path),
                }],
                details: serde_json::json!({
                    "diff": diff,
                }),
            })
        }).await
    }
}
```

### Legacy Argument Handling

LLMs sometimes send `oldText`/`newText` as top-level fields instead of inside `edits[]`. Handle this gracefully:

```rust
fn normalize_edit_args(args: &mut serde_json::Value) {
    if let (Some(old_text), Some(new_text)) = (
        args.get("oldText").cloned(),
        args.get("newText").cloned(),
    ) {
        // Convert legacy format to array format
        let mut edits = args.get("edits")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        edits.push(serde_json::json!({
            "oldText": old_text,
            "newText": new_text,
        }));

        args["edits"] = serde_json::Value::Array(edits);
        args.as_object_mut().map(|o| {
            o.remove("oldText");
            o.remove("newText");
        });
    }
}
```

### Tests

1. **Single edit:** Replace one block, verify diff.
2. **Multiple edits:** Replace two non-overlapping blocks.
3. **Fuzzy matching:** Trailing whitespace, smart quotes, Unicode dashes.
4. **Not found error:** Old text doesn't exist.
5. **Duplicate error:** Old text appears twice.
6. **Overlap error:** Two edits overlap.
7. **No change error:** Old text equals new text.
8. **BOM preservation:** Edit a file with BOM.
9. **CRLF preservation:** Edit a file with Windows line endings.
10. **Legacy args:** `oldText`/`newText` at top level.
11. **File mutation queue:** Concurrent edits to the same file are serialized.

### Acceptance Criteria

- All 11 test scenarios pass.
- Edit tool integrates with the agent loop and TUI.
- Diff output is displayed in the TUI with color coding.

---

## Sub-phase 5.3: Post-v1.0 — `anie-extensions` — Extension System

**Duration:** Days 5–7

### Extension Trait

```rust
// crates/anie-extensions/src/lib.rs

#[async_trait]
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;

    async fn before_agent_start(
        &self,
        system_prompt: &str,
        context: &[Message],
    ) -> Option<ExtensionResult> {
        None
    }

    async fn on_session_start(&self, session_id: &str) {}

    async fn before_tool_call(
        &self,
        tool_call: &ToolCall,
        args: &serde_json::Value,
    ) -> BeforeToolCallResult {
        BeforeToolCallResult::Allow
    }

    async fn after_tool_call(
        &self,
        tool_call: &ToolCall,
        result: &ToolResult,
        is_error: bool,
    ) -> Option<ToolResultOverride> {
        None
    }
}

pub struct ExtensionResult {
    pub system_prompt: Option<String>,
    pub inject_messages: Vec<Message>,
}

pub struct ToolResultOverride {
    pub content: Option<Vec<ContentBlock>>,
    pub details: Option<serde_json::Value>,
    pub is_error: Option<bool>,
}
```

### ExtensionRunner

```rust
pub struct ExtensionRunner {
    extensions: Vec<Box<dyn Extension>>,
}

impl ExtensionRunner {
    pub fn new() -> Self {
        Self { extensions: Vec::new() }
    }

    pub fn register(&mut self, extension: Box<dyn Extension>) {
        tracing::info!("Registered extension: {}", extension.name());
        self.extensions.push(extension);
    }

    pub async fn before_agent_start(
        &self,
        system_prompt: &str,
        context: &[Message],
    ) -> (String, Vec<Message>) {
        let mut final_prompt = system_prompt.to_string();
        let mut inject = Vec::new();

        for ext in &self.extensions {
            match ext.before_agent_start(&final_prompt, context).await {
                Some(result) => {
                    if let Some(new_prompt) = result.system_prompt {
                        final_prompt = new_prompt;
                    }
                    inject.extend(result.inject_messages);
                }
                None => {}
            }
        }

        (final_prompt, inject)
    }

    pub async fn before_tool_call(
        &self,
        tool_call: &ToolCall,
        args: &serde_json::Value,
    ) -> BeforeToolCallResult {
        for ext in &self.extensions {
            let result = ext.before_tool_call(tool_call, args).await;
            if let BeforeToolCallResult::Block { .. } = &result {
                return result;
            }
        }
        BeforeToolCallResult::Allow
    }

    pub async fn after_tool_call(
        &self,
        tool_call: &ToolCall,
        result: &ToolResult,
        is_error: bool,
    ) -> Option<ToolResultOverride> {
        for ext in &self.extensions {
            if let Some(override_result) = ext.after_tool_call(tool_call, result, is_error).await {
                return Some(override_result);
            }
        }
        None
    }
}
```

### Wire ExtensionRunner into AgentLoop

The `AgentLoopConfig` gains extension hooks:

```rust
pub struct AgentLoopConfig {
    // ... existing fields ...
    pub extension_runner: Option<Arc<ExtensionRunner>>,
}
```

In the agent loop, call extensions at the appropriate points:

```rust
// Before agent start
if let Some(ext) = &self.config.extension_runner {
    let (modified_prompt, injected) = ext.before_agent_start(
        &self.config.system_prompt,
        context,
    ).await;
    // Use modified_prompt for this run
    // Prepend injected messages to context
}

// Before tool call
if let Some(ext) = &self.config.extension_runner {
    match ext.before_tool_call(&tool_call, &args).await {
        BeforeToolCallResult::Block { reason } => {
            // Return error result
        }
        BeforeToolCallResult::Allow => {}
    }
}

// After tool call
if let Some(ext) = &self.config.extension_runner {
    if let Some(override_result) = ext.after_tool_call(&tool_call, &result, is_error).await {
        // Apply override
    }
}
```

### Acceptance Criteria

- Extensions can modify the system prompt.
- Extensions can block tool calls.
- Extensions can override tool results.
- Multiple extensions are called in order.

---

## Sub-phase 5.4: `anie-cli` — Full CLI Entry Point

**Duration:** Days 7–8

### Clap Argument Parsing

```rust
use clap::Parser;

#[derive(Parser)]
#[command(name = "anie", version, about = "A coding agent harness")]
pub struct Cli {
    /// Run in interactive mode (default)
    #[arg(short, long)]
    interactive: bool,

    /// Run in print mode (one-shot, output to stdout)
    #[arg(short, long)]
    print: bool,

    /// Run in RPC mode (JSONL over stdin/stdout)
    #[arg(long)]
    rpc: bool,

    /// Disable tool registration for provider/debugging work
    #[arg(long)]
    no_tools: bool,

    /// Initial prompt (for print and interactive modes)
    #[arg(trailing_var_arg = true)]
    prompt: Vec<String>,

    /// Model ID to use
    #[arg(short, long)]
    model: Option<String>,

    /// Provider name
    #[arg(long)]
    provider: Option<String>,

    /// API key (overrides auth.json and env var)
    #[arg(long)]
    api_key: Option<String>,

    /// Thinking level
    #[arg(long)]
    thinking: Option<ThinkingLevel>,

    /// Resume a previous session by ID
    #[arg(long)]
    resume: Option<String>,

    /// Working directory (default: current directory)
    #[arg(short = 'C', long)]
    cwd: Option<PathBuf>,
}
```

### Mode Dispatch

```rust
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Set up tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "anie=info".into())
        )
        .init();

    // Change CWD if specified
    if let Some(ref cwd) = cli.cwd {
        std::env::set_current_dir(cwd)?;
    }

    if cli.rpc {
        run_rpc_mode(cli).await
    } else if cli.print || !cli.prompt.is_empty() {
        run_print_mode(cli).await
    } else {
        run_interactive_mode(cli).await
    }
}
```

### Print Mode

One-shot execution: send prompt, stream output to stdout, exit.

```rust
async fn run_print_mode(cli: Cli) -> Result<()> {
    let prompt = cli.prompt.join(" ");
    if prompt.is_empty() {
        anyhow::bail!("No prompt provided. Usage: anie 'your prompt here'");
    }

    // Set up agent (similar to interactive mode but no TUI)
    let agent = setup_agent(&cli)?;

    let prompts = vec![Message::User(UserMessage {
        content: vec![ContentBlock::Text { text: prompt }],
        timestamp: now_millis(),
    })];

    let (event_tx, mut event_rx) = mpsc::channel(256);
    let cancel = CancellationToken::new();

    // Handle Ctrl+C
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        cancel_clone.cancel();
    });

    let handle = tokio::spawn(async move {
        agent.run(prompts, Vec::new(), event_tx, cancel).await
    });

    while let Some(event) = event_rx.recv().await {
        match event {
            AgentEvent::MessageDelta { delta: StreamDelta::TextDelta(text) } => {
                print!("{}", text);
                std::io::stdout().flush()?;
            }
            AgentEvent::AgentEnd { .. } => break,
            _ => {}
        }
    }

    println!();
    let _run_result = handle.await?;
    Ok(())
}
```

### RPC Mode

JSONL over stdin/stdout. Keep v1 intentionally small and versioned.

**Startup handshake (written once on stdout):**
```json
{"type":"hello","version":1}
```

**Input commands (v1):**
```json
{"type":"prompt","text":"read src/main.rs"}
{"type":"abort"}
{"type":"get_state"}
{"type":"set_model","model":"gpt-4o","provider":"openai"}
{"type":"set_thinking","level":"high"}
```

**Output events (v1):**
```json
{"type":"agent_start"}
{"type":"text_delta","text":"I'll read that file."}
{"type":"tool_exec_start","tool":"read","args":{"path":"src/main.rs"}}
{"type":"tool_exec_end","tool":"read","is_error":false}
{"type":"agent_end"}
{"type":"error","message":"..."}
```

```rust
async fn run_rpc_mode(cli: Cli) -> Result<()> {
    let agent = setup_agent(&cli)?;
    println!("{}", serde_json::to_string(&RpcEvent::Hello { version: 1 })?);

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut current_cancel: Option<CancellationToken> = None;

    while let Some(line) = lines.next_line().await? {
        let command: RpcCommand = serde_json::from_str(&line)?;
        match command {
            RpcCommand::Prompt { text } => {
                let (event_tx, mut event_rx) = mpsc::channel(256);
                let cancel = CancellationToken::new();
                current_cancel = Some(cancel.clone());

                let prompts = vec![Message::User(UserMessage {
                    content: vec![ContentBlock::Text { text }],
                    timestamp: now_millis(),
                })];

                tokio::spawn({
                    let agent = agent.clone();
                    async move {
                        agent.run(prompts, Vec::new(), event_tx, cancel).await;
                    }
                });

                while let Some(event) = event_rx.recv().await {
                    println!("{}", serde_json::to_string(&RpcEvent::from(event))?);
                }
            }
            RpcCommand::Abort => {
                if let Some(cancel) = &current_cancel {
                    cancel.cancel();
                }
            }
            RpcCommand::GetState => {
                println!("{}", serde_json::to_string(&RpcEvent::State { /* ... */ })?);
            }
            RpcCommand::SetModel { .. } | RpcCommand::SetThinking { .. } => {
                // Update controller state for the next prompt
            }
        }
    }

    Ok(())
}
```

### Acceptance Criteria

- `anie` (no args) starts interactive TUI.
- `anie "prompt text"` runs in print mode.
- `anie --rpc` starts RPC mode.
- `anie --model gpt-4o` overrides the model.
- `anie --resume <id>` resumes a session.

---

## Sub-phase 5.5: Onboarding Flow

**Duration:** Day 8

### First-Run Detection

```rust
fn check_first_run() -> bool {
    let config_path = dirs::home_dir().unwrap().join(".anie/config.toml");
    let auth_path = dirs::home_dir().unwrap().join(".anie/auth.json");
    !config_path.exists() && !auth_path.exists()
}
```

### Inline Onboarding

Since the TUI isn't set up yet during first run, use simple terminal prompts:

```rust
async fn run_onboarding() -> Result<()> {
    println!("Welcome to anie! Let's get you set up.\n");

    // Prefer zero-cost local providers if they are already running
    let local_servers = detect_local_servers().await;
    if let Some(server) = local_servers.first() {
        println!("✓ Detected local model server: {}", server.name);
        create_default_local_config(server)?;
        println!("\nCreated ~/.anie/config.toml using {} as the default provider.", server.name);
        return Ok(());
    }

    // Check for env vars next
    let providers = [
        ("anthropic", "ANTHROPIC_API_KEY"),
        ("openai", "OPENAI_API_KEY"),
        ("google", "GEMINI_API_KEY"),
    ];

    let mut found_provider = None;
    for (name, var) in &providers {
        if std::env::var(var).is_ok() {
            println!("✓ Found {} in environment.", var);
            found_provider = Some(*name);
            break;
        }
    }

    if let Some(provider) = found_provider {
        create_default_config(provider)?;
        println!("\nCreated ~/.anie/config.toml with {} as default provider.", provider);
        return Ok(());
    }

    println!("No API key found. Choose a provider:\n");
    println!("  1. Anthropic");
    println!("  2. OpenAI");
    println!("  3. Google (Gemini)");
    println!("  4. Custom (OpenAI-compatible endpoint)");
    print!("\nSelection [1]: ");
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let choice = input.trim().parse::<u32>().unwrap_or(1);

    let provider = match choice {
        1 => "anthropic",
        2 => "openai",
        3 => "google",
        4 => {
            print!("Base URL: ");
            std::io::stdout().flush()?;
            // ... handle custom
            "custom"
        }
        _ => "anthropic",
    };

    let key = rpassword::prompt_password("\nEnter your API key: ")?;
    save_api_key(provider, &key)?;
    create_default_config(provider)?;

    println!("\n✓ Configuration saved. Starting anie...\n");
    Ok(())
}
```

### Acceptance Criteria

- First run detects missing config and prompts for setup.
- API key is saved to `~/.anie/auth.json`.
- Default config is created at `~/.anie/config.toml`.
- Env vars are detected and used automatically.

---

## Sub-phase 5.6: Remaining Slash Commands

**Duration:** Day 9

Add all remaining slash commands:

| Command | Action |
|---|---|
| `/model [id]` | Show or switch model |
| `/thinking [level]` | Show or change thinking level |
| `/compact` | Force context compaction |
| `/clear` | Clear the output pane |
| `/session list` | List sessions |
| `/session <id>` | Switch to a session |
| `/fork` | Fork into a new child session at the current point |
| `/diff` | Show a diff of all changes made in this session |
| `/tools` | List registered tools |
| `/help` | Show help |
| `/quit` | Exit |

### `/diff` Command

Show all file modifications made during the session:

```rust
fn show_session_diff(&mut self) {
    let branch = self.session.get_branch(self.session.leaf_id().unwrap());
    let mut files_modified = HashSet::new();

    for entry in &branch {
        if let SessionEntry::Message { message: Message::ToolResult(tr), .. } = entry {
            if tr.tool_name == "edit" || tr.tool_name == "write" {
                if let Some(path) = tr.details.get("path").and_then(|v| v.as_str()) {
                    files_modified.insert(path.to_string());
                }
            }
        }
    }

    if files_modified.is_empty() {
        self.output_pane.add_system_message("No files modified in this session.");
    } else {
        self.output_pane.add_system_message(&format!(
            "Files modified: {}",
            files_modified.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
}
```

### Acceptance Criteria

- All slash commands listed above work.
- Unknown commands show an error.

---

## Sub-phase 5.7: Post-v1.0 — Memory Tool (`memory_write`)

**Duration:** Day 10

Optionally implement a simple `memory_write` tool for persistent notes. This is post-v1.0 polish, not part of the minimum shipping CLI.

```rust
pub struct MemoryWriteTool {
    memory_dir: PathBuf, // ~/.anie/memory/
}

#[async_trait]
impl Tool for MemoryWriteTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_write".into(),
            description: "Write a note to persistent memory. Use this to record things \
                worth remembering across sessions.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Note path (e.g. 'corrections/use-top-level-imports')" },
                    "body": { "type": "string", "description": "Note content" },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Categorization tags"
                    }
                },
                "required": ["id", "body"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ...) -> Result<ToolResult, ToolError> {
        let id = args["id"].as_str().unwrap();
        let body = args["body"].as_str().unwrap();
        let tags = args["tags"].as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect::<Vec<_>>())
            .unwrap_or_default();

        // Create directory structure based on id
        let note_path = self.memory_dir.join(format!("{}.md", id));
        if let Some(parent) = note_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Write note with frontmatter
        let frontmatter = format!("---\ntags: [{}]\nupdated: {}\n---\n\n",
            tags.join(", "),
            current_date_ymd()?,
        );
        let content = format!("{}{}", frontmatter, body);
        tokio::fs::write(&note_path, &content).await?;

        Ok(ToolResult {
            content: vec![ContentBlock::Text {
                text: format!("Saved note: {}", id),
            }],
            details: serde_json::json!({}),
        })
    }
}
```

Memory notes are injected into the system prompt on startup by scanning `~/.anie/memory/`.

### Acceptance Criteria

- Notes are saved as markdown files.
- Notes are loaded into the system prompt on subsequent sessions.

---

## Phase 5 Milestones Checklist

| # | Milestone | Verified By |
|---|---|---|
| 1 | WriteTool polish buffer is closed (or Phase 1 work is confirmed complete) | Unit tests / checklist |
| 2 | EditTool handles all edge cases (fuzzy, BOM, CRLF, overlaps) | 11 unit tests |
| 3 | `anie` CLI dispatches to interactive/print/rpc modes | Integration test |
| 4 | Print mode streams output to stdout | Manual test |
| 5 | RPC mode speaks the minimal versioned JSONL protocol | Manual test |
| 6 | Onboarding prefers local providers and hides API-key input | Manual test |
| 7 | All slash commands work | Manual test |
| 8 | Edit diffs render correctly in TUI | Manual test |
| 9 | Session resume works end-to-end | Manual test |
| 10 | Extension / memory features are clearly marked post-v1.0 if not shipped | Checklist |
