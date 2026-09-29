# Configuration

`agent.config.json` is created with defaults on first run.

| Key | Default | Meaning |
|-----|---------|---------|
| `provider` | `deepseek` | Active provider profile name (also the auth-file key when no profile matches) |
| `providers` | `{}` | Named endpoint profiles — see [Providers](providers.md) |
| `base_url` | `https://api.deepseek.com` | Legacy fallback base URL |
| `api_key` | `""` | Legacy fallback explicit key |
| `api_key_env` | `DEEPSEEK_API_KEY` | Legacy fallback env var |
| `auth_file` | `~/.pi/agent/auth.json` | Legacy fallback pi auth file |
| `default_model` | `deepseek-flash` | Last-resort model when nothing else is set |
| `models.plan` / `.build` / `.explore` / `.retro` | `deepseek-flash`… | **Per-mode model picking** |
| `token_limit` | `2000000` | Max tokens per run (prompt + completion) |
| `time_limit_secs` | `1800` | Max wall-clock time per run |
| `compact_threshold` | `0.70` | Fraction of `context_window` that triggers compaction |
| `compact_keep_recent` | `6` | Messages kept verbatim during compaction |
| `context_window` | `1000000` | Model context size, used with the threshold |
| `max_output_tokens` | `16000` | `max_tokens` sent to the API |
| `tool_result_max_bytes` | `24000` | Truncation limit for tool results |
| `bash_timeout_secs` | `120` | Default `bash` timeout |
| `spawn_timeout_secs` | `900` | Subagent timeout |
| `max_tool_iterations` | `80` | Max tool rounds per mode run |
| `max_cycles` | `30` | Max plan/build cycles |
| `max_subagent_depth` | `2` | Subagent nesting limit |
| `db_path` | `.genji/genji.db` | SQLite database |
| `requirements_dir` / `skills_dir` / `prompts_dir` | `requirements` / `skills` / `prompts` | Content dirs |
| `control_socket` | `.genji/control.sock` | Unix socket for mid-run steering |
| `control_enabled` | `true` | Open the control socket for top-level runs |
| `auto_ingest_requirements` | `true` | Ingest `requirements/**/*.md` on startup |
| `interactive` | `false` | Allow `requirement_ask` to read from the TTY |
| `verbose` | `false` | Log every tool call to stderr |

CLI flags `--verbose`, `--interactive`, `--workspace <dir>` override config.
Control clients: `--send <TEXT>`, `--status`, `--stop` (these do not start an
agent).

---

## Directory layout

`genji` is workspace-relative. Running it in a directory creates:

```
agent.config.json          # runtime configuration (created on first run)
requirements/
  stakeholder/*.md         # user-authored requirements (ingested automatically)
  system/*.md
skills/
  *.md                     # skills (frontmatter + body)
prompts/
  plan.md build.md explore.md   # editable extended system prompts (retro is fixed)
.genji/
  genji.db                # SQLite: sessions, tickets, requirements, history
  control.sock             # Unix socket for mid-run steering (while running)
  tmp/                     # scratch space for bash/spawn output
```

Everything is configurable (see the table above).
