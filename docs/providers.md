# Providers: local debug, Azure deploy

Every provider speaks the OpenAI chat-completions protocol, so the agent's
tools, agents and loop never change. You define named **profiles** once and pick
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

Azure's OpenAI-compatible endpoint takes the deployment name as `model` and an
`api-key` header:

```json
{
  "provider": "azure",
  "providers": {
    "azure": {
      "base_url": "https://<resource>.openai.azure.com/openai/v1",
      "api_key_env": "AZURE_OPENAI_API_KEY",
      "auth": "api-key",
      "model": "<deployment>",
      "max_tokens_field": "max_completion_tokens",
      "context_window": 128000,
      "max_output_tokens": 16384
    }
  }
}
```

## Endpoint profile fields

| Field | Default | Meaning |
|---|---|---|
| `base_url` | `http://127.0.0.1:8080/v1` | Requests go to `<base_url>/chat/completions` |
| `api_key` / `api_key_env` | empty | Literal key, or the env var holding it |
| `auth` | `bearer` | `bearer` sends `Authorization: Bearer`, `api-key` sends an `api-key` header |
| `model` | `qwen3-coder-30b-a3b` | Model or deployment name; an agent's `model:` overrides it |
| `max_tokens_field` | `max_tokens` | Or `max_completion_tokens` |
| `send_tool_choice` | `true` | Send `tool_choice: auto` |
| `headers` | `{}` | Extra request headers |
| `context_window` | `32768` | Model context size, used with `compact_threshold` |
| `max_output_tokens` | `8192` | Output cap sent to the API |
| `token_limit` | `4000000` | Max tokens (prompt + completion) per run |

## Same binary, two environments

`GENJI_PROVIDER=azure genji ...` selects a profile without editing the config.
Precedence: `--provider` > `$GENJI_PROVIDER` > `config.provider`.
