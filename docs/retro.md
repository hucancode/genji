# Retro

`genji retro` studies recorded sessions and improves the agent itself. It has
`read write edit ls bash` and queries `.genji/sessions/*.jsonl` with `jq` and
`grep` (format: [Events](events.md)).

Improvements are plain files, tracked by git:

- `.genji/agents/<name>.md`: agents, see [Agents](agents.md)
- `.agents/skills/<name>/SKILL.md`: skills, see [Skills](skills.md)

Keep them in git to review, diff and revert.

It measures before it edits: runs by end status, tokens per agent and run, context
peak, tool error rates and repeated calls. It reads the costliest and failed runs to
find where each first went wrong, classifies the causes, and changes the agents or
skills only for a cause seen in two or more sessions.
