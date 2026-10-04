# Configuration

`.genji/config.json` is created with defaults on first run.

| Key | Default | Meaning |
|-----|---------|---------|
| `provider` | `local` | Active provider profile |
| `providers` | `{ local }` | Named endpoint profiles, see [Providers](providers.md) |
| `time_limit_secs` | `1800` | Max wall-clock time per run |
| `compact_threshold` | `0.70` | Fraction of `context_window` that triggers compaction |
| `compact_keep_recent` | `6` | Messages kept verbatim during compaction |
| `tool_result_max_bytes` | `24000` | Inline limit for tool results; larger ones are clipped and the full text goes to `.genji/tmp` |
| `bash_timeout_secs` | `120` | Default `bash` timeout |
| `spawn_timeout_secs` | `900` | Subagent timeout |
| `max_tool_iterations` | `80` | Max tool rounds per run |
| `llm_max_retries` | `6` | Retries per model request on transport errors, 408/409/429/5xx and unparseable responses (exponential backoff, honors `Retry-After`) |
| `max_subagent_depth` | `2` | Subagent nesting limit |
| `control_enabled` | `true` | Open the control socket for top-level runs |
| `token_limit` | `0` | Max tokens (prompt + completion) per run; above 0 it overrides the provider's `token_limit` |

Run flags:

| Flag | Meaning |
|---|---|
| `--workspace <dir>` | Workspace (default: current directory) |
| `--provider <name>` | Provider profile |
| `--socket <path>` | Control socket path (default `.genji/control.sock`) |
| `--sessions-dir <dir>` | Where session files are written and read (default `.genji/sessions`); spawned subagents inherit it |
| `--token-limit <n>` | Per-run token budget; overrides `token_limit` in the config and the provider; spawned subagents inherit it |

## Directory layout

```
.genji/
  config.json          # this file
  agents/*.md          # agent definitions (`genji init` writes the defaults)
  plans/*.md           # plans written with plan_write
  sessions/<id>.jsonl  # one operation log per instance
  control.sock         # control socket while running
  tmp/                 # scratch space and spilled tool results
.agents/
  skills/<name>/SKILL.md   # skills, see [Skills](skills.md)
```
