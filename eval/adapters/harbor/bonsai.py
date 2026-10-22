"""Pinned Harbor installed-agent adapter for the Bonsai CLI.

Compatibility target:
harbor-framework/harbor@7331d8a3e23cb72e8277522059ff7c9ab9435d90

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

from .installed_common import isolated_command, private_home, required_environment, require_https_endpoint


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
        CliFlag("unlimited_turns", cli="--unlimited-turns", type="bool"),
        CliFlag(
            "max_generation_seconds",
            cli="--max-generation-seconds",
            type="int",
        ),
        CliFlag("max_output_chars", cli="--max-output-chars", type="int"),
        CliFlag("max_tool_seconds", cli="--max-tool-seconds", type="int"),
        CliFlag("timeout_seconds", cli="--timeout", type="int"),
        CliFlag("unlimited_time", cli="--unlimited-time", type="bool"),
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
        return self.extra_env.get("BONSAI_BIN") or "bonsai"

    @override
    def get_version_command(self) -> str | None:
        return f"{shlex.quote(self._binary())} --version"

    @override
    async def setup(self, environment: BaseEnvironment) -> None:
        guard = getattr(self, "qualification_setup_guard", None)
        if guard is not None:
            await guard(environment)
        await super().setup(environment)

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
            "max_generation_seconds",
            "max_output_chars",
            "max_tool_seconds",
        }
        unlimited = self._resolved_flags.get("unlimited_turns") is True
        if unlimited and "max_turns" in self._resolved_flags:
            raise ValueError("unlimited_turns conflicts with max_turns")
        if not unlimited:
            required_flags.add("max_turns")
        if self._resolved_flags.get("unlimited_time") is True:
            if "timeout_seconds" in self._resolved_flags:
                raise ValueError("unlimited_time conflicts with timeout_seconds")
        else:
            required_flags.add("timeout_seconds")
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
        home = private_home("bonsai")
        catalog = self.extra_env.get("BONSAI_MODEL_CATALOG")
        if catalog:
            prepared = await environment.exec(command=f"umask 077; mkdir -p {shlex.quote(home + '/models')}")
            if prepared.return_code:
                raise RuntimeError("private benchmark model directory creation failed")
            await environment.upload_file(catalog, home + "/models/benchmark.toml")
        # Explicitly forward only this provider's key and endpoint. A fresh
        # BONSAI_HOME alone does not remove image/host environment credentials.
        prefix = provider.upper().replace("-", "_")
        provider_env = required_environment(self.extra_env, (f"{prefix}_API_KEY", f"{prefix}_BASE_URL"))
        require_https_endpoint(provider_env[f"{prefix}_BASE_URL"])
        env = {
            **self._resolved_env_vars,
            **provider_env,
            f"{prefix}_MODEL": model,
            "BONSAI_PROVIDER": provider,
            "BONSAI_DOTENV": "off",
            "BONSAI_HOME": home,
            "BONSAI_DISABLE_MODELS_FETCH": "1",
            instruction_env: instruction,
        }
        checkpoint_path = self.extra_env.get("BONSAI_CHECKPOINT_LOG")
        if checkpoint_path is not None:
            expected = (EnvironmentPaths.agent_dir / "checkpoints.jsonl").as_posix()
            if checkpoint_path != expected:
                raise ValueError("diagnostic checkpoint path must be the private agent log path")
            env["BONSAI_CHECKPOINT_LOG"] = checkpoint_path
        command = (
            f"umask 077; mkdir -p {shlex.quote(home)} {shlex.quote(EnvironmentPaths.agent_dir.as_posix())}; "
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
        command, env = isolated_command(f"set -o pipefail; {command}", env, home)
        result = await environment.exec(command=command, env=env,
                                        timeout_sec=self._resolved_flags.get("timeout_seconds"))
        # Harbor merges process stderr into stdout. Read the private JSON file
        # separately so nonzero exits cannot contaminate terminal evidence.
        terminal = await environment.exec(
            command=f"cat {shlex.quote(output_path.as_posix())}", timeout_sec=10)
        payload = self._parse_payload(terminal.stdout or "") if terminal.return_code == 0 else None
        if checkpoint_path is not None:
            from .diagnostic_evidence import validate_receipts
            captured = await environment.exec(command=f"cat {shlex.quote(checkpoint_path)}", timeout_sec=10)
            if captured.return_code:
                raise RuntimeError("diagnostic checkpoint evidence missing; stop batch")
            try:
                validate_receipts(captured.stdout or "", expected_model=self.model_name,
                                  max_turns=self._resolved_flags.get("max_turns"))
            except ValueError as error:
                context.metadata = {"bonsai_terminal_state": "infrastructure_invalid"}
                raise RuntimeError("diagnostic checkpoint evidence invalid; stop batch") from error
            diagnostic_payload = payload
            if diagnostic_payload is None:
                context.metadata = {"bonsai_terminal_state": "infrastructure_invalid"}
                raise RuntimeError("diagnostic terminal evidence missing; stop batch")
            if (diagnostic_payload.get("provider") != provider
                    or diagnostic_payload.get("model") != self.model_name):
                context.metadata = {"bonsai_terminal_state": "infrastructure_invalid"}
                raise RuntimeError("diagnostic provider/model mismatch; stop batch")
            self._populate_context(context, diagnostic_payload)
        # Validate identity before admitting any outcome to Harbor's verifier.
        expected_model = self.model_name if checkpoint_path is not None else model
        if payload is not None and (
                payload.get("provider") != provider or payload.get("model") != expected_model):
            context.metadata = {"bonsai_terminal_state": "infrastructure_invalid"}
            raise RuntimeError("Bonsai selected provider/model mismatch; stop batch")
        # Do not include raw provider/tool output in Harbor's shared exception.
        if result.return_code != 0:
            if payload is not None:
                self._populate_context(context, payload)
            else:
                context.metadata = {"bonsai_terminal_state": "agent_failure"}
            from harbor.agents.installed.base import NonZeroAgentExitCodeError
            raise NonZeroAgentExitCodeError(
                f"Bonsai exited with status {result.return_code}; see private logs")
        if payload is not None:
            if payload.get("status") not in {"completed", "success"}:
                raise RuntimeError("Bonsai turn failed; see private logs")
            self._populate_context(context, payload)
            digest_result = await environment.exec(command=f'b3sum "$(command -v {binary})"')
            if digest_result.return_code != 0:
                raise RuntimeError("Qualification requires preinstalled b3sum")
            profile = {
                "declared_revision": self.extra_env.get("BONSAI_REVISION"),
                "binary_version": None,
                "provider": payload.get("provider"),
                "model": payload.get("model"),
                "reasoning_effort": self._resolved_flags["reasoning_effort"],
                "autonomy": self._resolved_flags["autonomy"],
                "network": self._resolved_env_vars["BONSAI_SANDBOX_NETWORK"],
                "budgets": {key: self._resolved_flags.get(key) for key in (
                    "max_turns", "max_generation_seconds", "max_output_chars",
                    "max_tool_seconds", "timeout_seconds")},
            }
            profile["budgets"]["unlimited_turns"] = self._resolved_flags.get("unlimited_turns", False)
            profile["budgets"]["unlimited_time"] = self._resolved_flags.get("unlimited_time", False)
            profile["budgets"]["max_patch_bytes"] = 1_000_000
            context.metadata.update({
                "bonsai_profile": profile,
                "bonsai_binary_digest": digest_result.stdout.split()[0],
                "bonsai_trial_id": self.extra_env.get("BONSAI_TRIAL_ID"),
            })
        else:
            context.metadata = {"bonsai_terminal_state": "agent_failure"}
        if payload is None:
            raise RuntimeError("Bonsai output missing or malformed; see private logs")

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
            "cost_source": "catalog_estimate" if cost_micros is not None else "unavailable",
        }
