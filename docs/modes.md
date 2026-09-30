# Modes

A mode is the pair of `(core system prompt, extended system prompt, tool set)`.
The **core** prompt is compiled in and minimal; the **extended** prompt is
stored in the database and versioned there. `retro` is the one exception:
it has no extended prompt (see [Retro mode](retro.md)), so it cannot rewrite its
own instructions.

| Mode | Purpose | Writes state? |
|------|---------|---------------|
| `plan` | Produce and persist an implementation plan | plans; requirements/tickets only with OCD |
| `build` | Implement requested changes and verify | code; tickets only with OCD |
| `explore` | Investigate and report; never touches the ticket system | read/inspect only |
| `retro` | Query history and improve prompts/skills | prompt & skill editing |

The effective system prompt is:

```
<shared preamble>
<mode core prompt>          # not editable
<OCD guidance>              # only when --ocd is on (plan/build)
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

The **OCD** column applies to `plan`/`build` only when the `--ocd` flag (or the
`ocd` subcommand) is on. Requirement/ticket tools are never exposed in
`explore`, `retro`, or in `plan`/`build` runs without OCD. See
[OCD mode](#ocd-requirements--tickets) below.

| Tool | plan | build | explore | retro |
|------|:----:|:-----:|:-------:|:-----:|
| `read` `write` `edit` `ls` `bash` | ✓ | ✓ | ✓ | ✓ |
| `plan_write` | ✓ |  |  |  |
| `skill_load` | ✓ | ✓ | ✓ | ✓ |
| `spawn` | ✓ | ✓ | ✓ |  |
| `ticket_create` (OCD) | ✓ |  |  |  |
| `ticket_read` `ticket_claim` `ticket_update` (OCD) | ✓ | ✓ |  |  |
| `ticket_close` (OCD) | ✓ | ✓ |  |  |
| `requirement_create` `requirement_update` `requirement_remove` (OCD) | ✓ |  |  |  |
| `requirement_read` `requirement_tree` `requirement_ask` (OCD) | ✓ | ✓ |  |  |
| `query_*`, `*_skill`, `prompt_*` |  |  |  | ✓ |

- `ls` respects `.gitignore`, `.ignore`, `.git/info/exclude` and global git
  ignores. Non-recursive by default; set `max_depth` to walk the tree
  (0 lists only immediate children, N walks N levels deep).
- `bash` runs `bash -c <command>` in the workspace with a timeout.
- Tool results longer than `tool_result_max_bytes` are truncated (head + marker).
  See [Context management](context-management.md).

---

## Plans

`plan` mode persists its plan with `plan_write` as a markdown file under
`plans_dir` (default `.genji/plans/`), named `<title-slug>.md`. Writing the plan
to disk (rather than only reporting it) means it outlives the run and can be
reviewed, versioned, or reused. Call `plan_write` again with the same title to
refine an existing plan.

Without OCD the plan file is the only artifact. With
[OCD](#ocd-requirements--tickets) on, plan mode *additionally* refines the
requirements/tickets system, and the plan should stay consistent with it.

A user can point a running agent at a specific plan with `/setplan <slug>`
(or `genji setplan <id> <slug>`) in **any** mode. The selection is injected
into the system prompt for the rest of the run: `plan` refines the file,
`build` follows it and reports changes it needs. See
[Mid-run instructions](control-socket.md#selecting-a-plan).


---

## OCD: requirements & tickets

OCD is an opt-in **flag**, not a mode or subcommand. It is off by default, so a
plain genji run has no requirement or ticket tools at all. Enable it with any
of:

```bash
genji --ocd "build me a cat classifier"
genji plan --ocd "..."    # --ocd works with plan/build; the run also cycles
```

When OCD is on:

- `plan` and `build` gain the full requirements/tickets surface
  (`requirement_*`, `ticket_*`, plus `ticket_claim`, `ticket_update`,
  and `requirement_tree`).
- The run **auto-cycles** between `plan` and `build`, starting from the chosen
  mode (default `build`), until there are no `active` requirements or
  `max_cycles` is reached.
- `explore` and `retro` still never expose requirement/ticket tools.

`spawn` propagates the flag, so a subagent runs with the same ticket surface.

---

## Automatic mode cycling

Cycling is part of OCD. With OCD on and a task, genji starts in **build** mode
and runs:

```
build → plan → build → plan → …   until active requirements == 0 (or max_cycles)
```
State (requirements, tickets, conversation) persists across cycles in one
run. Plan mode decides whether requirements are truly met; when none remain
active, the loop stops. `genji plan --ocd` starts the cycle at plan instead.
Without OCD there is no auto-cycle: `genji plan`, `genji build`, `genji
explore`, and `genji retro` each run a single mode. Subagents always run a
single mode.

With OCD on and **no explicit task**, active requirements are used as the work
queue. Running bare `genji` with **no task and no OCD** does not start a cycle:
it opens the control socket and waits for an instruction to be sent (see
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
