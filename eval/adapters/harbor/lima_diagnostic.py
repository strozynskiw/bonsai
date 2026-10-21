"""One fresh serial DSH/Bonsai diagnostic pair; never smoke or scored evidence."""

import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import shutil

from .comparison_journal import FileJournal
from .comparison_manifest import digest
from .comparison_schedule import Trial
from .lima_attempt import run_retained
from .lima_manifest import LimaManifest
from .lima_paid import OPTIONS, install_artifacts, validate_runtime
from .lima_trial import LimaExecutor, LimaTrial

POLICY = {"schema": "tb4-lima-diagnostic-v1", "run_id": "diagnostic-rs-archive-20261021-v1",
          "task": "rs-archive-clone", "agents": ["dsh", "bonsai"], "attempts_per_agent": 1,
          "spending_policy": "uncapped", "unknown_billing": "null", "output_mb": 512,
          "bonsai_options": OPTIONS, "dsh_turn_limit": None, "attempt_retry": False,
          "reasoning": "off", "fallback": False, "checkpoint_log": "/logs/agent/checkpoints.jsonl"}


def schedule(body: dict) -> list[Trial]:
    if body.get("diagnostic_policy") != POLICY:
        raise ValueError("frozen diagnostic policy required")
    model = body.get("model", {})
    if (model.get("id") != "deepseek-v4-flash" or model.get("reasoning") != "off"
            or model.get("fallback") is not False):
        raise ValueError("diagnostic requires DeepSeek V4 Flash, reasoning off, no fallback")
    tasks = [entry for entry in body["preparation"]["tasks"]
             if entry["id"] == POLICY["task"] and entry["candidate"]]
    if len(tasks) != 1:
        raise ValueError("exactly one qualified diagnostic task required")
    return [Trial(POLICY["task"], 0, agent, POLICY["run_id"] + "/diagnostic")
            for agent in POLICY["agents"]]


def check_approval(value: dict, manifest: LimaManifest, phase: str, prerequisite: str) -> None:
    if phase not in {"diagnostic-request", "diagnostic"}:
        raise ValueError("diagnostic phase required")
    expected = {"schema": "tb4-lima-approval-v1", "manifest_digest": manifest.sha256,
                "policy_digest": digest(POLICY), "phase": phase, "prerequisite_digest": prerequisite,
                "execute_approved": True, "uncapped_approved": True}
    if value != expected:
        raise ValueError("explicit diagnostic manifest/policy/phase-bound uncapped approval required")


def require_request(evidence: dict, manifest: LimaManifest, prerequisite: str) -> None:
    if (digest(evidence) != prerequisite or evidence.get("manifest_digest") != manifest.sha256
            or evidence.get("qualified") is not True
            or evidence.get("qualification_phase") != "diagnostic-request"
            or evidence.get("policy_digest") != digest(POLICY)
            or evidence.get("model") != manifest.body["model"]["id"]):
        raise ValueError("fresh diagnostic-bound request qualification required")


def remaining(records: list[dict], expected: list[Trial], manifest_digest: str) -> list[Trial]:
    identities = [trial.identity for trial in expected]
    seen = [record["identity"] for record in records]
    if (len(set(seen)) != len(seen) or set(seen) != set(identities[:len(seen)])
            or len(seen) > len(identities)):
        raise ValueError("diagnostic records must be a unique serial prefix")
    for record in records:
        if (record.get("manifest_digest") != manifest_digest
                or record.get("category") not in {"verified", "agent_failure", "verifier_failure"}
                or not record.get("raw_sha256")):
            raise ValueError("invalid occupied diagnostic slot; inspect, never retry")
    return expected[len(seen):]


async def execute(args):
    manifest = LimaManifest.parse(json.loads(args.manifest.read_text(encoding="utf-8")))
    await validate_runtime(manifest, args.artifacts, args.tasks, args.dependencies)
    expected = schedule(manifest.body)
    evidence = json.loads((args.results / "request.json").read_text(encoding="utf-8"))
    require_request(evidence, manifest, args.prerequisite)
    def approve():
        check_approval(json.loads(args.approval.read_text(encoding="utf-8")), manifest,
                       "diagnostic", args.prerequisite)
    approve()
    credential = os.environ.get("TB4_PROVIDER_KEY", "")
    if not credential.strip():
        raise ValueError("explicit TB4_PROVIDER_KEY required")
    with FileJournal(args.results / "diagnostic") as journal:
        pending = remaining(journal.records(), expected, manifest.sha256)
        for trial in pending:
            approve()
            if shutil.disk_usage("/var/lib/tb4-docker").free < 14 * 1024**3:
                raise ValueError("insufficient diagnostic retention headroom")
            async def setup(environment):
                approve()
                await install_artifacts(environment, args.artifacts)
            async def authorize(event):
                approve()
            executor = LimaExecutor(before_setup=setup, before_agent=authorize)
            entry = next(entry for entry in manifest.body["preparation"]["tasks"] if entry["id"] == trial.task)
            prefix = "ANTHROPIC_COMPATIBLE" if trial.agent == "bonsai" else "DEEPSEEK"
            environment = {prefix + "_API_KEY": credential,
                prefix + "_BASE_URL": manifest.body["model"]["endpoint"],
                "TB4_REASONING": "off", "TB4_TIMEOUT_SECONDS": "3600",
                "BONSAI_MODEL_CATALOG": str(Path(__file__).with_name("bonsai_flash_model.toml").resolve()),
                "BONSAI_BIN": "/opt/benchmark/artifacts/bonsai-linux-amd64", "DSH_BIN": "/opt/benchmark/bin/dsh"}
            if trial.agent == "bonsai":
                environment["BONSAI_CHECKPOINT_LOG"] = POLICY["checkpoint_log"]
            name = "tb4-" + hashlib.sha256((manifest.sha256 + trial.identity).encode()).hexdigest()[:24]
            specification = LimaTrial(args.tasks / trial.task, entry["content_digest"], name, args.results,
                trial.agent, ("anthropic-compatible/" if trial.agent == "bonsai" else "") + manifest.body["model"]["id"],
                environment, dict(OPTIONS) if trial.agent == "bonsai" else {}, 3600, 4, 10240, 40960)
            print(json.dumps({"starting": trial.identity, "manifest": manifest.sha256,
                              "diagnostic_only": True, "dsh_turn_limit": None}), flush=True)
            result = await run_retained(specification, trial, manifest.sha256, journal, executor,
                                        size_mb=POLICY["output_mb"])
            print(json.dumps({"finished": trial.identity, **result}), flush=True)
            remaining(journal.records(), expected, manifest.sha256)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("manifest", "artifacts", "tasks", "dependencies", "results", "approval"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--prerequisite", required=True)
    args = parser.parse_args()
    # LimaExecutor owns the trial lock until verified teardown. Acquiring it
    # here too deadlocks the executor against this process's other descriptor.
    asyncio.run(execute(args))


if __name__ == "__main__":
    main()
