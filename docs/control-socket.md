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
   # select a plan for the agent to follow/refine (any mode; `off` clears)
   printf '/setplan rate-limiting\n' | nc -U "$SOCK"
   # existing plan -> plan set to rate-limiting (1 pending, existing plan queued)
   # missing/empty  -> plan set to rate-limiting (empty; awaiting plan content)
   # graceful stop
   printf '/stop\n'   | nc -U "$SOCK"        # -> stopping
 ```

## Selecting a plan

`/setplan <slug>` points a running agent at a plan. The selection is injected
into the system prompt, so it survives context compaction, and it works in any
mode: `plan` refines the file, `build` follows it and reports changes it needs.
`/setplan off` clears the selection.

If `plans_dir/<slug>.md` (default `.genji/plans/<slug>.md`) already exists and
is non-empty, an instruction is queued telling the agent to read it, update it
if the approach changes, and continue until it is satisfied. If the file is
missing or empty, nothing is queued: the selection just tells the agent where
the plan lives, and `plan_write` populates it on the next plan step.

The same command is available from the CLI, which sends `/setplan` for you:

```bash
genji setplan <id> rate-limiting     # follow/refine .genji/plans/rate-limiting.md
genji setplan <id> off               # clear
```
