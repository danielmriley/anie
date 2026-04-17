# Provider Expansion and Auth

## Summary

Expand provider support beyond the current Anthropic/OpenAI/local set,
and add OAuth/subscription authentication for providers that support it.

## Current State

Anie supports:
- Anthropic (API key)
- OpenAI (API key)
- Local OpenAI-compatible servers (Ollama, LM Studio, vLLM)

Auth is handled via `CredentialStore` (keyring + JSON fallback) and
environment variables. No OAuth flow exists.

## Action Items

### 1. Additional providers to consider

| Provider | API type | Auth | Priority |
|----------|----------|------|----------|
| Google Gemini | google-generative-ai | API key | Medium |
| Mistral | OpenAI-compatible or native | API key | Low |
| Groq | OpenAI-compatible | API key | Low |
| xAI | OpenAI-compatible | API key | Low |
| OpenRouter | OpenAI-compatible | API key | Low |
| Azure OpenAI | OpenAI-compatible | API key | Low |
| Amazon Bedrock | Custom | IAM | Low |

Most of these are OpenAI-compatible and would work today if manually
configured. The value is in having them as built-in presets with correct
base URLs, model catalogs, and onboarding flows.

### 2. OAuth / subscription support
For providers that offer subscription access (Claude Pro, ChatGPT Plus,
GitHub Copilot, etc.):
- Add `/login <provider>` and `/logout` commands
- Support browser-based OAuth, device code flow, and manual token entry
- Store credentials in `CredentialStore` with auto-refresh
- Modify available model list based on subscription level

### 3. Auth resolution order
Currently: CLI flags → credential store → environment variables.
Consider adding: shell command resolution (`!command` syntax like pi)
for keychain/vault integration.

### 4. Custom provider registration
Users can already add providers via config. Consider whether a more
structured approach (like pi's `models.json`) would be cleaner for
power users managing many custom providers.

## Priority

Low-medium — the current provider set covers the most common use cases.
OAuth and subscription support would be nice but is not blocking anyone.
