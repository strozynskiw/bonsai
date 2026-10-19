"""Approval-gated backend for a narrow, explicitly frozen local Harbor lane.

Only single-step prebuilt-image tasks without Compose/sidecars are admitted.
Execution bindings are part of the manifest digest; unsupported policies fail
before agent execution. This module never downloads tasks, images or agents.
"""

import asyncio
from datetime import datetime
import hashlib
import json
from pathlib import Path, PurePosixPath
import platform
import re
import shlex
import time
import tomllib

from .comparison_gate import amount
from .comparison_manifest import FrozenManifest, digest, require_digest
from .comparison_runner import Consumption
from .local_trial import LocalExecutor, LocalTrial, require_runtime, tree_digest


class BillingFile:
    """Read an external observer's atomically replaced private JSON snapshot.

    The observer is operator-managed, outside task containers, and must include
    every root/child/retry request. This reader is not a provider hard cap.
    """

    def __init__(self, path: Path, manifest: FrozenManifest, phase: str):
        self.path, self.manifest, self.phase = path, manifest, phase
        self.sequence = -1
        self.last = None

    def read(self, *, after: float = 0, identity: str | None = None) -> dict:
        if not self.path.is_file() or self.path.is_symlink() or self.path.stat().st_mode & 0o077:
            raise ValueError("billing observation must be a private regular file")
        value = json.loads(self.path.read_text(encoding="utf-8"))
        if set(value) != {"manifest_digest", "phase", "sequence", "observed_at", "settled_identity",
                          "provider_usd", "infrastructure_usd", "external_cap_evidence_digest"}:
            raise ValueError("invalid billing observation fields")
        if (value["manifest_digest"] != self.manifest.sha256 or value["phase"] != self.phase
                or value["external_cap_evidence_digest"] != self.manifest.body["execution"]["external_cap_evidence_digest"]):
            raise ValueError("billing observation binding mismatch")
        timestamp = datetime.fromisoformat(value["observed_at"])
        if timestamp.tzinfo is None:
            raise ValueError("billing timestamp requires timezone")
        observed = timestamp.timestamp()
        now = time.time()
        if not after <= observed <= now + 2 or now - observed > 30:
            raise ValueError("stale or future billing observation")
        if type(value["sequence"]) is not int or value["sequence"] < max(0, self.sequence):
            raise ValueError("billing sequence regressed")
        if identity is not None and (value["settled_identity"] != identity or value["sequence"] <= self.sequence):
            raise ValueError("fresh settled trial billing required")
        for key in ("provider_usd", "infrastructure_usd"):
            current = amount(value[key], key)
            if self.last is not None and current < amount(self.last[key], key):
                raise ValueError("billing totals regressed")
        if value["sequence"] == self.sequence and value != self.last:
            raise ValueError("billing snapshot changed without sequence advance")
        self.sequence, self.last = value["sequence"], value
        return value


async def docker_json(*arguments: str):
    process = await asyncio.create_subprocess_exec(
        "docker", *arguments, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    stdout, _ = await asyncio.wait_for(process.communicate(), 20)
    if process.returncode:
        raise RuntimeError("Docker inspection failed")
    return json.loads(stdout)


class HarborBackend:
    """One serial frozen local task at a time, with explicit billing settlement."""

    def __init__(self, manifest: FrozenManifest, *, task_root: Path, output: Path,
                 billing: BillingFile, credential: str, evidence: dict, approval=None, records=None):
        self.manifest, self.task_root, self.output = manifest, task_root.resolve(), output
        self.billing, self.credential, self.evidence = billing, credential, evidence
        self.approval, self.records = approval, records
        self.executor = LocalExecutor(before_setup=self._before_setup, before_agent=self._before_agent)
        self.active = None
        self.before = None
        self.started = 0
        self.result = None

    async def preflight(self, manifest: FrozenManifest) -> None:
        if manifest.sha256 != self.manifest.sha256:
            raise ValueError("backend manifest mismatch")
        require_runtime()
        if platform.system() != "Linux":
            raise ValueError("live Harbor backend requires a Linux host, not Docker Desktop")
        body = manifest.body
        execution = body["execution"]
        if set(execution) != {"task_paths", "profiles", "artifact_paths", "image_ids",
                              "request_evidence_digest", "external_cap_evidence_digest", "disk_enforcement"}:
            raise ValueError("complete frozen execution bindings required")
        for key in ("request_evidence_digest", "external_cap_evidence_digest"):
            require_digest(execution[key], key)
        if execution["disk_enforcement"] != "dedicated-runner-volume":
            raise ValueError("dedicated externally quota-limited runner volume required")
        for key in ("request_evidence_digest", "external_cap_evidence_digest"):
            if digest(self.evidence[key]) != execution[key]:
                raise ValueError("reviewed evidence bytes do not match frozen identity")
        request = self.evidence["request_evidence_digest"]
        if (request.get("model") != body["model"] or request.get("protocol") != "messages"
                or request.get("both_agents_observed") is not True
                or request.get("fallback_disabled") is not True):
            raise ValueError("common model/protocol/reasoning request qualification required")
        isolation = self.evidence["isolation_evidence_digest"]
        if (isolation.get("platform") != "linux" or isolation.get("runner") != platform.node()
                or isolation.get("resources") != body["resources"]
                or isolation.get("image_ids") != execution["image_ids"]
                or any(isolation.get(key) is not True for key in (
                    "no_host_credentials", "no_host_home_or_socket", "network_enforced",
                    "disk_quota_enforced", "verifier_isolated", "cancellation_teardown_verified"))):
            raise ValueError("complete runner-specific Linux isolation evidence required")
        if (self.approval is None or self.records is None
                or self.approval.runner != isolation["runner"]):
            raise ValueError("approved runner must match isolation evidence and this host")
        if not self.credential:
            raise ValueError("explicit provider credential required")
        # The DSH native transport is Messages, unlike Bonsai's native DeepSeek
        # Chat connector. Do not admit a hostname-only purported model match.
        if execution["profiles"]["bonsai"]["environment"].get("BONSAI_PROVIDER") != "anthropic-compatible":
            raise ValueError("both agents must use the qualified Messages endpoint")
        if body["model"]["reasoning"] not in {"off", "high", "max"}:
            raise ValueError("unsupported common reasoning mode")
        tasks = {task["id"]: task for task in body["tasks"]}
        if set(execution["task_paths"]) != set(tasks) or set(execution["image_ids"]) != set(tasks):
            raise ValueError("all frozen tasks require local path and image bindings")
        for task_id, task in tasks.items():
            path = self._task_path(task_id)
            if tree_digest(path) != task["content_digest"]:
                raise ValueError("frozen local task digest mismatch")
            config = tomllib.loads((path / "task.toml").read_text(encoding="utf-8"))
            environment = config.get("environment", {})
            if config.get("steps") or list((path / "environment").glob("docker-compose*")):
                raise ValueError("backend supports only prebuilt single-service single-step tasks")
            image = environment.get("docker_image")
            if not isinstance(image, str) or not image.endswith("@" + task["image_digest"]):
                raise ValueError("immutable preinstalled task image required")
            inspected = (await docker_json("image", "inspect", image))[0]
            if inspected["Id"] != execution["image_ids"][task_id]:
                raise ValueError("local image identity mismatch")
            from harbor.models.task.task import Task
            loaded = Task(path)
            if loaded.config.mcp_servers or loaded.has_steps:
                raise ValueError("MCP, injected skills, and multi-step execution are not supported")
            if loaded.config.verifier.environment_mode.value != "separate":
                raise ValueError("separate verifier environment required for live execution")
            from urllib.parse import urlsplit
            endpoint_host = urlsplit(body["model"]["endpoint"]).hostname
            if environment.get("allowed_hosts") != [endpoint_host]:
                raise ValueError("task allowlist must match exactly the common provider host")
            if config.get("agent", {}).get("network_mode") or config.get("verifier", {}).get("network_mode"):
                raise ValueError("phase network overrides are unsupported")
            if environment.get("network_mode") != "allowlist":
                raise ValueError("explicit enforced task network allowlist required")
            if environment.get("gpus", 0) != 0 or environment.get("workdir") != body["resources"]["working_directory"]:
                raise ValueError("task GPU/workdir policy mismatch")
        for agent in ("bonsai", "dsh"):
            profile = execution["profiles"][agent]
            if set(profile) != {"options", "environment"} or digest(profile) != body["profiles"][agent]["overlay_digest"]:
                raise ValueError("frozen profile mismatch")
            permitted = {"BONSAI_PROVIDER"} if agent == "bonsai" else set()
            if set(profile["environment"]) != permitted:
                raise ValueError("only explicitly supported profile environment names allowed")
            if agent == "bonsai":
                options = profile["options"]
                if (options.get("reasoning_effort") != body["model"]["reasoning"]
                        or options.get("timeout_seconds") != body["resources"]["timeout_seconds"]
                        or options.get("network") != "allow"):
                    raise ValueError("Bonsai native profile differs from common settings")
            elif profile["options"]:
                raise ValueError("DSH native defaults must not have hidden adapter options")
            if any(re.search("KEY|TOKEN|SECRET|PASSWORD", name, re.I) for name in profile["environment"]):
                raise ValueError("credentials must never be frozen into profiles")
            artifact_path = execution["artifact_paths"][agent]
            if not PurePosixPath(artifact_path).is_absolute():
                raise ValueError("absolute installed artifact path required")
        self.output.mkdir(parents=True, exist_ok=True, mode=0o700)
        if self.output.stat().st_mode & 0o077:
            raise ValueError("private raw trial output required")

    def _task_path(self, task_id: str) -> Path:
        relative = Path(self.manifest.body["execution"]["task_paths"][task_id])
        path = (self.task_root / relative).resolve()
        if relative.is_absolute() or not path.is_relative_to(self.task_root):
            raise ValueError("task path escapes frozen root")
        return path

    async def consumption(self) -> Consumption:
        value = self.billing.read()
        self.before = value
        return Consumption(value["provider_usd"], value["infrastructure_usd"])

    async def _before_agent(self, event) -> None:
        # Repeat the checks immediately before releasing the scoped credential.
        await self._before_setup(self.executor.trial.agent_environment)

    async def _before_setup(self, environment) -> None:
        value = self.billing.read(after=self.started)
        if self.approval is None or self.records is None:
            raise ValueError("fresh spending authorization required")
        prior = [record for record in self.records() if record["identity"] != self.active.identity]
        self.approval.check_next(prior, observed_provider_usd=value["provider_usd"],
                                 observed_infrastructure_usd=value["infrastructure_usd"])
        body = self.manifest.body
        agent = self.active.agent
        from harbor.environments.docker.docker import _sanitize_docker_compose_project_name
        project = _sanitize_docker_compose_project_name(environment.session_id)
        containers = await docker_json("container", "ls", "--format", "json", "--filter",
                                       f"label=com.docker.compose.project={project}",
                                       "--filter", "label=com.docker.compose.service=main")
        if not isinstance(containers, dict) or not containers.get("ID"):
            raise ValueError("exactly one main container required")
        inspected = (await docker_json("inspect", containers["ID"]))[0]
        if inspected["Image"] != body["execution"]["image_ids"][self.active.task]:
            raise ValueError("running image mismatch")
        resources = body["resources"]
        host = inspected["HostConfig"]
        if (host.get("Privileged") or host.get("PidMode") == "host"
                or host.get("NetworkMode") == "host" or host.get("Devices")
                or host.get("CapAdd") or host.get("DeviceRequests")
                or host.get("Memory") != resources["ram_mb"] * 1024 * 1024
                or host.get("NanoCpus") != resources["cpu"] * 1_000_000_000):
            raise ValueError("live container confinement/resources mismatch")
        if any(re.search("KEY|TOKEN|SECRET|PASSWORD", item.split("=", 1)[0], re.I)
               for item in inspected["Config"].get("Env", [])):
            raise ValueError("image contains ambient credential environment")
        allowed = self.output.resolve()
        for mount in inspected["Mounts"]:
            if mount["Type"] != "bind" or not Path(mount["Source"]).resolve().is_relative_to(allowed):
                raise ValueError("unexpected host bind mount")
        # Inspect host configuration first; only then execute a digest utility,
        # with no API key present in setup's scoped environment.
        binary = body["execution"]["artifact_paths"][agent]
        checked = await environment.exec(command=f"sha256sum {shlex.quote(binary)}")
        expected = body["artifacts"][agent]["artifact_digest"]
        if checked.return_code or "sha256:" + checked.stdout.split()[0] != expected:
            raise ValueError("installed agent artifact mismatch")
        if agent == "dsh":
            checked = await environment.exec(command="sha256sum /usr/local/bin/node")
            if checked.return_code or "sha256:" + checked.stdout.split()[0] != body["node"]["artifact_digest"]:
                raise ValueError("installed Node artifact mismatch")

    async def run(self, trial) -> dict:
        if self.before is None:
            raise ValueError("fresh pre-dispatch billing required")
        self.started = time.time()
        self.active = trial
        body = self.manifest.body
        task = next(task for task in body["tasks"] if task["id"] == trial.task)
        profile = body["execution"]["profiles"][trial.agent]
        environment = dict(profile["environment"])
        prefix = "ANTHROPIC_COMPATIBLE" if trial.agent == "bonsai" else "DEEPSEEK"
        environment[prefix + "_API_KEY"] = self.credential
        environment[prefix + "_BASE_URL"] = body["model"]["endpoint"]
        environment["BONSAI_BIN" if trial.agent == "bonsai" else "DSH_BIN"] = body["execution"]["artifact_paths"][trial.agent]
        environment.update(TB4_REASONING=body["model"]["reasoning"],
                           TB4_TIMEOUT_SECONDS=str(body["resources"]["timeout_seconds"]))
        model = body["model"]["id"]
        name = "tb4-" + hashlib.sha256((self.manifest.sha256 + trial.identity).encode()).hexdigest()[:24]
        resources = body["resources"]
        specification = LocalTrial(self._task_path(trial.task), task["content_digest"], name,
                                   self.output, trial.agent,
                                   "anthropic-compatible/" + model if trial.agent == "bonsai" else model,
                                   environment, profile["options"], resources["timeout_seconds"],
                                   resources["cpu"], resources["ram_mb"], resources["disk_mb"])
        result = await self.executor.run(specification)
        result.update(identity=trial.identity, task=trial.task, agent=trial.agent,
                      manifest_digest=self.manifest.sha256, model=body["model"],
                      artifact_digest=body["artifacts"][trial.agent]["artifact_digest"],
                      profile_digest=body["profiles"][trial.agent]["overlay_digest"], redacted_log=None)
        self.result = result
        return result

    async def teardown(self, trial) -> None:
        await self.executor.teardown()
        # Settle after process/container teardown, so no descendant can spend
        # after the snapshot. Missing settlement remains unknown and halts resume.
        if self.result is not None:
            try:
                value = self.billing.read(after=self.started, identity=trial.identity)
                self.result["provider_billed_usd"] = float(amount(value["provider_usd"], "billing")
                    - amount(self.before["provider_usd"], "billing"))
                self.result["infrastructure_usd"] = float(amount(value["infrastructure_usd"], "billing")
                    - amount(self.before["infrastructure_usd"], "billing"))
            except (ValueError, OSError, KeyError):
                self.result.update(category="infrastructure_invalid", reward=None)
                raise
        self.active = self.result = None
