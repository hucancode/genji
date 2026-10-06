"""Harbor adapter for the genji coding agent, used by every `cargo eval` level.

Uploads statically linked ``genji`` and ``genji-drive`` binaries into the task
container, writes ``/genji/config.json`` (the runner's config, never the key
itself) and runs each task step through ``genji-drive``, which holds genji's
stdin/stdout.

An ``instruction.md`` may start with a TOML front matter block between ``+++``
lines; it is stripped from the prompt and steers the step:

    +++
    agent = "build"            # genji agent (default: the `mode` option)
    resume = true              # --resume the previous step's instance
    follow_handoffs = 5        # follow finish handoff/blocked verdicts
    before = "git commit -qam s1"   # shell command run in the workdir first
    token_limit = 300000
    kill_after_tool_calls = 6  # simulate a crash
    instructions = [{ after_tool_calls = 3, text = "Also ..." }]   # injected on stdin
    [config]                   # merged over the genji config for this step
    preferred_context_size = 20000
    +++

Every step leaves in ``/logs/agent``: ``genji.jsonl`` (events),
``genji.stderr.log``, ``metrics.json``, ``config.json``, ``sessions/`` and
``agents/``. Formats other harnesses expect (Harbor's token counts, libragent's
``compaction.json``) are translated here, from ``metrics.json``.
"""

from __future__ import annotations

import json
import os
import platform
import shlex
import shutil
import subprocess
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
_BIN_DIR = PurePosixPath("/usr/local/bin")
_OUT = "/logs/agent"


class GenjiOptions(InstalledAgentOptions):
    mode: Literal["build", "plan", "explore", "retro"] = "build"
    #: The runner's genji config file (eval/config.json); per-task overrides merge over it.
    genji_config: str | None = None
    #: Agents directory to test instead of `genji init`'s defaults.
    agents_dir: str | None = None
    #: eval/catalog.toml: per-task `config` overrides.
    catalog: str | None = None


def split_front_matter(instruction: str) -> tuple[dict[str, Any], str]:
    """Return the ``+++`` TOML front matter of an instruction and the prompt after it."""
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
    """Run genji inside a Harbor task container."""

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

    # -- build / install -------------------------------------------------- #

    @staticmethod
    def _target() -> str:
        """The Rust target of this host, which is also the Docker daemon's."""
        arch = {"x86_64": "x86_64", "amd64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}
        return f"{arch.get(platform.machine().lower(), platform.machine())}-unknown-linux-gnu"

    def _binaries(self) -> dict[str, Path]:
        """Build (once) the portable static genji and genji-drive binaries."""
        target = self._target()
        out = _REPO_ROOT / "target" / target / "release"
        bins = {name: out / name for name in ("genji", "genji-drive")}
        if all(path.is_file() for path in bins.values()):
            return bins
        if shutil.which("cargo") is None:
            raise RuntimeError("cargo not found; build genji and genji-drive first")
        self.logger.info("building static genji binaries for %s", target)
        subprocess.run(
            ["cargo", "build", "--release", "--target", target, "--workspace", "--bins"],
            cwd=_REPO_ROOT,
            env={**os.environ, "RUSTFLAGS": "-C target-feature=+crt-static"},
            check=True,
        )
        return bins

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        for name, path in self._binaries().items():
            await environment.upload_file(str(path), str(_BIN_DIR / name))
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
            environment,
            command=f"chmod 0755 {_BIN_DIR}/genji {_BIN_DIR}/genji-drive && "
            f"chmod -R a+rwX {_HOME} && genji --version",
        )

    # -- config / run ----------------------------------------------------- #

    def _model_config(self) -> tuple[dict[str, Any], dict[str, str]]:
        """The runner's genji config, and the env carrying the key its provider names."""
        if not self.options.genji_config:
            raise ValueError("set the genji_config option to a genji config file (see eval/config.example.json)")
        cfg = json.loads(Path(self.options.genji_config).read_text())
        provider = cfg.get("providers", {}).get(cfg.get("provider"), {})
        key_env = provider.get("api_key_env")
        env = {key_env: os.environ[key_env]} if key_env and os.environ.get(key_env) else {}
        return cfg, env

    def _catalog_config(self, environment: BaseEnvironment) -> dict[str, Any]:
        """The catalog's `config` for this task, matched by its full or short name."""
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
        cfg.update(
            agents_dir=str(_HOME / "agents"),
            sessions_dir=str(_HOME / "sessions"),
            # No human answers in an eval: `ask` takes its recommended option at once.
            ask_timeout_secs=0,
        )
        await self._upload_config_text(
            environment,
            content=json.dumps(cfg, indent=2) + "\n",
            remote_path=str(_HOME / "config.json"),
            filename="config.json",
        )
        step = {
            "workdir": await self._workdir(environment),
            "agent": self.options.mode,
            **directives,
            "prompt": prompt,
            "resume": bool(directives.get("resume") or self._resume),
            "out": _OUT,
        }
        await self._upload_config_text(
            environment,
            content=json.dumps(step) + "\n",
            remote_path=str(_HOME / "step.json"),
            filename="step.json",
        )
        # The uploads are private; the agent user may not be root.
        await self.exec_as_root(environment, command=f"chmod -R a+rwX {_HOME}")
        try:
            await self.exec_as_agent(
                environment,
                command=f"genji-drive {shlex.quote(str(_HOME / 'step.json'))}",
                env=env,
            )
        except NonZeroAgentExitCodeError as exc:
            # genji exits non-zero on LLM/budget failures or a planned kill; the verifier grades.
            self.logger.warning("genji exited non-zero: %s", exc)

    # -- metrics ---------------------------------------------------------- #

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        path = self.logs_dir / "metrics.json"
        if not path.exists():
            return
        m = json.loads(path.read_text())
        context.n_input_tokens = m.get("prompt_tokens") or None
        context.n_output_tokens = m.get("completion_tokens") or None
        context.n_cache_tokens = m.get("cached_tokens") or None
        context.metadata = {**(context.metadata or {}), "genji": m}
        # libragent's compaction tasks count a trial only when the harness reports a compaction.
        (self.logs_dir / "compaction.json").write_text(
            json.dumps({"compactionCount": m.get("compactions", 0)}) + "\n"
        )
