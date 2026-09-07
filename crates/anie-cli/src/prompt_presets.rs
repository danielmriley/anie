//! Model-specific system-prompt presets for local coding models.
//!
//! Selected by [`anie_config::ResolvedModelProfile::prompt_template`].
//! Hosted models with no template keep the historical default base.

use anie_agent::{
    collect_observed_evidence, render_evidence_brief, BeforeModelPolicy, BeforeModelRequest,
    BeforeModelResponse, EVIDENCE_FINAL_ANSWER_STANCE,
};
use anie_config::{PromptTemplateId, ResolvedModelProfile, ToolCallFormat};
use anie_protocol::{ContentBlock, Message, UserMessage};
use async_trait::async_trait;

/// Local-coder stance shared by every shipped preset.
const LOCAL_CODER_STANCE: &str = "\
You do not know this repository until you inspect it.
Search before making claims about code locations.
Read files before editing them.
Make one focused change at a time.
Run focused validation after code changes.
Do not claim tests pass unless you ran them.";

const QWEN_NOTE: &str = "\
Prefer a single XML tool call per step:
<tool_call>
{\"name\":\"<tool>\",\"arguments\":{}}
</tool_call>
Do not wrap tool JSON in commentary.";

const DEEPSEEK_NOTE: &str = "\
Be terse. One action per step. Prefer a single XML or JSON tool call.
Do not plan more than the next inspection or edit.";

const LLAMA_NOTE: &str = "\
Llama coding variant: follow the local-coder stance. One tool call per step.";

const MISTRAL_NOTE: &str = "\
Mistral/Codestral variant: follow the local-coder stance. One tool call per step.";

const GEMMA_NOTE: &str = "\
Gemma coding variant: follow the local-coder stance. State the next action, then emit one tool call.";

/// Replace the generic "expert coding assistant" base when a
/// local preset is selected. Tool list is still appended by
/// `build_system_prompt`.
#[must_use]
pub fn local_coder_base(profile: &ResolvedModelProfile, tool_list: &str) -> Option<String> {
    let template = profile.prompt_template?;
    let family = match template {
        PromptTemplateId::GenericLocalCoder => "",
        PromptTemplateId::QwenCoder => QWEN_NOTE,
        PromptTemplateId::DeepseekCoder => DEEPSEEK_NOTE,
        PromptTemplateId::LlamaCoder => LLAMA_NOTE,
        PromptTemplateId::MistralCoder => MISTRAL_NOTE,
        PromptTemplateId::GemmaCoder => GEMMA_NOTE,
    };
    let format_note = match profile.tool_call_format {
        ToolCallFormat::Native => "",
        ToolCallFormat::XmlJsonBlock => {
            "Emit tool calls as a <tool_call> JSON block when native tool calling is unavailable.\n"
        }
        ToolCallFormat::JsonFence => {
            "Emit tool calls as an ```anie-tool JSON fence when native tool calling is unavailable.\n"
        }
    };
    let strengths = render_strengths(profile);
    let mut body = String::from(
        "You are a local coding assistant. You help by reading files, \
         running commands, editing code, and writing new files.\n\n",
    );
    body.push_str(LOCAL_CODER_STANCE);
    body.push('\n');
    if !family.is_empty() {
        body.push('\n');
        body.push_str(family);
        body.push('\n');
    }
    if !format_note.is_empty() {
        body.push('\n');
        body.push_str(format_note);
    }
    if !strengths.is_empty() {
        body.push('\n');
        body.push_str(&strengths);
        body.push('\n');
    }
    if profile.max_tool_calls_per_step == Some(1) {
        body.push_str("\nCall at most one tool per step.\n");
    }
    if !tool_list.is_empty() {
        body.push_str("\nAvailable tools:\n");
        body.push_str(tool_list);
        body.push_str(
            "\n\nGuidelines:\n\
             - Use bash for file operations like ls, grep, find\n\
             - Use read to examine files (use offset + limit for large files)\n\
             - Use edit for precise changes\n\
             - Use write only for new files or complete rewrites\n\
             - Use web_search + web_read for live-world questions when those tools are available\n\
             - Be concise in your responses",
        );
    }
    body.push_str("\n\n");
    body.push_str(EVIDENCE_FINAL_ANSWER_STANCE);
    Some(body)
}

fn render_strengths(profile: &ResolvedModelProfile) -> String {
    if profile.good_at.is_empty() && profile.weak_at.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    if !profile.good_at.is_empty() {
        out.push_str("Known strengths: ");
        out.push_str(&profile.good_at.join(", "));
        out.push('.');
    }
    if !profile.weak_at.is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str("Known weaknesses: ");
        out.push_str(&profile.weak_at.join(", "));
        out.push('.');
        out.push_str(" Compensate with tools; do not bluff through them.");
    }
    out
}

const EVIDENCE_MARKER: &str = "Observed results (cite only these";

/// Injects the harness-observed Done/Validation/Not-run brief
/// once tools have produced results. Context-only; not persisted.
pub(crate) struct EvidencePolicy;

#[async_trait]
impl BeforeModelPolicy for EvidencePolicy {
    async fn before_model(&self, request: BeforeModelRequest<'_>) -> BeforeModelResponse {
        if request.context.iter().any(is_evidence_note) {
            return BeforeModelResponse::Continue;
        }
        let evidence = collect_observed_evidence(request.generated_messages);
        if evidence.is_empty() {
            return BeforeModelResponse::Continue;
        }
        let brief = render_evidence_brief(&evidence);
        BeforeModelResponse::AppendMessages(vec![Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: brief }],
            timestamp: 0,
        })])
    }
}

fn is_evidence_note(message: &Message) -> bool {
    match message {
        Message::User(user) => user.content.iter().any(|block| match block {
            ContentBlock::Text { text } => text.contains(EVIDENCE_MARKER),
            _ => false,
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_config::{PromptTemplateId, ResolvedModelProfile, ToolCallFormat};

    fn qwen_profile() -> ResolvedModelProfile {
        ResolvedModelProfile {
            prompt_template: Some(PromptTemplateId::QwenCoder),
            tool_call_format: ToolCallFormat::XmlJsonBlock,
            preferred_temperature: Some(0.1),
            max_tool_calls_per_step: Some(1),
            tool_call_repair: true,
            good_at: vec!["rust".into()],
            weak_at: vec!["long_horizon_planning".into()],
            max_parse_repairs: 2,
            execute_ambiguous_calls: false,
        }
    }

    #[test]
    fn qwen_preset_includes_stance_and_evidence_template() {
        let base = local_coder_base(&qwen_profile(), "- read: read a file").expect("preset");
        assert!(base.contains("You do not know this repository until you inspect it"));
        assert!(base.contains("<tool_call>"));
        assert!(base.contains("Done:"));
        assert!(base.contains("Validation:"));
        assert!(base.contains("Not run:"));
        assert!(base.contains("Do not claim tests passed unless"));
        assert!(base.contains("Known strengths: rust"));
        assert!(base.contains("Call at most one tool per step"));
        assert!(base.contains("- read: read a file"));
    }

    #[test]
    fn hosted_profile_does_not_select_a_local_preset() {
        assert!(local_coder_base(&ResolvedModelProfile::hosted_default(), "- read: x").is_none());
    }

    #[test]
    fn llama_stub_reuses_generic_stance() {
        let mut profile = qwen_profile();
        profile.prompt_template = Some(PromptTemplateId::LlamaCoder);
        let base = local_coder_base(&profile, "").expect("stub");
        assert!(base.contains("Llama coding variant"));
        assert!(base.contains("You do not know this repository until you inspect it"));
    }
}
