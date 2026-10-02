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
to pull a skill into context; the tool is offered only when at least one skill
file exists, and an unknown name returns the list of available skills. Loads are
recorded with the other tool calls, so `query_stats` reports them. Retro mode
edits skill files directly. See [Retro mode](retro.md).
