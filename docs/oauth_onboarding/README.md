# Plan 11 — OAuth providers in onboarding

**Feature. Makes OAuth login discoverable during first-run setup.**

Plan 07 PR D.2 shipped `/login <provider>` as a documentation
nudge inside the TUI — it tells users to exit and run `anie
login <provider>` in a regular shell because the callback server
can't run inside the alternate-screen terminal state. This works
but requires the user to know the command exists.

First-run onboarding is where new users pick a provider. Today
the preset list shows Anthropic / OpenAI / OpenRouter / xAI /
Groq / Mistral / Together.ai / Fireworks — all API-key flows.
OAuth providers are invisible here.

## Rationale

A new user hitting onboarding has to:

1. Pick a provider from the preset list.
2. If the provider they actually want (Copilot, Gemini) needs
   OAuth, discover that outside anie.
3. Run `anie login <provider>` in a shell.
4. Re-enter anie.

Surfacing OAuth presets inside onboarding collapses (2) into
"scroll down one more line."

## Design

### Approach: guided-exit path

Add OAuth-capable providers to `provider_presets()` with a
`ConfiguredProviderKind::OAuthLogin` variant. When the user
picks one, the onboarding flow:

1. Closes the onboarding screen.
2. Emits a system message with the exact command + a brief
   "your browser will open; return here when it's done" hint.
3. Exits the TUI (same effect as `quit`).
4. The user runs `anie login <provider>` as instructed.
5. The next `anie` launch picks up the credential
   automatically.

This is "guided exit," not "in-process login." Matches the
`/login` slash command's pattern.

### Why not run the login flow in-process

Three reasons:

- **Alternate-screen interference.** The callback server needs
  to listen on a port while the user interacts in a browser.
  The TUI would either freeze or need to yield the screen to
  the login flow, then restore. Terminals do not always cope
  with repeated enter/exit of the alt-screen cleanly.
- **Async complexity.** Login polling (device code) or
  awaiting a callback (auth code) is a multi-second blocking
  operation. Interleaving with the event loop is possible but
  adds complexity for a feature most users hit once.
- **CLI already works.** `anie login <provider>` is a complete
  end-to-end flow that already auto-opens the browser via
  `opener` and stores the credential correctly. Duplicating
  inside the TUI is re-implementation, not integration.

The guided-exit path keeps all the login complexity in one
place (CLI) and just surfaces the handoff point.

### UX sketch

Preset list adds entries:

    Built-in:
      Anthropic (Claude)                 [API key]
      OpenAI (GPT-4o, o-series)          [API key]
      OpenRouter (discovery, 500+ models)[API key]
      ...
    OAuth:
      GitHub Copilot                     [OAuth]
      OpenAI Codex (ChatGPT login)       [OAuth]
      Google Gemini CLI                  [OAuth]
      Google Antigravity                 [OAuth]
      Anthropic (Claude Pro/Max)         [OAuth] ⚠

The Anthropic OAuth entry carries a ⚠ marker linking to a note
about the ToS enforcement risk; we include it for completeness
but steer users away.

When the user selects an OAuth preset, the screen renders:

    Selected: GitHub Copilot (OAuth)

    anie needs you to complete OAuth login in a separate shell:

      anie login github-copilot

    A browser will open; finish the flow there. Once the
    credential is stored, re-launch anie and your Copilot
    models will be available.

    [Press Enter to exit anie]

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-tui/src/overlays/onboarding.rs::provider_presets` | Append OAuth entries with a new `OAuthLogin` kind. |
| `crates/anie-tui/src/overlays/onboarding.rs` state machine | `OnboardingState::OAuthLoginInstructions { provider }`. |
| `crates/anie-tui/src/overlays/onboarding.rs::handle_provider_preset_key` | Dispatch to the new state when an OAuth preset is picked. |

## Phased PRs

### PR A — OAuth entries + instructions screen

1. New `OAuthLoginInstructions` onboarding state.
2. Five OAuth presets added to `provider_presets()`, flagged
   distinctly in the list view.
3. Selecting one routes to the instructions screen.
4. Enter key exits the TUI.
5. Tests: entries present, state transitions, exit behavior.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | OAuth presets appear in `provider_presets()` output. | `anie-tui::overlays::onboarding` tests |
| 2 | Selecting OAuth preset transitions to `OAuthLoginInstructions`. | same |
| 3 | Instructions screen shows the exact `anie login <provider>` command. | same |
| 4 | Enter from instructions screen sets `should_quit`. | same |

## Risks

- **User confusion if they don't read the exit prompt.** Some
  users will press Enter expecting to advance. Mitigation: the
  instructions screen's dominant visual element is the command
  to run, not a "Continue" button.
- **Anthropic OAuth entry is a footgun.** We include it with a
  warning marker, but a determined user will still log in and
  get their Anthropic account flagged. Mitigation: the warning
  text is blunt ("⚠ Anthropic actively enforces third-party
  usage. Risk of account action.").

## Exit criteria

- [ ] PR A merged.
- [ ] Running `anie onboard` on a fresh install shows the OAuth
      presets alongside API-key ones.
- [ ] Selecting "GitHub Copilot" ends the TUI with the correct
      command on screen, and running that command in a shell
      completes a working login.

## Deferred

- **In-TUI login flow.** See "Why not run the login flow
  in-process" above — revisit only if terminal alt-screen
  handling around external callbacks improves.
- **Post-login auto-re-entry.** Could save the user typing
  `anie` again. Needs cross-process coordination; not worth it
  for a once-per-machine action.
