# genji

A minimal, headless coding agent. Agents are markdown files; every agent is a subcommand.

```bash
cargo build
genji build "add a --verbose flag to the CLI"
genji plan "build me a cat classifier in rust" --follow     # plan -> build -> plan ... until plan finishes `done`
```

Built-in agents: `plan`, `build`, `explore`, `retro` (see [Agents](docs/agents.md)). `genji --help` lists them.

Manage running agents

```bash
genji list                                   # list running instances
genji inspect <id>                           # summary from the session file
genji instruct <id> "text"                   # send an instruction to a running instance
genji stop <id>... | all                     # graceful stop
genji reset [-y]                             # delete sessions, plans and claims
genji build --resume <id>                    # continue a crashed or finished run
```

Docs: [Agents](docs/agents.md) · [Skills](docs/skills.md) · [Formal skill](docs/formal.md) · [Orchestration](docs/orchestration.md) · [Configuration](docs/configuration.md) · [Providers](docs/providers.md) · [Events](docs/events.md) · [Context](docs/context-management.md) · [Control socket](docs/control-socket.md) · [Retro](docs/retro.md)
