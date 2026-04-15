# Runtime State Integration Plan

This note captures the intended split between stable configuration, secrets, and mutable runtime state as the interactive controller is built.

## Goals

1. Keep user-authored defaults centralized and editable.
2. Keep secrets out of general config files.
3. Preserve the last-used model/thinking/provider so users do not need flags every launch.
4. Avoid mutating `config.toml` for transient runtime state.

## File responsibilities

### `~/.anie/config.toml`
Use for stable, user-authored defaults and preferences:
- default provider
- default model
- default thinking level
- custom provider definitions
- context caps
- future UX preferences

### `~/.anie/auth.json`
Use for secrets only:
- provider API keys
- future OAuth tokens

This file should remain the canonical secret store because it can be permission-locked separately from general config.

### `~/.anie/state.json` (planned)
Use for mutable, non-secret runtime state:
- last used provider
- last used model
- last used thinking level
- future: last resumed session / recent state

This file is intentionally separate from `config.toml` so runtime state does not overwrite user-authored defaults.

## Proposed precedence

### Model/provider/thinking selection
1. Explicit CLI flag or explicit UI action for the current run
2. Persisted runtime state from `state.json`
3. Stable defaults from `config.toml`
4. Local-model auto-detection fallback
5. Built-in catalog fallback

### API key resolution
1. Explicit CLI `--api-key`
2. `~/.anie/auth.json`
3. Provider-specific env var

## Planned implementation point

Do not implement this in Step 10.

Implement it with the interactive controller in Step 11, because the controller is the correct owner of mutable runtime state. The TUI should emit actions such as model/thinking changes, and the controller should:
- apply the change for the current run,
- persist session-local change events where appropriate,
- update global runtime state in `state.json` for the next launch.

## Immediate implication for current harness

The current one-shot CLI harness may still prefer auto-detected local models unless explicit flags are provided. That is acceptable for the temporary harness, but it should be replaced by the precedence order above once the interactive controller lands.
