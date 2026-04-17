# Internet Search Tool

## Summary

Add a search tool that lets the agent query the internet during a
conversation, using self-hosted infrastructure instead of commercial
search APIs.

## Current State

Anie has no internet search capability. The agent can only work with
local files and the user's input.

## Action Items

### 1. Search backend: SearXNG
Use SearXNG as the search engine:
- Open-source, self-hosted meta search engine
- Aggregates results from Google, Bing, DuckDuckGo, and many others
- No API keys or per-query costs
- Runs as a local daemon or Docker container
- Configurable result sources and ranking

### 2. Page content extraction
For fetching and reading full page content from search results:
- Use a headless browser (Playwright or Puppeteer equivalent) or
  a simpler HTTP client with readability parsing
- Handle JavaScript-heavy sites
- Extract clean readable content, strip boilerplate
- Preserve code blocks and structured content

### 3. Tool interface
Register as a tool the agent can call:
- `search(query)` — returns a list of results (title, URL, snippet)
- `fetch(url)` — fetches and extracts readable content from a URL

Or combine into a single tool with an action parameter.

### 4. Configuration
- SearXNG instance URL (default: `http://localhost:8888`)
- Max results per query
- Content extraction settings (max page size, timeout)
- Caching for fetched pages to avoid redundant requests

### 5. Hosting considerations
- Document how to run SearXNG locally (Docker compose)
- Support connecting to a remote self-hosted instance
- Graceful degradation if SearXNG is not running

## Priority

Low — useful but requires infrastructure setup. Could be implemented
as a skill or extension first, then promoted to built-in if it proves
valuable.
