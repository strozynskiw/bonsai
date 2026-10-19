"""Pinned Harbor local-task lifecycle, without downloads or provider selection.

This low-level executor is also used by synthetic integration tests. It is not
an approval boundary: the comparison driver must validate approval and billing
before dispatch. Raw Harbor output remains private; only normalized fields leave
this module.
"""

import asyncio
from dataclasses import dataclass, replace
import hashlib
import math
from pathlib import Path

from .comparison_manifest import HARBOR_COMMIT, canonical_bytes


def tree_digest(root: Path) -> str:
    """Hash local task bytes and executable bits; reject links and special files."""
    entries = []
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise ValueError("task links are not supported")
        if path.is_file():
            entries.append([path.relative_to(root).as_posix(),
                            bool(path.stat().st_mode & 0o111),
                            hashlib.sha256(path.read_bytes()).hexdigest()])
        elif not path.is_dir():
            raise ValueError("task special files are not supported")
    return "sha256:" + hashlib.sha256(canonical_bytes(entries)).hexdigest()


def require_runtime() -> None:
    """Validate the installed source pin without importing Harbor at module load."""
    from importlib.metadata import distribution
    import json
    direct = json.loads(distribution("harbor").read_text("direct_url.json") or "{}")
    if (direct.get("vcs_info", {}).get("commit_id") != HARBOR_COMMIT
            or direct.get("url") != "https://github.com/harbor-framework/harbor.git"):
        raise ValueError("exact pinned Harbor VCS installation required")


@dataclass(frozen=True)
class LocalTrial:
    """Explicit local input; no package names, Git URLs, or injected skills."""

    task_path: Path
    content_digest: str
    name: str
    output: Path
    agent: str
    model: str
    environment: dict[str, str]
    options: dict
    timeout_seconds: int
    cpu: int
    ram_mb: int
    disk_mb: int

    def config(self):
        from harbor.models.trial.config import TrialConfig
        if self.agent not in {"bonsai", "dsh"}:
            raise ValueError("unknown comparison agent")
        if tree_digest(self.task_path) != self.content_digest:
            raise ValueError("local task content changed")
        if not self.name.startswith("tb4-") or not self.name.replace("-", "").isalnum():
            raise ValueError("safe dedicated trial name required")
        if any(type(value) is not int or value <= 0 for value in
               (self.timeout_seconds, self.cpu, self.ram_mb, self.disk_mb)):
            raise ValueError("positive explicit resources required")
        return TrialConfig(
            task={"path": self.task_path.resolve()}, trial_name=self.name,
            trials_dir=self.output.resolve(),
            agent={"import_path": f"eval.adapters.harbor.{self.agent}:{'Bonsai' if self.agent == 'bonsai' else 'Dsh'}",
                   "model_name": self.model, "skip_model_parsing": True,
                   "env": self.environment, "kwargs": self.options,
                   "override_timeout_sec": self.timeout_seconds,
                   "override_setup_timeout_sec": 30},
            environment={"type": "docker", "delete": True,
                         "cpu_enforcement_policy": "limit", "memory_enforcement_policy": "limit",
                         "override_cpus": self.cpu, "override_memory_mb": self.ram_mb,
                         "override_storage_mb": self.disk_mb, "override_gpus": 0},
            verifier={"override_timeout_sec": 30})


class LocalExecutor:
    """Own one Harbor trial; await and independently verify Docker teardown."""

    def __init__(self, *, before_setup=None, before_agent=None):
        self.before_setup = before_setup
        self.before_agent = before_agent
        self.trial = None
        self.projects = set()

    async def run(self, specification: LocalTrial) -> dict:
        if self.trial is not None:
            raise RuntimeError("previous trial cleanup required")
        require_runtime()
        from harbor.trial.trial import Trial
        # Defer secrets until both guarded setup and the final budget decision
        # pass. Harbor otherwise executes --version with agent.extra_env.
        secrets = {key: value for key, value in specification.environment.items()
                   if key.endswith("_API_KEY")} if self.before_setup is not None else {}
        public_environment = {key: value for key, value in specification.environment.items()
                              if key not in secrets}
        self.trial = await Trial.create(replace(specification, environment=public_environment).config())
        from harbor.environments.docker.docker import _sanitize_docker_compose_project_name
        self.projects.add(_sanitize_docker_compose_project_name(self.trial.agent_environment.session_id))
        if self.before_setup is not None:
            self.trial.agent.qualification_setup_guard = self.before_setup
        if self.before_agent is not None:
            from harbor.trial.hooks import TrialEvent
            async def authorize(event):
                await self.before_agent(event)
                # Pinned Harbor copies this mapping when entering the run phase.
                # Never mutate it until setup and fresh authorization succeed.
                self.trial.agent._extra_env.update(secrets)
            self.trial.add_hook(TrialEvent.AGENT_START, authorize)
        result = await self.trial.run()
        return normalize_result(result)

    async def teardown(self) -> None:
        if self.trial is None:
            return
        # Harbor's internal stopped flag can mean stop *failed*. Call the public
        # environment stop again, then verify no project containers survive.
        await asyncio.wait_for(self.trial.agent_environment.stop(delete=True), 60)
        for project in self.projects:
            process = await asyncio.create_subprocess_exec(
                "docker", "ps", "-aq", "--filter", f"label=com.docker.compose.project={project}",
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
            stdout, _ = await asyncio.wait_for(process.communicate(), 15)
            if process.returncode or stdout.strip():
                raise RuntimeError("Docker teardown could not be verified")
        # Separate verifier environments are short-lived and not retained on
        # Trial. Search their deterministic, trial-specific project namespace.
        process = await asyncio.create_subprocess_exec(
            "docker", "ps", "-a", "--format", '{{.Label "com.docker.compose.project"}}',
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        stdout, _ = await asyncio.wait_for(process.communicate(), 15)
        prefix = self.trial.config.trial_name + "__verifier__"
        if process.returncode or any(line.startswith(prefix) for line in stdout.decode().splitlines()):
            raise RuntimeError("verifier teardown could not be verified")
        self.trial = None
        self.projects.clear()


def normalize_result(result) -> dict:
    """Never infer verification from an agent's final message or exit code."""
    context = result.agent_result
    metadata = (context.metadata or {}) if context is not None else {}
    state = metadata.get("terminal_state", metadata.get("bonsai_terminal_state"))
    exception = result.exception_info
    if exception is not None:
        kind = exception.exception_type
        category = ("timeout" if kind == "AgentTimeoutError" else
                    state if state in {"cancelled", "budget_exhausted", "agent_failure"}
                    else "infrastructure_invalid")
        reward = None
    elif state not in {"completed", "success"}:
        category, reward = "infrastructure_invalid", None
    elif result.verifier_result is None or not result.reward_valid:
        category, reward = "infrastructure_invalid", None
    else:
        rewards = result.verifier_result.rewards
        reward = rewards.get("reward") if set(rewards) == {"reward"} else None
        if type(reward) not in (int, float) or reward not in (0, 1):
            category, reward = "infrastructure_invalid", None
        else:
            category = "verified"
    usage = {}
    if context is not None:
        for field, key in (("n_input_tokens", "inputTokens"), ("n_output_tokens", "outputTokens"),
                           ("n_cache_tokens", "cacheReadTokens")):
            value = getattr(context, field)
            if value is not None:
                if type(value) is not int or value < 0:
                    raise ValueError("invalid Harbor usage")
                usage[key] = value
    elapsed = None
    if result.started_at is not None and result.finished_at is not None:
        elapsed = (result.finished_at - result.started_at).total_seconds()
        if not math.isfinite(elapsed) or elapsed < 0:
            raise ValueError("invalid Harbor elapsed time")
    estimate = context.cost_usd if context is not None else None
    if estimate is not None and (not math.isfinite(estimate) or estimate < 0):
        raise ValueError("invalid estimated cost")
    return {"category": category, "reward": reward, "usage": usage or None,
            "wall_seconds": elapsed, "estimated_usd": estimate,
            "provider_billed_usd": None, "infrastructure_usd": None}
