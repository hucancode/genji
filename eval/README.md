# genji evaluation

| level | checks | command |
|---|---|---|
| smoke | genji works at all | `cargo eval smoke` |
| unit | every promised feature is delivered, one small task each | `cargo eval unit` |
| benchmark | standard end-to-end benchmarks | `cargo eval bench SUITE` |

Tasks run in Docker through [Harbor](https://www.harborframework.com/), and a script scores each one. `cargo eval` prints results the way `cargo test` does and writes the job to `eval/jobs/<job>/`. `cargo eval report` and `cargo eval compare` read jobs back. 

## Config

Every run uses the runner's genji config. Copy `config.example.json` to `config.json`, which is gitignored, and fill in the endpoint and model, or pass `--config FILE`. Keep the key in the environment variable named by `api_key_env`; the adapter forwards that variable into the task container and never writes the key to a file. Task overrides from the catalog (`config = {...}`) and from instruction front matter (`[config]`) are merged over the runner's config.

```bash
cp eval/config.example.json eval/config.json   # then edit
export GENJI_API_KEY=...
cargo eval unit                    # all unit tasks, 1 attempt
cargo eval unit socket             # filter by name or capability
cargo eval bench tb-light -j 2     # 3 Terminal-Bench 2.1 tasks
cargo eval list                    # every catalog task and benchmark suite
cargo eval check --level unit      # nop scores 0 on every task; oracle scores 1 where a solution/ exists
```

## Layout

- `catalog.toml` lists every task with its level, capability and difficulty, plus the `[benchmarks]` suites.
- `dataset.toml` pins registry tasks by digest. Add one with `harbor add <org>/<name> --to eval`.
- `tasks/unit/<feature>/<task>/` holds the hand-authored unit tasks. `tasks/capabilities/` holds hand-authored benchmark tasks.
- `harbor/genji_agent.py` is the Harbor adapter. `src/drive.rs` (`genji-drive`) runs genji inside the container and leaves the trace and metrics there.

## Unit tasks

A unit task checks that isolated feature works, such as config, guardrails, skill loading, data-driven agents, handoff, subagent spawn, the control socket or follow-ups. It is trivial on purpose, so a failure points at the harness and not at the model. It must be quick: share the slim Dockerfile, need only a few tool calls, and set `[agent] timeout_sec <= 120` and `[verifier] timeout_sec <= 30` (a cargo test enforces both). Its verifier is `tests/test.py`: one short, self-contained, stdlib-only Python script about one aspect of the run, with plain `assert`s (`tests/test.sh` only starts it). It reads what `genji-drive` left in `/genji` (`trace/all.jsonl`, `trace/metrics.json`, `trace/socket.log`, `sessions/*.jsonl`) and the workspace, so a change to genji's event format breaks only the tasks that check that field, at a line you can see. Nothing is shared between tasks. Such a task has no oracle: no solution short of a genji run satisfies it, so `cargo eval check` proves only that `nop` scores 0 for it. A task with a `solution/` is also checked for oracle = 1.

## Benchmark suites

A `[benchmarks.NAME]` entry in `catalog.toml` takes one of two forms:

- A registry dataset pinned to a revision (`dataset = "org/name@N"`), optionally filtered by `category` and cut to a fixed `sample`. It is downloaded once into `eval/.store/`.
- The catalog tasks marked `level = "benchmark"` with `suite = NAME`.
