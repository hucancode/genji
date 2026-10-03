# Formal skill: requirements and tickets

`formal` is a built-in [skill](skills.md). It teaches an agent to track work as
markdown files and to claim tickets safely when several agents run in parallel.

## Files

`.genji/requirements/<id>.md`

```markdown
---
id: 3
level: stakeholder        # stakeholder | system
status: active            # active | met
parent_id: 1              # system requirements point at a stakeholder requirement
---
Text of the requirement.
```

`.genji/tickets/<id>.md`

```markdown
---
id: 7
title: Short title
status: open              # open | in_progress | resolved | closed
priority: 2               # 1 (high) to 3 (low)
requirement_id: 3
---
What to do and how to verify it.
```

New ids are the highest existing id plus one. `mkdir .genji/claims/<id>` is the
atomic claim on a ticket; `genji reset` clears the claims.

## Using it

The model sees `formal` in the system prompt's skill list and loads it with
`skill_load` when it applies. To force it, copy the built-in agent to
`.genji/agents/` and add `skills: formal`:

```markdown
---
description: Plans against requirements and tickets
tools: read, write, edit, ls, bash, plan_write, skill_load, spawn, finish
skills: formal
---
...
```

Forcing it on `plan` and `build` and running `genji plan "satisfy the active requirements" --follow`
gives plan → build → plan cycles that end when `plan` finishes `done`.
