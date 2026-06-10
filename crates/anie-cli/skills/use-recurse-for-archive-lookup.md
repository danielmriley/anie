---
name: use-recurse-for-archive-lookup
description: How to format `recurse` calls correctly — pass tool_call_ids without the parens or `id=` prefix, choose the right scope kind (message_grep vs tool_result vs summary), and avoid the literal string `(id=...)`. Load when (a) you're about to issue your first `recurse` call this run, OR (b) a recurse call returned [tool error] with bad arguments. The rlm-mode system prompt already tells you WHEN to use recurse; this skill covers HOW.
---

# When this applies

You're in `--harness-mode=rlm`. The per-turn ledger system-reminder lists prior tool calls (URLs, queries, bash commands, file paths) the agent has already issued. The user just asked a follow-up question whose answer would come from one of those prior results.

**Symptoms that this skill applies:**

- The user is asking "what did X say?" or "remind me what was on that page" or "what was the version number from earlier."
- You're about to call `web_read`, `web_search`, `read`, or `bash` with arguments that look similar to something already in the ledger.
- The conversation has been long, you can sense earlier context is gone (you're seeing `<system-reminder>` ledger entries instead of the original tool results), and you're tempted to re-fetch.

# What to do

**FIRST**, scan the ledger for the relevant prior tool call. The ledger format is one entry per tool call: `<value> (id=<call_id>)`. The `<value>` is the URL/query/command/path; the `<call_id>` is the runtime identifier.

**Then** call `recurse` with one of three scopes:

## Option 1: `message_grep` — easiest, no id needed

When you're not sure which prior result has the answer, but you can describe what you're looking for in a regex.

```json
{
  "query": "What was the install command for ripgrep on macOS?",
  "scope": {
    "kind": "message_grep",
    "pattern": "brew install|cargo install|ripgrep"
  }
}
```

The recurse sub-agent searches all archived messages by regex, reads the matches, and answers your question.

## Option 2: `tool_result` — when you know the exact prior call

When the ledger entry is unambiguous and you want the verbatim prior result.

```json
{
  "query": "What was the response status?",
  "scope": {
    "kind": "tool_result",
    "tool_call_id": "ollama_tool_call_8_2"
  }
}
```

Pass the `<call_id>` from the ledger as `tool_call_id`. Do NOT pass the parens or the literal string `(id=...)` — strip those.

## Option 3: `summary` — cheapest, gist only

When you just need the high-level idea of a prior result, not the full body.

```json
{
  "query": "What did that StackOverflow page recommend?",
  "scope": {
    "kind": "summary",
    "id": 14
  }
}
```

# Why this matters

Re-running a tool you already ran:
- Wastes the user's time (and money, if it's a paid API).
- Pollutes the active context with a result that's effectively a duplicate of something already evicted.
- Misses the harness's whole point — the archive + recurse pair is faster and cheaper than re-fetch.

If you read the ledger, find the relevant entry, and call `recurse`, you've just done in one tool call what would otherwise have been re-fetch + parse + summarize.

# Anti-pattern

Don't call `recurse` for things that are NOT in the archive. If the question is about live state (current weather, today's news, fresh stock price), and the ledger doesn't list a recent fetch of that data, reach for `web_search` / `web_read` directly. Recurse is for revisiting; web tools are for visiting.
