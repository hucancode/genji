"""Harbor adapter for the genji coding agent.

Uploads a statically linked ``genji`` binary into the task container, points it
at the Harbor-resolved model endpoint with a minimal ``.genji/config.json``
(model + API key, read from an env var), and runs one instruction. Token usage
is read back from genji's JSONL event trace.
"""

from __future__ import annotations

import json
import os
import shlex
import shutil
import subprocess
from pathlib import Path, PurePosixPath
from typing import Any, Literal, override

from harbor.agents.capabilities import AgentCapabilities
from harbor.agents.installed.base import (
    BaseInstalledAgent,
    NonZeroAgentExitCodeError,
    with_prompt_template,
)
from harbor.agents.model_connection import (
    ModelConnectionSpec,
    ResolvedModelConnection,
)
from harbor.agents.options import InstalledAgentOptions
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

_REPO_ROOT = Path(__file__).resolve().parents[1]
_TARGET = "x86_64-unknown-linux-gnu"  # portable static build target
_REMOTE_BIN = PurePosixPath("/usr/local/bin/genji")

_OUTPUT = "genji.jsonl"
_STDERR = "genji.stderr.log"

#: genji reads the key from this env var, keeping it out of the config file.
_API_KEY_ENV = "GENJI_API_KEY"

#: Per-provider context/output caps. Reasoning models can spend a whole turn
#: thinking, so they need a large output budget; DeepSeek is capped at 64K.
#: Extend this map for other providers as needed.
_DEFAULT_LIMITS = {"context_window": 262144, "max_output_tokens": 131072}
_PROVIDER_LIMITS = {
    "deepseek": {"context_window": 131072, "max_output_tokens": 65536},
}


class GenjiOptions(InstalledAgentOptions):
    mode: Literal["build", "plan", "explore", "retro"] = "build"


class Genji(BaseInstalledAgent):
    """Run genji inside a Harbor task container."""

    capabilities = AgentCapabilities()
    MODEL_CONNECTION = ModelConnectionSpec(passthrough=True)

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

    def _binary(self) -> Path:
        """Build (once) and return the portable static genji binary."""
        binary = _REPO_ROOT / "target" / _TARGET / "release" / "genji"
        if binary.is_file():
            return binary
        if shutil.which("cargo") is None:
            raise RuntimeError(
                "cargo not found; run 'make bench-build' or build genji first"
            )
        self.logger.info("building static genji binary")
        subprocess.run(
            ["cargo", "build", "--release", "--target", _TARGET],
            cwd=_REPO_ROOT,
            env={**os.environ, "RUSTFLAGS": "-C target-feature=+crt-static"},
            check=True,
        )
        return binary

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        await environment.upload_file(str(self._binary()), str(_REMOTE_BIN))
        await self.exec_as_root(
            environment,
            command=f"chmod 0755 {_REMOTE_BIN} && genji --version",
        )

    # -- config / run ----------------------------------------------------- #

    def _genji_config(
        self, model_id: str, access: ResolvedModelConnection
    ) -> dict[str, Any]:
        base_url = (access.configured_base_url or access.base_url or "").rstrip("/")
        if not base_url:
            raise ValueError(
                f"no base URL resolved for provider {access.provider!r}; "
                "set the provider's *_BASE_URL via --agent-env"
            )
        limits = _PROVIDER_LIMITS.get((access.provider or "").lower(), _DEFAULT_LIMITS)
        return {
            "provider": "harbor",
            "providers": {
                "harbor": {
                    "base_url": base_url,
                    "api_key_env": _API_KEY_ENV,
                    "model": model_id,
                    **limits,
                }
            },
        }

    async def _workdir(self, environment: BaseEnvironment) -> str:
        if environment.task_env_config.workdir:
            return environment.task_env_config.workdir
        result = await environment.exec(command="pwd", user="root")
        return (result.stdout or "/").strip().splitlines()[-1] or "/"

    def _command(self, instruction: str) -> str:
        task = shlex.quote(instruction)
        core = f"genji {self.options.mode} {task}"
        return f"{core} > /logs/agent/{_OUTPUT} 2> /logs/agent/{_STDERR}"

    @override
    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        if not self.model_name or "/" not in self.model_name:
            raise ValueError(
                "model must be 'provider/model', e.g. deepseek/deepseek-flash"
            )
        model_id = self.model_name.split("/", 1)[1]
        access = self.model_connection
        workdir = await self._workdir(environment)
        config_dir = PurePosixPath(workdir) / ".genji"

        await self.exec_as_agent(environment, command=f"mkdir -p {shlex.quote(str(config_dir))}")
        await self._upload_config_text(
            environment,
            content=json.dumps(self._genji_config(model_id, access), indent=2) + "\n",
            remote_path=str(config_dir / "config.json"),
            filename="config.json",
        )

        env = {_API_KEY_ENV: access.api_key} if access.api_key else {}
        try:
            await self.exec_as_agent(
                environment, command=self._command(instruction), cwd=workdir, env=env
            )
        except NonZeroAgentExitCodeError as exc:
            # genji exits non-zero on LLM/budget failures; let the verifier grade.
            self.logger.warning("genji exited non-zero: %s", exc)

    # -- metrics ---------------------------------------------------------- #

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        output = self.logs_dir / _OUTPUT
        if not output.exists():
            return
        input_tokens = output_tokens = total_tokens = 0
        report = status = None
        for line in output.read_text(errors="replace").splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            kind = event.get("type")
            if kind == "tokens":
                input_tokens += int(event.get("prompt") or 0)
                output_tokens += int(event.get("completion") or 0)
                total_tokens = int(event.get("used") or total_tokens)
            elif kind == "instance_end":
                report, status = event.get("report"), event.get("status")

        if input_tokens or output_tokens:
            context.n_input_tokens = input_tokens
            context.n_output_tokens = output_tokens
        context.metadata = {
            **(context.metadata or {}),
            "genji_status": status,
            "genji_tokens_used": total_tokens,
            "genji_report": report,
            "genji_log": str(output),
        }
