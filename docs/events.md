# Trace events

genji talks to **machines** over stdin and stdout, and to **humans** over the
[control socket](control-socket.md). stderr is for real errors only.

- **stdout** is the JSONL event stream, one event per line. Nothing else goes to stdout
  during a run, so a UI (or another agent) can parse it line by line with no filtering.
- **Session file** `.genji/sessions/<id>.jsonl` holds the same events, durably and append-only.
- **stdin** takes JSONL commands (below).
- **stderr** carries errors: a failed run, rejected LLM requests and their retries, a bad
  agent or skill definition, a bad stdin line. It never carries progress or reports.

```bash
genji build "add a flag" > run.jsonl 2> errors.log
```

The final report is delivered in `instance_end`. `instance_start` carries the
`control_socket` path.

## Commands on stdin

When stdin is not a terminal, genji reads one JSON object per line and applies it to the
running agent. A bad line is reported on stderr and skipped; end of input changes nothing.
A subagent's stdin is closed, so it takes commands over its socket only.

```json
{"type":"instruction","text":"focus on the parser first"}
{"type":"answer","id":"call_abc","text":"Postgres"}
{"type":"stop"}
```

`answer` replies to a pending `ask` call; take `id` from that call's `tool_call` event.
These are the same actions as the socket's plain-text commands.

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
| `instance_end` | `status` (done/failed/stopped), `reason`, `tokens_used`, `report`, `result` | the run ends; `reason` says why a `stopped` run stopped (`token_limit`, `time_limit`, `max_iterations`, `repeated_call`, `user`) and is `null` otherwise; `result` is the `finish` or `verdict` result `{status, summary, next}` (a review's `status` is `done`, `reject`, `handoff` or `blocked`) or `null` |

`tool_call.id` pairs a request with its `tool_result`.

## Session file

`.genji/sessions/<id>.jsonl` (or `<--sessions-dir>/<id>.jsonl`) receives every event; stdout mirrors all of them
except `system`. The file is an operation log of the context: `system`, `user`,
`assistant`, `tool_result`, `prune` and `compaction` replayed in order rebuild
exactly the messages the model last saw, which is what `--resume` does.
`tool_call`, `tokens`, `status` and `error` are not replayed.

## Review passes

A `review: true` agent's process emits one `instance_start`/`instance_end` pair per
pass. The review instance `<work id>-review-<n>` has agent `<name>:review` and
`parent` = the work id. After a `reject`, the work instance starts again with
`resumed: true` and appends to its own session file. The last `instance_end` of
the process carries the final verdict (see [Agents](agents.md#review-pass)).

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

## Following the stream

The stream is the process's stdout. Redirect it to a file to keep it, and tail
that file to follow a run:

```bash
genji build "…" > run.jsonl &
tail -f run.jsonl | jq -c 'select(.type=="tool_call")'
```

The `instance_end` event is written last, so a follow that reaches it is complete.
