# Modes

A mode is the pair of `(core system prompt, extended system prompt, tool set)`.
The **core** prompt is compiled in and minimal; the **extended** prompt is a
markdown file and is versioned in the database. `retro` is the one exception:
it has no extended prompt (see [Retro mode](retro.md)), so it cannot rewrite its
own instructions.

| Mode | Purpose | Writes state? |
|------|---------|---------------|
| `plan` | Turn stakeholder intent into system requirements + tickets | yes (full ticket/requirement surface) |
| `build` | Implement open tickets, verify, resolve them | reduced surface |
| `explore` | Investigate and report; no ticket/requirement tools | read/inspect only |
| `retro` | Query history and improve prompts/skills | prompt & skill editing |

The effective system prompt is:

```
<shared preamble>
<mode core prompt>          # not editable
## Extended guidance
<editable extended prompt>  # versioned in DB; retro may edit it
```

The `## Extended guidance` section is omitted for `retro`, which has no extended
prompt. Edit `prompts/plan.md`, `prompts/build.md` or `prompts/explore.md` by
hand, or let retro mode call `prompt_edit`. Both are recorded as versions and can
be rolled back (`prompt_history` / `prompt_rollback`). `prompt_edit` rejects
`mode: "retro"`.

---

## Tool matrix

| Tool | plan | build | explore | retro |
|------|:----:|:-----:|:-------:|:-----:|
| `read` `write` `edit` `ls` `bash` | ✓ | ✓ | ✓ | ✓ |
| `ticket_create` | ✓ |  |  |  |
| `ticket_read` | ✓ | ✓ |  |  |
| `ticket_resolve` `ticket_close` | ✓ | ✓ |  |  |
| `requirement_create` `requirement_update` `requirement_remove` | ✓ |  |  |  |
| `requirement_read` | ✓ | ✓ |  |  |
| `requirement_ask` | ✓ | ✓ |  |  |
| `skill_load` | ✓ | ✓ | ✓ | ✓ |
| `spawn` | ✓ | ✓ | ✓ |  |
| `query_*`, `*_skill`, `prompt_*` |  |  |  | ✓ |

- `ls` respects `.gitignore`, `.ignore`, `.git/info/exclude` and global git
  ignores. Non-recursive by default; `recursive: true` walks the tree.
- `bash` runs `bash -c <command>` in the workspace with a timeout.
- Tool results longer than `tool_result_max_bytes` are truncated (head + marker).
  See [Context management](context-management.md).

---

## Automatic mode cycling

With no mode subcommand, genji runs:

```
plan → build → plan → build → …   until active requirements == 0 (or max_cycles)
```

State (requirements, tickets, conversation) persists across cycles in one
session. Plan mode decides whether requirements are truly met; when none remain
active, the loop stops. A mode subcommand (`genji plan`, `genji build`,
`genji explore`, `genji retro`) runs a single mode instead. Subagents always
run a single mode.

---

## Subagents

The `spawn` tool runs **the same executable** as a subagent:

```
spawn {mode: "explore"|"plan"|"build", instructions: "...", task?: "..."}
```

- The subagent runs a **single** mode and never cycles. It reports back by
  printing its final message to stdout, which the parent captures.
- Its session is recorded with `parent_session` and `depth`.
- Nesting is bounded by `max_subagent_depth`.
