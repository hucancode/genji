# Configuration

`.genji/config.json` is created with defaults on first run.

| Key | Default | Meaning |
|-----|---------|---------|
| `provider` | `local` | Active provider profile name (also the auth-file key when no profile matches) |
| `providers` | `{ local }` | Named endpoint profiles — see [Providers](providers.md) |
| `base_url` | `http://127.0.0.1:8080/v1` | Legacy fallback base URL |
| `api_key` | `""` | Legacy fallback explicit key |
| `api_key_env` | `""` | Legacy fallback env var |
| `auth_file` | `""` | Legacy fallback auth file|
| `default_model` | `qwen2.5-coder-7b` | Last-resort model when nothing else is set |
| `models.plan` / `.build` / `.explore` / `.retro` | `qwen2.5-coder-7b` | **Per-mode model picking** |
| `token_limit` | `2000000` | Fallback max tokens per run (prompt + completion); a provider or per-model limit overrides it |
| `time_limit_secs` | `1800` | Max wall-clock time per run |
| `compact_threshold` | `0.70` | Fraction of `context_window` that triggers compaction |
| `compact_keep_recent` | `6` | Messages kept verbatim during compaction |
| `context_window` | `32768` | Fallback model context size, used with the threshold |
| `max_output_tokens` | `4096` | Fallback `max_tokens` sent to the API |
| `tool_result_max_bytes` | `24000` | Truncation limit for tool results |
| `bash_timeout_secs` | `120` | Default `bash` timeout |
| `spawn_timeout_secs` | `900` | Subagent timeout |
| `max_tool_iterations` | `80` | Max tool rounds per mode run |
| `max_cycles` | `30` | Max plan/build cycles |
| `max_subagent_depth` | `2` | Subagent nesting limit |
| `db_path` | `.genji/genji.db` | SQLite database |
| `requirements_dir` / `skills_dir` | `.genji/requirements` / `.genji/skills` | Content dirs |
| `control_socket` | `.genji/control.sock` | Unix socket for mid-run steering |
| `control_enabled` | `true` | Open the control socket for top-level runs |
| `auto_ingest_requirements` | `true` | Load (and migrate legacy DB rows to) `.genji/requirements/**/*.md` on startup |
| `interactive` | `false` | Allow `requirement_ask` to read from the TTY |
| `verbose` | `false` | Log every tool call to stderr |

CLI flags `--verbose`, `--interactive`, `--workspace <dir>` override config.
Control subcommands `list`, `inspect <id>`, `instruct <id> <text>` and
`stop <id>...|all` steer running agents (these do not start an agent).

---

## Directory layout

`genji` is workspace-relative. Running it in a directory creates:

```
requirements/
  stakeholder/*.md         # user-authored requirements (ingested automatically)
  system/*.md
.genji/
  config.json              # runtime configuration (created on first run)
  genji.db                 # SQLite: sessions, tickets, requirements, prompts, history
  skills/
    *.md                   # skills (frontmatter + body)
  control.sock             # Unix socket for mid-run steering (while running)
  tmp/                     # scratch space for bash/spawn output
```

Extended system prompts are stored only in the database (see
[Modes](modes.md)); there is no `prompts/` directory.

Everything is configurable (see the table above).
