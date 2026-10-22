"""Preinstalled DSH headless CLI at the comparison's immutable source pin."""

import json
from pathlib import Path
import re
import shlex
import tempfile

from harbor.agents.installed.base import BaseInstalledAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.trial.paths import EnvironmentPaths

from .dsh_outcome import parse_sessions
from .installed_common import isolated_command, private_home, required_environment, require_https_endpoint


def fresh_session_id(stdout: str) -> str:
    """Read one safe fresh-session identity, never infer it from final text."""
    identities = []
    try:
        for line in stdout.splitlines():
            if not line.strip():
                continue
            event = json.loads(line)
            if not isinstance(event, dict):
                raise ValueError("non-object event")
            if event.get("type") == "session":
                identities.append(event.get("sessionId"))
    except (ValueError, TypeError) as error:
        raise RuntimeError("DSH emitted invalid JSONL; see private trial artifacts") from error
    if (len(identities) != 1 or not isinstance(identities[0], str)
            or re.fullmatch(r"session-[a-zA-Z0-9_-]{1,128}", identities[0]) is None):
        raise RuntimeError("DSH must emit exactly one safe fresh-session identity")
    return identities[0]


def benchmark_overlay(model: str, endpoint: str, reasoning: str) -> str:
    """Cordis patch array; JSON is YAML-compatible and contains no credential."""
    require_https_endpoint(endpoint)
    if not isinstance(model, str) or not model.strip():
        raise ValueError("explicit model required")
    if reasoning not in {"off", "low", "high", "max"}:
        raise ValueError("unsupported DeepSeek reasoning setting")
    return json.dumps([
        {"id": "agent-default-model", "config": {"provider": "deepseek-official", "model": model, "reasoningEffort": reasoning}},
        {"id": "llm-deepseek", "config": {"baseURL": endpoint, "thinking": "disabled" if reasoning == "off" else "enabled", "reasoningEffort": reasoning}},
    ], sort_keys=True)


class Dsh(BaseInstalledAgent):
    @staticmethod
    def name() -> str:
        return "dsh-pinned"

    def _binary(self) -> str:
        return self.extra_env.get("DSH_BIN") or "/opt/benchmark/bin/dsh"

    def get_version_command(self) -> str:
        return f"{shlex.quote(self._binary())} --version"

    async def setup(self, environment: BaseEnvironment) -> None:
        guard = getattr(self, "qualification_setup_guard", None)
        if guard is not None:
            await guard(environment)
        await super().setup(environment)

    async def install(self, environment: BaseEnvironment) -> None:
        result = await environment.exec(command=f"test -x {shlex.quote(self._binary())}")
        if result.return_code:
            raise RuntimeError("DSH must be preinstalled by explicit operator setup")

    async def run(self, instruction: str, environment: BaseEnvironment, context: AgentContext) -> None:
        model = self.model_name
        selected = required_environment(self.extra_env, (
            "DEEPSEEK_BASE_URL", "TB4_REASONING", "DEEPSEEK_API_KEY", "TB4_TIMEOUT_SECONDS"))
        endpoint = selected["DEEPSEEK_BASE_URL"]
        reasoning = selected["TB4_REASONING"]
        key = selected["DEEPSEEK_API_KEY"]
        timeout = selected["TB4_TIMEOUT_SECONDS"]
        if timeout is None:
            raise ValueError("DSH requires explicit finite timeout or unlimited")
        duration = None if timeout == "unlimited" else int(timeout)
        if not model or not endpoint or not key or (duration is not None and duration <= 0):
            raise ValueError("explicit model, endpoint, credential and timeout required")
        home = private_home("dsh")
        remote = EnvironmentPaths.agent_dir.as_posix()
        overlay_path = f"{home}/benchmark.patch.yml"
        overlay = benchmark_overlay(model, endpoint, reasoning)
        setup = await environment.exec(command=f"umask 077; mkdir -p {shlex.quote(home)} {shlex.quote(remote)}")
        if setup.return_code:
            raise RuntimeError("DSH isolated-home setup failed")
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "benchmark.patch.yml"
            source.write_text(overlay, encoding="utf-8")
            await environment.upload_file(source, overlay_path)
        command = (
            'umask 077; set -o pipefail; printf "%s" "$TB4_INSTRUCTION" | '
            f"{shlex.quote(self._binary())} --profile headless --patch {shlex.quote(overlay_path)} "
            "--json - "
            f">{shlex.quote(remote + '/dsh.jsonl')} 2>{shlex.quote(remote + '/dsh.stderr')}"
        )
        command, env = isolated_command(command, {"DSH_HOME": home, "DSH_PERMISSION_MODE": "danger-full-access",
            "DEEPSEEK_API_KEY": key, "DEEPSEEK_BASE_URL": endpoint, "TB4_INSTRUCTION": instruction}, home)
        result = await environment.exec(command=command, env=env, timeout_sec=duration)
        with tempfile.TemporaryDirectory() as directory:
            local = Path(directory) / "stdout.jsonl"
            await environment.download_file(remote + "/dsh.jsonl", local)
            session = fresh_session_id(local.read_text(encoding="utf-8"))
        export_path = remote + "/dsh-sessions.json"
        export_command, export_env = isolated_command(
            shlex.join(["node", "/opt/benchmark/dsh_export.mjs", "/opt/benchmark/dsh", home + "/sessions", session, export_path]), {}, home)
        exported = await environment.exec(command=export_command, env=export_env, timeout_sec=30)
        if exported.return_code:
            raise RuntimeError("DSH durable export failed; stop the batch")
        with tempfile.TemporaryDirectory() as directory:
            local = Path(directory) / "sessions.json"
            await environment.download_file(export_path, local)
            sessions = json.loads(local.read_text(encoding="utf-8"))
        outcome = parse_sessions(sessions, session, result.return_code)
        usage = outcome.usage or {}
        context.n_input_tokens = usage.get("inputTokens")
        context.n_output_tokens = usage.get("outputTokens")
        context.n_cache_tokens = usage.get("cacheReadTokens")
        context.cost_usd = None
        context.metadata = {"comparison_agent": "dsh", "terminal_state": outcome.state,
                            "session_id": session, "telemetry_complete": outcome.telemetry_complete,
                            "cost_source": "unavailable", "selected_model": model,
                            "reasoning": reasoning, "endpoint": endpoint}
        if outcome.state != "completed":
            raise RuntimeError(f"DSH {outcome.state}; see private trial artifacts")
