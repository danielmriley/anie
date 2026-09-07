//! Canonical example calls for the system-prompt tool catalog.
//!
//! Small local models follow tool schemas far more reliably
//! with one concrete invocation in front of them than with the
//! schema alone. This module owns a static map of tool name →
//! example arguments, rendered as a `# Tool-call examples`
//! block appended to the rlm-mode system prompt (hosted-
//! provider prompts are byte-identical to before).
//!
//! anie-specific deviation from the plan sketch (a
//! `ToolDef.example` field): a static map here avoids touching
//! the protocol type and its ~55 construction sites, and makes
//! it structurally impossible for examples to leak into the
//! wire `tools` array. The
//! `example_arguments_validate_against_the_real_tool_schemas`
//! test keeps the map from drifting away from the actual
//! schemas.
//!
//! Plan 01 PR 3 of `docs/local_model_augmentation/`.

use anie_protocol::ToolDef;

use crate::controller::PromptTier;

/// Plan 04 §2a of `docs/local_model_augmentation/`: in the
/// Small tier the example block keeps only the core tools —
/// examples are the highest-value tokens we send a small
/// model, but nine of them outweigh a 16k window.
const SMALL_TIER_EXAMPLE_TOOLS: &[&str] = &["read", "write", "edit", "bash", "grep"];

/// Example arguments for a known tool, as a JSON value so the
/// rendering (and its escaping) is owned by serde rather than
/// hand-written strings.
fn example_for(tool_name: &str) -> Option<serde_json::Value> {
    let example = match tool_name {
        "read" => serde_json::json!({"path": "src/main.rs", "offset": 1, "limit": 80}),
        "edit" => serde_json::json!({
            "path": "src/main.rs",
            "edits": [{"oldText": "fn old()", "newText": "fn new()"}]
        }),
        "write" => serde_json::json!({"path": "notes.md", "content": "# Notes\n"}),
        "bash" => serde_json::json!({"command": "cargo test --workspace", "timeout": 300}),
        "find" => serde_json::json!({"pattern": "**/*.rs", "limit": 100}),
        "ls" => serde_json::json!({"path": "src"}),
        "grep" => serde_json::json!({"pattern": "fn main", "path": "src", "glob": "*.rs"}),
        "apply_patch" => serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-old line\n+new line\n*** End Patch"
        }),
        "todo_write" => serde_json::json!({
            "todos": [{"content": "run the tests", "status": "in_progress"}]
        }),
        _ => return None,
    };
    Some(example)
}

/// Render the example block for the given (already sorted)
/// tool definitions. Tools without a known example are simply
/// omitted; an empty result means no block at all, so callers
/// can append unconditionally. In the Small tier only the
/// [`SMALL_TIER_EXAMPLE_TOOLS`] subset renders (plan 04 §2a).
/// The output depends only on the registered tool names and
/// the tier — static per registry, so the system prompt stays
/// prefix-cache stable across turns.
pub(crate) fn render_tool_examples(definitions: &[ToolDef], tier: PromptTier) -> String {
    let lines: Vec<String> = definitions
        .iter()
        .filter(|def| {
            tier == PromptTier::Full || SMALL_TIER_EXAMPLE_TOOLS.contains(&def.name.as_str())
        })
        .filter_map(|def| {
            example_for(&def.name).map(|example| format!("- {}: {example}", def.name))
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "\n\n# Tool-call examples\n\nOne canonical call per tool — match these argument \
         shapes exactly:\n{}",
        lines.join("\n")
    )
}

/// Tool name → rendered example arguments for the registered
/// definitions, for grounding the repair side-request prompt
/// (`AgentLoopConfig::with_repair_examples`). Built from the
/// same static map as the catalog block so the two can't
/// drift. Plan 01 §9b of `docs/local_model_augmentation/`.
pub(crate) fn repair_examples(
    definitions: &[ToolDef],
) -> std::collections::HashMap<String, String> {
    definitions
        .iter()
        .filter_map(|def| {
            example_for(&def.name).map(|example| (def.name.clone(), example.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_agent::ValidatorState;

    fn def(name: &str) -> ToolDef {
        ToolDef {
            name: name.to_string(),
            description: format!("{name} tool"),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn examples_render_only_for_registered_tools() {
        let rendered =
            render_tool_examples(&[def("bash"), def("not-a-real-tool")], PromptTier::Full);
        assert!(rendered.contains("- bash: "), "{rendered}");
        assert!(!rendered.contains("not-a-real-tool"), "{rendered}");
    }

    #[test]
    fn no_known_tools_renders_empty_string() {
        assert_eq!(
            render_tool_examples(&[def("mystery")], PromptTier::Full),
            ""
        );
        assert_eq!(render_tool_examples(&[], PromptTier::Full), "");
    }

    /// Plan 04 §2a: the Small tier keeps the example block but
    /// drops to the five core tools, capping its prompt weight.
    /// Full tier still renders every known example.
    #[test]
    fn small_tier_examples_cover_only_the_core_tools() {
        let defs: Vec<ToolDef> = [
            "read",
            "write",
            "edit",
            "bash",
            "grep",
            "find",
            "ls",
            "todo_write",
        ]
        .iter()
        .map(|name| def(name))
        .collect();
        let small = render_tool_examples(&defs, PromptTier::Small);
        for core in SMALL_TIER_EXAMPLE_TOOLS {
            assert!(small.contains(&format!("- {core}: ")), "{small}");
        }
        for dropped in ["find", "ls", "todo_write"] {
            assert!(!small.contains(&format!("- {dropped}: ")), "{small}");
        }
        let full = render_tool_examples(&defs, PromptTier::Full);
        assert!(full.contains("- find: "), "{full}");
        assert!(full.contains("- todo_write: "), "{full}");
    }

    #[test]
    fn repair_examples_map_covers_known_tools_and_skips_unknown_ones() {
        let map = repair_examples(&[def("bash"), def("not-a-real-tool")]);
        assert_eq!(map.len(), 1, "{map:?}");
        let bash = map.get("bash").expect("bash example present");
        assert!(bash.contains("\"command\""), "{bash}");
    }

    /// The drift guard: every example in the static map must
    /// validate against the REAL schema of the tool it claims
    /// to exemplify, as registered by the default tool
    /// registry. A schema change that invalidates an example
    /// fails here, not in front of a confused 8B model.
    #[test]
    fn example_arguments_validate_against_the_real_tool_schemas() {
        let registry = crate::bootstrap::build_tool_registry(&std::env::temp_dir(), false);
        let mut validated = 0;
        for definition in registry.definitions() {
            let Some(example) = example_for(&definition.name) else {
                continue;
            };
            match registry.validator(&definition.name) {
                Some(ValidatorState::Ready(validator)) => {
                    let errors: Vec<String> = validator
                        .iter_errors(&example)
                        .map(|error| error.to_string())
                        .collect();
                    assert!(
                        errors.is_empty(),
                        "example for `{}` violates its schema: {}",
                        definition.name,
                        errors.join("; ")
                    );
                    validated += 1;
                }
                other => panic!(
                    "tool `{}` has no usable validator: {}",
                    definition.name,
                    match other {
                        Some(ValidatorState::Invalid(message)) => message.clone(),
                        _ => "missing".to_string(),
                    }
                ),
            }
        }
        assert!(
            validated >= 6,
            "expected the default registry to exercise most examples, got {validated}"
        );
    }
}
