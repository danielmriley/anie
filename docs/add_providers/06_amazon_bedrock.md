# Plan 06 — Amazon Bedrock

AWS-hosted frontier models via Bedrock. Most complex provider in
this folder: AWS SigV4 auth, new wire protocol (`Converse`
streaming API), and a runtime dependency on AWS credentials
that's different from every other provider.

Lowest-priority plan. Don't start this until plans 01–05 have
landed and the user has a concrete need.

## User value

- **AWS-tied organizations.** Customers who must keep inference
  inside their AWS account for compliance or VPC reasons.
- **Bedrock-exclusive models.** Some Anthropic / Meta / Cohere
  models ship first (or only) via Bedrock.
- **Consolidated billing / IAM.** Inference as an AWS line item,
  authentication via existing IAM roles.

## Wire protocol

**New `ApiKind::BedrockConverseStream`** (not in the enum yet —
this plan adds it) and **new provider module
`anie-providers-builtin::bedrock`**.

Endpoint:
`https://bedrock-runtime.{region}.amazonaws.com/model/{modelId}/converse-stream`

Protocol: AWS's Converse / ConverseStream API. Binary-framed
events (AWS Event Stream), not SSE. This is the most significant
single departure from every other provider.

## Auth shape

Two auth paths, both supported by pi
(`packages/ai/src/providers/amazon-bedrock.ts` around the
`AWS_BEARER_TOKEN_BEDROCK` reference). Ship both; the
simpler one covers more users.

### Bearer token (recommended first)

- Env: `AWS_BEARER_TOKEN_BEDROCK`.
- Sent as `Authorization: Bearer <token>` — **bypasses SigV4
  entirely**, which is the whole reason it exists as an auth
  mode.
- Requires the `bedrock:CallWithBearerToken` IAM permission on
  the token's identity, which the token-issuing admin already
  configured if the token was handed out.
- **No AWS SDK needed.** We can send the bearer token ourselves
  over `reqwest` and use our own Event Stream decoder. This path
  dodges the multi-MB binary size cost of the AWS SDK.

### SigV4 signing (full compatibility)

- Env: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, optionally
  `AWS_SESSION_TOKEN`, `AWS_REGION`.
- Full AWS credential-discovery chain (`~/.aws/credentials`,
  IAM instance metadata service, AWS SSO, etc.).
- **Use the official `aws-sdk-bedrockruntime` crate.** Rolling
  our own SigV4 + Event Stream decoder is weeks of work. The
  crate handles signing, credential discovery, region failover,
  and Event Stream framing.

### Implementation sequencing

Phase A ships the bearer-token path only — users with an
`AWS_BEARER_TOKEN_BEDROCK` can use Bedrock without the SDK in
their binary. SigV4 + credential-chain discovery lands in a
later phase behind the same `bedrock` feature flag.

The feature flag still gates the AWS SDK dependency — users who
only ever use bearer tokens don't compile the SDK at all.

## Dependencies on upstream crates

Feature-gated under `bedrock`:

```toml
[features]
bedrock = []              # bearer-token path only (default when "bedrock" is set)
bedrock-sigv4 = ["bedrock", "dep:aws-config", "dep:aws-sdk-bedrockruntime"]

[dependencies]
aws-config = { version = "1", optional = true }
aws-sdk-bedrockruntime = { version = "1", optional = true }
```

Splitting into two feature flags lets the bearer-token user
compile Bedrock support without the AWS SDK. Advanced users with
full IAM flows enable `bedrock-sigv4`.

## `ApiKind` extension

```rust
pub enum ApiKind {
    AnthropicMessages,
    OpenAICompletions,
    OpenAIResponses,
    GoogleGenerativeAI,
    BedrockConverseStream,   // new
}
```

Serde-wise, add the new variant with the `Deserialize` default
macro unchanged. Bump session schema version (per plan 05 of
completed/api_integrity_plans).

## Model catalog entries

Bedrock's model IDs are fully qualified — provider-model-region
triples sometimes. Keep a small curated set; users can add more
via config.

| Model ID | Display | Context | Reasoning | Notes |
|---|---|---|---|---|
| `us.anthropic.claude-sonnet-4-v1:0` | Claude Sonnet 4 (Bedrock, us-region) | 1M | native | |
| `us.anthropic.claude-opus-4-v1:0` | Claude Opus 4 (Bedrock) | 1M | native | |
| `us.meta.llama3-3-70b-instruct-v1:0` | Llama 3.3 70B (Bedrock) | 128k | none | |
| `us.amazon.nova-pro-v1:0` | Amazon Nova Pro | 300k | none | |

Region prefix (`us.`, `eu.`, etc.) baked into the model ID
reflects Bedrock's cross-region inference. For users in non-US
regions, they'll configure `[providers.bedrock] region = "eu-west-1"`
and get different catalog entries.

## Converse → `AssistantMessage` mapping

The Converse API has a roughly Anthropic-ish shape (messages,
content blocks, tool use) but with its own JSON field names and
stream-event names. Round-trip contract:

```
| Field                              | Source event                           | Landing spot                      |
|------------------------------------|----------------------------------------|-----------------------------------|
| `contentBlockDelta.delta.text`     | `contentBlockDelta`                    | `ContentBlock::Text`              |
| `contentBlockStart.start.toolUse`  | `contentBlockStart`                    | `ContentBlock::ToolCall` (header) |
| `contentBlockDelta.delta.toolUse.input`
                                     | `contentBlockDelta`                    | `ContentBlock::ToolCall` (args)  |
| `contentBlockDelta.delta.reasoningContent.text`
                                     | `contentBlockDelta`                    | `ContentBlock::Thinking`          |
| `messageStop.stopReason`           | `messageStop`                          | `AssistantMessage::stop_reason`  |
| `metadata.usage.*`                 | `metadata`                             | `AssistantMessage::usage`         |
```

Intentionally dropped:

- `messageStart.role` — always `"assistant"`; not useful to echo.
- `metadata.metrics.latencyMs` — Bedrock-specific telemetry;
  could surface in status bar later.
- `contentBlockStart.start.reasoningContent.signature` —
  Bedrock-provided Anthropic-style signatures. **Capture these
  in turn-2 replay** when the upstream is Anthropic (otherwise
  we get the same 400 we got directly from Anthropic pre-plan-
  01). Mark the capability per-model.

## Replay capabilities

Bedrock + Anthropic upstream: `requires_thinking_signature: true`
(same as direct Anthropic).
Bedrock + Llama / Nova: `None` (no replay-sensitive reasoning).

## Implementation phases

Four phases. Each is large; each is a standalone PR.

### Phase A — Crate + feature flag

- Add `bedrock` feature to `anie-providers-builtin`.
- Add `aws-config`, `aws-sdk-bedrockruntime` deps behind the
  feature.
- Register a provider shell behind `ApiKind::BedrockConverseStream`
  that errors with "not yet implemented" but compiles cleanly.
- Gate via a build-time CI matrix: build without `bedrock`
  feature is the default; build with it is a separate CI job.

### Phase B — Non-streaming request + Converse basics

- `BedrockProvider::convert_messages` → Converse message shape.
- Non-streaming `converse` call (buffered, not `converse-stream`
  yet). Enough to prove auth works end-to-end.
- Error classifier for Bedrock-specific error codes
  (`ThrottlingException`, `ValidationException`, etc.).
- First catalog entry: `us.anthropic.claude-sonnet-4-v1:0`.
- Auth: rely on the SDK's default credential provider chain.

### Phase C — Streaming + Event Stream decoding

- Switch to `converse_stream`. The SDK exposes it as an async
  stream of `ConverseStreamOutput` events.
- Build the round-trip contract doc block from the table above.
- Handle tool-use chunks, reasoning chunks, metadata.
- Replay signatures captured and echoed on turn 2 for
  Anthropic-upstream catalog entries.

### Phase D — Catalog, onboarding, invariants

- Four catalog entries per the table above.
- Onboarding: Bedrock appears under `ProviderCategory::Cloud`
  category. The onboarding form collects region + credential-
  chain confirmation rather than a raw API key.
- Invariant-suite integration.
- Manual smoke against a real AWS account.

## Test plan

Per phase:

### Phase A
- `bedrock_feature_is_opt_in` — default build doesn't include AWS SDK.
- `bedrock_feature_enabled_compiles_cleanly`

### Phase B
- `bedrock_request_body_matches_converse_shape`
- `bedrock_auth_reads_from_credential_chain`
- `bedrock_error_classifier_routes_throttling_to_rate_limited`

### Phase C
- `bedrock_stream_text_delta_to_content_block`
- `bedrock_stream_tool_use_roundtrips`
- `bedrock_stream_reasoning_signature_captured_for_anthropic_upstream`
- `bedrock_turn_two_replay_includes_signature_for_anthropic_upstream`

### Phase D
- Invariant suite covers Bedrock-Anthropic and
  Bedrock-Llama entries.
- Manual two-turn smoke.

## Exit criteria

- [ ] Bedrock feature flag is opt-in; default build is
      unaffected by compile time / binary size.
- [ ] Four catalog entries with correct replay capabilities.
- [ ] Converse streaming events flow into `ContentBlock` with
      round-trip contract documented.
- [ ] AWS credential chain (env + file + IMDS) discovery works.
- [ ] Invariant suite exercises Bedrock.

## Out of scope

- Bedrock Knowledge Bases (server-side retrieval).
- Bedrock Guardrails (content-moderation wrappers).
- Bedrock Agents (AWS's own agent framework — a separate path
  entirely).
- Inference profiles for custom models.

## Dependencies

- Plan 00 (provider selection UX).
- No hard dependency on plans 01–05 — Bedrock is independent.
  Order-of-landing should still be last because the AWS SDK
  adds the most compile-time surface.

## Risks

1. **Binary size.** AWS SDK adds ~10+ MB compile time
   dependency graph. The feature flag is essential; default
   builds must stay lean.
2. **Credential chain behavior differs by environment.** IAM
   roles inside EC2 behave differently than `~/.aws/credentials`
   locally. Test both.
3. **Event Stream format changes.** AWS has stable this format
   for years, but the SDK is the only sane way to stay current.
4. **Cross-region pricing differences.** Catalog entries may
   need per-region variants if users care about pricing —
   defer to follow-up.
