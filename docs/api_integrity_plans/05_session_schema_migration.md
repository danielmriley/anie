# 05 — Session schema migration strategy

> **Priority: P2.** Must land *before* any protocol-type change ships to
> users, so plan 01's `signature` field is the forcing function.
> Enforces principle 8.

## The problem

Anie persists conversations as JSON session files. `ContentBlock` and
related types in `anie-protocol` are serialized via `serde` directly —
there is no intermediate schema version marker. When a new field is
added (like `signature` in plan 01), sessions written by older binaries
must still load, and sessions written by newer binaries must still be
readable by older binaries in the near term (especially during
gradual rollout or if a user downgrades).

Without a migration strategy, we risk:

1. **Load-time crashes** on legacy sessions when a required field is
   missing.
2. **Replay failures** when optional fields are `None` but the wire
   requires them (already covered by plan 01's sanitizer).
3. **Silent data loss** when newer session files are loaded by an
   older binary that doesn't know about new fields.
4. **Forward-compatibility breakage** if a user upgrades,
   rolls back, upgrades again — fields written by the newer binary
   may be stripped on the rollback pass.

## Current state

- `ContentBlock` uses `#[serde(tag = "type")]` adjacently-tagged enum
  encoding.
- No session-file version field.
- No `#[serde(default)]` on variant fields (so adding a field is a
  breaking change for existing sessions unless done carefully).
- Session load path: TBD — need to confirm during phase 1.

## Design principles

Adopt the classic "forward-compatible optional, backward-compatible
additive" contract:

1. **Every new field is optional** (`Option<T>` with `#[serde(default)]`).
2. **Every new field has `skip_serializing_if = "Option::is_none"`** —
   older binaries reading the file don't see a key they don't understand.
3. **Every new variant is added to the enum**, never renamed; old
   tags keep working.
4. **Unknown variants in old binaries are handled gracefully** — either
   via a catch-all `Unknown` variant or by dropping the block on load.
5. **Session files carry a `schema_version`** at the top level so we
   can detect incompatibility and refuse to load (better than crashing
   or silently losing data).

## Phase 1 — Survey current session format

**Goal:** Document the on-disk shape and every place it's read/written.

### Files to read

- `crates/anie-session/src/lib.rs` — session store impl.
- Any `serde_json::to_string`/`from_str` over `Message` or `Session`.
- Tests in `crates/anie-integration-tests/tests/session_resume.rs` —
  these have fixture files or inline JSON that document the expected
  shape.

### Deliverable

Add a `docs/session_format.md` (not in this folder — a reference doc
next to the implementation) describing:

- Top-level structure.
- Every variant of `ContentBlock` with an example payload.
- Every field's optional/required status.
- Current implicit schema version ("v1 — no explicit versioning").

## Phase 2 — Introduce explicit schema version

**Files:** `crates/anie-session/src/lib.rs`,
`crates/anie-protocol/src/lib.rs`.

Add at the top level of the serialized session:

```rust
#[derive(Serialize, Deserialize)]
pub struct Session {
    pub schema_version: u32,  // new; defaults to 1 via #[serde(default = "...")]
    pub messages: Vec<Message>,
    // ... existing fields
}

fn default_schema_version() -> u32 { 1 }
```

- Version 1 = pre-migration baseline (today).
- Version 2 = post plan 01 (thinking signatures).
- Version N = bump each time we add a non-`Option` field or change a
  tag rename.

On load:

- `schema_version > CURRENT_VERSION` → refuse with a clear error
  ("session was written by a newer anie; upgrade to continue").
- `schema_version < CURRENT_VERSION` → load with migration fallbacks.
- Missing `schema_version` → treat as 1.

## Phase 3 — Migration-safe field additions

**Goal:** A recipe (checklist) that contributors follow every time
they touch a protocol type.

### Checklist for adding a field

1. Field type is `Option<T>` unless there's a compelling reason it must
   be required (there usually isn't).
2. `#[serde(default, skip_serializing_if = "Option::is_none")]` attribute.
3. Bump `schema_version` in the next binary release's default.
4. Add a `#[test]` that deserializes a session written without the
   field and verifies it still loads.
5. Add a `#[test]` that serializes a session with the field set to
   `None` and verifies the JSON has no key for it.
6. Add a `#[test]` that deserializes a session written *with* the field
   and verifies the field round-trips.

### Checklist for adding a variant

1. New variant uses a new tag string — never reuse or rename old ones.
2. Older binaries reading the session should either (a) have a
   catch-all `Unknown` variant, or (b) skip the message on load with a
   warning. Pick one behavior project-wide.
3. Bump `schema_version`.

### Checklist for renaming a field or variant

**Don't.** If truly unavoidable, go through a deprecation cycle: add
the new name, serialize to both during a transition release, then
remove the old name in a later release. Coordinate with session-file
consumers.

## Phase 4 — Implement for plan 01

Plan 01's `signature: Option<String>` field is the first real exercise
of this plan. Make sure:

- The field has both `#[serde(default)]` and `#[serde(skip_serializing_if)]`.
- Old sessions (no `signature` key) load, and re-serialize without a
  `signature: null` artifact.
- The session `schema_version` bumps from 1 → 2.

## Phase 5 — Documentation

Add a prominent note to `docs/session_format.md` (created in phase 1)
listing the migration checklist and version history. Every schema bump
gets a line in a changelog table:

| Version | Change | Compatibility |
|---------|--------|---------------|
| 1 | Baseline | — |
| 2 | `ContentBlock::Thinking.signature` optional | Forward- and backward-compatible |
| 3 | `ContentBlock::RedactedThinking` variant | Forward-compatible only (old binaries drop the variant) |

## Phase 6 — CI check

Add a test that parses a checked-in fixture of every past session
schema version. When we bump the version, we add a new fixture
alongside the existing ones. If any breaks, CI fails — so we can never
accidentally break a user's stored history without noticing.

**File:** `crates/anie-session/tests/schema_migration.rs`.

Fixtures under `crates/anie-session/tests/fixtures/session_v{N}.json`.

## Out of scope

- Binary or columnar session format. JSON is fine at our scale.
- Live migration of session files (rewriting on load to new schema).
  Add-only + `Option` fields make this unnecessary.
- Remote session sync. Not a project goal today.
