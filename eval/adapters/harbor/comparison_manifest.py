"""Offline-only, fail-closed identities for the TB4 CPU comparison.

No import or function in this module accesses the network or launches a process.
Artifact identities are supplied from explicit operator setup, never guessed.
"""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import re
from typing import Any
from urllib.parse import urlsplit

HARBOR_COMMIT = "7331d8a3e23cb72e8277522059ff7c9ab9435d90"
DSH_COMMIT = "687f282e8625c6b8d11ff58211a90524be2e4317"
DATASET = "terminal-bench/terminal-bench@4.0.0"
SCHEMA_VERSION = 1


def canonical_bytes(value: Any) -> bytes:
    """Stable UTF-8 JSON, rejecting NaN/Infinity rather than hashing them."""
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=False, allow_nan=False).encode("utf-8")


def digest(value: Any) -> str:
    return "sha256:" + hashlib.sha256(canonical_bytes(value)).hexdigest()


def require_digest(value: str, field: str) -> None:
    if not isinstance(value, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", value):
        raise ValueError(f"{field} requires a sha256 content digest")


def require_text(value: str, field: str) -> None:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{field} must be explicitly supplied")


@dataclass(frozen=True)
class Artifact:
    """A build identity includes source content, not only a Git revision."""

    revision: str
    source_digest: str
    dirty: bool
    artifact_digest: str
    version: str

    @classmethod
    def parse(cls, value: dict, name: str) -> Artifact:
        result = cls(**value)
        require_text(result.revision, f"{name}.revision")
        require_text(result.version, f"{name}.version")
        require_digest(result.source_digest, f"{name}.source_digest")
        require_digest(result.artifact_digest, f"{name}.artifact_digest")
        if type(result.dirty) is not bool:
            raise ValueError(f"{name}.dirty must be a boolean")
        return result


@dataclass(frozen=True)
class FrozenManifest:
    """Validated, digest-bound preparation input, not permission to execute."""

    _canonical: bytes
    sha256: str

    @property
    def body(self) -> dict:
        """Return a detached view; callers cannot mutate the frozen identity."""
        return json.loads(self._canonical)

    @classmethod
    def parse(cls, envelope: dict) -> FrozenManifest:
        if set(envelope) != {"manifest", "digest"}:
            raise ValueError("expected manifest and digest envelope")
        body = envelope["manifest"]
        if envelope["digest"] != digest(body):
            raise ValueError("manifest digest mismatch")
        validate_manifest(body)
        # Own a detached copy so caller mutation cannot change a frozen identity.
        return cls(canonical_bytes(body), envelope["digest"])

    @classmethod
    def load(cls, path: Path) -> FrozenManifest:
        return cls.parse(json.loads(path.read_text(encoding="utf-8")))


def validate_manifest(body: dict) -> None:
    """Validate every preparation identity before scheduling or result import."""
    expected = {"schema_version", "dataset", "harbor_commit", "dataset_digest",
                "dependencies_digest", "node", "artifacts", "profiles", "model",
                "resources", "tasks", "selection", "exclusions"}
    if "execution" in body:
        expected.add("execution")
        if not isinstance(body["execution"], dict):
            raise ValueError("execution bindings must be an object")
    if set(body) != expected:
        raise ValueError(f"manifest fields differ: {sorted(set(body) ^ expected)}")
    if body["schema_version"] != SCHEMA_VERSION or body["dataset"] != DATASET:
        raise ValueError("expected distinct TB4 4.0.0 comparison schema")
    if body["harbor_commit"] != HARBOR_COMMIT:
        raise ValueError("unsupported TB4 Harbor commit")
    for name in ("dataset_digest", "dependencies_digest"):
        require_digest(body[name], name)
    if set(body["artifacts"]) != {"bonsai", "dsh"}:
        raise ValueError("exactly bonsai and dsh artifacts required")
    for name, value in body["artifacts"].items():
        artifact = Artifact.parse(value, name)
        if name == "dsh" and (artifact.revision != DSH_COMMIT or artifact.dirty):
            raise ValueError("DSH must use the clean locked upstream revision")
    if set(body["node"]) != {"version", "artifact_digest"}:
        raise ValueError("Node version and artifact digest required")
    require_text(body["node"]["version"], "node.version")
    require_digest(body["node"]["artifact_digest"], "node.artifact_digest")
    if set(body["profiles"]) != {"bonsai", "dsh"}:
        raise ValueError("both native profiles must be frozen")
    for name, profile in body["profiles"].items():
        require_digest(profile["overlay_digest"], f"{name}.overlay_digest")
        require_text(profile["native_limits"], f"{name}.native_limits")
    model = body["model"]
    if set(model) != {"id", "endpoint", "reasoning", "alias_limitation", "fallback"}:
        raise ValueError("explicit common model contract required")
    require_text(model["id"], "model.id")
    require_text(model["reasoning"], "model.reasoning")
    if model["reasoning"] not in {"off", "low", "high", "max"}:
        raise ValueError("unsupported shared DeepSeek reasoning setting")
    if not isinstance(model["alias_limitation"], str) or model["fallback"] is not False:
        raise ValueError("disclose model alias limitation and disable fallback")
    endpoint = urlsplit(model["endpoint"])
    if endpoint.scheme != "https" or not endpoint.hostname or endpoint.username or endpoint.password or endpoint.query or endpoint.fragment:
        raise ValueError("shared endpoint must be credential-free HTTPS")
    resources = body["resources"]
    if set(resources) != {"cpu", "ram_mb", "disk_mb", "timeout_seconds", "network", "working_directory", "confinement"}:
        raise ValueError("explicit common resource contract required")
    for key in ("cpu", "ram_mb", "disk_mb", "timeout_seconds"):
        if type(resources[key]) is not int or resources[key] <= 0:
            raise ValueError(f"resources.{key} must be a positive integer")
    for key in ("network", "working_directory", "confinement"):
        require_text(resources[key], f"resources.{key}")
    tasks = body["tasks"]
    seen = set()
    for task in tasks:
        if set(task) != {"id", "content_digest", "resource_digest", "image_digest", "phase"}:
            raise ValueError("task contents, authoritative resources and image identities required")
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,199}", task["id"]) or task["id"] in seen:
            raise ValueError("unsafe or duplicate task identity")
        seen.add(task["id"])
        for key in ("content_digest", "resource_digest", "image_digest"):
            require_digest(task[key], f"task.{key}")
        if task["phase"] not in {"smoke", "pilot"}:
            raise ValueError("unknown task phase")
    if sum(t["phase"] == "smoke" for t in tasks) != 3 or sum(t["phase"] == "pilot" for t in tasks) != 10:
        raise ValueError("freeze 3 distinct smoke and 10 pilot tasks")
    selection = body["selection"]
    if set(selection) != {"seed", "rule", "inventory_digest", "repetitions"} or selection["rule"] != "sha256-seed-task-v1" or selection["repetitions"] != 3:
        raise ValueError("unsupported deterministic selection/repetition rule")
    require_text(selection["seed"], "selection.seed")
    require_digest(selection["inventory_digest"], "selection.inventory_digest")
    excluded = set()
    for item in body["exclusions"]:
        if set(item) != {"id", "reason", "resource_digest"} or item["id"] in seen | excluded:
            raise ValueError("duplicate or malformed exclusion")
        excluded.add(item["id"])
        require_text(item["reason"], "exclusion.reason")
        require_digest(item["resource_digest"], "exclusion.resource_digest")


def freeze(body: dict) -> dict:
    """Create the same envelope used by preparation, import and execution."""
    validate_manifest(body)
    return {"manifest": body, "digest": digest(body)}
