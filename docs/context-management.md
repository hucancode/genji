# Context management

## Composition and budget

The prompt sent to the model has three parts: the **system prompt** (shared +
mode + Formal + extended guidance, plus any active plan), the **system tools** (the
tool definitions for the mode), and the **turn messages** (the conversation).
`ContextComposer` owns all three and the operations that shape them: pushing
messages and tool results, switching mode, compaction, and stats.

Inspect the current context:

- **Live snapshot:** `genji instruct <id> /context` sends `/context` over the
  control socket and prints the live prompt the model will receive next — the
  system prompt, tool definitions, conversation turns, and window size — read
  directly from the composer. Context is pull-only: it is never written to the
  event stream.

## Auto compaction

Before each model call, if the last reported prompt size exceeds
`compact_threshold × context_window`, the middle of the conversation is
summarized by the model and replaced with a single summary message; the system
prompt and the most recent `compact_keep_recent` messages are preserved.
Tool-call/result pairs are never split. Each compaction is logged in the
`compactions` table (before/after token estimates, removed count, summary).

## Resume

`genji --resume <id>` continues the recorded instance under the same id. Each
pruning or compaction writes the full message list to `context_checkpoints`, so
the resumed conversation is the exact list the model last saw, followed by any
messages recorded after the checkpoint; the provider's prompt cache keeps
matching. The system prompt and tools are rebuilt from the current files, with
the skills the run had loaded. Nothing is appended unless a task is given or the
transcript ends on a finished answer.

### Interrupted tool calls

Every tool call is journaled in `tool_calls` as `started` before it runs. Its
result row (`done`) and its tool message are written in one transaction, so a
call either has both or neither. On resume, each call of the last assistant turn
that has no result is settled before anything else:

| journal state | action |
| --- | --- |
| no row (never started) | run it |
| `started`, safe tool (`read`, `ls`, `write`, `plan_write`, read-only ticket/requirement/query tools) | mark the row `interrupted` and run it again |
| `started`, `spawn` | reattach the subagent (below) |
| `started`, any other tool (`bash`, `edit`, …) | result `ERROR: interrupted … it may have partially run`; the model checks state itself |

A `spawn` journals the child's instance id before launching it. To reattach,
genji uses the child's recorded report if the child has finished. If the child
is still running (an orphan of the stopped parent), genji waits for it up to
`spawn_timeout_secs`. If it died, or the wait times out, genji kills its process
group and resumes it with `--resume <child>`; the child settles its own
interrupted calls the same way. If the child never started, the spawn runs
again. The tool result carries `"reattached": true` when it comes from the
child's record.

## Retries and hints

Transport errors, 408/409/429/5xx and unparseable responses are retried with
backoff (`llm_max_retries`). A truncated response or a malformed tool call
(invalid JSON, unknown tool, missing required field) is discarded and
re-requested with a one-request `[note]` hint appended after the context; the
hint is never stored. A failed tool or a repeated call adds a hint to the next
request in the same way.

## How tool results are truncated

Every tool result is truncated to `tool_result_max_bytes` at the dispatch
boundary (UTF-8 safe, keeps the head and appends `… [N bytes truncated]`).
`bash` and `spawn` additionally cap how much they read from child processes so a
runaway command cannot exhaust memory.

See [Configuration](configuration.md) for the related keys
(`compact_threshold`, `compact_keep_recent`, `context_window`,
`tool_result_max_bytes`).
