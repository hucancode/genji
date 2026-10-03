# Agents

An agent is a markdown file: frontmatter plus the system prompt.

```markdown
---
description: Reviews a diff and reports problems     # shown in `genji --help` and in other agents' prompts
tools: read, ls, bash, finish                        # read write edit ls bash plan_write skill_load spawn finish
skills: formal                                       # optional: rendered into the system prompt at start
finish: handoff, blocked                             # optional: statuses `finish` accepts (default: done, handoff, blocked)
model: some-model                                    # optional: overrides the provider's model
---
You review changes. ...
```

Built-ins ship in the binary: `plan`, `build`, `explore`, `retro`. A file
`.genji/agents/<name>.md` adds an agent or replaces the built-in of that name.
Every agent is a subcommand: `genji <name> "task"`.

Names that collide with a command (`list stop instruct inspect reset help`) and
agents that list an unknown tool are skipped with a warning on stderr.

| agent | tools | `finish` | role |
|---|---|---|---|
| `plan` | read write edit ls bash plan_write skill_load spawn finish | done, handoff, blocked | coordinator; the only built-in agent that can declare the goal done |
| `build` | read write edit ls bash skill_load spawn finish | handoff, blocked | implements a step, hands evidence back to `plan` |
| `explore` | read ls bash skill_load spawn finish | handoff, blocked | investigates, hands findings back to `plan` |
| `retro` | read write edit ls bash skill_load finish | done, blocked | improves agents and skills from recorded sessions |

## Finishing and handoff

An agent ends a run by calling `finish`:

```json
{ "status": "done | handoff | blocked", "summary": "...", "next": { "agent": "build", "task": "..." } }
```

- `done`: the goal is achieved and verified.
- `handoff`: this agent's part is done; `next.agent` continues with `next.task`. `next` is required for `handoff` and not allowed otherwise. `next.task` must stand alone: the next agent starts with a fresh context.
- `blocked`: a human must step in.

`finish` rejects statuses outside the agent's `finish:` list, unknown agents and
an empty summary. Every agent with `finish` sees the list of agents in its system
prompt. The verdict is recorded in `instance_end.result`; it is `null` when the
model stops without calling `finish`.

The exit code reports how the run went, not the verdict: 0 finished, 1 LLM
failure, 2 stopped (limit or `stop`).

## Following handoffs

`genji plan "goal" --follow[=N]` (N defaults to 10) follows handoffs inside the
same process. Each handoff starts a new instance (new id, `parent` = the previous
one, its own session file, fresh context, the next agent's prompt and tools),
behind the same control socket. `genji list` shows the instance that is running
now. The chain stops on `done`, `blocked`, a missing verdict, a failed or stopped
run, or after N handoffs (an `error` event records the cap). `stop` ends the chain.

Without `--follow`, genji runs one agent and exits; the handoff is data in
`instance_end`. See [Orchestration](orchestration.md).

## Subagents

`spawn {agent, instructions}` runs a child genji and returns its report as the
tool result:

```json
{ "subagent": "<id>", "agent": "explore", "status": "handoff|blocked", "report": "...", "run": "done|failed|stopped|timed_out" }
```

A subagent reports through the same `finish`: it hands off to its parent's agent
with the report in `next.task`, whether or not `--follow` is set. If it crashes,
times out or never calls `finish`, genji returns `status: blocked` with its last
output. The child id is `<parent id>-<tool call id>`, so a resumed parent finds
the child's session: a finished child's report is delivered, an unfinished child
is resumed. Depth is capped by `max_subagent_depth`.
