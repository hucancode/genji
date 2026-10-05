# Control socket

While a top-level run is active, genji listens on a Unix-domain socket
(`.genji/control.sock` by default, or the path given with `--socket`). It is the
interface for humans: steer a run **without restarting** and follow it as readable text.
Machines use the JSONL event stream on stdout and JSONL commands on stdin
(see [Trace events](events.md)).

The socket is optional. `--socket-disabled` turns it off for a run (subagents inherit that), and
building without the default `socket` cargo feature (`cargo build --no-default-features`)
removes it from the binary, along with `/watch`. Either way an agent still reads JSONL commands
from stdin and writes JSONL events to stdout; only the human interface is gone. With no socket,
an `ask` call is answered by a stdin `answer` command or, after `ask_timeout_secs`, takes the
recommended option.

A typical session uses two terminals:

```bash
# window 1: follow the run
nc -U .genji/control.sock
/watch

# window 2: keep one connection open and type
nc -U .genji/control.sock
focus on the parser first; ignore the docs for now
/status
/stop
```

Connections are long-lived: every line is a command and gets one reply line. For
a one-shot command, close the write side after sending (`nc -N -U` on OpenBSD netcat):

```bash
printf '/status\n' | nc -N -U .genji/control.sock    # -> status: idle
```

## Commands

| line | effect |
| --- | --- |
| any text not starting with `/` | queued as a user instruction (`queued (1 pending)`) |
| `/watch` | stream events on this connection as readable text; the connection still accepts commands |
| `/status` | current status line |
| `/context` | the live context as JSON: `context_window`, `last_prompt_tokens`, `messages`, `tools` |
| `/answer <callId> <text>` | answer a pending `ask` call; `<text>` is a JSON string or plain text |
| `/stop` | graceful stop |
| `/ping` | liveness check (`pong`) |
| `/help` | list the commands |

Any other line starting with `/` is rejected with `error: unknown command …`.

`/watch` shows user and assistant messages, tool calls and results, compactions,
prunes, errors, and the start and end of each instance. An `ask` call prints its
question, options and the `/answer` line to paste. A watcher that stops reading
misses events; it never slows the agent.

Every agent has a socket, subagents included. A subagent listens next to its parent's, at
`.genji/control-<instance id>.sock`, and its `instance_start` event names it. Connect to it
to watch or steer that subagent directly.

Stopping is **graceful**: it takes effect at the next safe point. A long-running
`bash` command is not interrupted, but the agent will stop instead of making
another model call once it returns. The socket is created with mode `0600` and
removed on exit; a stale socket left by a crash is detected and replaced
automatically. The socket stays open across `hand_off`s and always talks to the
agent running now.

`/context` is a direct read of the composer, not a cached copy. Context is
pull-only: it is never written to the event stream.
