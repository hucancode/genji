# Skills

Skills are markdown files in `.genji/skills/<name>.md` with simple frontmatter:

```markdown
---
description: House style for Rust changes
---
- Run `cargo fmt` and `cargo clippy -- -D warnings` before resolving a ticket.
- Prefer standard library over new dependencies.
```

The file name (without `.md`) is the skill name. Agents call `skill_load(name)`
to add a skill to their system prompt under `## Loaded skills`; the tool is
offered only when at least one skill file exists. Loaded skills survive
compaction and resume. Each load changes the system prompt, so the provider's
prompt cache restarts once.

`skill_load` is not part of the conversation: neither the call nor its result is
sent to the model. A turn that only loads skills leaves no message. An unknown
name is reported to the model as a one-request hint listing the available
skills. Loads are recorded with the other tool calls and appear as
`tool_call`/`tool_result` events, so `query_stats` and event consumers see them.
Retro mode edits skill files directly. See [Retro mode](retro.md).
