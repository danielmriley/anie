# 05 — Stream the built-in `read` tool instead of loading full text files

## Rationale

The built-in `read` tool caps output to 50 KiB / 2000 lines, but it
loads the entire file first:

- `crates/anie-tools/src/read.rs:58-72` — `tokio::fs::read(&abs_path)`
  reads the whole file into memory.
- `crates/anie-tools/src/read.rs:97-116` — binary detection and line
  counting happen after the full read.

This is a resource-risk finding, not a sandbox finding. The tool should
still be able to read absolute paths and parent traversal per anie's
current full-access model, but it should not allocate a huge file just
to return a truncated excerpt.

## Design

For text files, switch to a streaming read pipeline:

1. Open the file.
2. Read in chunks with `tokio::io::BufReader` or `tokio::fs::File`.
3. Detect binary content early by scanning chunks for NUL bytes.
4. Skip lines until `offset`.
5. Collect output until any of these is reached:
   - requested `limit`, if present;
   - `MAX_READ_LINES`;
   - `MAX_READ_BYTES`.
6. Stop reading once enough output has been collected.

This preserves long-running-agent behavior: there is no arbitrary
wall-clock stop. The tool simply stops reading once it has enough data to
satisfy the documented output cap.

### Footer semantics

The current code computes `total_lines` and reports a precise remaining
line count. Precise total line counts require scanning the whole file.
For very large files, prefer bounded memory and latency over exact
footer math.

Acceptable replacement:

- If the tool stops because of output caps before EOF, emit a footer like
  `[output truncated. Use offset to read more.]` without a precise
  remaining count.
- If EOF is reached cheaply, keep precise details.

Document this as a UI/tool-output change and update tests accordingly.

### Images

Image reads currently load the whole image and then enforce
`MAX_IMAGE_BYTES`. For images, use metadata first:

- `tokio::fs::metadata(&abs_path).await?.len()`
- reject above `MAX_IMAGE_BYTES` before reading the image body.

Then read and base64-encode small images as before.

## Files to touch

- `crates/anie-tools/src/read.rs`
  - Replace full-file text read with streaming/chunked logic.
  - Metadata-check images before read.
  - Adjust truncation footer/details.
- `crates/anie-tools/src/tests.rs` or module tests
  - Update read-tool truncation tests.
  - Add large-file regression coverage.
- `crates/anie-tools/src/shared.rs`
  - Optional shared streaming helpers if useful.

## Phased PRs

### PR A — Metadata pre-check for images

**Change:**

- Before reading an image file, use metadata length to reject files above
  `MAX_IMAGE_BYTES`.
- Keep existing behavior for small images.

**Tests:**

- Oversized image path is rejected without reading full body. Use a
  sparse/temp file if practical.
- Small image still returns an `Image` block.

**Exit criteria:**

- Image size cap applies before allocation.

### PR B — Streaming text read

**Change:**

- Implement chunked text reading.
- Stop after the output cap is satisfied.
- Preserve UTF-8 lossiness semantics as closely as possible. If using
  streaming UTF-8 decoding, ensure multi-byte characters split across
  chunks are handled safely.
- Maintain `offset` / `limit` behavior.

**Tests:**

- `read_tool_does_not_load_entire_large_text_file_for_small_limit`
  (behavioral proxy: a huge file with early requested lines returns
  quickly and bounded output).
- Offset/limit behavior still works.
- Long line truncation remains UTF-8 safe.
- Binary file with NUL byte returns the existing binary-file message.

**Exit criteria:**

- Text read memory usage scales with output cap/chunk size, not file
  size.

### PR C — Footer/detail cleanup

**Change:**

- Adjust details payload and footer text where exact remaining line
  counts are no longer available without scanning the full file.
- Keep backward-compatible fields where possible: `path`, `lines`,
  `bytes`, `truncated`, `offset`.
- Add a new optional detail like `precise_remaining: false` only if the
  UI or tests need to distinguish the modes.

**Tests:**

- Existing footer assertions updated to the new wording.
- Forward behavior documented in tool description if needed.

**Exit criteria:**

- Tool output remains clear and actionable.

## Test plan

- `cargo test -p anie-tools read`
- `cargo test -p anie-tools`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual smoke:
  - `read` a small text file.
  - `read` a large log file with `limit = 20`.
  - `read` with a high offset.
  - `read` an oversized image.

## Risks

- Streaming UTF-8 decoding can accidentally split characters. Prefer
  using existing safe helpers or maintain a small carry buffer between
  chunks.
- Exact line counts may change. Make the footer wording intentionally
  less precise when the tool stops early.
- Reading to a very high offset still requires scanning bytes up to that
  offset. This is acceptable; it is streaming and bounded in memory.

## Exit criteria

- No full-file allocation for ordinary text reads.
- Image cap is checked before image allocation.
- Output semantics remain useful for agents and users.

## Deferred

- Binary previews or hex dumps. Current binary-file detection remains
  enough for this finding.
