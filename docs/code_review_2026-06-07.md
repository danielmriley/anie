# Code review — 2026-06-07

Meticulous review of the work landed on `dev_rlm` this cycle, with a
deliberate focus on the **fix commits** (`083d3e7..HEAD`). Those fixes
landed *after* the multi-agent feature review
(`docs/` / workflow `wf_45adb6e3`), so they had not themselves been
audited until now.

## Scope

- **Feature commits** (already reviewed in the prior pass): MCP client,
  todo/verifier, apply_patch, cost/budget, eval harness, tool sandbox,
  session UX (picker + checkpoint/rewind + branch summaries), skills
  loader.
- **Fix commits reviewed here** (22 files, 7 crates):
  - `083d3e7` cost meter reset on `/new` `/switch` `/fork`
  - `8040e67` apply_patch duplicate-path reject
  - `961958a` session listing degrade (non-fatal)
  - `b7e18f9` skills whitespace-name + same-root collision
  - `f064100` verifier run-scoped `armed` trigger
  - `0582733` evals subprocess timeout (process-group kill)
  - `01de420` MCP reader fast-fail + pagination + routing
  - `4791295` apply_patch async `tokio::fs`
  - `ddeeb69` ChainedBeforeModelPolicy deferred clone
  - `2fc18d7` evals schema-validate / crash-record / dotted-out / empty-expect
  - `ae78695` sandbox AF_UNIX + alloc-free pre_exec + degrade + truncate-doc
  - `e7975e2` checkpoint missing-blob atomicity + per-turn clone removal
  - `8a7c5be` checkpoint post-restore drift baseline
  - `d18ade3` budget limitations documented

## Method

Four independent adversarial reviewers (general-purpose subagents), each
on a cluster, instructed to *verify the fixes are correct/complete* and to
hunt for **new** bugs/regressions the fixes themselves introduced — not to
re-report the already-fixed issues. Each traced execution paths, read
tests + callers, and cited `file:line`. Findings cross-checked and the
clear/cheap ones fixed during the review (see "Fixed during review").
`cargo test --workspace` (1669) + `clippy --workspace -D warnings`
(default + `sandbox-linux`) + `fmt` green throughout.

## Verdict by area

| Area | Fix | Verdict |
|------|-----|---------|
| cost meter reset (`controller.rs`) | 083d3e7 | **Correct + complete.** Zeroes a fresh session; rebuilds from the *target* branch on switch/fork (reassigned before rebuild); re-prices to current model; all 3 callers guarded by `current_run.is_none()`, so `reset_run` can't clobber an in-flight run. |
| listing degrade (`controller.rs`) | 961958a | **Correct + complete.** Both (and only) `list()` call sites now degrade; error path leaves the editor focused (no `SessionList` → no hung picker). |
| verifier `armed` (`verifier.rs`) | f064100 | **Correct + complete.** Every transition traced (plan-less, incomplete→complete, carried-over-complete, empty→appears, done→reopen→done). No race — `armed` store/load happen under the `list` mutex. |
| chained-policy clone (`agent_loop.rs`) | ddeeb69 | **Correct + complete — byte-identical output.** All combos traced (all-Continue, Append-then-Continue, Replace-then-Append, StopRun, empty-Replace). Borrow of `working` released before the mutating match arm. |
| checkpoint missing-blob (`checkpoint.rs`) | e7975e2 | **Correct + complete.** Pre-loads all referenced blobs (deduped) before any write; the `&blobs[hash]` index is provably infallible (write keys ⊆ pre-loaded keys). |
| checkpoint clone removal (`lib.rs`) | e7975e2 | **Correct + complete.** `branch_details` is behavior-equivalent to old `branch_messages`+`extract_compaction_details` (same sets, same strict-after off-by-one), now borrowing `&Message`. |
| checkpoint drift baseline (`checkpoint.rs`, `session_handle.rs`) | 8a7c5be | **Correct** across all rewind sequences (incl. `Absent` and partial-coverage). One tracked limitation (growth) + one edge case (persist-failure window) below. |
| evals timeout / process-group kill (`runner.rs`) | 0582733 | **Correct + complete.** Group-kill takes down grandchildren so drain threads finish; success path reaps + joins cleanly; timeout-vs-exit race handled. |
| evals schema/crash/out/expect (`runner.rs`,`scenario.rs`,`bin`) | 2fc18d7 | **Correct + complete.** Schema check runs before scoring; non-zero exit → FAIL result with defensively-rendered `Null` metrics; `--out` appends; empty `[expect]` rejected at load. |
| apply_patch dup-path (`apply_patch.rs`) | 8040e67 | **Correct for the common case.** Lexical (not canonical) dedup — see edge case below. |
| apply_patch async fs (`apply_patch.rs`) | 4791295 | **Correct + complete.** Validate→dry_run→write structure intact; only I/O primitives changed. |
| skills name/collision (`skills.rs`) | b7e18f9 | **Correct + complete.** Whitespace is the only char class that breaks `/skill:<name>` dispatch (`/`,`:` don't); same-root dedup distinct from cross-root shadow. |
| MCP reader fast-fail / routing (`transport.rs`) | 01de420 | **Correct + complete — no stuck waiter.** The `pending` tokio-Mutex establishes happens-before across the reader's `closed.store`+`clear` and `request()`'s `insert`+re-check; every interleaving resolves a waiter. Routing discriminator (`method` absent) is sound. |
| MCP pagination (`client.rs`) | 01de420 | **Correct + complete.** Cursor-advance + `MAX_PAGES` cap correct; no early stop. |
| sandbox AF_UNIX (`linux.rs`) | ae78695 | **Correct.** `Dword` arg0 compare is correct *and* security-relevant (defeats a `0x1_0000_0002` truncation bypass); deny INET/INET6, allow mismatch; Landlock untouched. |
| sandbox alloc-free pre_exec (`linux.rs`) | ae78695 | **Correct.** `io::Error::from(ErrorKind)` is `Repr::Simple` — no heap alloc, unlike `io::Error::new`/`other`. |
| sandbox degrade off-backend (`lib.rs`) | ae78695 | **Correct + consistent** with the Linux backend's compat-level honoring of the same knob. |

## Findings worth acting on

### Fixed during review
- **HYGIENE — `Cargo.lock` uncommitted.** `0582733` added `libc` to
  `anie-evals/Cargo.toml` but the matching `Cargo.lock` edge (`+ "libc"`
  under the `anie-evals` package) was left uncommitted, so the tree was
  internally inconsistent (no `--locked` CI today, so non-breaking).
  **Committed.**
- **INCOMPLETE/doc — `anie-sandbox/src/lib.rs` `apply()` doc stale.** It
  still claimed "returns `Unsupported` … fail closed" unconditionally,
  contradicting the new `require_kernel_support`-driven degrade path the
  same commit (`ae78695`) introduced. **Doc corrected.**
- **NIT — non-unix unused `pid`.** `kill_process_tree(pid, …)` reads `pid`
  only inside `#[cfg(unix)]`, so a non-unix build would warn (and fail
  `-D warnings`). Added a `#[cfg(not(unix))] let _ = pid;`. (Theoretical —
  the crate is unix-targeted.) **Fixed.**
- **NIT — checkpoint comment inaccuracy.** `record_restore_baseline` said
  "real session entry ids are UUIDs"; they are 8-char hex
  (`Uuid::new_v4().simple()[..8]`). The no-collision conclusion holds for
  a better reason (hex ids can't contain `#restored`). **Comment fixed.**

### Tracked (not fixed — flagged for a future pass)
- **INCOMPLETE (low–moderate) — unbounded `baseline_only` manifest
  growth** (`checkpoint.rs`). Every `/rewind` appends one `baseline_only`
  manifest entry; nothing prunes them, and `persist_manifest` rewrites the
  whole manifest each time → O(rewinds²) cumulative write, persisted
  across reopen. Blobs stay deduped (no blob bloat); entries are small
  (N paths × ~70-byte hashes); human-paced. **Not trivially prunable:** an
  older baseline can be the newest entry for a path that newer
  baselines/captures omit, so naive "keep newest" is unsafe — safe
  coalescing must merge per-path (or GC only fully-shadowed baselines on
  open). Recommend: GC fully-shadowed baselines at `open()` as a bounded,
  safe mitigation.
- **EDGE-CASE — apply_patch dup-guard is lexical, not canonical**
  (`apply_patch.rs` + `shared.rs:resolve_path`). `resolve_path` only does
  `cwd.join(requested)` (no `canonicalize`). `./a.rs` vs `a.rs` *is*
  caught (PathBuf `Components` normalizes a mid-path `.`), but `..`
  -traversal aliases (`a.rs` vs `dir/../a.rs`) and symlink aliases to the
  same file are **not** — a second section could still clobber the first.
  Contrived/adversarial; the common-case data-loss path is closed.
  Recommend: canonicalize for the dedup key (or document the lexical
  limitation).
- **EDGE-CASE — `rewind_to` manifest-persist-failure window**
  (`session_handle.rs`). If `record_restore_baseline`'s `persist_manifest`
  fails *after* `restore` mutated disk, the tree is rewound but the
  manifest's newest entry is still stale → a later `/rewind` could see a
  false `WorkingTreeDrifted`. Same failure class (tmp+rename I/O) that
  breaks `capture`; rare. No clean fix without a 2-phase persist.
- **INCOMPLETE (minor) — apply_patch `abs.exists()` still sync std::fs**
  (`apply_patch.rs`). `4791295` switched read/write/create/remove to
  `tokio::fs` but left two `exists()` stat calls blocking. A single stat
  is cheap; `tokio::fs::try_exists(...).await` for full parity.
- **NIT — `try_wait()?` error path** (`runner.rs`) detaches the drain
  threads and skips reaping. `try_wait` only errors on pathological
  `waitpid` failures — effectively unreachable.

### Pre-existing limitations (not regressions; noted for parity)
- **seccomp `socketcall` portability** (`linux.rs`): the network deny keys
  on `SYS_socket`, which doesn't exist on `socketcall`-multiplexed arches
  (x86-32, etc.). anie's targets (x86-64/aarch64) have direct `SYS_socket`.
  Worth a one-line module note like the truncate gap now has.
- **MCP batch / string-id responses dropped** (`transport.rs`): the reader
  routes only numeric ids; JSON-RPC batch arrays and string ids are
  ignored. MCP v1 over stdio uses neither.
- **verifier doc wording** (`verifier.rs`): "arms it the moment the plan
  is observed with active work" understates — `armed` is true by default
  for any non-all-done/empty plan at construction. Behavior correct.

## Bottom line

The fix set is sound: **all 14 audited fixes are correct and complete on
their tested paths**, with no new functional bug, race, or regression
introduced. Four trivial items (one real hygiene bug, one doc
inconsistency I introduced, two comment/cfg nits) were corrected during
the review. Five lower-severity items are tracked above, the most
substantive being the append-only `baseline_only` manifest growth — a
bounded GC-on-open is the recommended follow-up.
