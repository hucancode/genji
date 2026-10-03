# Trace events

genji exposes the same structured run events through two machine-facing
outputs:

- **Event file** is the complete, durable, append-only JSONL output, flushed per
  event.
- **stdout** mirrors the JSONL stream for process-based integrations.

**stderr** is cosmetic human output: startup banners, tool progress, retries,
budget/timeout notices, warnings, and a final `[report] …` line. A machine
frontend must not depend on stderr.

Nothing structured ever goes to stderr and nothing unstructured ever goes to
stdout, so a UI (or another agent) can parse stdout line by line with no
filtering.

```bash
genji "build me a cat classifier" 2>/tmp/genji.log
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
| `instance_start` | `mode`, `model`, `parent`, `depth`, `task` | a run begins |
| `user` | `content` | task or injected instruction is queued |
| `cycle` | `cycle`, `max`, `mode`, `active_requirements` | each auto-cycle iteration |
| `mode` | `mode`, `model` | the active mode/model changes |
| `assistant` | `content`, `reasoning` | the model produces a message |
| `tool_call` | `id`, `name`, `arguments` (or `raw_arguments` when not valid JSON) | the model requests a tool |
| `tool_result` | `id`, `name`, `is_error`, `duration_ms`, `result` | a tool finishes |
| `tokens` | `used`, `prompt`, `completion`, `cached` | after each model call |
| `status` | `status` | progress/status text (also mirrored on stderr) |
| `compaction` | `removed`, `before`, `after`, `summary` | history is compacted |
| `error` | `message` | recoverable/terminal problems (budget, LLM, loop limit) |
| `instance_end` | `status`, `tokens_used`, `report` | the run ends |

`tool_call.id` lets a UI pair a request with its `tool_result`.

## Agent-to-agent communication

Subagents speak the machine form. The `spawn` tool runs a child genji and
reads its JSONL event stream, keeping only its identity (`instance_start`) and
its final `instance_end`. The tool result is a JSON object with
`subagent_instance`, `mode`, `status`, `exit_code`, `timed_out`, `duration_ms`
and the subagent's `report`.

Subagent events are **not** relayed into the parent's stdout stream, so the
parent's stream stays a faithful record of that one agent. Every run records its
messages and tool calls in the workspace SQLite database, which `genji inspect`
and retro mode read.

## Example

```json
{"type":"instance_start","seq":1,"ts":1790692886453,"instance":"18d9…","mode":"build","model":"qwen2.5-coder-7b","parent":null,"depth":0,"task":"read the readme"}
{"type":"user","seq":2,"ts":1790692886453,"instance":"18d9…","content":"read the readme"}
{"type":"cycle","seq":3,"ts":1790692886454,"instance":"18d9…","cycle":1,"max":30,"mode":"build","active_requirements":0}
{"type":"tokens","seq":4,"ts":1790692886455,"instance":"18d9…","used":15,"prompt":10,"completion":5}
{"type":"tool_call","seq":5,"ts":1790692886455,"instance":"18d9…","id":"call_1","name":"read","arguments":{"path":"README.md"}}
{"type":"tool_result","seq":6,"ts":1790692886455,"instance":"18d9…","id":"call_1","name":"read","is_error":false,"duration_ms":0,"result":"     1\thello\n"}
{"type":"assistant","seq":7,"ts":1790692886455,"instance":"18d9…","content":"Done.","reasoning":null}
{"type":"instance_end","seq":8,"ts":1790692886455,"instance":"18d9…","status":"done","tokens_used":40,"report":"Done."}
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
the instance's record from the workspace database and prints a single JSON object
to stdout and a short human summary to stderr: id, mode, model, parent, depth,
task, status, tokens used, message count, start/end times and report. For a live
instance (found via `genji list`, which also supplies its workspace) it adds pid,
uptime, control socket and live status. For a finished instance, run it from the
instance's workspace or pass `--workspace`.

```bash
genji inspect 7ab121
# stdout: {"id":"7ab121",…,"status":"done","tokens_used":40,"messages":12}
# stderr:
# id            7ab121
# mode          build
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
