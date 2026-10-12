# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "swebench @ git+https://github.com/SWE-bench/SWE-bench.git@f7bbbb2ccdf479001d6467c9e34af59e44a840f9",
# ]
# ///
"""Phase E — external qualification driver for the SWE-bench Verified adapter.

This is the deliberately-remote glue described in
`plans/external_benchmark_adapters.plan`. It is NOT part of Bonsai CI and MUST
NOT be run on a machine that cannot afford dataset + container-image pulls.
Running the `grade`/`all` stages downloads the SWE-bench Verified dataset and
pulls one multi-GB Docker image per instance, and the `run` stage spends real
provider tokens. See README.md for the full prerequisite contract.

The Bonsai adapter (`bonsai eval adapter run`) owns only the agent run and
prediction extraction. This script owns the surrounding, externally-owned
lifecycle the adapter deliberately does not: dataset access, host workspace
provisioning at the pinned base commit, request-batch generation, invocation of
the pinned official grader, and normalization of the grader report into a
Bonsai baseline profile.

Stages:
  selftest       Offline wiring check — synthetic instance + stub bonsai, no
                 network / Docker / provider spend. Proves request generation is
                 accepted by the real adapter CLI.
  provision      Clone each instance repo at its base commit and emit the
                 AdapterRequest batch JSON. (network)
  run            Invoke `bonsai eval adapter run` over the batch. (provider spend)
  grade          Feed predictions.jsonl to the pinned official SWE-bench
                 harness. (dataset + Docker image pulls)
  import-report  Normalize the grader report into a Bonsai baseline profile.
  all            provision -> run -> grade -> import-report.
"""

from __future__ import annotations

import argparse
import json
import platform
import importlib.metadata
import os
import shutil
import stat
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

# --- Pinned upstream identity. Must match src/eval/adapters/contract.rs. -------
# If the adapter's pins change, these must change with them; a mismatch is
# rejected by the adapter before any agent launches, which is the intended
# fail-closed behavior.
SWE_BENCH_HARNESS_COMMIT = "f7bbbb2ccdf479001d6467c9e34af59e44a840f9"
SWE_BENCH_PREDICTION_SCHEMA_COMMIT = "b679692b8b7e274a6c89fd0842f25b02da4b9256"
ADAPTER_SCHEMA_VERSION = 1
DATASET_NAME = "princeton-nlp/SWE-bench_Verified"
DATASET_HF = "swe-bench_verified"
DATASET_SPLIT = "test"

# Conservative per-task budgets for a canary. Deliberately explicit — benchmark
# mode never inherits ambient defaults (see plan "Safety and Reproducibility").
DEFAULT_BUDGETS = {
    "max_turns": 80,
    "max_generation_seconds": 900,
    "max_output_chars": 400_000,
    "max_tool_seconds": 300,
    "timeout_seconds": 3_600,
    "max_patch_bytes": 1_000_000,
}

# Trusted scaffolding wrapped around the untrusted problem statement. The host
# workspace is a bare git checkout with no test environment installed, so the
# agent's own attempts to run the suite fail on missing deps — which then flips
# its completion report to `failed` (exit 1) and burns tokens ruminating about
# the unavailable verification, even when the fix is correct. SWE-bench grades
# the patch in its own container, so self-verification here is both impossible
# and unnecessary. Standard SWE-bench agent scaffolding; reveals no test/oracle
# content. Kept short and generic on purpose.
SWE_BENCH_SCAFFOLD = (
    "You are resolving a GitHub issue in a checked-out repository. Make the "
    "minimal source-code change that fixes the problem described below.\n\n"
    "IMPORTANT: The test environment and dependencies are NOT installed in this "
    "workspace, and an external harness runs the test suite after you finish. Do "
    "NOT attempt to run the tests, install dependencies, or otherwise verify by "
    "execution — those commands will fail on missing setup. Make the code change, "
    "then stop and report what you changed.\n\n"
    "--- Issue ---\n"
)


@dataclass
class Instance:
    instance_id: str
    repo: str
    base_commit: str
    problem_statement: str


def eprint(*args: object) -> None:
    print(*args, file=sys.stderr, flush=True)


def run_checked(cmd: list[str], **kwargs: object) -> subprocess.CompletedProcess:
    eprint(f"$ {' '.join(cmd)}")
    return subprocess.run(cmd, check=True, **kwargs)  # type: ignore[arg-type]


# --- Request generation -------------------------------------------------------

def build_request(
    inst: Instance,
    workspace: Path,
    binary: Path,
    revision: str,
    provider: str,
    model: str,
    effort: str,
    autonomy: str,
    network: str,
) -> dict:
    """One AdapterRequest object matching the swe_bench_verified contract."""
    return {
        "schema_version": ADAPTER_SCHEMA_VERSION,
        "benchmark": {
            "kind": "swe_bench_verified",
            "dataset": DATASET_HF,
            "dataset_version": DATASET_SPLIT,
            "harness_commit": SWE_BENCH_HARNESS_COMMIT,
            "contract_commit": SWE_BENCH_PREDICTION_SCHEMA_COMMIT,
        },
        "task": {
            "id": inst.instance_id,
            "workspace": str(workspace),
            # Trusted scaffolding + the untrusted problem statement. The adapter
            # passes the whole thing on stdin, never as a system instruction
            # (plan safety section); the scaffold is our own text, so it does
            # not widen the untrusted-content surface.
            "instruction": SWE_BENCH_SCAFFOLD + inst.problem_statement,
            "base_commit": inst.base_commit,
        },
        "runner": {
            "bonsai_binary": str(binary),
            "bonsai_revision": revision,
            "provider": provider,
            "model": model,
            "reasoning_effort": effort,
            "autonomy": autonomy,
            "network": network,
            "budgets": dict(DEFAULT_BUDGETS),
        },
    }


def clone_at_commit(repo: str, base_commit: str, dest: Path) -> None:
    """Provision a host checkout of `repo` at `base_commit` for the agent run.

    The official grader re-applies the emitted patch inside its own image; this
    checkout only needs to match base_commit so the extracted diff is coherent.
    """
    dest.mkdir(parents=True, exist_ok=True)
    url = f"https://github.com/{repo}.git"
    run_checked(["git", "init", "-q"], cwd=dest)
    run_checked(["git", "remote", "add", "origin", url], cwd=dest)
    # Fetch just the needed commit when the server allows it; fall back to a full
    # fetch. Either way we end in a detached checkout at base_commit.
    try:
        run_checked(
            ["git", "fetch", "-q", "--depth", "1", "origin", base_commit], cwd=dest
        )
    except subprocess.CalledProcessError:
        run_checked(["git", "fetch", "-q", "origin"], cwd=dest)
    run_checked(["git", "checkout", "-q", base_commit], cwd=dest)


def load_instances(instance_ids: list[str], revision: str | None = None) -> list[Instance]:
    from datasets import load_dataset  # deferred: heavy import, network on first use

    eprint(f"Loading {DATASET_NAME}[{DATASET_SPLIT}] ...")
    ds = load_dataset(DATASET_NAME, split=DATASET_SPLIT, revision=revision)
    wanted = set(instance_ids)
    found: dict[str, Instance] = {}
    for row in ds:
        iid = row["instance_id"]
        if iid in wanted:
            found[iid] = Instance(
                instance_id=iid,
                repo=row["repo"],
                base_commit=row["base_commit"],
                problem_statement=row["problem_statement"],
            )
    missing = wanted - set(found)
    if missing:
        raise SystemExit(f"Instance ids not present in dataset: {sorted(missing)}")
    # Preserve caller order for stable, resumable output.
    return [found[i] for i in instance_ids]


# --- Stages -------------------------------------------------------------------

def stage_provision(args: argparse.Namespace) -> None:
    out = Path(args.out)
    workspaces = out / "workspaces"
    out.mkdir(parents=True, exist_ok=True)
    instance_ids = read_instance_ids(args)
    instances = load_instances(instance_ids, args.dataset_revision)

    batch = []
    for inst in instances:
        ws = workspaces / inst.instance_id
        if ws.exists() and not args.force:
            eprint(f"reuse workspace {ws}")
        else:
            if ws.exists():
                shutil.rmtree(ws)
            clone_at_commit(inst.repo, inst.base_commit, ws)
        batch.append(
            build_request(
                inst,
                ws.resolve(),
                Path(args.binary).resolve(),
                args.revision,
                args.provider,
                args.model,
                args.effort,
                args.autonomy,
                args.network,
            )
        )

    batch_path = out / "requests.json"
    batch_path.write_text(json.dumps(batch, indent=2))
    eprint(f"Wrote {len(batch)} requests -> {batch_path}")


def stage_run(args: argparse.Namespace) -> None:
    out = Path(args.out)
    batch_path = out / "requests.json"
    if not batch_path.exists():
        raise SystemExit(f"Missing {batch_path}; run `provision` first.")
    # The adapter exits non-zero when any task's bonsai terminal_state is not
    # `completed`. For benchmark scoring that is only diagnostic: the official
    # grader judges the produced patch, so we proceed to grade any non-empty
    # prediction and only surface the terminal state as a warning.
    cmd = [
        args.binary, "eval", "adapter", "run",
        "--request", str(batch_path), "--out", str(out / "adapter"), "--json",
    ]
    eprint(f"$ {' '.join(cmd)}")
    proc = subprocess.run(cmd)
    preds = out / "adapter" / "predictions.jsonl"
    non_empty = [
        json.loads(line)
        for line in (preds.read_text().splitlines() if preds.exists() else [])
        if line.strip() and len(json.loads(line).get("model_patch", "")) > 0
    ]
    if not non_empty:
        raise SystemExit(
            f"Adapter produced no non-empty predictions at {preds} "
            f"(adapter exit {proc.returncode}). Nothing to grade."
        )
    if proc.returncode != 0:
        eprint(
            f"WARNING: adapter exited {proc.returncode} (a task's bonsai "
            f"terminal_state != completed), but {len(non_empty)} non-empty "
            "prediction(s) were produced and will be graded."
        )
    eprint(f"Predictions -> {preds} ({len(non_empty)} gradeable)")


def stage_grade(args: argparse.Namespace) -> None:
    out = Path(args.out)
    preds = out / "adapter" / "predictions.jsonl"
    if not preds.exists():
        raise SystemExit(f"Missing {preds}; run `run` first.")
    instance_ids = read_instance_ids(args)
    from datasets import load_dataset
    snapshot = load_dataset(DATASET_NAME, split=DATASET_SPLIT, revision=args.dataset_revision)
    snapshot_path = out / "pinned-dataset.json"
    snapshot.to_json(str(snapshot_path), orient="records", lines=False)
    # The harness runs in `out` (it writes its report + logs there), so the
    # predictions path must be absolute, not relative to the repo cwd.
    cmd = [
        sys.executable,
        "-m",
        "swebench.harness.run_evaluation",
        "--dataset_name",
        str(snapshot_path.resolve()),
        "--split",
        DATASET_SPLIT,
        "--predictions_path",
        str(preds.resolve()),
        "--run_id",
        args.run_id,
        "--max_workers",
        str(args.max_workers),
        "--instance_ids",
        *instance_ids,
    ]
    run_checked(cmd, cwd=out)
    eprint("Grading complete; locating report ...")
    report = find_report(out, args.run_id)
    eprint(f"Grader report -> {report}")


def stage_import(args: argparse.Namespace) -> None:
    out = Path(args.out)
    report_path = Path(args.report) if args.report else find_report(out, args.run_id)
    report = json.loads(report_path.read_text())
    if not args.manifest:
        raise SystemExit("import-report requires --manifest with the frozen task selection")
    manifest = json.loads(Path(args.manifest).read_text(encoding="utf-8"))
    suites = [s for s in manifest["suites"] if s["kind"] == "swe_bench_verified"]
    if len(suites) != 1 or suites[0]["repetitions"] != 1:
        raise SystemExit("SWE import requires exactly one suite and one repetition per report")
    suite = suites[0]
    verified = json.loads((out / "verified-runner.json").read_text(encoding="utf-8"))
    if verified["binary_blake3"] != manifest["candidate"]["binary_blake3"] or verified["revision"] != manifest["candidate"]["revision"]:
        raise SystemExit("Verified candidate identity differs from manifest")
    if verified["dataset_revision"] != suite["dataset_version"]:
        raise SystemExit("Verified dataset identity differs from manifest")
    if suite["harness_commit"] != SWE_BENCH_HARNESS_COMMIT or suite["contract_commit"] != SWE_BENCH_PREDICTION_SCHEMA_COMMIT:
        raise SystemExit("Manifest upstream pins differ from supported pins")
    sidecars = [json.loads(p.read_text(encoding="utf-8")) for p in (out / "adapter").glob("*/bonsai-sidecar.json")]
    sidecar_ids = [s["task_id"] for s in sidecars]
    if len(sidecar_ids) != len(set(sidecar_ids)) or set(sidecar_ids) != set(suite["task_ids"]):
        raise SystemExit("Missing, duplicate or foreign sidecars")
    for sidecar in sidecars:
        actual = sidecar["bonsai"]
        if any(actual.get(key) != value for key, value in manifest["profile"].items()):
            raise SystemExit("Sidecar execution profile differs from manifest")
        if actual["declared_revision"] != manifest["candidate"]["revision"]:
            raise SystemExit("Sidecar candidate revision differs from manifest")
        if sidecar["benchmark"]["harness_commit"] != suite["harness_commit"]:
            raise SystemExit("Sidecar harness differs from manifest")
    counts = validate_grading(report, suite["task_ids"])
    total = counts["total"]
    resolved = counts["pass"]
    score = (resolved / total * 100.0) if total else 0.0
    scorecard = {
        "suite": "swe_bench_verified_canary",
        "dataset": DATASET_HF,
        "dataset_version": suite["dataset_version"],
        "harness_commit": SWE_BENCH_HARNESS_COMMIT,
        "profile": manifest["profile"],
        "counts": counts,
        "complete": counts["error"] == 0 and counts["missing"] == 0,
        "total_instances": total,
        "resolved_instances": resolved,
        "score_percent": round(score, 2),
        "resolved_ids": report.get("resolved_ids", []),
        "unresolved_ids": report.get("unresolved_ids", []),
        "error_ids": report.get("error_ids", []),
    }
    scorecard_path = out / "scorecard.json"
    scorecard_path.write_text(json.dumps(scorecard, indent=2))
    eprint(f"Scorecard -> {scorecard_path}")
    manifest_digest = subprocess.run(["b3sum", args.manifest], capture_output=True,
                                     text=True, check=True).stdout.split()[0]
    profile = dict(sidecars[0]["bonsai"])
    suites_out = []
    for selected in manifest["suites"]:
        ids = selected["task_ids"]
        expected = ids if selected["kind"] == "swe_bench_verified" else [
            f"{task}-run-{rep}" for task in ids for rep in range(1, selected["repetitions"] + 1)]
        pin = {key: selected[key] for key in ("kind", "dataset", "dataset_version", "harness_commit", "contract_commit")}
        suites_out.append({"pin": pin, "expected_trials": expected, "trials": []})
    harbor_trials = []
    if args.harbor_trials:
        harbor_trials = json.loads(Path(args.harbor_trials).read_text(encoding="utf-8"))
    bundle = {
        "scorecard": {"schema_version": 1, "fixture": manifest["evidence_kind"] != "real",
                      "reviewed": False, "manifest_digest": manifest_digest,
                      "binary_digest": verified["binary_blake3"], "profile": profile,
                      "suites": suites_out},
        "swe_report": report,
        "swe_provenance": {**verified, "sidecars": sidecars},
        "harbor_trials": harbor_trials,
    }
    (out / "import-bundle.json").write_text(json.dumps(bundle, indent=2), encoding="utf-8")

    # Import is diagnostic, never an automatic baseline promotion.
    if not scorecard["complete"]:
        raise SystemExit("Incomplete grading: baseline promotion is forbidden")


def stage_selftest(args: argparse.Namespace) -> None:
    """Offline proof that generated requests are accepted by the real adapter.

    No network, no Docker, no provider spend: a synthetic instance backed by a
    local git repo and a stub bonsai that emits the headless JSON contract.
    """
    binary = Path(args.binary)
    if not binary.exists():
        raise SystemExit(f"Adapter binary not found at {binary}; build it first.")

    work = Path(tempfile.mkdtemp(prefix="phase-e-selftest-"))
    try:
        ws = work / "workspace"
        ws.mkdir()
        run_checked(["git", "init", "-q"], cwd=ws)
        run_checked(["git", "config", "user.email", "eval@example.test"], cwd=ws)
        run_checked(["git", "config", "user.name", "Eval"], cwd=ws)
        (ws / "tracked.txt").write_text("before\n")
        run_checked(["git", "add", "tracked.txt"], cwd=ws)
        run_checked(["git", "commit", "-qm", "base"], cwd=ws)
        base = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=ws, capture_output=True, text=True
        ).stdout.strip()

        stub = work / "stub-bonsai"
        stub.write_text(
            "#!/bin/sh\n"
            'if [ "$1" = "--version" ]; then echo "bonsai selftest"; exit 0; fi\n'
            "cat >/dev/null\n"
            "printf 'after\\n' > tracked.txt\n"
            "printf 'new\\n' > added.txt\n"
            "printf '%s\\n' '{\"status\":\"completed\",\"output\":\"done\","
            '"provider":"stub","model":"stub","session_id":1,'
            '"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2,'
            '"cost_micros":0},"budget_exhaustion":null,'
            '"verification":{"repair_attempts":0},"completion_report":{}}\'\n'
        )
        stub.chmod(stub.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)

        inst = Instance(
            instance_id="selftest__repo-1",
            repo="selftest/repo",
            base_commit=base,
            problem_statement="Selftest: make the tracked change.",
        )
        req = build_request(
            inst, ws.resolve(), stub.resolve(), "selftest", "stub", "stub",
            "high", "auto-accept", "deny",
        )
        batch_path = work / "requests.json"
        batch_path.write_text(json.dumps([req], indent=2))

        out_dir = work / "adapter"
        run_checked(
            [
                str(binary), "eval", "adapter", "run",
                "--request", str(batch_path), "--out", str(out_dir), "--json",
            ]
        )
        preds = out_dir / "predictions.jsonl"
        body = preds.read_text()
        assert "selftest__repo-1" in body, "instance id missing from prediction"
        assert "tracked.txt" in body and "added.txt" in body, "patch incomplete"
        eprint("\nSELFTEST PASSED: adapter accepted generated request and emitted a "
               "complete prediction.")
    finally:
        shutil.rmtree(work, ignore_errors=True)


# --- Helpers ------------------------------------------------------------------

def validate_grading(report: dict, expected_ids: list[str]) -> dict[str, int]:
    """Validate pinned upstream schema v2; completed overlaps errors legitimately."""
    expected = set(expected_ids)
    if not expected or len(expected) != len(expected_ids):
        raise ValueError("Manifest task IDs must be nonempty and unique")
    if report.get("schema_version") != 2:
        raise ValueError("Expected upstream grading schema_version 2")
    fields = ("submitted", "completed", "resolved", "unresolved", "error",
              "incomplete", "empty_patch")
    groups = {}
    for name in fields:
        values = report.get(f"{name}_ids")
        if not isinstance(values, list) or any(not isinstance(v, str) for v in values):
            raise ValueError(f"Missing or invalid {name}_ids")
        group = set(values)
        if len(group) != len(values) or not group <= expected:
            raise ValueError(f"Duplicate or foreign IDs in {name}_ids")
        count = report.get(f"{name}_instances")
        if count is not None and (type(count) is not int or count != len(group)):
            raise ValueError(f"Contradictory {name}_instances")
        groups[name] = group
    if report.get("total_instances") != len(expected):
        raise ValueError("Grader dataset selection differs from manifest")
    submitted = groups["submitted"]
    if groups["incomplete"] != expected - submitted:
        raise ValueError("Contradictory incomplete_ids")
    classified = [groups[n] for n in ("resolved", "unresolved", "error", "empty_patch")]
    for index, group in enumerate(classified):
        if not group <= submitted or any(group & other for other in classified[index + 1:]):
            raise ValueError("Contradictory outcome classifications")
    if not (groups["resolved"] | groups["unresolved"]) <= groups["completed"]:
        raise ValueError("Resolved/unresolved results require completed grading")
    if not groups["completed"] <= (groups["resolved"] | groups["unresolved"] | groups["error"]):
        raise ValueError("Completed IDs lack a grading classification")
    accounted = set().union(*classified, groups["incomplete"])
    return {"total": len(expected), "pass": len(groups["resolved"]),
            "fail": len(groups["unresolved"] | groups["empty_patch"]),
            "error": len(groups["error"]),
            "missing": len(groups["incomplete"] | (expected - accounted))}


def preflight(args: argparse.Namespace) -> None:
    """Fail before downloads/spend; inspect installed source, not declared pins."""
    if not args.manifest or not args.approve_live:
        raise SystemExit("Live stages require --manifest and --approve-live")
    manifest = json.loads(Path(args.manifest).read_text(encoding="utf-8"))
    if manifest["evidence_kind"] != "real":
        raise SystemExit("Example/fixture manifest is not approved for live execution")
    if platform.system() != "Linux" or platform.machine() not in ("x86_64", "AMD64"):
        raise SystemExit("Qualification requires Linux amd64")
    docker = subprocess.run(["docker", "info"], capture_output=True, check=False)
    if docker.returncode:
        raise SystemExit("Docker daemon is unavailable")
    runner = manifest["runner"]
    if not os.environ.get(runner["credential_env"]):
        raise SystemExit("Required provider credential environment variable is absent")
    if runner["max_spend_usd"] <= 0:
        raise SystemExit("An explicit positive spend ceiling is required")
    out = Path(args.out)
    if out.resolve() != Path(runner["output_directory"]).resolve():
        raise SystemExit("Output directory differs from manifest")
    ancestor = out.resolve()
    while not ancestor.exists():
        ancestor = ancestor.parent
    if shutil.disk_usage(ancestor).free < runner["required_disk_gib"] * 1024 ** 3:
        raise SystemExit("Insufficient disk space for declared requirement")
    for package, pin in (("swebench", SWE_BENCH_HARNESS_COMMIT),
                         ("harbor", "2d3f78d55a703df2f76c005d7df44a5ce2d8adf5")):
        direct = importlib.metadata.distribution(package).read_text("direct_url.json")
        if not direct or json.loads(direct).get("vcs_info", {}).get("commit_id") != pin:
            raise SystemExit(f"Installed {package} source revision differs from pin")
    profile = manifest["profile"]
    args.provider = profile["provider"]
    args.model = profile["model"]
    args.effort = profile["reasoning_effort"]
    args.autonomy = profile["autonomy"]
    args.network = profile["network"]
    args.revision = manifest["candidate"]["revision"]
    suite = next(s for s in manifest["suites"] if s["kind"] == "swe_bench_verified")
    args.dataset_revision = suite["dataset_version"]
    if len(args.dataset_revision) != 40:
        raise SystemExit("Dataset must use an immutable 40-character revision")
    for key, value in profile["budgets"].items():
        if DEFAULT_BUDGETS.get(key) != value:
            raise SystemExit("This runner currently requires the explicit default canary budgets")
    digest = subprocess.run(["b3sum", str(Path(args.binary).resolve())],
                            capture_output=True, text=True, check=True).stdout.split()[0]
    if digest != manifest["candidate"]["binary_blake3"]:
        raise SystemExit("Candidate binary digest differs from manifest")
    if args.stage in ("run", "all") and (out / "requests.json").exists():
        requests = json.loads((out / "requests.json").read_text(encoding="utf-8"))
        for request in requests:
            if Path(request["runner"]["bonsai_binary"]).resolve() != Path(args.binary).resolve():
                raise SystemExit("Stored request references a different binary")
            if request["runner"]["bonsai_revision"] != args.revision:
                raise SystemExit("Stored request references a different candidate")
    identity = {"binary_blake3": digest, "revision": args.revision,
                "dataset_revision": args.dataset_revision,
                "harness_commit": SWE_BENCH_HARNESS_COMMIT}
    evidence_path = out / "verified-runner.json"
    if evidence_path.exists():
        if json.loads(evidence_path.read_text(encoding="utf-8")) != identity:
            raise SystemExit("Existing runner evidence conflicts; use a fresh output directory")
    elif args.stage not in ("provision", "all"):
        raise SystemExit("Missing original runner evidence; provision a fresh run")
    else:
        out.mkdir(parents=True, exist_ok=True)
        with evidence_path.open("x", encoding="utf-8") as evidence:
            json.dump(identity, evidence)
    eprint("Preflight passed; credentials were not printed. Spend ceiling is operator-enforced.")


def read_instance_ids(args: argparse.Namespace) -> list[str]:
    if args.manifest:
        manifest = json.loads(Path(args.manifest).read_text(encoding="utf-8"))
        suites = [s for s in manifest["suites"] if s["kind"] == "swe_bench_verified"]
        if len(suites) != 1 or not suites[0]["task_ids"]:
            raise SystemExit("Manifest requires one nonempty SWE task selection")
        ids = suites[0]["task_ids"]
        if len(ids) != len(set(ids)):
            raise SystemExit("Duplicate manifest tasks")
        if args.instance_id and args.instance_id != ids:
            raise SystemExit("CLI selection differs from manifest")
        return ids
    if args.instance_id:
        return list(args.instance_id)
    path = Path(args.instances)
    ids = [
        line.strip()
        for line in path.read_text().splitlines()
        if line.strip() and not line.strip().startswith("#")
    ]
    if not ids:
        raise SystemExit(f"No instance ids in {path}.")
    return ids


def find_report(out: Path, run_id: str) -> Path:
    matches = sorted(out.glob(f"*.{run_id}.json"))
    if not matches:
        # swebench writes the report to CWD (out) as <model>.<run_id>.json.
        matches = sorted(out.glob(f"*{run_id}*.json"))
    if not matches:
        raise SystemExit(
            f"No grader report matching *.{run_id}.json under {out}; grading may "
            "have failed."
        )
    if len(matches) != 1:
        raise SystemExit("Ambiguous grader reports; select --report explicitly")
    return matches[0]


def add_common(p: argparse.ArgumentParser, *, needs_model: bool = True) -> None:
    default_binary = str(
        (Path(__file__).resolve().parents[3] / "target" / "release" / "bonsai")
    )
    p.add_argument("--out", default="target/eval/phase-e",
                   help="Working/output directory.")
    p.add_argument("--binary", default=default_binary,
                   help="Path to the prebuilt bonsai binary.")
    p.add_argument("--instances",
                   default=str(Path(__file__).with_name("canary_instances.txt")),
                   help="File of instance ids, one per line.")
    p.add_argument("--instance-id", action="append",
                   help="Override instance ids (repeatable).")
    p.add_argument("--run-id", default="bonsai-canary")
    p.add_argument("--force", action="store_true",
                   help="Re-provision workspaces even if present.")
    p.add_argument("--max-workers", type=int, default=1)
    p.add_argument("--report", help="Explicit grader report path for import.")
    p.add_argument("--manifest", help="Frozen qualification manifest (required for import).")
    p.add_argument("--harbor-trials", help="Private JSON array of pinned Harbor trial envelopes for common import.")
    p.add_argument("--dataset-revision", help="Immutable Hugging Face dataset revision.")
    p.add_argument("--approve-live", action="store_true", help="Explicit approval for downloads and provider spend.")
    if needs_model:
        # Config default is codex (OAuth), which the adapter's isolated
        # BONSAI_HOME strips — so the canary default is an env-key provider.
        p.add_argument("--provider", default="anthropic")
        p.add_argument("--model", default="anthropic/claude-sonnet-4-5")
        # "default" = provider default reasoning; required for models with no
        # reasoning options (e.g. mimo), which reject an explicit "none"/"off".
        p.add_argument("--effort", default="high",
                       choices=["default", "none", "minimal", "low", "medium", "high"])
        p.add_argument("--autonomy", default="auto-accept")
        p.add_argument("--network", default="deny", choices=["deny", "allow"])
        p.add_argument("--revision", default=os.environ.get("BONSAI_REVISION", "local"))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="stage", required=True)
    for name, fn in [
        ("selftest", stage_selftest),
        ("provision", stage_provision),
        ("run", stage_run),
        ("grade", stage_grade),
        ("import-report", stage_import),
        ("all", None),
    ]:
        sp = sub.add_parser(name)
        add_common(sp)
        sp.set_defaults(fn=fn)

    args = parser.parse_args()
    if args.stage not in ("selftest", "import-report"):
        preflight(args)
    if args.stage == "all":
        stage_provision(args)
        stage_run(args)
        stage_grade(args)
        stage_import(args)
    else:
        args.fn(args)


if __name__ == "__main__":
    main()
