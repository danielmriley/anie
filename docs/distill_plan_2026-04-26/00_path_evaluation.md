# 00 — Path evaluation

Detailed comparison of the four candidate implementation paths
against the user's stated requirements.

## The user's requirements (recap)

1. CLI **and** library, callable as a tool by other AI agents
2. Fetch with robots.txt, rate limits, user-agent, cookies
3. Use **Defuddle's reader-mode logic** specifically
4. Output Markdown + YAML frontmatter; JSON when asked
5. **100% local** when possible — no required cloud services
6. Fast and reliable, handles JS-heavy modern sites
7. Good error handling, logging, configuration

The constraint that drives the recommendation: **#3 specifies
Defuddle's logic, not a generic readability extractor.** That
rules out "just use `readability-rs`" — we'd lose Kepano's
ongoing improvements and site-specific extractors.

## Evaluation rubric

For each path I score against:

| Dimension | What it measures |
|-----------|------------------|
| **Fidelity** | How close is output to upstream Defuddle? |
| **Maintenance** | How much ongoing work to track upstream? |
| **Performance** | Cold-start + per-call latency. Critical for agents that call the tool many times. |
| **Distribution** | Can we ship a single static binary? |
| **Risk** | Likelihood we hit blockers we can't work around. |
| **Time to MVP** | Wall-clock time to "agent can call this and get usable output." |

## Path 1: Native Rust port

Fork or extend `defuddle-rs` (or `trek-rs`), or write our own
port from the upstream TypeScript.

| Dimension | Assessment |
|-----------|------------|
| Fidelity | **Lower over time.** Even if we start at parity, every Defuddle release adds drift. Site-specific extractors (Twitter/X, YouTube, Substack, etc.) are continuously updated upstream — a fork falls behind unless we actively backport. |
| Maintenance | **High.** We own the algorithm forever. New site quirks reported to Defuddle don't fix our tool until we port the patch. |
| Performance | **Highest.** No JS runtime, no IPC. Likely <5 ms cold-start, <1 ms per-call after warm. |
| Distribution | **Best.** Single small static binary (~5-10 MB). |
| Risk | **Medium.** `defuddle-rs` and `trek-rs` are early-stage per the user's note; we'd be inheriting an immature foundation. |
| Time to MVP | **Worst.** 1-3 months minimum for credible parity, depending on how much of `defuddle-rs` we can build on. |

**Verdict:** Best long-term, worst short-term. **Defer** — make
this a Phase 3 option contingent on `defuddle-rs` maturing
or on strong user pull (e.g., binary size complaints).

## Path 2: Subprocess + official CLI

Spawn `npx defuddle` (or a globally-installed `defuddle` CLI)
and parse stdout.

| Dimension | Assessment |
|-----------|------------|
| Fidelity | **Perfect.** Always upstream. Bumping the npm version is a one-line change. |
| Maintenance | **Lowest.** We don't own any extraction logic. |
| Performance | **Worst per-call.** `npx defuddle ...` spawns Node — typically 100-500 ms of cold-start overhead per invocation. Caching helps once but agents calling the tool 50× per session is hopeless. |
| Distribution | **Worst.** Hard runtime dep on Node.js. Conflicts with "production-ready single-binary" goal. |
| Risk | **Low** for correctness, **high** for adoption: users who don't have Node installed can't use the tool. |
| Time to MVP | **Best.** Half a day. Wrap a Command, parse JSON output. |

**Verdict:** **Best for Phase 1 (MVP)**, but cannot be the
permanent answer. Use it to validate the API surface, the
output format, and the agent integration story without
committing to the bigger questions yet.

## Path 3: Embedded JS runtime

Embed `deno_core` (V8 subset) or `rusty_v8` directly and
execute Defuddle's JavaScript module from Rust. No external
Node runtime; the entire JS engine ships inside our binary.

| Dimension | Assessment |
|-----------|------------|
| Fidelity | **Perfect.** We're literally running Defuddle's JS code, plus jsdom (or a smaller HTML→DOM polyfill we ship). |
| Maintenance | **Low.** Bump the npm tarball, run a build script that bundles it. |
| Performance | **Good.** ~50 ms cold-start (V8 init), ~5-15 ms per-call after warm. Snapshot V8 heap to amortize cold-start across invocations. |
| Distribution | **Single binary**, but heavy: V8 is ~30-50 MB. Acceptable if the user values "no system Node required" more than binary size — which they do based on "production-ready single binary." |
| Risk | **Medium.** `deno_core` is a real engineering project but well-supported. Bundling jsdom + Defuddle into a self-contained snapshot is the trickiest part. |
| Time to MVP | **2-4 weeks** for a Phase-2 implementation that replaces Phase 1's subprocess. |

**Verdict:** **Best for Phase 2 (production).** Single binary
+ perfect fidelity + acceptable performance. The 50 MB binary
size is a real cost but matches Deno's own distribution
strategy and is the price of perfect Defuddle parity.

### What about `boa_engine`?

`boa_engine` is a pure-Rust JS engine. Tempting because it
would avoid V8's binary cost. But:
- It's slower than V8 by ~10-50× for non-trivial code.
- Some modern JS features (private class fields, top-level
  await, etc.) are still partial.
- jsdom — which Defuddle relies on for DOM operations — is a
  large module that exercises corners `boa` doesn't always
  cover.

Worth re-evaluating if `boa` matures, but not in 2026.

## Path 4: WASM

Compile Defuddle to WebAssembly, embed via `wasmtime` or
`wasmer`.

| Dimension | Assessment |
|-----------|------------|
| Fidelity | **Perfect** if we can compile it. |
| Maintenance | **Low** if upstream ships a WASM build; **high** if we maintain a custom `wasm-pack` recipe. |
| Performance | **Excellent** post-load. WASM startup is faster than V8. Per-call latency would beat Path 3. |
| Distribution | **Single binary.** `wasmtime` adds ~5-10 MB, much less than V8. |
| Risk | **High.** Defuddle uses jsdom. Compiling jsdom to WASM is non-trivial; it relies on browser-DOM APIs that need polyfilling. Defuddle would need to be rewritten to take an already-parsed DOM rather than HTML, OR we port a minimal HTML→DOM layer to WASM. |
| Time to MVP | **6-8 weeks**, mostly spent on the jsdom-vs-WASM problem. |

**Verdict:** **Promising long-term, not viable today.**
Revisit if Defuddle upstream ever ships a WASM build (which
they might, since the broader Obsidian Web Clipper ecosystem
runs in browser environments). Until then, the engineering
cost is too high relative to Path 3.

## The recommended order

```
Phase 1 (Week 1-2):    Path 2 — subprocess MVP
Phase 2 (Week 3-6):    Path 3 — embedded deno_core
Phase 3 (open-ended):  Path 1 — native port if conditions favor
```

This sequence buys us:

1. **Fast time to value.** Phase 1 ships in days. The user
   can try `distill` and report bugs against the API design,
   the markdown output, the metadata extraction — before
   we've over-committed.
2. **Validation of the design under real use.** What does the
   metadata structure actually need to be? Which template
   knobs do agents care about? We answer these in Phase 1
   and lock them in for Phase 2.
3. **Production binary at Phase 2.** No system Node
   requirement. Single static distribution. Perfect Defuddle
   fidelity. This is the "ship it" milestone.
4. **Optional optimization at Phase 3.** Native port becomes
   attractive only if (a) `defuddle-rs` proves mature, (b)
   binary size becomes a real complaint, or (c) per-call
   latency is the bottleneck for some specific agent
   workflow. None of those are guaranteed; don't pre-pay.

## Decision triggers

When in doubt later, these are the signals that should move us
between paths:

| Signal | Move to |
|--------|---------|
| User reports "Node isn't installed" | Skip rest of Phase 1 → Phase 2 immediately |
| Phase 2 binary size > 80 MB | Investigate `boa` or Path 4 |
| Per-call latency > 200 ms | Optimize V8 snapshot, or move to Path 1 for hot paths |
| Defuddle ships a WASM build | Reconsider Path 4 |
| `defuddle-rs` reaches feature parity | Migrate to Path 1 incrementally |
| Tool gets popular and 50 MB binary becomes a barrier | Path 1 |

## What we're NOT doing

- **A general-purpose web scraper.** This tool extracts
  reader-mode content. People who want full DOM access can
  use Selenium / Playwright wrappers; that's a different
  product.
- **A search engine.** No crawling, no indexing, no
  full-text search.
- **A site cloner.** No saving full pages with assets;
  Markdown + metadata only. Image references are URLs (with
  optional download via a separate flag).
- **JavaScript execution by default.** Most pages don't need
  it. We add `--javascript` as an opt-in flag (Phase 2/3)
  using a headless browser library. See
  [`03_edge_cases.md`](03_edge_cases.md).
