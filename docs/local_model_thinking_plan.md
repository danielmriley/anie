# Local Model Thinking / Reasoning — Design Reference

**Status:** design reference. Implementation sequencing lives in `docs/IMPLEMENTATION_ORDER_V_1_0_1.md` and `docs/phased_plan_v1-0-1/`.

This document captures the **design rationale** for local-model reasoning support: why `supports_reasoning: bool` is insufficient, what the capability model looks like, how profile resolution works, and what architectural constraints must be preserved.

It is not the implementation plan.

---

## Updated ecosystem assumption (April 2026)

- **native OpenAI-compatible reasoning controls are now common enough to be first-class**, especially on newer Ollama, vLLM, and LM Studio paths
- **tagged reasoning output is still extremely common**, often as `<think>...</think>` or `<thinking>...</thinking>`
- **native control and tagged output often coexist**

The right mental model is:
- reasoning is not a boolean
- local backends expose a capability ladder
- request control and output shape must be modeled separately

---

## Design principles

### 1. Keep `ThinkingLevel` as the user-facing API

The user chooses only `off`, `low`, `medium`, `high`. The complexity belongs in provider/model capability resolution, not in the UI.

### 2. Treat local reasoning as a capability matrix, not a boolean

`supports_reasoning: bool` is too coarse. We need to distinguish:
- **how reasoning is requested**
- **how reasoning appears in the stream**

### 3. Control mode and output mode are orthogonal

Common real-world combinations include:
- native control + tagged output
- native control + native separated reasoning output
- prompt-with-tags + tagged output
- prompt-only + no separated reasoning output

### 4. Provider layer owns local reasoning behavior

Prompt augmentation, native-field fallback, tag parsing, and separated-reasoning parsing live in `anie-providers-builtin`. The controller holds only `ThinkingLevel` and passes it in `StreamOptions`.

### 5. Explicit config must beat heuristics

Resolution order:
1. explicit model config override
2. built-in hosted model metadata
3. detected local-server profile
4. local model-family heuristic/profile
5. safe fallback

### 6. Failure must be non-fatal

If a local server rejects native reasoning fields, the provider retries without them.

### 7. Prefer native when known, fallback when uncertain

Native OpenAI-compatible reasoning is the preferred path for many current local reasoners. But universal coverage still comes from prompt steering + tagged reasoning parsing + safe fallback.

---

## Proposed capability model

```rust
pub enum ReasoningControlMode {
    None,
    NativeOpenAiReasoning,
    PromptOnly,
    PromptWithTags,
}

pub enum ReasoningOutputMode {
    None,
    NativeDeltas,
    TaggedText,
}

pub struct ReasoningTags {
    pub open: String,
    pub close: String,
}

pub struct ReasoningCapabilities {
    pub control: ReasoningControlMode,
    pub output: ReasoningOutputMode,
    pub tags: Option<ReasoningTags>,
}
```

Interpretation:
- `NativeOpenAiReasoning` — backend accepts `reasoning_effort` and related compatible fields
- `NativeDeltas` — backend exposes separated reasoning output through native response fields
- `TaggedText` — reasoning is embedded in assistant text but can be separated via tags

Internal model metadata only. No new UI commands.

---

## Suggested default profiles

| Model/server category | Default control mode | Default output mode |
|---|---|---|
| Anthropic hosted | existing vendor-native path | `NativeDeltas` |
| Hosted OpenAI reasoning models | `NativeOpenAiReasoning` | provider-specific |
| Local backend with native controls + separated output | `NativeOpenAiReasoning` | `NativeDeltas` |
| Local backend with native controls + tagged output | `NativeOpenAiReasoning` | `TaggedText` |
| Local model family known for think tags | `PromptWithTags` | `TaggedText` |
| Unknown local OpenAI-compatible model | `PromptOnly` | `None` |

---

## Thinking-level semantics for local models

| Level | Native-control interpretation | Prompt-based interpretation |
|---|---|---|
| `off` | omit reasoning controls | answer directly |
| `low` | small reasoning effort | brief planning |
| `medium` | balanced reasoning effort | moderate planning |
| `high` | largest reasoning effort | deliberate planning |

---

## Request-shaping modes

### Mode A: `NativeOpenAiReasoning`
Send `reasoning_effort = low|medium|high`. Retry once without native fields on unsupported-field errors. Cache negative results per `(base_url, model_id)`.

### Mode B: `PromptOnly`
Inject provider-owned reasoning instruction into the system prompt. No tagged output expected.

### Mode C: `PromptWithTags`
Inject reasoning instruction requesting explicit tags. Parse tags from assistant text into `ThinkingDelta`. Keep non-tagged content as `TextDelta`.

---

## Tagged-text parsing policy

- recognize built-in aliases: `<think>`, `<thinking>`, plus configured tags
- handle tags split across chunk boundaries
- degrade to visible text on malformed sequences
- keep tool-call parsing isolated

---

## Token-budget implications

- do not reuse Anthropic's `budget_tokens` model for local backends
- local reasoning consumes output tokens and may increase compaction pressure
- conservative defaults first; per-model overrides later

---

## Configuration shape

```toml
[[providers.ollama.models]]
id = "qwen3:32b"
name = "Qwen3 32B"
context_window = 32768
max_tokens = 16384
supports_reasoning = true
reasoning_control = "native_openai_reasoning"
reasoning_output = "tagged_text"
reasoning_tag_open = "<think>"
reasoning_tag_close = "</think>"
```

Old configs without new fields continue to load unchanged.

---

## Controller, session, and TUI impact

No conceptual changes required above the provider layer:
- controller continues to store and pass `ThinkingLevel`
- sessions already serialize `ContentBlock::Thinking`
- TUI already renders thinking blocks

---

## Risks

1. System prompt prerequisite — prompt-based local thinking requires the OpenAI-compatible path to honor `LlmContext.system_prompt`
2. Control/output mismatch — native control must not accidentally disable tag parsing
3. Heuristic drift — config overrides must remain first-class
4. Tag collision — generic tags may appear in code output
5. Native-field compatibility — partial implementations require careful fallback
6. Verbose transcript growth — reasoning increases compaction pressure

---

## Related docs

- `docs/IMPLEMENTATION_ORDER_V_1_0_1.md`
- `docs/phased_plan_v1-0-1/`
- `docs/phase_detail_plans/phase_2_providers.md`
- `docs/runtime_state_integration_plan.md`
- `docs/notes.md`
