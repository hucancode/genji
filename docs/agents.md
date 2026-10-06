# Agents

An agent is a markdown file: frontmatter plus the system prompt.

```markdown
---
description: Reviews a diff and reports problems     # shown in `genji --help` and in other agents' prompts
tools: read, ls, bash, finish                        # read write edit ls bash plan_write spawn ask hand_off finish verdict
skills: formal                                         # optional: skills inlined into the system prompt at start
context: docs/notes.md                                 # optional: files (or `dir/` listings) put in front of a fresh instance's task, after which come those of forced skills (`metadata.context`)
finish: handoff, blocked                             # optional: statuses `finish` accepts (default: done, handoff, blocked)
spawns: explore                                      # optional: agents `spawn` may start, listed in the prompt (default: all)
model: some-model                                    # optional: overrides the provider's model
internal: true                                       # optional: reached only through `hand_off`/`spawn`; front ends do not offer it for a new session
review: true                                         # optional: each submitted run is judged by a review pass, see below
---
You review changes. ...
```

`.genji/agents/` is the single source of agent definitions: the engine loads
exactly the `<name>.md` files found there and assumes nothing about their names or
contents. A deleted file stays deleted.

`genji init [agent...] [--force] [--workspace DIR]` writes the default agents
(`plan`, `build`, `explore`, `retro`) and the review prompts of `plan` and `build`
(`review/plan.md`, `review/build.md`) into that directory and prints
`{"written":[...],"skipped":[...]}` (agents as `<name>`, review prompts as `review/<name>`). Existing files are kept unless `--force`;
naming agents limits what is written (and, with `--force`, what is reset). Running an agent
in a workspace without `.genji/agents/` runs `init` first. With the directory present,
nothing is added to it, and an empty directory means no agents.
Every agent is a subcommand: `genji <name> "task"`.
`genji help agent --json` prints the agents in `.genji/agents/` as
`[{name, description, tools, skills, finish, model, internal, review}]`; `genji help tool --json`
prints the tools as `[{name, description, parameters}]`.

Names that collide with a command (`init help`) and
agents that list an unknown tool are skipped with a warning on stderr.

The files `genji init` writes:

| agent | tools | `finish` | role |
|---|---|---|---|
| `plan` | read write edit ls bash plan_write spawn ask finish | done, blocked; reviewed | breaks the request into requirements (or follows a requirements skill), settles decisions through `ask`, delegates fact-finding to `explore`, and writes a self-contained plan to `docs/notes/` (or tickets, under a ticket skill): steps with acceptance criteria, the tests that prove them, seed data, and a test and verification strategy; never writes code or tests, never hands off |
| `build` | read write edit ls bash spawn finish | done, blocked; reviewed | implements and verifies; delegates exploration to `explore` via `spawn`; `finish done` submits the work to its review pass |
| `explore` | read ls bash spawn finish | handoff, blocked | internal; investigates read-only, hands terse `path:line` findings back to the agent that spawned it |
| `retro` | read write edit ls bash finish | done, blocked | improves agents and skills from recorded sessions |

## The `ask` tool

`ask {question, options (2-6), recommended}` blocks the run until a human answers or
`ask_timeout_secs` (default 600) passes. `recommended` must be one of `options`. The call
is logged as a `tool_call` before it blocks, so a pending ask is a `tool_call` without a
`tool_result`. A human answers over the control socket with
`/answer <callId> <json string>`, where `callId` is that tool call's id (see
[Control socket](control-socket.md)). Results:

- `answer: <option>`
- `answer (free text): <text>` when the answer is not one of `options`
- `answer: <recommended> (no reply within Ns; recommended option used)` on timeout
- an error result when the run is stopped while waiting

## Finishing and handoff

An agent ends a run by calling `finish`:

```json
{ "status": "done | handoff | blocked", "summary": "...", "next": { "agent": "build", "task": "..." } }
```

- `done`: the goal is achieved and verified.
- `handoff`: this agent's part is done; `next.agent` continues with `next.task`. `next` is required for `handoff` and not allowed otherwise. `next.task` must stand alone: the next agent starts with a fresh context. Longer handoff notes go in `/tmp/handoff-<short>.txt`, never in the repository.
- `blocked`: a human must step in.

`finish` rejects statuses outside the agent's `finish:` list, unknown agents and
an empty summary. Every agent with `finish` sees the list of agents in its system
prompt. The verdict is recorded in `instance_end.result`; it is `null` when the
model stops without calling `finish`.

The exit code reports how the run went, not the verdict: 0 finished, 1 LLM
failure, 2 stopped (limit or `stop`).

## `hand_off`

`hand_off {agent, task}` ends the run and continues in the same process with a
fresh instance of `agent` (it may be the caller) working on `task`: new id,
`parent` = the previous one, its own session file, empty context, the next
agent's prompt and tools, behind the same control socket. `task` must stand alone.
Chains are not capped; `/stop` on the control socket ends one. Subagents cannot `hand_off`.

A review pass's `handoff` verdict uses the same path (see [Review pass](#review-pass)).

A `finish` handoff is not followed; it is data in `instance_end`. See
[Orchestration](orchestration.md).

## Review pass

An agent with `review: true` runs in two passes. Its review prompt is
`<agents dir>/review/<name>.md`; an agent with the flag and no such file is skipped
with a warning.

1. **Work pass**: the agent's own run. It edits, asks the human, and receives the
   instructions sent over the control socket.
2. **Review pass**: when the work pass calls `finish done` or runs out of tool
   iterations, and its largest prompt reached `review_threshold` (default 40%) of the
   effective context window (see [Configuration](configuration.md)), a fresh instance `<work id>-review-<n>` (agent `<name>:review`,
   `parent` = the work id) starts with an empty context. Its system prompt is the
   review prompt, with the same environment, project instructions and skills. Its tools are the
   work tools without `hand_off`/`finish`, plus `verdict`. Its task is built from the work
   instance's session file, so compaction and resumes lose nothing: the request (the
   first `instance_start` task), follow-up requests (the tasks of later resumes), the user instructions, the `ask` questions and
   answers, earlier review findings, and the work pass's last report.

`verdict {verdict, notes}` ends the review:

- `done`: the request is met; the run ends. If user instructions are queued at that
  moment, the work pass continues instead and receives them.
- `reject`: the work instance resumes in its own context (same id and session
  file), with `[review rejected]` and `notes` as the next user message, and is
  reviewed again when it submits.
- `handoff`: a fresh work instance continues with `notes` as its task, like `hand_off`.
- `blocked`: a human must step in; the run ends.

A review pass leaves queued user instructions for the work pass. Only top-level
runs are reviewed; a spawned subagent reports to its parent. Reject loops are
bounded by the run limits (time, tokens), like `hand_off` chains.

## Subagents

`spawn {agent, instructions}` runs a child genji and returns its report as the
tool result:

```json
{ "subagent": "<id>", "agent": "explore", "status": "handoff|blocked", "report": "...", "run": "done|failed|stopped|timed_out" }
```

A subagent reports through the same `finish`: it hands off to its parent's agent
with the report in `next.task`. If it crashes,
times out or never calls `finish`, genji returns `status: blocked` with its last
output. The child id is `<parent id>-<tool call id>`, so a resumed parent finds
the child's session: a finished child's report is delivered, an unfinished child
is resumed. Depth is capped by `max_subagent_depth`.
