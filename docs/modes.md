# Modes

A mode is the pair of `(core system prompt, extended system prompt, tool set)`.
The **core** prompt is compiled in and minimal; the **extended** prompt is
stored in the database and versioned there. `retro` is the one exception:
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
prompt. Extended prompts start empty and are edited in the database by letting
retro mode call `prompt_edit`. Every change is recorded as a version and can be
rolled back (`prompt_history` / `prompt_rollback`). `prompt_edit` rejects
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

With no mode subcommand **and a task**, genji starts in **build** mode and runs:

```
build → plan → build → plan → …   until active requirements == 0 (or max_cycles)
```

State (requirements, tickets, conversation) persists across cycles in one
run. Plan mode decides whether requirements are truly met; when none remain
active, the loop stops. The `--cycle` flag applies the same cycling to an
explicit mode subcommand, starting from that mode (`genji build --cycle` starts
at build, `genji plan --cycle` starts at plan). A mode subcommand without
`--cycle` (`genji plan`, `genji build`, `genji explore`, `genji retro`) runs a
single mode instead. Subagents always run a single mode.

Running bare `genji` with **no task at all** does not start a cycle: it opens
the control socket and waits for an instruction to be sent (see
[Mid-run instructions](control-socket.md#running-with-no-instruction)).

---

## Subagents

The `spawn` tool runs **the same executable** as a subagent:

```
spawn {mode: "explore"|"plan"|"build", instructions: "...", task?: "..."}
```

- The subagent runs a **single** mode and never cycles. It reports back by
  printing its final message to stdout, which the parent captures.
- Its run is recorded with `parent_instance` and `depth`.
- Nesting is bounded by `max_subagent_depth`.
