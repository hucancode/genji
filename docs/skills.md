# Skills

Skills are markdown files in `skills/` with simple frontmatter:

```markdown
---
name: rust-style
description: House style for Rust changes
---
- Run `cargo fmt` and `cargo clippy -- -D warnings` before resolving a ticket.
- Prefer standard library over new dependencies.
```

Agents call `skill_load(name)` to pull a skill into context (loads are counted
in the DB for retrospection). Retro mode can create/edit skills
(`write_skill`, `edit_skill`); every change is versioned (`skill_history`,
`skill_rollback`). See [Retro mode](retro.md).
