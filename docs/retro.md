# Retro mode

`genji retro` gives the agent tools to study its own recorded behavior and
improve itself:

| Tool | Purpose |
|------|---------|
| `query_sessions` | list sessions (mode, depth, tokens, status) |
| `query_session` | dump a session's messages |
| `query_messages` | search messages by role/text/session |
| `query_tool_call` | inspect tool calls (name, errors, duration, args/result) |
| `query_stats` | aggregated tool usage, error rate, skill loads, tokens |
| `list_skills` / `read_skill` | inspect skills and their version history |
| `write_skill` / `edit_skill` | create/improve skills (versioned) |
| `skill_history` / `skill_rollback` | browse / revert skill versions |
| `prompt_read` / `prompt_edit` | read / edit the extended prompt for `plan`/`build`/`explore` |
| `prompt_history` / `prompt_rollback` | browse / revert prompt versions |

Every prompt and skill change is appended to a version table, so any previous
version can be restored. The core prompt is intentionally **not** editable, and
neither is `retro` itself: its mode has no extended prompt (it is fixed so it
cannot rewrite its own instructions). Retro may only change the extended part of
the other modes.
