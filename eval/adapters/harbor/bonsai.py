"""Pinned Harbor installed-agent adapter for the Bonsai CLI.

Compatibility target:
harbor-framework/harbor@2d3f78d55a703df2f76c005d7df44a5ce2d8adf5

The Bonsai binary must already exist in the task environment. This adapter
never downloads a binary, runtime, dataset, package, or container image.
"""

import json
import shlex
import uuid
from typing import override

from harbor.agents.installed.base import (
    BaseInstalledAgent,
    CliFlag,
    EnvVar,
    with_prompt_template,
)
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.trial.paths import EnvironmentPaths


class Bonsai(BaseInstalledAgent):
    """Run a preinstalled Bonsai binary through its structured headless surface."""

    CLI_FLAGS = [
        CliFlag(
            "autonomy",
            cli="--autonomy",
            type="enum",
            choices=["ask", "conservative", "balanced", "auto-accept", "yolo"],
        ),
        CliFlag(
            "reasoning_effort",
            cli="--effort",
            type="enum",
            choices=[
                "default",
                "off",
                "on",
                "minimal",
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultra",
            ],
        ),
        CliFlag("max_turns", cli="--max-turns", type="int"),
        CliFlag(
            "max_generation_seconds",
            cli="--max-generation-seconds",
            type="int",
        ),
        CliFlag("max_output_chars", cli="--max-output-chars", type="int"),
        CliFlag("max_tool_seconds", cli="--max-tool-seconds", type="int"),
        CliFlag("timeout_seconds", cli="--timeout", type="int"),
    ]
    ENV_VARS = [
        EnvVar(
            "network",
            env="BONSAI_SANDBOX_NETWORK",
            type="enum",
            choices=["deny", "allow"],
        )
    ]

    @staticmethod
    @override
    def name() -> str:
        return "bonsai"

    def _binary(self) -> str:
        return self._get_env("BONSAI_BIN") or "bonsai"

    @override
    def get_version_command(self) -> str | None:
        return f"{shlex.quote(self._binary())} --version"

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        binary = shlex.quote(self._binary())
        result = await environment.exec(command=f"command -v {binary} >/dev/null 2>&1")
        if result.return_code != 0:
            raise RuntimeError(
                "Bonsai is not preinstalled in the Harbor environment. "
                "Provide it in the task image or set BONSAI_BIN; the adapter "
                "does not download binaries."
            )

    def _require_explicit_settings(self) -> None:
        required_flags = {
            "autonomy",
            "reasoning_effort",
            "max_turns",
            "max_generation_seconds",
            "max_output_chars",
            "max_tool_seconds",
            "timeout_seconds",
        }
        missing = sorted(required_flags.difference(self._resolved_flags))
        if "BONSAI_SANDBOX_NETWORK" not in self._resolved_env_vars:
            missing.append("network")
        if missing:
            raise ValueError(
                "Bonsai Harbor runs require explicit settings: " + ", ".join(missing)
            )
        for name in required_flags.difference({"autonomy", "reasoning_effort"}):
            if self._resolved_flags[name] <= 0:
                raise ValueError(f"Bonsai Harbor setting '{name}' must be positive")

    @override
    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        self._require_explicit_settings()
        if not self.model_name or "/" not in self.model_name:
            raise ValueError("Bonsai Harbor model must be '<provider>/<model>'")
        provider, model = self.model_name.split("/", maxsplit=1)
        if not provider or not model:
            raise ValueError("Bonsai Harbor model must be '<provider>/<model>'")

        instruction_env = f"BONSAI_BENCHMARK_INSTRUCTION_{uuid.uuid4().hex.upper()}"
        output_path = EnvironmentPaths.agent_dir / "bonsai.json"
        stderr_path = EnvironmentPaths.agent_dir / "bonsai.stderr"
        flags = self.build_cli_flags()
        binary = shlex.quote(self._binary())
        env = {
            **self._resolved_env_vars,
            "BONSAI_PROVIDER": provider,
            "BONSAI_DOTENV": "off",
            instruction_env: instruction,
        }
        command = (
            f"mkdir -p {shlex.quote(EnvironmentPaths.agent_dir.as_posix())}; "
            f'printf "%s" "${instruction_env}" | '
            f"{binary} -p - --output-format json --model {shlex.quote(model)} "
            f"{flags} --isolation off "
            f"> {shlex.quote(output_path.as_posix())} "
            f"2> {shlex.quote(stderr_path.as_posix())}; "
            "bonsai_status=$?; "
            f"cat {shlex.quote(output_path.as_posix())}; "
            "if [ $bonsai_status -ne 0 ]; then "
            f"cat {shlex.quote(stderr_path.as_posix())} >&2; "
            "fi; exit $bonsai_status"
        )
        result = await environment.exec(command=f"set -o pipefail; {command}", env=env)
        payload = self._parse_payload(result.stdout or "")
        if payload is not None:
            self._populate_context(context, payload)
        else:
            context.metadata = {"bonsai_terminal_state": "agent_failure"}
        if result.return_code != 0:
            raise self._classify_exec_error(command, result)

    @staticmethod
    def _parse_payload(stdout: str) -> dict | None:
        try:
            payload = json.loads(stdout)
        except (TypeError, json.JSONDecodeError):
            return None
        return payload if isinstance(payload, dict) else None

    @staticmethod
    def _populate_context(context: AgentContext, payload: dict) -> None:
        usage = payload.get("usage") or {}
        cache = usage.get("input_cache") or {}
        context.n_input_tokens = usage.get("prompt_tokens")
        context.n_cache_tokens = cache.get("read_tokens")
        context.n_output_tokens = usage.get("completion_tokens")
        cost_micros = usage.get("cost_micros")
        context.cost_usd = (
            float(cost_micros) / 1_000_000 if cost_micros is not None else None
        )
        verification = payload.get("verification") or {}
        context.metadata = {
            "bonsai_terminal_state": payload.get("status", "internal_error"),
            "bonsai_session_id": payload.get("session_id"),
            "bonsai_repair_turns": verification.get("repair_attempts", 0),
        }
