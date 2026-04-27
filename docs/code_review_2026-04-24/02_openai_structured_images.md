# 02 — OpenAI structured image serialization

## Rationale

OpenAI-compatible models can be marked image-capable, and the protocol
already supports `ContentBlock::Image`. Anthropic conversion emits real
structured image blocks, but OpenAI conversion currently flattens every
user message to a string:

- `crates/anie-providers-builtin/src/openai/mod.rs` converts user
  messages with `join_text_content`.
- `crates/anie-providers-builtin/src/openai/convert.rs` turns
  `ContentBlock::Image` into a text placeholder:
  `[image:<media-type>;base64,<data>]`.

pi's OpenAI chat-completions provider serializes mixed user content as
OpenAI content parts: text blocks become `{ "type": "text" }`, images
become `{ "type": "image_url", "image_url": { "url":
"data:<mime>;base64,<data>" } }`.

This is a concrete provider compatibility gap. A model may claim image
support while never receiving image bytes.

## Design

OpenAI-compatible chat-completions user messages should choose one of
two shapes:

- If user content contains only text/thinking text that should be sent
  as text, preserve the existing string shape for compatibility.
- If user content contains at least one image, serialize the content as
  an array of OpenAI content parts:
  - text: `{ "type": "text", "text": "..." }`
  - image: `{ "type": "image_url", "image_url": { "url":
    "data:<media_type>;base64,<data>" } }`

Assistant replay can remain string-based unless a provider requires
otherwise. Tool results should remain the existing tool-message shape.

If a model or backend is known not to support image arrays despite
advertising image support, that should be represented as a compat flag
or catalog correction, not by flattening images globally.

## Files to touch

- `crates/anie-providers-builtin/src/openai/convert.rs`
  - Add a user-content conversion helper that can return either a string
    or content-part array.
  - Keep `join_text_content` for assistant/tool-compatible paths.
- `crates/anie-providers-builtin/src/openai/mod.rs`
  - Use the new helper for `Message::User`.
- `crates/anie-providers-builtin/src/openai/convert.rs` tests
  - Add request-shape regression tests for text-only and mixed
    text/image user messages.
- Potentially `crates/anie-provider/src/model.rs`
  - Only if a compat flag is needed for a backend that rejects content
    arrays.

## Phased PRs

### PR A — Structured content arrays for image-bearing user messages

**Change:**

- Add a helper such as `user_content_to_openai(content: &[ContentBlock])
  -> serde_json::Value`.
- Preserve existing text-only behavior.
- Serialize image blocks as data URLs in `image_url.url`.
- Keep thinking text behavior consistent with the current flattened
  implementation unless a provider-specific rule says otherwise.

**Tests:**

- Text-only user message still serializes as a JSON string.
- Mixed text/image user message serializes as an array with ordered text
  and image parts.
- Empty content is either skipped or represented consistently with the
  current provider behavior.

**Exit criteria:**

- No OpenAI user image is serialized as an inert `[image:...]`
  placeholder on the image-capable path.

### PR B — Provider compatibility audit

**Change:**

- Audit built-in OpenAI-compatible model catalog entries that set
  `supports_images`.
- If OpenRouter/local backends need different image behavior, document
  it with a compat knob rather than special-casing silently.

**Tests:**

- Catalog conversion tests still produce expected `supports_images`
  values.
- Any new compat flag gets serde round-trip coverage.

**Exit criteria:**

- Image-capable models and wire serialization agree.

## Test plan

- `cargo test -p anie-providers-builtin openai`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual smoke with an OpenAI-compatible image-capable model and a
  `read` tool result that attaches an image.

## Risks

- Some OpenAI-compatible providers accept string content but not content
  arrays. Preserve string output for text-only messages to reduce
  compatibility risk.
- Do not send images to models that are known not to support them.
- Keep base64 data unchanged; do not decode/re-encode unless necessary.

## Exit criteria

- OpenAI-compatible image support is real on the wire.
- Text-only OpenAI-compatible requests remain backward compatible.
- Tests cover the exact request JSON shape.

