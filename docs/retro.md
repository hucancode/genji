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

Each fix goes where it can be enforced. A mechanical mistake gets a deterministic
check (a script a skill requires to pass, a test); an existing check that is unwired
or broken is fixed before a new one is added. A judgement call becomes a rule in
`.genji/agents/review/<name>.md`, since the review pass has the room to apply it and
the work agent's prompt does not. Missing orientation becomes a pointer or skill,
missing information becomes wider access. It never adds a "don't do X again" line to
a work agent's prompt, ties every change to a run and event, and prunes lines and
rules that no longer change behavior.
