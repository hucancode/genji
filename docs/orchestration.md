# Orchestration

Agents cooperate through `finish` handoffs. genji does not follow them; the
caller does. The one exception is `hand_off`, which continues in a fresh
instance of the named agent, e.g. `build` ⇄ `review` (see [Agents](agents.md#hand_off)).

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
