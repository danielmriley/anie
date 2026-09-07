//! Repo-map injection policy + `repo_map` drill-down tool.
//! Plan 02 §2b/§2c of `docs/local_model_augmentation/`.
//!
//! The pure builder lives in `anie_tools::repo_map` (that crate
//! already carries the `ignore` + `regex` deps); this module owns
//! the harness wiring: env gating, tier-aware token budget, the
//! session-scoped cache, the [`BeforeModelPolicy`] that injects
//! the map at the first model turn, and the on-demand tool.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anie_agent::{
    BeforeModelPolicy, BeforeModelRequest, BeforeModelResponse, Tool, ToolError,
    ToolExecutionContext,
};
use anie_protocol::{ContentBlock, Message, ToolDef, ToolResult, UserMessage, now_millis};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::controller::PromptTier;
use crate::harness_mode::HarnessMode;

/// Session-scoped map cache shared between the per-run policy and
/// the `repo_map` tool. Built lazily at first use; rebuilt only
/// when the stamp goes stale at a run boundary, never mid-turn
/// (the policy only fires at the first model turn).
pub(crate) type SharedRepoMap = Arc<Mutex<Option<anie_tools::repo_map::RepoMap>>>;

const REPO_MAP_REMINDER_OPEN: &str = "<system-reminder source=\"repo-map\">";
const REPO_MAP_GUIDANCE: &str = "Use this map to go directly to relevant files. Prefer `read` \
     with offset/limit at the line numbers shown over exploratory `find`/`ls` calls.";

/// Plan §2a sets a flat 2000-token default; plan 04's tiers
/// override that here: the Small tier halves the budget because
/// every prompt token visibly displaces working room on ≤32k
/// windows. `ANIE_REPO_MAP_TOKENS` overrides both.
const DEFAULT_REPO_MAP_TOKENS_FULL: u64 = 2_000;
const DEFAULT_REPO_MAP_TOKENS_SMALL: u64 = 1_000;

/// Per-file symbol listing cap for the drill-down tool (plan §2c:
/// "capped per-file at ~200 lines").
const MAX_DRILL_DOWN_LINES: usize = 200;

/// Gating (pattern of `should_wrap_failed_tool_results`): default
/// on in `--harness-mode=rlm`, off elsewhere; `ANIE_REPO_MAP=0`
/// disables, `=1` force-enables in other modes for A/B runs.
pub(crate) fn repo_map_enabled(mode: HarnessMode) -> bool {
    repo_map_enabled_with(mode, std::env::var("ANIE_REPO_MAP").ok().as_deref())
}

fn repo_map_enabled_with(mode: HarnessMode, env: Option<&str>) -> bool {
    match env.map(str::trim) {
        Some("0") => false,
        Some("1") => true,
        _ => mode.installs_rlm_features(),
    }
}

/// Tier-aware token budget for the injected map.
pub(crate) fn repo_map_token_budget(tier: PromptTier) -> u64 {
    if let Some(parsed) = std::env::var("ANIE_REPO_MAP_TOKENS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    {
        return parsed;
    }
    match tier {
        PromptTier::Small => DEFAULT_REPO_MAP_TOKENS_SMALL,
        PromptTier::Full => DEFAULT_REPO_MAP_TOKENS_FULL,
    }
}

/// Get the cached map text, building (or rebuilding, when the
/// stamp went stale) under the lock. The build is a bounded
/// gitignore-aware walk (≤5000 entries) — cheap enough to run
/// inline at the at-most-once-per-run call sites.
fn current_map_text(cwd: &Path, max_tokens: u64, cache: &SharedRepoMap) -> String {
    let mut guard = match cache.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let needs_build = match guard.as_ref() {
        Some(map) => map.is_stale(cwd),
        None => true,
    };
    if needs_build {
        *guard = Some(anie_tools::repo_map::build_repo_map(cwd, max_tokens));
    }
    guard
        .as_ref()
        .map(|map| map.text.clone())
        .unwrap_or_default()
}

fn is_repo_map_reminder(message: &Message) -> bool {
    matches!(message, Message::User(user)
        if user.content.iter().any(|block|
            matches!(block, ContentBlock::Text { text } if text.starts_with(REPO_MAP_REMINDER_OPEN))))
}

/// Injects the repo map once at the first model turn of a run as
/// a synthetic `<system-reminder source="repo-map">` user message
/// (the rlm ledger convention). As an early, byte-stable message
/// it sits in the prompt prefix and costs nothing to re-send
/// under Ollama's prefix cache; the rlm evictor may page it out
/// late in a long run like any other message (no pinning in v1).
pub(crate) struct RepoMapPolicy {
    cwd: PathBuf,
    max_tokens: u64,
    cache: SharedRepoMap,
}

impl RepoMapPolicy {
    pub(crate) fn new(cwd: PathBuf, max_tokens: u64, cache: SharedRepoMap) -> Self {
        Self {
            cwd,
            max_tokens,
            cache,
        }
    }
}

#[async_trait]
impl BeforeModelPolicy for RepoMapPolicy {
    async fn before_model(&self, request: BeforeModelRequest<'_>) -> BeforeModelResponse {
        // Inject whenever the working context lacks the map. The
        // appended message is context-only (never persisted), so
        // each new run's rebuilt context needs it again — gating
        // on "first turn of the session" made the map vanish for
        // every run after the first (2026-06-12 review). Within a
        // run the presence check makes this once-per-run; if rlm
        // eviction drops it late in a long run, re-adding it is
        // the desired behavior, not churn.
        if request.context.iter().any(is_repo_map_reminder) {
            return BeforeModelResponse::Continue;
        }
        let text = current_map_text(&self.cwd, self.max_tokens, &self.cache);
        if text.is_empty() {
            return BeforeModelResponse::Continue;
        }
        BeforeModelResponse::AppendMessages(vec![Message::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: format!(
                    "{REPO_MAP_REMINDER_OPEN}\n{text}\n\n{REPO_MAP_GUIDANCE}\n</system-reminder>"
                ),
            }],
            timestamp: now_millis(),
        })])
    }
}

/// `repo_map` tool — on-demand drill-down past the breadth-capped
/// injected map (plan §2c). No `path`: the current full map
/// (re-orientation after heavy eviction). `path` = file: full
/// symbol list for that file. `path` = directory: tree +
/// per-file symbol counts beneath it.
pub(crate) struct RepoMapTool {
    cwd: PathBuf,
    /// Budget used only when the cache is empty (the policy,
    /// which knows the prompt tier, normally populates it at the
    /// first model turn — before any tool call can land).
    max_tokens: u64,
    cache: SharedRepoMap,
}

impl RepoMapTool {
    pub(crate) fn new(cwd: PathBuf, max_tokens: u64, cache: SharedRepoMap) -> Self {
        Self {
            cwd,
            max_tokens,
            cache,
        }
    }

    fn file_listing(&self, requested: &str, resolved: &Path) -> String {
        let symbols = anie_tools::repo_map::extract_signatures(resolved);
        if symbols.is_empty() {
            return format!(
                "No symbols recognized in `{requested}` (signature extraction covers Rust, \
                 Python, and TS/JS). Use `read` to inspect the file directly."
            );
        }
        let total = symbols.len();
        let mut lines: Vec<String> = symbols
            .into_iter()
            .take(MAX_DRILL_DOWN_LINES)
            .map(|(line, sig)| format!("{requested}:{line}: {sig}"))
            .collect();
        if total > MAX_DRILL_DOWN_LINES {
            lines.push(format!(
                "… {} more symbols truncated; use `read` with offset/limit.",
                total - MAX_DRILL_DOWN_LINES
            ));
        }
        lines.join("\n")
    }
}

#[async_trait]
impl Tool for RepoMapTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "repo_map".into(),
            description: "Look up the repo map. Without arguments, returns the full repo skeleton (file tree + top-level symbols + recent commits). With `path` set to a file, returns that file's full symbol list with line numbers; set to a directory, returns its tree with per-file symbol counts. Prefer this over exploratory find/ls calls.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Optional file or directory path, relative to the working directory. Omit for the full repo map."
                    }
                },
                "required": [],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
        _update_tx: Option<mpsc::Sender<ToolResult>>,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty());
        let text = match path {
            None => current_map_text(&self.cwd, self.max_tokens, &self.cache),
            Some(requested) => {
                let resolved = if Path::new(requested).is_absolute() {
                    PathBuf::from(requested)
                } else {
                    self.cwd.join(requested)
                };
                if resolved.is_file() {
                    self.file_listing(requested, &resolved)
                } else if resolved.is_dir() {
                    let overview = anie_tools::repo_map::dir_overview(&self.cwd, &resolved);
                    let mut lines: Vec<&str> = overview.lines().collect();
                    if lines.len() > MAX_DRILL_DOWN_LINES {
                        lines.truncate(MAX_DRILL_DOWN_LINES);
                        lines.push("… truncated; drill into a subdirectory.");
                    }
                    lines.join("\n")
                } else {
                    return Err(ToolError::ExecutionFailed(format!(
                        "path `{requested}` not found under {}. Pass a file or directory path \
                         relative to the working directory, or omit `path` for the full repo map.",
                        self.cwd.display()
                    )));
                }
            }
        };
        Ok(ToolResult {
            content: vec![ContentBlock::Text { text }],
            details: serde_json::Value::Null,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anie_protocol::Usage;
    use anie_provider::{ApiKind, CostPerMillion, Model, ModelCompat};
    use tempfile::tempdir;

    use super::*;

    fn model() -> Model {
        Model {
            id: "m".into(),
            name: "m".into(),
            provider: "p".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "http://localhost".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    fn user(text: &str) -> Message {
        Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: now_millis(),
        })
    }

    fn assistant(text: &str) -> Message {
        Message::Assistant(anie_protocol::AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            usage: Usage::default(),
            stop_reason: anie_protocol::StopReason::Stop,
            error_message: None,
            provider: "p".into(),
            model: "m".into(),
            timestamp: now_millis(),
            reasoning_details: None,
        })
    }

    async fn fire(policy: &RepoMapPolicy, context: &[Message]) -> BeforeModelResponse {
        let model = model();
        let usage = Usage::default();
        policy
            .before_model(BeforeModelRequest {
                context,
                generated_messages: &[],
                model: &model,
                step_index: 0,
                run_usage: &usage,
            })
            .await
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("main.py"), "def entry():\n    pass\n").expect("write");
        dir
    }

    fn injected_text(response: &BeforeModelResponse) -> String {
        let BeforeModelResponse::AppendMessages(messages) = response else {
            panic!("expected AppendMessages, got {response:?}");
        };
        assert_eq!(messages.len(), 1, "exactly one injected message");
        let Message::User(user) = &messages[0] else {
            panic!("expected a synthetic user message");
        };
        let ContentBlock::Text { text } = &user.content[0] else {
            panic!("expected a text block");
        };
        text.clone()
    }

    #[tokio::test]
    async fn map_is_injected_exactly_once_on_first_model_turn() {
        let dir = fixture();
        let policy = RepoMapPolicy::new(dir.path().to_path_buf(), 2_000, SharedRepoMap::default());

        let first = fire(&policy, &[user("find the entry point")]).await;
        let text = injected_text(&first);
        assert!(text.starts_with(REPO_MAP_REMINDER_OPEN), "{text}");
        assert!(text.contains("main.py"), "{text}");

        // Second model turn: an assistant message exists → noop.
        let second_context = vec![
            user("find the entry point"),
            Message::User(UserMessage {
                content: vec![ContentBlock::Text { text }],
                timestamp: now_millis(),
            }),
            assistant("looking"),
        ];
        assert_eq!(
            fire(&policy, &second_context).await,
            BeforeModelResponse::Continue,
            "map must not be re-injected after the first model turn"
        );
    }

    /// Prefix stability: a later run's policy (fresh instance,
    /// shared session cache) renders byte-identical map text, so
    /// nothing about the injection is request-time-dependent.
    #[tokio::test]
    async fn second_turn_context_contains_the_same_map_bytes() {
        let dir = fixture();
        let cache = SharedRepoMap::default();
        let prompt = [user("orient me")];

        let policy = RepoMapPolicy::new(dir.path().to_path_buf(), 2_000, Arc::clone(&cache));
        let first_bytes = injected_text(&fire(&policy, &prompt).await);

        let rebuilt = RepoMapPolicy::new(dir.path().to_path_buf(), 2_000, cache);
        let second_bytes = injected_text(&fire(&rebuilt, &prompt).await);
        assert_eq!(
            first_bytes, second_bytes,
            "same cache + unchanged files must render identical bytes"
        );
    }

    #[test]
    fn policy_is_noop_outside_rlm_unless_force_enabled() {
        assert!(repo_map_enabled_with(HarnessMode::Rlm, None));
        assert!(!repo_map_enabled_with(HarnessMode::Current, None));
        assert!(!repo_map_enabled_with(HarnessMode::Baseline, None));
        assert!(!repo_map_enabled_with(HarnessMode::Rlm, Some("0")));
        assert!(repo_map_enabled_with(HarnessMode::Current, Some("1")));
        assert!(repo_map_enabled_with(HarnessMode::Rlm, Some("garbage")));
    }

    /// Chained order (map BEFORE the context-virtualization
    /// policy): the map lands in the working set the evictor
    /// budgets against, and the combined response carries both
    /// the map and the virt policy's ledger.
    #[tokio::test]
    async fn map_injection_composes_with_context_virtualization_policy() {
        let dir = fixture();
        let map_policy: Arc<dyn BeforeModelPolicy> = Arc::new(RepoMapPolicy::new(
            dir.path().to_path_buf(),
            2_000,
            SharedRepoMap::default(),
        ));
        let store = Arc::new(tokio::sync::RwLock::new(
            crate::external_context::ExternalContext::from_messages(Vec::new()),
        ));
        let virt: Arc<dyn BeforeModelPolicy> =
            Arc::new(crate::context_virt::ContextVirtualizationPolicy::new(
                8_192,
                6,
                0,
                store,
                Arc::new(Mutex::new(std::collections::HashSet::new())),
            ));
        let chain = anie_agent::ChainedBeforeModelPolicy::new(vec![map_policy, virt]);

        let context = [user("orient me")];
        let model = model();
        let usage = Usage::default();
        let response = chain
            .before_model(BeforeModelRequest {
                context: &context,
                generated_messages: &[],
                model: &model,
                step_index: 0,
                run_usage: &usage,
            })
            .await;
        let BeforeModelResponse::ReplaceMessages(messages) = response else {
            panic!("chain with mutations must replace, got {response:?}");
        };
        assert!(
            messages.iter().any(is_repo_map_reminder),
            "evictor must see (and keep) the injected map"
        );
        assert!(
            messages.iter().any(|m| matches!(m, Message::User(u)
                if u.content.iter().any(|b| matches!(b, ContentBlock::Text { text }
                    if text.contains("external context") || text.contains("Archive:"))))),
            "virt policy must still inject its ledger after the map"
        );
    }

    fn tool_fixture() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn first() {}\npub fn second() {}\n",
        )
        .expect("write");
        dir
    }

    async fn run_tool(tool: &RepoMapTool, args: serde_json::Value) -> Result<String, ToolError> {
        let ctx = ToolExecutionContext::default();
        tool.execute("call_1", args, CancellationToken::new(), None, &ctx)
            .await
            .map(|result| match &result.content[0] {
                ContentBlock::Text { text } => text.clone(),
                other => panic!("expected text, got {other:?}"),
            })
    }

    #[tokio::test]
    async fn repo_map_tool_returns_per_file_symbols_for_a_file_path() {
        let dir = tool_fixture();
        let tool = RepoMapTool::new(dir.path().to_path_buf(), 2_000, SharedRepoMap::default());
        let text = run_tool(&tool, json!({"path": "src/lib.rs"}))
            .await
            .expect("file listing");
        assert!(text.contains("src/lib.rs:1: pub fn first()"), "{text}");
        assert!(text.contains("src/lib.rs:2: pub fn second()"), "{text}");
    }

    #[tokio::test]
    async fn repo_map_tool_unknown_path_errors_with_actionable_message() {
        let dir = tool_fixture();
        let tool = RepoMapTool::new(dir.path().to_path_buf(), 2_000, SharedRepoMap::default());
        let error = run_tool(&tool, json!({"path": "no/such/file.rs"}))
            .await
            .expect_err("unknown path must error");
        let message = error.to_string();
        assert!(message.contains("no/such/file.rs"), "{message}");
        assert!(
            message.contains("omit `path` for the full repo map"),
            "error must tell the model how to recover: {message}"
        );
    }

    #[tokio::test]
    async fn repo_map_tool_without_path_returns_the_full_map() {
        let dir = tool_fixture();
        let tool = RepoMapTool::new(dir.path().to_path_buf(), 2_000, SharedRepoMap::default());
        let text = run_tool(&tool, json!({})).await.expect("full map");
        assert!(text.contains("## Files"), "{text}");
        assert!(text.contains("lib.rs"), "{text}");
    }
}
