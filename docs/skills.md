# Skills

Skills follow the [Agent Skills](https://agentskills.io/specification) format,
the same one pi uses. A skill is a directory holding `SKILL.md`:

```text
pdf-tools/
├── SKILL.md
├── scripts/extract.sh
└── references/formats.md
```

```markdown
---
name: pdf-tools
description: Extract text and tables from PDF files. Use when reading or converting PDFs.
---
Read `references/formats.md` first. Run `scripts/extract.sh <file>`.
```

| Field | Meaning |
|---|---|
| `name` | Lowercase letters, digits and single hyphens, at most 64 chars. Defaults to the directory name. |
| `description` | Required. Shown to the model to decide when the skill applies. |
| `disable-model-invocation` | `true` leaves the skill out of the prompt's skill list; it is then used only through an agent's `skills:`. |
| `license`, `compatibility`, `metadata`, `allowed-tools` | Accepted and ignored. |

A skill without a description or with an invalid name is skipped with a warning.

## Lookup

- **Directory:** every `SKILL.md` below `.agents/skills/`, found recursively in
  path order; the first skill of a name wins. Other Agent Skills tools (pi)
  read the same folder. It is the only skills location.

## Use

- **On demand:** for agents with `read`, the system prompt lists each skill as
  `- <name> — <description> (<absolute path of SKILL.md>)`. The model reads the
  `SKILL.md` with `read` when the task matches and resolves paths relative to
  the skill's directory.
- **Forced:** an agent's `skills:` list is inlined into its system prompt at
  start as `# Skill: <name>`, its description, its directory and its body.
  Forced skills are not listed again. An unknown forced skill is a startup error.
