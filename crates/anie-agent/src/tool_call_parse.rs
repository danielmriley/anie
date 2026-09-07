//! Embedded tool-call parse for local models that emit XML/JSON
//! in text instead of (or in addition to) native `toolCall` blocks.
//!
//! Native `ContentBlock::ToolCall` blocks always win. Text parse
//! runs only when the configured format is not [`EmbeddedToolCallFormat::NativeOnly`].

use anie_protocol::{AssistantMessage, ContentBlock, ToolCall};

/// How the agent looks for tool calls on an assistant turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmbeddedToolCallFormat {
    /// Provider-native tool-call blocks only. Hosted default.
    #[default]
    NativeOnly,
    /// `<tool_call>…</tool_call>` plus JSON / `anie-tool` fences.
    XmlJsonBlock,
    /// Markdown fences only (`anie-tool` or a JSON object with name).
    JsonFence,
}

/// Outcome of resolving the tool calls for one assistant turn.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedToolCalls {
    Execute(Vec<ToolCall>),
    NeedsRepair { reason: String, excerpt: String },
    None,
}

/// Parse embedded calls from assistant text.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallParse {
    None,
    Calls(Vec<ToolCall>),
    Malformed { reason: String, excerpt: String },
    Ambiguous { reason: String, excerpt: String },
}

/// Resolve native + embedded tool calls for one assistant message.
#[must_use]
pub fn resolve_assistant_tool_calls(
    assistant: &AssistantMessage,
    format: EmbeddedToolCallFormat,
    max_tool_calls_per_step: Option<u32>,
    execute_ambiguous_calls: bool,
) -> ResolvedToolCalls {
    let native = native_tool_calls(assistant);
    if !native.is_empty() {
        return ResolvedToolCalls::Execute(cap_calls(native, max_tool_calls_per_step));
    }
    if format == EmbeddedToolCallFormat::NativeOnly {
        return ResolvedToolCalls::None;
    }

    let text = join_text(assistant);
    let parsed = parse_embedded_tool_calls(&text, format);
    match parsed {
        ToolCallParse::None => ResolvedToolCalls::None,
        ToolCallParse::Calls(calls) => {
            ResolvedToolCalls::Execute(cap_calls(calls, max_tool_calls_per_step))
        }
        ToolCallParse::Malformed { reason, excerpt } => {
            ResolvedToolCalls::NeedsRepair { reason, excerpt }
        }
        ToolCallParse::Ambiguous { reason, excerpt } => {
            let _ = execute_ambiguous_calls;
            ResolvedToolCalls::NeedsRepair { reason, excerpt }
        }
    }
}

/// Build the in-context repair prompt. Asks for exactly one
/// corrected tool call and no other text.
#[must_use]
pub fn parse_repair_prompt(reason: &str, excerpt: &str) -> String {
    format!(
        "Your tool call was invalid:\n- {reason}\n\n\
         Offending text:\n{excerpt}\n\n\
         Return exactly one corrected tool call and no other text.\n\
         Use this format:\n\
         <tool_call>\n\
         {{\"name\":\"<tool>\",\"arguments\":{{}}}}\n\
         </tool_call>"
    )
}

fn native_tool_calls(assistant: &AssistantMessage) -> Vec<ToolCall> {
    assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall(tool_call) => Some(tool_call.clone()),
            _ => None,
        })
        .collect()
}

fn join_text(assistant: &AssistantMessage) -> String {
    let mut out = String::new();
    for block in &assistant.content {
        if let ContentBlock::Text { text } = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

fn cap_calls(mut calls: Vec<ToolCall>, max: Option<u32>) -> Vec<ToolCall> {
    if let Some(max) = max.filter(|n| *n > 0) {
        let keep = usize::try_from(max).unwrap_or(calls.len());
        if calls.len() > keep {
            calls.truncate(keep);
        }
    }
    calls
}

/// Parse XML/JSON-friendly tool calls from assistant text.
#[must_use]
pub fn parse_embedded_tool_calls(text: &str, format: EmbeddedToolCallFormat) -> ToolCallParse {
    if text.trim().is_empty() {
        return ToolCallParse::None;
    }

    let mut found = Vec::new();
    let mut first_error: Option<(String, String)> = None;

    if format == EmbeddedToolCallFormat::XmlJsonBlock {
        collect_xml_blocks(text, &mut found, &mut first_error);
    }
    collect_fenced_blocks(text, format, &mut found, &mut first_error);

    if found.is_empty() {
        if let Some((reason, excerpt)) = first_error {
            return ToolCallParse::Malformed { reason, excerpt };
        }
        return match try_bare_json_object(text.trim()) {
            Ok(Some(call)) => ToolCallParse::Calls(vec![call]),
            Ok(None) => ToolCallParse::None,
            Err((reason, excerpt)) => ToolCallParse::Malformed { reason, excerpt },
        };
    }

    if found.len() > 1 {
        let names: Vec<&str> = found.iter().map(|call| call.name.as_str()).collect();
        let unique = names
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        if unique.len() > 1 && overlapping_regions(text, &found) {
            return ToolCallParse::Ambiguous {
                reason: format!(
                    "multiple overlapping tool-call interpretations: {}",
                    names.join(", ")
                ),
                excerpt: excerpt_of(text, 240),
            };
        }
    }

    ToolCallParse::Calls(found)
}

fn collect_xml_blocks(
    text: &str,
    found: &mut Vec<ToolCall>,
    first_error: &mut Option<(String, String)>,
) {
    let mut rest = text;
    while let Some(start) = rest.find("<tool_call") {
        let after_open = &rest[start + "<tool_call".len()..];
        let Some(tag_end) = after_open.find('>') else {
            record_error(
                first_error,
                "unclosed <tool_call> tag",
                excerpt_of(&rest[start..], 160),
            );
            break;
        };
        let attrs = &after_open[..tag_end];
        let body_start = start + "<tool_call".len() + tag_end + 1;
        let after_body = &rest[body_start..];
        let Some(close) = after_body.find("</tool_call>") else {
            record_error(
                first_error,
                "missing </tool_call> closer",
                excerpt_of(&rest[start..], 160),
            );
            break;
        };
        let body = after_body[..close].trim();
        let name_attr = xml_name_attr(attrs);
        match decode_call_payload(body, name_attr.as_deref(), found.len()) {
            Ok(call) => found.push(call),
            Err(reason) => record_error(first_error, reason, excerpt_of(body, 160)),
        }
        rest = &after_body[close + "</tool_call>".len()..];
    }
}

fn xml_name_attr(attrs: &str) -> Option<String> {
    let trimmed = attrs.trim();
    let prefix = "name=";
    let idx = trimmed.find(prefix)?;
    let value = trimmed[idx + prefix.len()..].trim();
    let quote = value
        .chars()
        .next()
        .filter(|ch| *ch == '"' || *ch == '\'')?;
    let inner = value.get(1..)?;
    let end = inner.find(quote)?;
    Some(inner[..end].to_string())
}

fn collect_fenced_blocks(
    text: &str,
    format: EmbeddedToolCallFormat,
    found: &mut Vec<ToolCall>,
    first_error: &mut Option<(String, String)>,
) {
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let (info, body_start_rel) = match after.find('\n') {
            Some(nl) => (after[..nl].trim().to_ascii_lowercase(), nl + 1),
            None => break,
        };
        let body_src = &after[body_start_rel..];
        let Some(end) = body_src.find("```") else {
            record_error(
                first_error,
                "unclosed markdown fence",
                excerpt_of(&rest[start..], 160),
            );
            break;
        };
        let body = body_src[..end].trim();
        let accept = info == "anie-tool"
            || info == "json"
            || (format == EmbeddedToolCallFormat::JsonFence && info.is_empty());
        if accept {
            match decode_call_payload(body, None, found.len()) {
                Ok(call) => {
                    if !found.iter().any(|existing| {
                        existing.name == call.name && existing.arguments == call.arguments
                    }) {
                        found.push(call);
                    }
                }
                Err(reason) => {
                    if info == "anie-tool" || looks_like_tool_json(body) {
                        record_error(first_error, reason, excerpt_of(body, 160));
                    }
                }
            }
        }
        rest = &body_src[end + 3..];
    }
}

fn try_bare_json_object(text: &str) -> Result<Option<ToolCall>, (String, String)> {
    let trimmed = text.trim();
    if !(trimmed.starts_with('{') && trimmed.ends_with('}')) {
        return Ok(None);
    }
    if !looks_like_tool_json(trimmed) {
        return Ok(None);
    }
    match decode_call_payload(trimmed, None, 0) {
        Ok(call) => Ok(Some(call)),
        Err(reason) => Err((reason.to_string(), excerpt_of(trimmed, 160))),
    }
}

fn looks_like_tool_json(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("\"name\"") && (lower.contains("\"arguments\"") || lower.contains("\"args\""))
}

fn decode_call_payload(
    body: &str,
    name_override: Option<&str>,
    index: usize,
) -> Result<ToolCall, &'static str> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "tool call body is not valid JSON")?;
    let object = value
        .as_object()
        .ok_or("tool call JSON must be an object")?;
    let name = name_override
        .map(str::to_string)
        .or_else(|| {
            object
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .filter(|name| !name.is_empty())
        .ok_or("tool call is missing name")?;
    let arguments = object
        .get("arguments")
        .or_else(|| object.get("args"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let arguments = match arguments {
        serde_json::Value::String(raw) => {
            serde_json::from_str(&raw).map_err(|_| "arguments string is not valid JSON")?
        }
        other => other,
    };
    if !arguments.is_object() && !arguments.is_null() {
        return Err("arguments must be a JSON object");
    }
    Ok(ToolCall {
        id: format!("embedded_{}", index + 1),
        name,
        arguments,
    })
}

fn overlapping_regions(text: &str, calls: &[ToolCall]) -> bool {
    if calls.len() < 2 {
        return false;
    }
    let first = calls[0].name.as_str();
    text.matches(&format!("\"name\":\"{first}\"")).count()
        + text.matches(&format!("\"name\": \"{first}\"")).count()
        > 1
}

fn record_error(slot: &mut Option<(String, String)>, reason: impl Into<String>, excerpt: String) {
    if slot.is_none() {
        *slot = Some((reason.into(), excerpt));
    }
}

fn excerpt_of(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= max {
        return trimmed.to_string();
    }
    let mut end = max;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &trimmed[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_protocol::{AssistantMessage, StopReason, Usage};

    fn assistant(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "mock".into(),
            timestamp: 1,
            reasoning_details: None,
        }
    }

    #[test]
    fn xml_json_block_happy_path_parses_name_and_arguments() {
        let text = r#"
I'll inspect the file.
<tool_call>
{"name":"read","arguments":{"path":"crates/anie-tui/src/app.rs"}}
</tool_call>
"#;
        let parsed = parse_embedded_tool_calls(text, EmbeddedToolCallFormat::XmlJsonBlock);
        match parsed {
            ToolCallParse::Calls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "read");
                assert_eq!(calls[0].arguments["path"], "crates/anie-tui/src/app.rs");
            }
            other => panic!("expected calls, got {other:?}"),
        }
    }

    #[test]
    fn anie_tool_fence_happy_path_parses() {
        let text = "```anie-tool\n{\"name\":\"bash\",\"arguments\":{\"command\":\"rg AgentUiState\"}}\n```";
        let parsed = parse_embedded_tool_calls(text, EmbeddedToolCallFormat::JsonFence);
        match parsed {
            ToolCallParse::Calls(calls) => {
                assert_eq!(calls[0].name, "bash");
                assert_eq!(calls[0].arguments["command"], "rg AgentUiState");
            }
            other => panic!("expected calls, got {other:?}"),
        }
    }

    #[test]
    fn broken_xml_is_malformed_not_executed() {
        let text = "<tool_call>{\"name\":\"read\",\"arguments\":{</tool_call>";
        let parsed = parse_embedded_tool_calls(text, EmbeddedToolCallFormat::XmlJsonBlock);
        match parsed {
            ToolCallParse::Malformed { reason, .. } => {
                assert!(reason.contains("not valid JSON"), "{reason}");
            }
            other => panic!("expected malformed, got {other:?}"),
        }
    }

    #[test]
    fn broken_json_fence_is_malformed() {
        let text = "```anie-tool\n{\"name\":\"read\",\"arguments\":\n```";
        let parsed = parse_embedded_tool_calls(text, EmbeddedToolCallFormat::XmlJsonBlock);
        match parsed {
            ToolCallParse::Malformed { reason, .. } => {
                assert!(reason.contains("not valid JSON"), "{reason}");
            }
            other => panic!("expected malformed, got {other:?}"),
        }
    }

    #[test]
    fn missing_name_is_malformed() {
        let text = "<tool_call>{\"arguments\":{\"path\":\"x\"}}</tool_call>";
        let parsed = parse_embedded_tool_calls(text, EmbeddedToolCallFormat::XmlJsonBlock);
        match parsed {
            ToolCallParse::Malformed { reason, .. } => {
                assert!(reason.contains("missing name"), "{reason}");
            }
            other => panic!("expected malformed, got {other:?}"),
        }
    }

    #[test]
    fn native_blocks_win_over_embedded_text() {
        let mut message = assistant("<tool_call>{\"name\":\"bash\",\"arguments\":{}}</tool_call>");
        message.content.push(ContentBlock::ToolCall(ToolCall {
            id: "native_1".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        }));
        match resolve_assistant_tool_calls(
            &message,
            EmbeddedToolCallFormat::XmlJsonBlock,
            None,
            false,
        ) {
            ResolvedToolCalls::Execute(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "read");
                assert_eq!(calls[0].id, "native_1");
            }
            other => panic!("expected native execute, got {other:?}"),
        }
    }

    #[test]
    fn one_tool_per_step_keeps_only_the_first_well_formed_call() {
        let text = r#"
<tool_call>
{"name":"read","arguments":{"path":"a.rs"}}
</tool_call>
<tool_call>
{"name":"bash","arguments":{"command":"ls"}}
</tool_call>
"#;
        match resolve_assistant_tool_calls(
            &assistant(text),
            EmbeddedToolCallFormat::XmlJsonBlock,
            Some(1),
            false,
        ) {
            ResolvedToolCalls::Execute(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "read");
            }
            other => panic!("expected capped execute, got {other:?}"),
        }
    }

    #[test]
    fn native_only_ignores_embedded_xml() {
        let message = assistant("<tool_call>{\"name\":\"read\",\"arguments\":{}}</tool_call>");
        assert_eq!(
            resolve_assistant_tool_calls(&message, EmbeddedToolCallFormat::NativeOnly, None, false),
            ResolvedToolCalls::None
        );
    }

    #[test]
    fn string_encoded_arguments_are_accepted() {
        let text = r#"<tool_call>{"name":"read","arguments":"{\"path\":\"a.rs\"}"}</tool_call>"#;
        match parse_embedded_tool_calls(text, EmbeddedToolCallFormat::XmlJsonBlock) {
            ToolCallParse::Calls(calls) => {
                assert_eq!(calls[0].arguments["path"], "a.rs");
            }
            other => panic!("expected calls, got {other:?}"),
        }
    }
}
