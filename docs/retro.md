# Retro

`genji retro` studies recorded sessions and improves the agent itself. It has
`read write edit ls bash` and queries `.genji/sessions/*.jsonl` with `jq` and
`grep` (format: [Events](events.md)).

Improvements are plain files, tracked by git:

- `.genji/agents/<name>.md`: agents, see [Agents](agents.md)
- `.agents/skills/<name>/SKILL.md`: skills, see [Skills](skills.md)

Keep them in git to review, diff and revert.
