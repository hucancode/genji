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
| `tokens` | `used`, `prompt`, `completion` | after each model call |
| `status` | `status` | progress/status text (also mirrored on stderr) |
| `compaction` | `removed`, `before`, `after`, `summary` | history is compacted |
| `error` | `message` | recoverable/terminal problems (budget, LLM, loop limit) |
| `instance_end` | `status`, `tokens_used`, `report` | the run ends |

`tool_call.id` lets a UI pair a request with its `tool_result`.

## Agent-to-agent communication

Subagents speak the machine form. The `spawn` tool runs a child genji and
reads its JSONL event stream. The child's events are returned to the model as
the `spawn` tool result: a JSON object with an `events` array. The subagent's
final report is **the `report` field of its `instance_end` event** in that array.
The returned stream is bounded so the `instance_end` event (and thus the report)
can never be truncated away.

Subagent events are **not** relayed into the parent's stdout stream, so the
parent's stream stays a faithful record of that one agent. Every run instead
appends its events to a per-instance trace file. `genji inspect` prints the path
to that file:

```bash
genji inspect <subagent_instance>
```

## Event files

Each instance writes its events to `<registry>/events/<instance>.jsonl` (by
default `~/.genji/events/`), append-only and flushed per event. The process
fails to start if this file cannot be opened. It covers runs whose events never
appeared on the parent stream — most importantly subagents — and survives after
the process exits.

To follow a live run, tail its trace file. Obtain the trace path with
`genji inspect <id>`:

```bash
tail -f ~/.genji/events/<instance>.jsonl
```

Events emitted before attaching remain available because the trace is the
complete durable record.

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

`<id>` is an instance id: a live one from `genji list`, or a finished run
(including a subagent) whose trace still exists. `inspect` prints a single JSON
object to stdout and a short human summary to stderr — no trace. It reports the
trace path, event count, whether the run ended, and, when available, the
`instance_start` metadata (workspace, mode, model, parent, task) and the live
instance fields (pid, uptime, status).

For a **live** instance it also asks the control socket for the current context
(`/context`) and adds a brief token/percentage breakdown (system prompt, system
tools, turn messages). Context is pull-only: it is never written to the event
trace, so a finished run has no context summary.

```bash
genji inspect 7ab121              # by live instance id
genji inspect 18d9ef8d77430f7e    # by finished instance id (or unique prefix)
# stdout: {"type":"instance","id":"…","trace":"…","events":12,"ended":true,"mode":"build",…}
# stderr:
# id:      18d9ef8d77430f7e
# trace:   ~/.genji/events/18d9ef8d77430f7e.jsonl
# events:  12 (ended)
# mode:    build  model: qwen2.5-coder-7b
# context: 1234 / 32768 tokens (3.8%)
#   system prompt: 500 (1.5%)
#   system tools: 2000 (6.1%)
#   turn messages: 2734 (8.3%)
```

Errors (unknown ids, unreachable sockets, missing arguments) go to stderr with a
non-zero exit code.

### Following the stream

The trace file *is* the event stream: append-only and flushed after every
event. So to follow an instance, read its `trace:` path from `genji inspect <id>`
and tail the file directly — no extra subcommand required:

```bash
genji inspect <id>
#   …
#   trace:   ~/.genji/events/<instance>.jsonl

cat  ~/.genji/events/<instance>.jsonl     # replay the trace so far
tail -f ~/.genji/events/<instance>.jsonl  # follow it live (Ctrl-C to stop)
```

Each line is a JSON object, so pipe through `jq` to filter:

```bash
tail -f ~/.genji/events/<instance>.jsonl | jq -c 'select(.type=="tool_call")'
```

The `instance_end` event is written last, so a follow that reaches it is complete.
