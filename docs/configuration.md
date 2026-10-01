# Configuration

`.genji/config.json` is created with defaults on first run.

| Key | Default | Meaning |
|-----|---------|---------|
| `provider` | `local` | Active provider profile name (also the auth-file key when no profile matches) |
| `providers` | `{ local }` | Named endpoint profiles — see [Providers](providers.md) |
| `token_limit` | `4000000` | Fallback max tokens per run (prompt + completion); a provider or per-model limit overrides it |
| `time_limit_secs` | `1800` | Max wall-clock time per run |
| `compact_threshold` | `0.70` | Fraction of `context_window` that triggers compaction |
| `compact_keep_recent` | `6` | Messages kept verbatim during compaction |
| `context_window` | `32768` | Fallback model context size, used with the threshold |
| `max_output_tokens` | `8192` | Fallback `max_tokens` sent to the API |
| `tool_result_max_bytes` | `24000` | Inline limit for tool results; larger results are spilled to `tmp_dir` and truncated with a pointer to the log |
| `bash_timeout_secs` | `120` | Default `bash` timeout |
| `spawn_timeout_secs` | `900` | Subagent timeout |
| `max_tool_iterations` | `80` | Max tool rounds per mode run |
| `max_cycles` | `30` | Max plan/build cycles |
| `max_subagent_depth` | `2` | Subagent nesting limit |
| `db_path` | `.genji/genji.db` | SQLite database |
| `requirements_dir` / `plans_dir` / `tickets_dir` / `skills_dir` | `.genji/requirements` / `.genji/plans` / `.genji/tickets` / `.genji/skills` | Content dirs |
| `tmp_dir` | `.genji/tmp` | Scratch space for bash/spawn output and spilled tool results |
| `control_socket` | `.genji/control.sock` | Unix socket for mid-run steering |
| `control_enabled` | `true` | Open the control socket for top-level runs |
| `auto_ingest_requirements` | `true` | Load `.genji/requirements/**/*.md` on Formal startup |

CLI flag `--workspace <dir>` overrides config.
Control subcommands `list`, `inspect <id>`, `instruct <id> <text>` and
`stop <id>...|all` steer running agents (these do not start an agent).

---

## Directory layout

`genji` is workspace-relative. Running it in a directory creates:

```
.genji/
  requirements/*.md        # requirements, level in frontmatter (ingested automatically)
  tickets/*.md             # all formal-mode tickets, including closed tickets
  config.json              # runtime configuration (created on first run)
  genji.db                 # SQLite: instances, messages, prompts, and history
  plans/
    *.md                   # implementation plans written by plan mode
  skills/
    *.md                   # skills (frontmatter + body)
  control.sock             # Unix socket for mid-run steering (while running)
  tmp/                     # scratch space for bash/spawn output and spilled tool results
```

Extended system prompts are stored only in the database (see
[Modes](modes.md)); there is no `prompts/` directory.

Everything is configurable (see the table above).
