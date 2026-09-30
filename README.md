# genji

A minimal coding agent.

```bash
cargo build
genji "build me a cat classifier in rust. make no mistake"
```

By default genji is a plain coding agent. Plan mode writes its plan to
`.genji/plans/`. Turn on **OCD** to use the requirements/tickets system and an
automatic plan/build cycle:

```bash
genji --ocd "build me a cat classifier in rust. make no mistake"
genji plan --ocd "..."   # start the cycle in plan; --ocd works with any mode
```

Manage running agents

```bash
genji list                    # list running instances
genji inspect <id>            # summary
genji instruct <id> "text"    # send an instruction to a running instance
genji setplan <id> <slug>     # follow/refine .genji/plans/<slug>.md
genji stop <id>... | all      # graceful stop
```
