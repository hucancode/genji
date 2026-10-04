# genji

A minimal, headless coding agent. Agents are markdown files; every agent is a subcommand.

```bash
cargo build
genji build "add a --verbose flag to the CLI"
genji plan "build me a cat classifier in rust" --follow     # plan -> build -> plan ... until plan finishes `done`
```

Manage running agents

```bash
genji list                                   # list running instances
genji inspect <id>                           # summary from the session file
genji instruct <id> "text"                   # send an instruction to a running instance
genji stop <id>... | all                     # graceful stop
genji build --resume <id>                    # continue a crashed or finished run
```
