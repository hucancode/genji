# Retro mode

`genji retro` gives the agent tools to study its own recorded behavior and
improve itself:

| Tool | Purpose |
|------|---------|
| `query_instances` | list instances (mode, depth, tokens, status) |
| `query_messages` | read recorded messages; filter by `instance_id` to read one conversation, or by role/text |
| `query_tool_calls` | inspect tool calls (name, errors, duration, args/result) |
| `query_stats` | aggregated tool usage, error rate, skill loads, tokens, compactions |

Retro changes behavior by editing plain files with the normal `read`/`write`/
`edit` tools:

- `.genji/prompts/<mode>.md` — the extended prompt for `plan`, `build` or
  `explore`, appended to the system prompt under `## Extended guidance`.
- `.genji/skills/<name>.md` — skills, see [Skills](skills.md).

Keep these directories in git to review, diff and revert changes. The core
prompts are compiled in and not editable, and `retro` has no extended prompt, so
it cannot rewrite its own instructions.
