//! `skill` tool — load a skill body into context.
//! PR 2 of `docs/skills_2026-05-02/`.
//!
//! The model invokes `skill(name="cpp_rule_of_five")` and gets
//! back the skill's body wrapped in `<system-reminder
//! source="skill:NAME">` tags. The same channel the per-turn
//! ledger uses (Plan 06 Phase D) — the model already treats
//! this content as injected guidance, not identity-shaping.
//!
//! Loaded skill bodies are normal user-role messages in the
//! run history. Under context-virtualization pressure they
//! evict like anything else; the embedding reranker can page
//! them back in if relevance scores high.

#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use anie_agent::{Tool, ToolError, ToolExecutionContext};
use anie_protocol::{ContentBlock, ToolDef, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::skills::SkillRegistry;

/// Active skills are tracked in a shared set so PR 4 (TUI
/// visibility) can render the active set in the status bar.
/// Wrapped in `RwLock` so reads (status renders) don't block
/// other readers, and the brief writes during tool execution
/// don't poison the whole controller on panic.
pub(crate) type ActiveSkills = Arc<RwLock<HashSet<String>>>;

/// `skill` tool — load a skill body into context.
pub struct SkillTool {
    registry: Arc<SkillRegistry>,
    active_skills: ActiveSkills,
}

impl SkillTool {
    pub fn new(registry: Arc<SkillRegistry>, active_skills: ActiveSkills) -> Self {
        Self {
            registry,
            active_skills,
        }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "skill".into(),
            description: SKILL_TOOL_DESCRIPTION.into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the skill to load. Must match an entry from the catalog at the top of the system prompt.",
                    }
                },
                "required": ["name"],
                "additionalProperties": false,
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
        let name = args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::ExecutionFailed("missing or non-string `name`".into()))?
            .to_string();

        let Some(skill) = self.registry.get(&name) else {
            // Unknown skill — return is_error with the list of
            // available names. PR 1's failed-tool-result wrapper
            // will prepend the standard re-verify directive
            // ("try a different URL or query — do not assume the
            // original target succeeded"); the listing is what
            // gets the model to retry with a real name.
            let available: Vec<&str> = self
                .registry
                .iter()
                .filter(|s| !s.disable_model_invocation)
                .map(|s| s.name.as_str())
                .collect();
            let body = if available.is_empty() {
                format!("Skill `{name}` not found. No skills are currently registered.")
            } else {
                format!(
                    "Skill `{name}` not found. Available skills:\n{}",
                    available
                        .iter()
                        .map(|n| format!("- {n}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            };
            return Err(ToolError::ExecutionFailed(body));
        };

        // Track activation. Lock contention here is negligible
        // — write briefly, release before any await would happen
        // (this method has no awaits past the lookup). Poison
        // recovery: if a prior write panicked, take the inner
        // state and continue rather than fail the load.
        let already_active = match self.active_skills.write() {
            Ok(mut guard) => !guard.insert(skill.name.clone()),
            Err(poisoned) => !poisoned.into_inner().insert(skill.name.clone()),
        };
        if already_active {
            // Repeat-load smoke signal — model previously loaded
            // this skill but is asking again, suggesting the
            // body slipped from active context (eviction) or
            // the model isn't using the guidance. Debug-level
            // because we don't want to spam users; PR 4 may
            // surface this in the TUI.
            tracing::debug!(
                skill = skill.name.as_str(),
                "skill re-loaded — body may have evicted, or guidance not landing"
            );
        } else {
            tracing::info!(
                skill = skill.name.as_str(),
                source = skill.source.label(),
                body_bytes = skill.body.len(),
                "skill loaded"
            );
        }

        // Wrap the body in the system-reminder framing the
        // harness uses elsewhere (per-turn ledger, etc.). The
        // model treats this as injected guidance — same channel,
        // same expected behavior.
        let wrapped = format!(
            "<system-reminder source=\"skill:{name}\">\n{body}\n</system-reminder>",
            name = skill.name,
            body = skill.body.trim_end(),
        );

        Ok(ToolResult {
            content: vec![ContentBlock::Text { text: wrapped }],
            details: json!({
                "skill_name": skill.name,
                "source": skill.source.label(),
            }),
        })
    }
}

/// Description surfaced in the tool catalog the model sees.
/// Tight on purpose — the agent already sees the full skill
/// catalog (name + per-skill description) in the system
/// prompt, so this text just needs to explain the mechanism.
const SKILL_TOOL_DESCRIPTION: &str = "Load a skill's body into the conversation. Skills are pre-written guidance for specific situations — see the `Available skills` section near the top of the system prompt for the catalog. The body comes back wrapped as a `<system-reminder>`, similar to other harness-injected guidance: read it carefully and let it shape your next action. Loading is idempotent within a run (re-loading is allowed if the body has fallen out of active context).";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{SkillRegistry, SkillSource};
    use anie_agent::ToolExecutionContext;
    use std::fs;
    use tempfile::tempdir;
    use tokio::runtime::Runtime;

    fn registry_with_skill(name: &str, description: &str, body: &str) -> Arc<SkillRegistry> {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("skills");
        fs::create_dir_all(&root).expect("create root");
        let manifest = format!(
            "---\nname: {name}\ndescription: {description}\n---\n{body}\n"
        );
        fs::write(root.join(format!("{name}.md")), manifest).expect("write");
        let mut reg = SkillRegistry::empty();
        reg.absorb_root_for_test(&root, SkillSource::Bundled);
        // Tempdir kept alive via the registry's stored paths is
        // moot — we read the body eagerly. Drop is fine here.
        drop(dir);
        Arc::new(reg)
    }

    fn empty_active_skills() -> ActiveSkills {
        Arc::new(RwLock::new(HashSet::new()))
    }

    fn ctx() -> ToolExecutionContext {
        ToolExecutionContext::default()
    }

    #[test]
    fn skill_tool_definition_has_name_param() {
        let tool = SkillTool::new(Arc::new(SkillRegistry::empty()), empty_active_skills());
        let def = tool.definition();
        assert_eq!(def.name, "skill");
        let required = def
            .parameters
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array");
        assert!(required.iter().any(|v| v.as_str() == Some("name")));
    }

    #[test]
    fn skill_tool_loads_existing_skill_with_system_reminder_wrap() {
        let registry = registry_with_skill(
            "cpp-rule-of-five",
            "When implementing a class with raw new/delete, define all five.",
            "## Body\n\nGuidance text.",
        );
        let active = empty_active_skills();
        let tool = SkillTool::new(registry, Arc::clone(&active));

        let rt = Runtime::new().expect("rt");
        let result = rt
            .block_on(async {
                tool.execute(
                    "call_1",
                    json!({"name": "cpp-rule-of-five"}),
                    CancellationToken::new(),
                    None,
                    &ctx(),
                )
                .await
            })
            .expect("ok");

        let text = match &result.content[0] {
            ContentBlock::Text { text } => text,
            other => panic!("expected text block, got {other:?}"),
        };
        assert!(
            text.starts_with("<system-reminder source=\"skill:cpp-rule-of-five\">"),
            "wrong wrap: {text}"
        );
        assert!(text.ends_with("</system-reminder>"), "wrong close: {text}");
        assert!(text.contains("Guidance text"), "body missing: {text}");

        let active_set = active.read().expect("read active");
        assert!(active_set.contains("cpp-rule-of-five"));
    }

    #[test]
    fn skill_tool_returns_is_error_for_unknown_skill_with_available_list() {
        let registry = registry_with_skill("real-skill", "Exists.", "body");
        let tool = SkillTool::new(registry, empty_active_skills());

        let rt = Runtime::new().expect("rt");
        let err = rt
            .block_on(async {
                tool.execute(
                    "call_1",
                    json!({"name": "missing-skill"}),
                    CancellationToken::new(),
                    None,
                    &ctx(),
                )
                .await
            })
            .expect_err("should error");

        let message = match err {
            ToolError::ExecutionFailed(m) => m,
            other => panic!("wrong error variant: {other:?}"),
        };
        assert!(message.contains("missing-skill"), "{message}");
        assert!(message.contains("Available skills"), "{message}");
        assert!(message.contains("- real-skill"), "{message}");
    }

    #[test]
    fn skill_tool_unknown_skill_no_skills_message() {
        let tool = SkillTool::new(Arc::new(SkillRegistry::empty()), empty_active_skills());
        let rt = Runtime::new().expect("rt");
        let err = rt
            .block_on(async {
                tool.execute(
                    "call_1",
                    json!({"name": "anything"}),
                    CancellationToken::new(),
                    None,
                    &ctx(),
                )
                .await
            })
            .expect_err("should error");
        let message = match err {
            ToolError::ExecutionFailed(m) => m,
            other => panic!("wrong error variant: {other:?}"),
        };
        assert!(
            message.contains("No skills are currently registered"),
            "{message}"
        );
    }

    #[test]
    fn skill_tool_repeated_load_returns_body_again() {
        // Models may re-load if the body got evicted; the
        // tool returns the body each time (with a debug log
        // that PR 4 could surface).
        let registry = registry_with_skill("test", "Test.", "BODY_TEXT");
        let active = empty_active_skills();
        let tool = SkillTool::new(registry, Arc::clone(&active));
        let rt = Runtime::new().expect("rt");
        for _ in 0..3 {
            let result = rt
                .block_on(async {
                    tool.execute(
                        "call_1",
                        json!({"name": "test"}),
                        CancellationToken::new(),
                        None,
                        &ctx(),
                    )
                    .await
                })
                .expect("ok");
            let text = match &result.content[0] {
                ContentBlock::Text { text } => text,
                other => panic!("expected text block, got {other:?}"),
            };
            assert!(text.contains("BODY_TEXT"));
        }
    }

    #[test]
    fn skill_tool_excludes_disable_model_invocation_from_unknown_listing() {
        // disable_model_invocation skills are loadable by name
        // (slash command pathway, PR 4) but should NOT appear
        // in the unknown-skill error listing — the model
        // shouldn't be told they exist.
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("skills");
        fs::create_dir_all(&root).expect("mkdir");
        fs::write(
            root.join("public.md"),
            "---\nname: public\ndescription: Visible.\n---\nbody\n",
        )
        .expect("write public");
        fs::write(
            root.join("internal.md"),
            "---\nname: internal\ndescription: Hidden.\ndisable_model_invocation: true\n---\nbody\n",
        )
        .expect("write internal");

        let mut registry = SkillRegistry::empty();
        registry.absorb_root_for_test(&root, SkillSource::Bundled);
        let tool = SkillTool::new(Arc::new(registry), empty_active_skills());

        let rt = Runtime::new().expect("rt");
        let err = rt
            .block_on(async {
                tool.execute(
                    "call_1",
                    json!({"name": "missing"}),
                    CancellationToken::new(),
                    None,
                    &ctx(),
                )
                .await
            })
            .expect_err("should error");
        let ToolError::ExecutionFailed(message) = err else {
            panic!("wrong variant");
        };
        assert!(message.contains("- public"), "{message}");
        assert!(!message.contains("- internal"), "{message}");
    }

    #[test]
    fn skill_tool_missing_name_arg_errors() {
        let tool = SkillTool::new(Arc::new(SkillRegistry::empty()), empty_active_skills());
        let rt = Runtime::new().expect("rt");
        let err = rt
            .block_on(async {
                tool.execute(
                    "call_1",
                    json!({}),
                    CancellationToken::new(),
                    None,
                    &ctx(),
                )
                .await
            })
            .expect_err("should error");
        match err {
            ToolError::ExecutionFailed(m) => assert!(m.contains("name"), "{m}"),
            _ => panic!("wrong variant"),
        }
    }
}
