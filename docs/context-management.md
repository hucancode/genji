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

## How tool results are truncated

Every tool result is truncated to `tool_result_max_bytes` at the dispatch
boundary (UTF-8 safe, keeps the head and appends `… [N bytes truncated]`).
`bash` and `spawn` additionally cap how much they read from child processes so a
runaway command cannot exhaust memory.

See [Configuration](configuration.md) for the related keys
(`compact_threshold`, `compact_keep_recent`, `context_window`,
`tool_result_max_bytes`).
