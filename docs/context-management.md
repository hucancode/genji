# Context management

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
