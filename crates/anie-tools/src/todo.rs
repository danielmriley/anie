//! Plan/todo tool: a TodoWrite-style surface the model uses to maintain
//! an ordered task list with `pending | in_progress | done` statuses.
//!
//! The durable state ([`TodoList`]) is owned by the controller as an
//! `Arc<Mutex<_>>` and shared with the verifier policy and the status
//! bar — the same shared-state arrangement the recurse tool uses with
//! `ExternalContext`. [`TodoWriteTool`] is the model's writable keyhole
//! into it. Write semantics are full-replace (the model sends the whole
//! list each call), matching Claude Code's TodoWrite contract and
//! sidestepping id-tracking / partial-merge bugs. See
//! `docs/plan_todo_verifier/README.md`.

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_agent::{Tool, ToolError, ToolExecutionContext};
use anie_protocol::{ToolDef, ToolResult};

use crate::shared::text_result;

/// One task in the plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// Status of a single task. Deliberately three flat states — no ids,
/// dependencies, or subtasks (that is task-decomposition territory,
/// explicitly deferred).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

impl TodoStatus {
    /// Transcript marker for a compact rendering.
    fn marker(self) -> &'static str {
        match self {
            TodoStatus::Done => "[x]",
            TodoStatus::InProgress => "[~]",
            TodoStatus::Pending => "[ ]",
        }
    }
}

/// The ordered task list. Run-scoped, in-memory; not persisted (no
/// session-schema change).
#[derive(Debug, Clone, Default)]
pub struct TodoList {
    items: Vec<TodoItem>,
}

impl TodoList {
    /// Replace the whole list (the model always sends the full set).
    pub fn replace(&mut self, items: Vec<TodoItem>) {
        self.items = items;
    }

    #[must_use]
    pub fn items(&self) -> &[TodoItem] {
        &self.items
    }

    /// `(done, total)` for the status bar.
    #[must_use]
    pub fn counts(&self) -> (usize, usize) {
        let done = self
            .items
            .iter()
            .filter(|item| item.status == TodoStatus::Done)
            .count();
        (done, self.items.len())
    }

    /// True when there is at least one item and every item is `done` —
    /// the verifier's "model believes it's finished" trigger.
    #[must_use]
    pub fn all_done(&self) -> bool {
        !self.items.is_empty()
            && self
                .items
                .iter()
                .all(|item| item.status == TodoStatus::Done)
    }

    /// Compact multi-line rendering for a tool result / transcript.
    #[must_use]
    pub fn render(&self) -> String {
        if self.items.is_empty() {
            return "(empty plan)".to_string();
        }
        self.items
            .iter()
            .map(|item| format!("{} {}", item.status.marker(), item.content))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The model's writable surface over a controller-owned [`TodoList`].
pub struct TodoWriteTool {
    list: Arc<Mutex<TodoList>>,
}

impl TodoWriteTool {
    #[must_use]
    pub fn new(list: Arc<Mutex<TodoList>>) -> Self {
        Self { list }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "todo_write".into(),
            description: "Record or update your plan as an ordered task list. Send the COMPLETE list every call (it replaces the previous one). Use it to track multi-step work: mark a task `in_progress` before starting it and `done` only once you have actually verified the result. Keep it concise.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The complete, ordered task list. Replaces any previous list.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "Short description of the task."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "done"],
                                    "description": "pending = not started, in_progress = active now, done = verified complete."
                                }
                            },
                            "required": ["content", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["todos"],
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
        let todos = args.get("todos").ok_or_else(|| {
            ToolError::ExecutionFailed("missing required `todos` array".to_string())
        })?;
        let items: Vec<TodoItem> = serde_json::from_value(todos.clone()).map_err(|error| {
            ToolError::ExecutionFailed(format!(
                "invalid `todos`: {error}. Each item needs `content` (string) and `status` (one of pending|in_progress|done)."
            ))
        })?;
        if items.is_empty() {
            return Err(ToolError::ExecutionFailed(
                "todo list cannot be empty; send at least one task".to_string(),
            ));
        }

        let (rendered, done, total) = {
            let mut guard = self.list.lock().unwrap_or_else(PoisonError::into_inner);
            guard.replace(items);
            let (done, total) = guard.counts();
            (guard.render(), done, total)
        };

        let details = serde_json::json!({ "done": done, "total": total });
        Ok(text_result(
            format!("Plan ({done}/{total} done):\n{rendered}"),
            details,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> (TodoWriteTool, Arc<Mutex<TodoList>>) {
        let list = Arc::new(Mutex::new(TodoList::default()));
        (TodoWriteTool::new(Arc::clone(&list)), list)
    }

    async fn run(tool: &TodoWriteTool, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        tool.execute(
            "call",
            args,
            CancellationToken::new(),
            None,
            &ToolExecutionContext::default(),
        )
        .await
    }

    fn text(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                anie_protocol::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn todo_write_replaces_list_wholesale() {
        let (tool, list) = tool();
        run(
            &tool,
            serde_json::json!({ "todos": [
                { "content": "a", "status": "done" },
                { "content": "b", "status": "pending" }
            ] }),
        )
        .await
        .expect("first write");
        // A second call with a shorter list fully replaces the first.
        run(
            &tool,
            serde_json::json!({ "todos": [ { "content": "c", "status": "in_progress" } ] }),
        )
        .await
        .expect("second write");

        let guard = list.lock().expect("lock");
        assert_eq!(guard.items().len(), 1);
        assert_eq!(guard.items()[0].content, "c");
        assert_eq!(guard.items()[0].status, TodoStatus::InProgress);
    }

    #[tokio::test]
    async fn todo_write_rejects_unknown_status_with_execution_failed() {
        let (tool, _list) = tool();
        let err = run(
            &tool,
            serde_json::json!({ "todos": [ { "content": "x", "status": "blocked" } ] }),
        )
        .await
        .expect_err("unknown status must fail");
        assert!(matches!(err, ToolError::ExecutionFailed(msg) if msg.contains("invalid `todos`")));
    }

    #[tokio::test]
    async fn todo_write_empty_list_is_execution_failed() {
        let (tool, _list) = tool();
        let err = run(&tool, serde_json::json!({ "todos": [] }))
            .await
            .expect_err("empty list must fail");
        assert!(matches!(err, ToolError::ExecutionFailed(msg) if msg.contains("cannot be empty")));
    }

    #[tokio::test]
    async fn todo_write_result_renders_status_markers() {
        let (tool, _list) = tool();
        let result = run(
            &tool,
            serde_json::json!({ "todos": [
                { "content": "did it", "status": "done" },
                { "content": "doing it", "status": "in_progress" },
                { "content": "later", "status": "pending" }
            ] }),
        )
        .await
        .expect("write");
        let body = text(&result);
        assert!(body.contains("[x] did it"), "{body}");
        assert!(body.contains("[~] doing it"), "{body}");
        assert!(body.contains("[ ] later"), "{body}");
    }

    #[tokio::test]
    async fn todo_counts_reports_done_over_total() {
        let (tool, list) = tool();
        run(
            &tool,
            serde_json::json!({ "todos": [
                { "content": "a", "status": "done" },
                { "content": "b", "status": "done" },
                { "content": "c", "status": "pending" }
            ] }),
        )
        .await
        .expect("write");
        let guard = list.lock().expect("lock");
        assert_eq!(guard.counts(), (2, 3));
        assert!(!guard.all_done());
    }
}
