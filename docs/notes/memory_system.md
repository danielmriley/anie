# Memory System

## Summary

Implement a persistent memory system that lets anie learn about its user,
their projects, preferences, and patterns over time.

## Current State

Anie has no memory across sessions. Each session starts fresh with only
the system prompt and context files. There is no mechanism for the agent
to remember corrections, preferences, or project-specific knowledge.

## Early Design Thinking

### Graph/cluster-based approach
Inspired by tools like Obsidian:
- Memories stored as nodes with links between related concepts
- Links connect people, projects, preferences, corrections, patterns
- Clusters of related memories form naturally over time
- Operations on clusters: surface related context, detect contradictions,
  notice patterns

### What to remember
- User corrections ("I prefer X over Y")
- Project-specific facts ("this codebase uses convention Z")
- Tool usage patterns ("user always wants verbose bash output")
- People and relationships ("Alice maintains the auth module")
- Non-obvious codebase facts discovered during sessions

### Retrieval
- Before each turn, retrieve relevant memories based on the current
  conversation context
- Inject retrieved memories into the system prompt or as context
- Relevance scoring: semantic similarity, recency, link density

### Storage
- File-based (markdown or JSON) in `~/.anie/memory/`
- Or SQLite for structured queries and indexing
- Must support incremental updates, not full rewrites

## Open Questions

- What format captures memories with enough structure for linking
  but enough flexibility for varied content?
- How to avoid memory bloat over time? Consolidation? Expiry?
- How to handle contradictions between old and new memories?
- Should the agent decide what to remember, or should the user
  explicitly trigger memory writes?
- How to make retrieval fast enough to not add latency to every turn?

## Priority

Low — this is a complex, long-term effort with high potential impact.
Needs significant design iteration before implementation begins.
