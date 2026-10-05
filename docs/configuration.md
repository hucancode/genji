# Configuration

`.genji/config.json` is created with defaults on first run. `--config FILE` reads another file, which must exist; no default config is written.

| Key | Default | Meaning |
|-----|---------|---------|
| `provider` | `local` | Active provider profile |
| `providers` | `{ local }` | Named endpoint profiles, see [Providers](providers.md) |
| `sessions_dir` | `.genji/sessions` | Where session files live (`--sessions-dir` overrides) |
| `agents_dir` | `.genji/agents` | Where agent definitions live (`--agents-dir` overrides) |
| `skills_dir` | `.agents/skills` | Where skills live |
| `time_limit_secs` | `1800` | Max wall-clock time per run |
| `preferred_context_size` | `0` | Project cap on the context window in tokens (0 = none). The effective window is the smallest of this, the provider's `context_window` and the model's size as the server reports it; compaction starts at 80% of it |
| `compact_keep_recent` | `6` | Messages kept verbatim during compaction |
| `prune_keep_recent` | `24` | Messages kept verbatim by the cache-aware batched prune that drops superseded reads and elides old bulky tool output, `write`/`edit` payloads and reasoning |
| `tool_result_max_bytes` | `24000` | Inline limit for tool results; larger ones are clipped and the full text goes to `/tmp` |
| `bash_timeout_secs` | `120` | Default `bash` timeout |
| `spawn_timeout_secs` | `900` | Subagent timeout |
| `ask_timeout_secs` | `600` | How long the `ask` tool waits for a human answer before using its recommended option |
| `max_tool_iterations` | `200` | Max tool rounds per run |
| `llm_max_retries` | `6` | Retries per model request on transport errors, 408/409/429/5xx and unparseable responses (exponential backoff, honors `Retry-After`) |
| `max_subagent_depth` | `2` | Subagent nesting limit |
| `token_limit` | `0` | Max tokens (prompt + completion) per run; above 0 it overrides the provider's `token_limit` |

Run flags:

| Flag | Meaning |
|---|---|
| `--workspace <dir>` | Workspace (default: current directory) |
| `--provider <name>` | Provider profile |
| `--socket <path>` | Control socket path (default `.genji/control.sock`); spawned subagents listen next to it |
| `--socket-disabled` | Do not open a control socket (subagents inherit this). Stdin commands still work |
| `--sessions-dir <dir>` | Where session files are written and read (default `.genji/sessions`); spawned subagents inherit it |
| `--token-limit <n>` | Per-run token budget; overrides `token_limit` in the config and the provider; spawned subagents inherit it |

## Directory layout

```
.genji/
  config.json          # this file
  agents/*.md          # agent definitions (`genji init` writes the defaults)
  agents/review/*.md   # review prompts of `review: true` agents
  sessions/<id>.jsonl  # one operation log per instance
  control.sock         # control socket while running
.agents/
  skills/<name>/SKILL.md   # skills, see [Skills](skills.md)
```
