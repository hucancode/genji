"""Harbor adapter that runs each task step as one `genji` command in the task container.

An instruction.md may start with TOML front matter between `+++` lines, stripped from the prompt:

    +++
    agent = "build"       # default: the `mode` option
    resume = true         # --resume last
    before = "..."        # shell command in the workdir before the step
    after = "..."         # and after it
    token_limit = 300000
    socket = true         # keep the control socket on (default: off)
    wrap = "python3 /opt/task/wrap.py"   # run as `wrap genji ...`
    [config]              # merged over the genji config
    +++

Events go to /logs/agent/genji.jsonl and /genji/trace/all.jsonl, the exit code to
/genji/trace/exit_code.
"""

from __future__ import annotations

import json
import os
import platform
import shlex
import shutil
import subprocess
import time
import tomllib
from pathlib import Path, PurePosixPath
from typing import Any, Literal, override

from harbor.agents.capabilities import AgentCapabilities
from harbor.agents.installed.base import (
    BaseInstalledAgent,
    NonZeroAgentExitCodeError,
    with_prompt_template,
)
from harbor.agents.options import InstalledAgentOptions
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

_REPO_ROOT = Path(__file__).resolve().parents[2]
_HOME = PurePosixPath("/genji")
_BIN = PurePosixPath("/usr/local/bin/genji")
_OUT = "/logs/agent"
_FAKE_PORT = 18080


class GenjiOptions(InstalledAgentOptions):
    mode: Literal["build", "plan", "explore", "retro"] = "build"
    genji_config: str | None = None
    agents_dir: str | None = None
    catalog: str | None = None
    fake_dir: str | None = None


def split_front_matter(instruction: str) -> tuple[dict[str, Any], str]:
    text = instruction.lstrip("﻿")
    if not text.startswith("+++\n"):
        return {}, instruction
    head, sep, body = text[4:].partition("\n+++\n")
    if not sep:
        return {}, instruction
    return tomllib.loads(head), body.lstrip("\n")


def merge(base: dict[str, Any], over: dict[str, Any]) -> dict[str, Any]:
    out = dict(base)
    for key, value in over.items():
        if isinstance(value, dict) and isinstance(out.get(key), dict):
            out[key] = merge(out[key], value)
        else:
            out[key] = value
    return out


class Genji(BaseInstalledAgent):
    capabilities = AgentCapabilities(resume=True)

    options_model = GenjiOptions
    options: GenjiOptions

    @staticmethod
    @override
    def name() -> str:
        return "genji"

    @override
    def get_version_command(self) -> str | None:
        return "genji --version"

    @override
    def parse_version(self, stdout: str) -> str:
        return stdout.strip().split()[-1] if stdout.strip() else ""

    def _binary(self) -> Path:
        arch = {"x86_64": "x86_64", "amd64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}
        machine = platform.machine().lower()
        target = f"{arch.get(machine, machine)}-unknown-linux-gnu"
        path = _REPO_ROOT / "target" / target / "release" / "genji"
        if path.is_file():
            return path
        if shutil.which("cargo") is None:
            raise RuntimeError("cargo not found; build genji first")
        subprocess.run(
            ["cargo", "build", "--release", "--target", target, "--bin", "genji"],
            cwd=_REPO_ROOT,
            env={**os.environ, "RUSTFLAGS": "-C target-feature=+crt-static"},
            check=True,
        )
        return path

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        await environment.upload_file(str(self._binary()), str(_BIN))
        agents = _HOME / "agents"
        await self.exec_as_root(environment, command=f"mkdir -p {_HOME}/sessions {_HOME}/trace")
        if self.options.agents_dir:
            await environment.upload_dir(self.options.agents_dir, str(agents))
        else:
            await self.exec_as_root(
                environment,
                command=f"genji init --workspace /tmp/genji-init >/dev/null && "
                f"mv /tmp/genji-init/.genji/agents {agents} && rm -rf /tmp/genji-init",
            )
        await self.exec_as_root(
            environment, command=f"chmod 0755 {_BIN} && chmod -R a+rwX {_HOME} && genji --version"
        )
        if self.options.fake_dir:
            await self._start_fake(environment)

    async def _start_fake(self, environment: BaseEnvironment) -> None:
        script = Path(self.options.fake_dir) / f"{environment.environment_name}.json"
        if not script.is_file():
            raise FileNotFoundError(f"no fake LLM script {script}")
        await environment.upload_file(str(Path(__file__).resolve().parents[1] / "fake_llm.py"), str(_HOME / "fake_llm.py"))
        await environment.upload_file(str(script), str(_HOME / "fake.json"))
        await self.exec_as_root(
            environment,
            command=f"chmod -R a+rwX {_HOME}; cd {_HOME} && "
            f"(setsid python3 fake_llm.py fake.json --port {_FAKE_PORT} --log fake.log >/dev/null 2>&1 </dev/null &) && "
            "python3 -c \"import socket,time\n"
            "for _ in range(50):\n"
            f" try: socket.create_connection(('127.0.0.1',{_FAKE_PORT})).close(); break\n"
            " except OSError: time.sleep(0.1)\n"
            "else: raise SystemExit('fake llm did not start')\"",
        )

    def _model_config(self) -> tuple[dict[str, Any], dict[str, str]]:
        if not self.options.genji_config:
            raise ValueError("set the genji_config option to a genji config file (see eval/config.example.json)")
        cfg = json.loads(Path(self.options.genji_config).read_text())
        provider = cfg.get("providers", {}).get(cfg.get("provider"), {})
        key_env = provider.get("api_key_env")
        env = {key_env: os.environ[key_env]} if key_env and os.environ.get(key_env) else {}
        return cfg, env

    def _catalog_config(self, environment: BaseEnvironment) -> dict[str, Any]:
        if not self.options.catalog:
            return {}
        catalog = tomllib.loads(Path(self.options.catalog).read_text())
        short = environment.environment_name
        for name, entry in (catalog.get("tasks") or {}).items():
            if name == short or name.rsplit("/", 1)[-1] == short:
                return entry.get("config") or {}
        return {}

    async def _workdir(self, environment: BaseEnvironment) -> str:
        if environment.task_env_config.workdir:
            return environment.task_env_config.workdir
        result = await environment.exec(command="pwd", user="root")
        return (result.stdout or "/").strip().splitlines()[-1] or "/"

    @override
    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        directives, prompt = split_front_matter(instruction)
        model_cfg, env = self._model_config()
        cfg = merge(model_cfg, self._catalog_config(environment))
        cfg = merge(cfg, directives.pop("config", {}) or {})
        if self.options.fake_dir:
            provider = cfg["providers"][cfg["provider"]]
            provider.update(base_url=f"http://127.0.0.1:{_FAKE_PORT}/v1", api_key="fake")
            provider.pop("api_key_env", None)
            env = {}
        cfg.update(
            agents_dir=str(_HOME / "agents"),
            sessions_dir=str(_HOME / "sessions"),
            ask_timeout_secs=0,
        )
        await self._upload_config_text(
            environment,
            content=json.dumps(cfg, indent=2) + "\n",
            remote_path=str(_HOME / "config.json"),
            filename="config.json",
        )
        await self.exec_as_root(environment, command=f"chmod -R a+rwX {_HOME}")

        workdir = shlex.quote(await self._workdir(environment))
        if directives.get("before"):
            await self.exec_as_agent(environment, command=f"cd {workdir} && {directives['before']}")
        argv = ["genji", directives.get("agent") or self.options.mode, prompt, "--config", str(_HOME / "config.json")]
        if directives.get("resume") or self._resume:
            argv += ["--resume", "last"]
        if directives.get("token_limit"):
            argv += ["--token-limit", str(directives["token_limit"])]
        if not directives.get("socket"):
            argv.append("--socket-disabled")
        command = " ".join(map(shlex.quote, argv))
        if directives.get("wrap"):
            command = f"{directives['wrap']} {command}"
        script = (
            f"cd {workdir} && {command} 2>> {_OUT}/genji.stderr.log "
            f"| tee -a {_HOME}/trace/all.jsonl >> {_OUT}/genji.jsonl; "
            f"code=${{PIPESTATUS[0]}}; echo $code > {_HOME}/trace/exit_code; exit $code"
        )
        started = time.monotonic()
        try:
            await self.exec_as_agent(environment, command=f"bash -c {shlex.quote(script)}", env=env)
        except NonZeroAgentExitCodeError as exc:
            self.logger.warning("genji exited non-zero: %s", exc)
        finally:
            self._duration = time.monotonic() - started
            if directives.get("after"):
                await self.exec_as_agent(environment, command=f"cd {workdir} && {directives['after']}")
            await self.exec_as_root(
                environment,
                command=f"cp -r {_HOME}/config.json {_HOME}/sessions {_HOME}/agents {_OUT}/ 2>/dev/null; true",
            )

    def _metrics(self) -> dict[str, Any]:
        m = dict.fromkeys(("prompt_tokens", "completion_tokens", "cached_tokens", "tool_calls", "compactions"), 0)
        path = self.logs_dir / "genji.jsonl"
        for line in path.read_text(errors="replace").splitlines() if path.exists() else []:
            try:
                e = json.loads(line)
            except json.JSONDecodeError:
                continue
            match e.get("type"):
                case "tokens":
                    m["prompt_tokens"] += e.get("prompt") or 0
                    m["completion_tokens"] += e.get("completion") or 0
                    m["cached_tokens"] += e.get("cached") or 0
                case "tool_call":
                    m["tool_calls"] += 1
                case "compaction":
                    m["compactions"] += 1
        m["duration_secs"] = getattr(self, "_duration", 0.0)
        return m

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        m = self._metrics()
        (self.logs_dir / "metrics.json").write_text(json.dumps(m, indent=2))
        context.n_input_tokens = m["prompt_tokens"] or None
        context.n_output_tokens = m["completion_tokens"] or None
        context.n_cache_tokens = m["cached_tokens"] or None
        context.metadata = {**(context.metadata or {}), "genji": m}
        (self.logs_dir / "compaction.json").write_text(json.dumps({"compactionCount": m["compactions"]}) + "\n")
