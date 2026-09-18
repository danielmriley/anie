# anie desktop

Electron shell around `anie --rpc`. Run it from the repo checkout after building the CLI.

## Build anie

```bash
cargo build -p anie-cli
```

The debug binary is `target/debug/anie`. A release build at `target/release/anie` also works.

## Run the app

```bash
cd desktop
npm install
npm start
```

## Tests

```bash
npm test
npm run verify
npm run verify -- --real
```

`npm run verify` drives the UI with Chrome DevTools Protocol and a fake `anie` binary. `npm run verify -- --real` uses the built `anie` binary under a fresh `HOME` with no credentials and needs network access, because it expects the provider's authentication error to reach the window.

## Environment variables

| Variable | Purpose |
| --- | --- |
| `ANIE_DESKTOP_BIN` | Path to the `anie` executable. Checked first. |
| `ANIE_DESKTOP_CWD` | Working directory for the child (`-C`). Overrides saved settings. |
| `ANIE_DESKTOP_SHOTS` | Directory for verification PNGs. Default `/tmp/anie-desktop-verify/`. |

## Binary resolution

When `ANIE_DESKTOP_BIN` is unset, main looks for `../target/release/anie`, then `../target/debug/anie` relative to `desktop/`, then `anie` on `PATH`.

## What this app does

- Spawns one `anie --rpc` child and paints its JSONL events as plain text.
- Pick a project folder, set thinking level and model name, start a new session, send prompts, and abort runs.
- Persists the last chosen folder under Electron user data.

## What it does not do

- No prompt queue. A second prompt while a run is active stays in the composer with a hint.
- No markdown rendering.
- No tool output bodies on the live path.
- Closing the window quits the app on every platform so a hidden child cannot hold a session lock.
