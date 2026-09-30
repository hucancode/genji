# Mid-run instructions (control socket)

While a top-level run is active, genji listens on a Unix-domain socket
(`.genji/control.sock` by default) so you can steer it **without restarting**:

```bash
# in another terminal, any workspace
genji list           # find the instance id
genji instruct <id> "focus on the parser first; ignore the docs for now"
genji inspect <id>   # brief summary
genji stop <id>      # ask for a graceful stop
genji stop id1,id2   # several ids (comma-separated or spaced)
genji stop all       # every registered instance
```

Stopping is **graceful**: it takes effect at the next safe point. A long-running
`bash` command is not interrupted, but the agent will stop instead of making
another model call once it returns. The socket is removed on exit; a stale socket
left by a crash is detected and replaced automatically.

## Interact via netcat

```bash
   SOCK=.genji/control.sock 
   # SOCK=$(genji list ... )   / or read ~/.genji/instances/<id>.json
   # liveness
   printf '/ping\n'   | nc -U "$SOCK"        # -> pong
   # current status
   printf '/status\n' | nc -U "$SOCK"        # -> status: idle
   # inject an instruction (any line not starting with "/")
   printf 'focus on the parser first\n' | nc -U "$SOCK"
   # -> queued (1 pending)
   # graceful stop
   printf '/stop\n'   | nc -U "$SOCK"        # -> stopping
 ```
