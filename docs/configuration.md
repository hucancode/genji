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

`--workspace <dir>` selects the workspace; `--provider <name>` selects a profile.

## Directory layout

```
.genji/
  config.json          # this file
  agents/*.md          # agents (add or replace built-ins)
  skills/*.md          # skills
  plans/*.md           # plans written with plan_write
  sessions/<id>.jsonl  # one operation log per instance
  requirements/, tickets/, claims/   # used by the formal skill
  control.sock         # control socket while running
  tmp/                 # scratch space and spilled tool results
```
