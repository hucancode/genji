# genji

A minimal, headless coding agent.

```bash
cargo build
genji build "build me a cat classifier in rust"
genji plan "brainstorm with me to build a cat classifier spec"
```

Manage running agents

```bash
genji list                    # list running instances
genji inspect <id>            # summary from the session file
genji instruct <id> "text"    # send an instruction to a running instance
genji stop <id>... | all      # graceful stop
genji build --resume <id>     # continue a crashed or finished run
```
