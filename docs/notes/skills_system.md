# Skills System

## Summary

Implement support for the Agent Skills standard — on-demand capability
packages loaded from markdown files with YAML frontmatter.

## Current State

Anie has no skills system. All behavior is determined by the system prompt,
built-in tools, and user messages.

## Action Items

### 1. Skill loading
Load skills from multiple directories (checked in order, project takes
precedence):
- `~/.anie/skills/` (global user skills)
- `.anie/skills/` (project-level skills)

Each skill is a directory containing a `SKILL.md` file.

### 2. Skill format
Follow the Agent Skills standard (https://agentskills.io/specification):

```markdown
---
name: my-skill
description: What this skill does
license: MIT
allowed-tools: [read, bash, edit]
---

# My Skill

Instructions for the model when this skill is active.

## Steps
1. Do this
2. Then that
```

YAML frontmatter fields:
- `name`, `description` (required)
- `license`, `compatibility`, `metadata` (optional)
- `allowed-tools` — restrict which tools the skill can use
- `disable-model-invocation` — prevent automatic loading

### 3. Skill registration
- Register each skill as a `/skill:name` command
- When invoked, inject the skill's markdown content into the conversation
  context (either as a system prompt addition or a user message)
- Include skill descriptions in the system prompt so the model knows
  what skills are available

### 4. Skill discovery
- On startup, scan skill directories and report loaded skills
- `/reload` should re-scan for new/changed skills
- Consider a `/skills` command to list available skills

## Design Reference

Pi implements the Agent Skills standard with `/skill:name` commands. Skills
can be loaded from `~/.pi/agent/skills/`, `.pi/skills/`, and pi packages.

## Priority

Medium — useful for power users who want repeatable, project-specific
agent behaviors without modifying the core system.
