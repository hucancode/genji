# genji

A minimal, headless coding agent.

```bash
genji plan "brainstorm with me to build a cat classifier spec"
genji build "build me a cat classifier in rust"
nc -U .genji/control.sock       # window 1: type /watch to follow the run
nc -U .genji/control.sock       # window 2: type instructions, /status, /stop, /help
genji build --resume <id>       # continue a crashed or finished run
```
