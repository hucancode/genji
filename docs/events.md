# Trace events

genji exposes the same structured run events through two machine-facing
outputs:

- **Session file** `.genji/sessions/<id>.jsonl` is the complete, durable, append-only JSONL log.
- **stdout** mirrors the JSONL stream for process-based integrations.

**stderr** is cosmetic human output: startup banners, tool progress, retries,
budget/timeout notices, warnings, and a final `[report] …` line. A machine
frontend must not depend on stderr.

Nothing structured ever goes to stderr and nothing unstructured ever goes to
stdout, so a UI (or another agent) can parse stdout line by line with no
filtering.

```bash
genji build "add a flag" 2>/tmp/genji.log
```

The final report is delivered in the `instance_end`. A human still sees it summarised on stderr.

## Events

Every event has:

| field | meaning |
| --- | --- |
| `type` | event name (below) |
| `seq` | monotonically increasing per-instance sequence number |
| `ts` | wall-clock time in milliseconds since the Unix epoch |
| `instance` | instance id |

Event types:

| `type` | fields | emitted when |
| --- | --- | --- |
| `instance_start` | `agent`, `model`, `parent`, `depth`, `task`, `pid`, `resumed` | a run begins |
| `system` | `prompt`, `tools` | once at start; **session file only** |
| `user` | `content` | task or injected instruction enters the context |
| `assistant` | `content`, `reasoning`, `tool_calls` (wire shape, raw argument strings) | the model produces a message |
| `tool_call` | `id`, `name`, `arguments` (or `raw_arguments` when not valid JSON) | the model requests a tool (display only) |
| `tool_result` | `id`, `name`, `is_error`, `duration_ms`, `result` | a tool finishes |
| `tokens` | `used`, `prompt`, `completion`, `cached` | after each model call |
| `prune` | `keep` | old tool results are elided |
| `compaction` | `summary`, `kept`, `removed`, `used` | history is summarized |
| `status` | `status` | progress text |
| `error` | `message` | budget, LLM, loop-limit, handoff-cap problems |
| `instance_end` | `status` (done/failed/stopped), `reason`, `tokens_used`, `report`, `result` | the run ends; `reason` says why a `stopped` run stopped (`token_limit`, `time_limit`, `max_iterations`, `user`) and is `null` otherwise; `result` is the `finish` verdict `{status, summary, next}` or `null` |

`tool_call.id` pairs a request with its `tool_result`.

## Session file

`.genji/sessions/<id>.jsonl` (or `<--sessions-dir>/<id>.jsonl`) receives every event; stdout mirrors all of them
except `system`. The file is an operation log of the context: `system`, `user`,
`assistant`, `tool_result`, `prune` and `compaction` replayed in order rebuild
exactly the messages the model last saw, which is what `--resume` does.
`tool_call`, `tokens`, `status` and `error` are not replayed.

## Subagents

`spawn` runs a child genji, reads its event stream and returns its handoff as the
tool result (see [Agents](agents.md#subagents)). Subagent events are not relayed
into the parent's stdout; the child's own session file holds them.

## Example

```json
{"type":"instance_start","seq":1,"instance":"18d9ab","agent":"explore","model":"qwen2.5-coder-7b","parent":null,"depth":0,"task":"read the readme","pid":4242,"resumed":false}
{"type":"user","seq":3,"instance":"18d9ab","content":"read the readme"}
{"type":"assistant","seq":5,"instance":"18d9ab","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"finish","arguments":"{\"status\":\"handoff\",\"summary\":\"s\",\"next\":{\"agent\":\"plan\",\"task\":\"README is empty\"}}"}}]}
{"type":"tool_result","seq":7,"instance":"18d9ab","id":"c1","name":"finish","is_error":false,"duration_ms":0,"result":"ok"}
{"type":"instance_end","seq":8,"instance":"18d9ab","status":"done","reason":null,"tokens_used":40,"report":"s","result":{"status":"handoff","summary":"s","next":{"agent":"plan","task":"README is empty"}}}
```

## Instance-management commands

`genji list`, `stop`, and `instruct` follow the same rule: stdout is machine
JSON (no redundant `type`/`action` wrapper), the human-readable view goes to
stderr.

```bash
genji list
# stdout: [{"id":"…","root":true,"pid":1234,"status":"idle"},…]
# stderr: the usual ID/ROOT/PID/UPTIME/WORKSPACE table (`*` = root/control owner)

genji stop <id>
# stdout: [{"id":"…","ok":true,"message":"stopping"}]

genji instruct <id> "focus on the parser"
# stdout: {"id":"…","message":"queued (1 pending)"}
```

### `genji inspect <id>` — a brief summary

`<id>` is an instance id (or unique prefix), live or finished. `inspect` reads
the instance's session file and prints a single JSON object
to stdout and a short human summary to stderr: id, agent, model, parent, depth,
task, status, tokens used, message count, start/end times and report. For a live
instance (found via `genji list`, which also supplies its workspace) it adds pid,
uptime, control socket and live status. For a finished instance, run it from the
instance's workspace or pass `--workspace`.

```bash
genji inspect 7ab121
# stdout: {"id":"7ab121",…,"status":"done","reason":null,"tokens_used":40,"messages":12}
# stderr:
# id            7ab121
# agent         build
# status        done
# …
```

Errors (unknown ids, unreachable sockets, missing arguments) go to stderr with a
non-zero exit code.

### Following the stream

The stream is the process's stdout. Redirect it to a file to keep it, and tail
that file to follow a run:

```bash
genji build "…" > run.jsonl &
tail -f run.jsonl | jq -c 'select(.type=="tool_call")'
```

The `instance_end` event is written last, so a follow that reaches it is complete.
