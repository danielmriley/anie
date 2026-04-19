# Phase 4 — Config → Provider Registry Wiring

## Why this phase exists

The startup path loads configuration from TOML files, builds a model catalog, creates a provider registry, and wires up authentication. Each of these steps is unit-tested in its respective crate, but no test verifies that a loaded config produces a correctly wired system that could actually serve a provider request.

These tests are lightweight — they don't run the agent loop or touch the filesystem beyond temp config files — but they verify the integration seam between `anie-config`, `anie-auth`, `anie-providers-builtin`, and `anie-provider`.

---

## File to create

### `crates/anie-integration-tests/tests/config_wiring.rs`

---

## Test cases

### Test 12: `default_provider_registry_has_builtin_providers`

**Scenario:** The standard startup path creates a provider registry with both built-in provider implementations registered.

**Setup:**
- Create a `ProviderRegistry` using the same call the controller uses.
- Call `register_builtin_providers(&mut registry)`.

**Assertions:**
- `registry.get(&ApiKind::OpenAICompletions)` returns `Some`.
- `registry.get(&ApiKind::AnthropicMessages)` returns `Some`.
- `registry.get(&ApiKind::GoogleGenerativeAI)` returns `None` (not implemented).

---

### Test 13: `custom_provider_config_produces_correct_model_catalog`

**Scenario:** A TOML config with a custom local provider produces a model catalog entry with the correct base URL, API kind, and model metadata.

**Setup:**
- Write a TOML config string:
  ```toml
  [model]
  provider = "ollama"
  id = "qwen3:32b"

  [providers.ollama]
  base_url = "http://localhost:11434/v1"
  api = "OpenAICompletions"

  [[providers.ollama.models]]
  id = "qwen3:32b"
  name = "Qwen 3 32B"
  context_window = 32768
  max_tokens = 8192
  ```
- Write it to a temp file.
- Load the config using `load_config_with_paths(Some(path), None, CliOverrides::default())`.

**Execution:**
- Call `configured_models(&config)`.

**Assertions:**
- The returned model list has exactly 1 entry.
- `model.id == "qwen3:32b"`.
- `model.provider == "ollama"`.
- `model.base_url == "http://localhost:11434/v1"`.
- `model.api == ApiKind::OpenAICompletions`.
- `model.context_window == 32768`.
- `model.max_tokens == 8192`.

---

### Test 14: `auth_resolver_with_config_env_var_resolves_key`

**Scenario:** A config file declares an `api_key_env` for a provider. The auth resolver reads the key from the environment variable.

**Setup:**
- Write a TOML config that includes:
  ```toml
  [providers.openai]
  api_key_env = "ANIE_INTEGRATION_TEST_KEY"
  ```
- Load the config.
- Set the environment variable `ANIE_INTEGRATION_TEST_KEY=test-key-value`.
- Create an `AuthResolver` with `cli_api_key: None` and no auth file (point `auth_path` at a nonexistent temp path).

**Execution:**
- Call `resolver.resolve(&model, &[]).await` where `model.provider == "openai"`.

**Assertions:**
- The resolved options have `api_key == Some("test-key-value")`.

**Cleanup:**
- Remove the environment variable after the test.

**Note on safety:** Use a unique, test-specific env var name to avoid collisions. The existing auth tests already use `ANIE_TEST_OPENAI_KEY` as a precedent. Use `unsafe { std::env::set_var(...) }` with a comment explaining why, matching the pattern in `crates/anie-auth/src/lib.rs`.

---

## Key implementation notes

### Config loading

The config loader can be called directly without any filesystem hierarchy:

```rust
let config = load_config_with_paths(
    Some(global_config_path),
    None,                       // no project config
    CliOverrides::default(),
)?;
```

### Model catalog construction

The controller uses:
```rust
let mut model_catalog = builtin_models();
model_catalog.extend(configured_models(&config));
```

Integration tests can follow the same pattern to get a combined catalog.

### Auth resolver construction

```rust
let resolver = AuthResolver::new(None, config.clone())
    .with_auth_path(Some(nonexistent_path));
```

The `with_auth_path` override lets the test bypass the real `~/.anie/auth.json` file.

### What NOT to test here

- Full controller startup (model resolution, system prompt construction) — that involves too many moving parts for a config-focused test.
- Real HTTP requests to providers — use the registry's `get()` method to verify registration, not `stream()`.
- Complex config merging (global + project + CLI) — that's covered by `anie-config` unit tests.

---

## Exit criteria

- [ ] `crates/anie-integration-tests/tests/config_wiring.rs` exists with 3 passing tests
- [ ] at least one test verifies that `register_builtin_providers` populates the registry correctly
- [ ] at least one test verifies that a custom TOML config produces the expected model catalog
- [ ] at least one test verifies that the auth resolver reads from a configured env var
- [ ] `cargo test -p anie-integration-tests` passes
- [ ] `cargo test --workspace` passes
