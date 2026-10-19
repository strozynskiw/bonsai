"""Deterministic CPU-only selection and serial paired schedules (offline)."""

from __future__ import annotations

from dataclasses import asdict, dataclass
import hashlib
from typing import Iterable

from .comparison_manifest import FrozenManifest, digest, require_digest, require_text


@dataclass(frozen=True)
class Trial:
    task: str
    repetition: int
    agent: str
    phase: str

    @property
    def identity(self) -> str:
        return f"{self.phase}/{self.task}/{self.repetition}/{self.agent}"

    def to_dict(self) -> dict:
        return {**asdict(self), "identity": self.identity}


def select_tasks(inventory: dict, seed: str, resources: dict) -> dict:
    """Select before outcomes exist, from a complete local resource inventory.

    Inventory must be exported/reviewed from the authoritative task.toml files;
    unknown requirements are exclusions, never inferred to mean zero GPUs.
    """
    require_text(seed, "seed")
    if set(inventory) != {"dataset", "dataset_digest", "tasks"}:
        raise ValueError("complete authoritative inventory required")
    from .comparison_manifest import DATASET
    if inventory["dataset"] != DATASET:
        raise ValueError("wrong inventory dataset")
    require_digest(inventory["dataset_digest"], "inventory.dataset_digest")
    eligible, excluded, seen = [], [], set()
    for task in inventory["tasks"]:
        if set(task) != {"id", "content_digest", "image_digest", "requirements", "resource_source"}:
            raise ValueError("task inventory fields differ")
        if task["id"] in seen:
            raise ValueError("duplicate inventory task")
        seen.add(task["id"])
        for key in ("content_digest", "image_digest"):
            require_digest(task[key], key)
        require_text(task["resource_source"], "task.resource_source")
        req = task["requirements"]
        reason = None
        if set(req) != {"gpus", "cpu", "ram_mb", "disk_mb", "unsupported"}:
            reason = "unknown authoritative resource requirements"
        elif any(type(req[key]) is not int or req[key] < 0 for key in ("gpus", "cpu", "ram_mb", "disk_mb")):
            reason = "unknown authoritative resource requirements"
        elif req["gpus"] != 0:
            reason = "requires GPU"
        elif req["unsupported"]:
            reason = "unsupported resources: " + str(req["unsupported"])
        elif any(req[key] > resources[key] for key in ("cpu", "ram_mb", "disk_mb")):
            reason = "exceeds approved CPU/RAM/disk"
        resource_digest = digest({"requirements": req, "source": task["resource_source"]})
        if reason:
            excluded.append({"id": task["id"], "reason": reason, "resource_digest": resource_digest})
        else:
            eligible.append({"id": task["id"], "content_digest": task["content_digest"],
                             "image_digest": task["image_digest"], "resource_digest": resource_digest})
    eligible.sort(key=lambda task: (hashlib.sha256((seed + "\0" + task["id"]).encode()).hexdigest(), task["id"]))
    if len(eligible) < 13:
        raise ValueError(f"need 13 eligible CPU tasks; found {len(eligible)}")
    selected = [{**task, "phase": "smoke" if i < 3 else "pilot"} for i, task in enumerate(eligible[:13])]
    excluded.extend({"id": task["id"], "reason": "not sampled by fixed rule", "resource_digest": task["resource_digest"]}
                    for task in eligible[13:])
    return {"tasks": selected, "exclusions": sorted(excluded, key=lambda t: t["id"]),
            "selection": {"seed": seed, "rule": "sha256-seed-task-v1", "inventory_digest": digest(inventory), "repetitions": 3}}


def schedule(manifest: FrozenManifest, phase: str) -> list[Trial]:
    if phase not in {"smoke", "pilot"}:
        raise ValueError("phase must be smoke or pilot")
    seed = manifest.body["selection"]["seed"]
    tasks = sorted(t["id"] for t in manifest.body["tasks"] if t["phase"] == phase)
    trials = []
    # Adjacent paired trials, deterministic alternating order; each scored task
    # gets both orders over three repetitions, with no concurrent contention.
    offset = int(hashlib.sha256(seed.encode()).hexdigest(), 16) % 2
    for i, task in enumerate(tasks):
        for repetition in range(1, (3 if phase == "pilot" else 1) + 1):
            order = ("bonsai", "dsh") if (i + repetition + offset) % 2 else ("dsh", "bonsai")
            trials.extend(Trial(task, repetition, agent, phase) for agent in order)
    return trials


def remaining(manifest: FrozenManifest, phase: str, records: Iterable[dict]) -> list[Trial]:
    expected = {trial.identity: trial for trial in schedule(manifest, phase)}
    seen = set()
    for record in records:
        identity = record["identity"]
        if identity not in expected or identity in seen:
            raise ValueError("foreign or duplicate trial identity")
        if record["manifest_digest"] != manifest.sha256:
            raise ValueError("resume manifest mismatch")
        seen.add(identity)
    # Every persisted attempt, including invalid/failed/interrupted attempts,
    # occupies its identity. Reruns need a separately reviewed experiment.
    return [trial for key, trial in expected.items() if key not in seen]
