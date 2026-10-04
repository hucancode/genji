# Orchestration

Agents cooperate through `finish` handoffs. genji either follows them
(`--follow`) or leaves it to the caller.

## `--follow`

```bash
genji plan "ship the feature" --follow=20
```

Runs the first agent, then each `next.agent` it hands off to, until an agent finishes `done`
or `blocked`, or 20 handoffs have been followed. See [Agents](agents.md#following-handoffs).

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
