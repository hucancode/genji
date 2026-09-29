# Providers: local debug, Azure deploy

Every provider speaks the OpenAI chat-completions protocol, so the agent's
tools, modes and loop never change. You define named **profiles** once and pick
one at run time:

```bash
GENJI_PROVIDER=local  ./target/release/genji explore "..."   # dev
GENJI_PROVIDER=azure  ./target/release/genji explore "..."   # prod
# or: ./target/release/genji --provider azure ...
```

Selection precedence: `--provider` > `$GENJI_PROVIDER` > `config.provider`.
Because the switch is an environment variable, the same binary/container image
deploys to Azure with no code change.

## Local: llama.cpp

`llama-server` exposes an OpenAI-compatible endpoint; start it with a chat
template that supports tools (`--jinja`):

```bash
llama-server -hf ggml-org/Qwen2.5-Coder-7B-Instruct-GGUF --port 8080 --jinja
```

```json
{
  "provider": "local",
  "providers": {
    "local": {
      "kind": "openai",
      "base_url": "http://127.0.0.1:8080/v1",
      "api_key": "sk-no-key-required",
      "model": "qwen2.5-coder-7b",
      "context_window": 32768
    }
  }
}
```

`api_key` is optional for llama.cpp; omit it (or leave it empty) and no auth
header is sent. `model` can be any label — llama-server ignores it unless you
run multiple models. Set `context_window` to the server's `--ctx-size` so auto
compaction triggers at the right point.

## Cloud: Azure OpenAI

Azure differs from plain OpenAI in three ways, all handled by `kind: "azure"`:

1. URL is `{base_url}/openai/deployments/{deployment}/chat/completions`.
2. `?api-version=…` is appended.
3. Auth is the `api-key` header.

The per-mode `models` (or a single `model`) are **deployment names**:

```json
{
  "provider": "azure",
  "providers": {
    "azure": {
      "kind": "azure",
      "base_url": "https://my-resource.openai.azure.com",
      "api_version": "2024-10-21",
      "api_key_env": "AZURE_OPENAI_API_KEY",
      "models": {
        "plan": "gpt-4o",
        "build": "gpt-4o",
        "explore": "gpt-4o-mini",
        "retro": "gpt-4o"
      },
      "max_tokens_field": "max_completion_tokens"
    }
  }
}
```

Use `max_tokens_field: "max_completion_tokens"` for reasoning/`o`-series
deployments that reject `max_tokens`; set `send_tool_choice: false` if a
deployment rejects `tool_choice`. `extra_headers` / `extra_query` pass anything
else through.

## Endpoint profile fields

| Field | Default | Meaning |
|-------|---------|---------|
| `kind` | `openai` | `openai` (llama.cpp, DeepSeek, OpenAI, OpenRouter, …) or `azure` |
| `base_url` | inherited | Service root (no trailing `/chat/completions`) |
| `api_key` | `""` | Explicit key (highest priority) |
| `api_key_env` | inherited | Env var consulted next |
| `auth_file` / `auth_key` | inherited | pi auth file and the key inside it |
| `auth` | from `kind` | `bearer` or `api-key` |
| `api_version` | `""` | Azure `api-version` query value |
| `model` | `""` | Single model/deployment for all modes |
| `models.plan` / `.build` / `.explore` / `.retro` | inherited | Per-mode model/deployment |
| `max_tokens_field` | `max_tokens` | Or `max_completion_tokens` |
| `context_window` | inherited | Override the top-level context window (e.g. a small local context) |
| `max_output_tokens` | inherited | Override the top-level `max_output_tokens` |
| `send_tool_choice` | `true` | Send `tool_choice: "auto"` |
| `extra_headers` / `extra_query` | `{}` | Extra request headers / query params |

The field resolution order is: profile single `model` → profile per-mode
`models` → top-level per-mode `models` → `default_model`. Any field a profile
leaves empty is inherited from the legacy top-level fields, so a flat config
(no `providers`) keeps working exactly as before.

API key order: `api_key` → `$api_key_env` → `auth_file[auth_key].key`. If none
is found the key is empty and no auth header is sent (fine for a local server,
and Azure will return a clear 401).

## Same binary, two environments

Keep both profiles in one `agent.config.json` (see
`examples/agent.config.example.json`) and choose with the env var. Typical
split: `explore` on the cheap/local model, `build`/`plan`/`retro` on the strong
one.
