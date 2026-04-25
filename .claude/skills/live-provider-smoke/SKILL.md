---
name: live-provider-smoke
description: "How to smoke-test anie end-to-end against a live LLM provider from a non-interactive agent session. Use when the user asks to verify the provider / agent / streaming pipeline works after changes, when confirming an API key is wired up correctly, or when exit-criteria include 'manual smoke against a real response'. Covers the one-shot print mode, recommended free-tier models on OpenRouter, rate-limit fallback pattern, and the limits of non-TUI verification."
---

# Live-provider smoke testing

anie's one-shot `--print` (`-p`) mode runs the full agent loop —
provider registry, request resolver, streaming parser, tool
orchestration — but writes the final assistant text to stdout
instead of opening the TUI. That makes it usable from a
non-interactive Claude Code session where no real terminal is
attached.

## When to use this

- After any change that touches the provider layer
  (`anie-provider`, `anie-providers-builtin`), the compaction
  pipeline, the retry policy, or the session/config loader.
- When an exit criterion says "manual smoke against a real
  response" and you want to get as close as you can without a
  human at the terminal.
- Before committing a change that could plausibly break the
  happy path on a real provider (e.g. schema bumps, auth wiring,
  request-builder edits).

## When NOT to use this

- **TUI visual smoke.** This mode never opens the TUI, so it
  can't verify markdown rendering, pane layout, streaming
  animation, slash commands, or anything the user sees on
  screen. Those remain human tasks.
- **Tool execution reality checks.** By default we run with
  `--no-tools` so the smoke stays hermetic and doesn't touch
  the filesystem. If a test needs to exercise tools, remove the
  flag and expect slower / more variable runs.

## Prerequisites

1. `~/.anie/config.toml` must have an `[providers.openrouter]`
   block with a `base_url` and at least one model entry under
   `[[providers.openrouter.models]]`.
2. An OpenRouter API key must be stored in `~/.anie/auth.json`
   (or whatever auth path the current config points at).
   **Don't read that file to inspect it** — the user may have
   sandboxed it, and the contents are credentials. Trust the
   user's claim that the key is active and let a failed run
   surface auth problems through the retry policy.

## The command

```bash
timeout 45 ./target/debug/anie -p --no-tools \
  --provider openrouter \
  --model "nvidia/nemotron-3-super-120b-a12b:free" \
  "Reply with exactly: hello from openrouter"
```

Flags explained:

| Flag | Why |
|------|-----|
| `-p` / `--print` | One-shot print mode — no TUI. |
| `--no-tools` | Skip tool registration so the agent can't `read`/`write`/`bash` during the smoke. Keeps runs hermetic. |
| `--provider openrouter` | Override whatever the default-model config says. OpenRouter's free-tier catalog is the most reliable path for smoke tests. |
| `--model ...` | Pin the exact model. Free models rate-limit aggressively — see the fallback pattern below. |
| `timeout 45` | OpenRouter's free models can take 10–20s to respond; 45s leaves headroom without hanging a failed run forever. |

## Free-tier model fallback pattern

Free OpenRouter models hit rate limits often, especially during
any testing burst. The pattern: try one, and if you see
`Rate limited (no retry hint from provider)`, swap to another.
Verified working during Plan 06 work (2026-04-21):

- `nvidia/nemotron-3-super-120b-a12b:free` — worked cleanly on
  the first try.
- `google/gemma-4-31b-it:free` — rate-limited with no
  retry-after hint.

If both rate-limit, try a different free model from the local
config (`grep -A 2 "^\[\[providers.openrouter.models\]\]"
~/.anie/config.toml` to list them), or give up the smoke and
escalate to the user to run it manually.

## Expected output

A successful run writes a `Session: <8-hex>` line first, then
the assistant's text on following lines. Example:

```
Session: 9b39e3ad
hello from openrouter
```

The prompt asking for a verbatim short reply makes regression
detection trivial — anything other than the expected string
means the pipeline mangled the request or the model wandered.

A failed run surfaces the taxonomy error directly
(`Rate limited (no retry hint from provider)`,
`Authentication failed`, `Provider not configured`, etc.),
which is exactly the information needed to decide whether the
failure is in anie or in the upstream provider.

## After running, report

- What you verified (provider round-trip works / fails).
- Which model answered (models swap, so specificity matters).
- What this does NOT verify (TUI rendering, tool execution,
  interactive slash commands). Keep the user's expectations
  honest — a green one-shot smoke is not the same as a passing
  interactive-mode smoke.

## Related

- `docs/pi_adoption_plan/execution/README.md` — exit-criteria
  items that need human verification are called out explicitly
  in the per-plan notes. "Manual smoke" there usually means
  "human-at-terminal smoke" that this skill cannot satisfy.
- `.claude/skills/adding-providers/SKILL.md` — the companion
  skill for the build side. After adding a provider, use this
  skill to round-trip a real request through it.
