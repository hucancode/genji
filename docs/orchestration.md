# Orchestration

Agents cooperate through `finish` handoffs. genji either follows them
(`--follow`) or leaves it to the caller.

## Built-in: `--follow`

```bash
genji plan "ship the feature" --follow=20
```

Runs plan → build/explore → plan → … until `plan` finishes `done`, `blocked`, or
20 handoffs have been followed. See [Agents](agents.md#following-handoffs).

## External loop

Each run ends with an `instance_end` event on stdout whose `result` is the verdict.
`--parent` records the chain.

```sh
agent=plan task="$GOAL" parent=
for _ in $(seq 10); do
  end=$(genji "$agent" "$task" ${parent:+--parent "$parent"} | jq -c 'select(.type=="instance_end")')
  [ "$(jq -r .result.status <<<"$end")" = handoff ] || { echo "$end"; break; }
  agent=$(jq -r .result.next.agent <<<"$end"); task=$(jq -r .result.next.task <<<"$end"); parent=$(jq -r .instance <<<"$end")
done
```

## Patterns

- **Plan/build/judge:** add `.genji/agents/judge.md` with `finish: handoff, blocked` and a prompt that hands off to `plan` with a verdict on the build.
- **Lead agent:** a custom agent with `spawn` and `finish: done, blocked` that splits work across subagents.
- **Parallel workers:** run several `genji build` in git worktrees with the [formal skill](formal.md); `mkdir .genji/claims/<id>` keeps them off the same ticket.
