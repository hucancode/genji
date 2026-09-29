# Mid-run instructions (control socket)

While a top-level run is active, genji listens on a Unix-domain socket
(`.genji/control.sock` by default) so you can steer it **without restarting**:

```bash
# in another terminal, same workspace
./target/release/genji --send "focus on the parser first; ignore the docs for now"
./target/release/genji --status      # status: mode=build tokens=64328 messages=57
./target/release/genji --stop        # ask for a graceful stop
```

How it works:

- A background thread accepts one newline-terminated command per connection and
  replies with one line.
- Plain text is queued as a user instruction. The run loop drains the queue at a
  safe point — the top of each iteration, after all tool results from the
  previous assistant turn — and injects it as `[instruction from user] …`. It is
  never inserted between an assistant `tool_calls` message and its results.
- If an instruction arrives while the model is producing its final message, the
  loop resumes instead of ending.
- `/status`, `/stop` and `/ping` are reserved commands. `--send`/`--status`/
  `--stop` are thin clients for them; any Unix socket client works (the protocol
  is one line in, one line out).

Stopping is **graceful**: it takes effect at the next safe point. A long-running
`bash` command is not interrupted, but the agent will stop instead of making
another model call once it returns. The socket is removed on exit; a stale socket
left by a crash is detected and replaced automatically. Only one top-level agent
may listen per workspace (a second one aborts with a clear error).

Set `control_enabled: false` or pass `--no-control` to disable; subagents never
open a control socket.
