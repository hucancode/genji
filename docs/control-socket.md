# Mid-run instructions (control socket)

While a top-level run is active, genji listens on a Unix-domain socket
(`.genji/control.sock` by default, or the path given with `--socket`) so you can
steer it **without restarting**:

```bash
# in another terminal, any workspace
genji list           # find the instance id
genji instruct <id> "focus on the parser first; ignore the docs for now"
genji inspect <id>   # brief summary
genji instruct <id> /context   # full context that will be sent to the model
genji stop <id>      # ask for a graceful stop
genji stop id1,id2   # several ids (comma-separated or spaced)
genji stop all       # every registered instance
```

Stopping is **graceful**: it takes effect at the next safe point. A long-running
`bash` command is not interrupted, but the agent will stop instead of making
another model call once it returns. The socket is created with mode `0600` and
removed on exit; a stale socket left by a crash is detected and replaced
automatically.

Commands use one short-lived connection and receive one response line. The
event stream on stdout and `.genji/sessions/<id>.jsonl` are the durable output.

## Interact via netcat

```bash
   SOCK=.genji/control.sock 
   # SOCK=$(genji list ... )   / or read ~/.genji/instances/<id>.json
   # liveness
   printf '/ping\n'   | nc -U "$SOCK"        # -> pong
   # current status
   printf '/status\n' | nc -U "$SOCK"        # -> status: idle
   # the full context about to be sent to the model (messages + tools)
   printf '/context\n' | nc -U "$SOCK"
   # token breakdown of that context
   printf '/context stats\n' | nc -U "$SOCK"
   # inject an instruction (any line not starting with "/")
   printf 'focus on the parser first\n' | nc -U "$SOCK"
   # -> queued (1 pending)
   # answer a pending `ask` tool call (callId = the tool_call id in the session JSONL)
   printf '/answer call_abc "Postgres"\n' | nc -U "$SOCK"   # -> answered
   # graceful stop
   printf '/stop\n'   | nc -U "$SOCK"        # -> stopping
 ```

## Inspecting the context

`/context` returns the live context the agent is about to send on its next
model call: a JSON object with `context_window`, `last_prompt_tokens`, the
`messages` array (system prompt first), and the `tools` array. It is a direct
read of the composer — no cached copy. Context is pull-only: it is never
broadcast to the event stream.

```bash
genji instruct <id> /context    # full context snapshot (JSON) on stdout
```

The socket stays open across `hand_off`s and always talks to the agent running now.
