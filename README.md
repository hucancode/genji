# genji

A minimal coding agent.

```bash
cargo build
genji "build me a cat classifier in rust. make no mistake"
```

Manage running agents

```bash
genji list                    # list running instances
genji inspect <id>            # inspect 1 instance
genji instruct <id> "text"    # send an instruction to a running instance
genji stop <id>... | all      # graceful stop
```
