# Context management

## Composition and budget

The prompt sent to the model has three parts: the **system prompt** (the agent's
prompt, environment, project instructions, skill list, forced skills, agent list,
and for subagents a reporting section), the **tools** of the agent, and the
**turn messages**. `ContextComposer` owns all three and the operations that
shape them: push, prune and compaction.

`genji instruct <id> /context` prints the live prompt the model will receive
next. Context is pull-only: it is never written to the event stream.

## Pruning and compaction

Before each model call, if the prompt size reaches `compact_threshold ×
context_window`, tool results older than the last `compact_keep_recent` messages
are elided (a `prune` event). If still over, the older conversation is summarized
by the model and replaced with one summary message (a `compaction` event); the
system prompt and the most recent messages stay. Tool-call/result pairs are never
split.

## Resume

`genji <agent> --resume <id>` (the agent is optional) replays the session file
through the same operations the run applied: `system`, `user`, `assistant`,
`tool_result`, `prune`, `compaction`. The resumed run uses the **recorded** system
prompt, tools and model, and the recorded raw tool-call argument strings, so its
first request is a byte-identical prefix of the last one and the provider's
prompt cache keeps matching (as long as its TTL has not expired).
`GENJI_DUMP_REQUESTS=<dir>` writes each request body to `<dir>` to check this.

A truncated last line (crash mid-write) is dropped. Then:

| session ends at | action |
| --- | --- |
| `user` / `tool_result` | the request is re-sent unchanged |
| `assistant` with unanswered calls | `spawn` and `finish` run again; other calls get `ERROR: interrupted … it may have partially run`, and the model checks state itself |
| a crash during compaction | no `compaction` event was written; it compacts again |
| a finished run | the given task, or "Continue from where you left off.", is appended |

An unfinished `spawn` is settled from the child's session file: a finished child's
report is delivered; an unfinished child is stopped and resumed with
`--resume <child>`; a child that never started runs normally.

## Retries and notes

Transport errors, 408/409/429/5xx and unparseable responses are retried with
backoff (`llm_max_retries`). A response cut off by the output limit is re-requested
up to 3 times with a one-request `[note]` appended after the context. A model that
answers in plain text without `finish` gets one `[note] End by calling finish.`
Notes are never stored.

## How tool results are truncated

Every tool result is truncated to `tool_result_max_bytes` at the dispatch
boundary (UTF-8 safe, keeps the head and appends `… [N bytes truncated]`).
`bash` and `spawn` additionally cap how much they read from child processes so a
runaway command cannot exhaust memory.

See [Configuration](configuration.md) for the related keys
(`compact_threshold`, `compact_keep_recent`, `context_window`,
`tool_result_max_bytes`).
