# Skills

A skill is a markdown file with a `description` in the frontmatter:

```markdown
---
description: House style for Rust changes
---
- Run `cargo fmt` and `cargo clippy -- -D warnings` before finishing.
- Prefer the standard library over new dependencies.
```

Lookup: `.genji/skills/<name>.md`, then the built-in skills (`formal`, see
[Formal skill](formal.md)). A workspace file shadows a built-in of the same name.

- **On demand:** the system prompt lists every skill (name — description). An agent with `skill_load` loads one with `skill_load(name)`; the text comes back as a normal tool result.
- **Forced:** an agent's `skills:` list is rendered into its system prompt at start. An unknown forced skill is a startup error.
