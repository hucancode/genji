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

A `prune` event drops what the model no longer needs. Rewriting old messages
invalidates the provider's cached prompt prefix, so pruning is batched: every 8 new
messages the agent previews how many tokens a prune would free and prunes only when
the gain is at least 2000 tokens and 15% of the context, or the cache has been idle
for 5 minutes (so it is cold anyway) and the gain is at least 500 tokens, or the
prompt is past half the window. Between prunes the prefix stays byte-stable. Below
half the window a prune never elides old tool results (`bulk: false`), so the agent
keeps what it already read. When
the prompt reaches 80% of the effective context window (see
[Configuration](configuration.md), `preferred_context_size`) it prunes with
`prune_keep_recent`, then with `compact_keep_recent` if still over.
Replay applies the same operation, so the rebuilt context is identical:

- a `read` result is replaced by a stub when the file was written or edited later,
  or the same range was read again later;
- with `bulk`, tool results larger than 1000 bytes older than the last
  `prune_keep_recent` messages (`compact_keep_recent` at the threshold) are elided
  to a short head;
- older `write`/`edit` payloads shrink to the path and size, and older assistant
  reasoning is dropped, since the file on disk is the source of truth.

If the prompt is still over the threshold, the older conversation is summarized by
the model into sections (goal, decisions, files, commands and outcomes, open
problems, next step) and replaced with one summary message (a `compaction` event);
the system prompt and the most recent messages stay. Tool-call/result pairs are
never split.

A `read`, `ls` or `bash` call with the same arguments as an earlier one, whose
output is identical and whose earlier result is still verbatim in context, returns
`[unchanged since call <id>: …]` instead of a second copy. The call still runs, so
changed output is returned in full.

Tool output is bounded at `tool_result_max_bytes` and the full text is spilled to
`/tmp`. `bash` output keeps its head and its tail, because failures and test
summaries come last. `read` returns at most 500 lines unless `limit` is given.

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
(`preferred_context_size`, `compact_keep_recent`, `context_window`,
`tool_result_max_bytes`).
