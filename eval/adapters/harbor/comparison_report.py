"""Deterministic paired CPU-pilot summaries; no harness/provider execution."""

from collections import Counter
import math
import random
import statistics

from .comparison_manifest import FrozenManifest
from .comparison_schedule import remaining, schedule


def summarize(manifest: FrozenManifest, records: list[dict]) -> dict:
    """Missing trials and infrastructure-invalid attempts never become successes."""
    missing = [t.identity for t in remaining(manifest, "pilot", records)]
    expected = {t.identity: t for t in schedule(manifest, "pilot")}
    by_id = {}
    public_fields = {"identity", "task", "agent", "manifest_digest", "model", "artifact_digest", "profile_digest",
                     "reward", "category", "wall_seconds", "provider_billed_usd", "estimated_usd",
                     "infrastructure_usd", "usage", "redacted_log"}
    for record in records:
        if set(record) != public_fields:
            raise ValueError("only normalized public attempt fields are accepted; raw traces stay private")
        if record["category"] not in {"verified", "agent_failure", "timeout", "cancelled", "budget_exhausted", "infrastructure_invalid"}:
            raise ValueError("unknown failure category")
        log = record["redacted_log"]
        if log is not None:
            from pathlib import PurePosixPath
            path = PurePosixPath(log)
            if path.is_absolute() or ".." in path.parts or not log.startswith("redacted/"):
                raise ValueError("log references must be relative redacted/ paths")
        usage = record["usage"]
        if usage is not None and (not isinstance(usage, dict) or set(usage) - {"inputTokens", "outputTokens", "cacheReadTokens", "cacheWriteTokens", "reasoningTokens", "totalTokens"}
                                  or any(type(v) is not int or v < 0 for v in usage.values())):
            raise ValueError("invalid normalized usage")
        if record["category"] == "infrastructure_invalid" and record["reward"] is not None:
            raise ValueError("infrastructure-invalid attempt cannot supply comparative reward")
        if record["category"] != "verified" and record["reward"] == 1:
            raise ValueError("failed attempt cannot supply a successful comparative reward")
        trial = expected[record["identity"]]
        if record["agent"] != trial.agent or record["task"] != trial.task:
            raise ValueError("trial identity fields mismatch")
        if record["model"] != manifest.body["model"]:
            raise ValueError("model/profile mismatch")
        if record["artifact_digest"] != manifest.body["artifacts"][trial.agent]["artifact_digest"]:
            raise ValueError("artifact mismatch")
        if record["profile_digest"] != manifest.body["profiles"][trial.agent]["overlay_digest"]:
            raise ValueError("profile mismatch")
        if record["reward"] not in (None, 0, 1) or type(record["reward"]) is bool:
            raise ValueError("binary verifier reward required")
        for key in ("wall_seconds", "provider_billed_usd", "estimated_usd", "infrastructure_usd"):
            value = record.get(key)
            if value is not None and (type(value) not in (int, float) or not math.isfinite(value) or value < 0):
                raise ValueError(f"invalid {key}")
        by_id[trial.identity] = record
    agents = {}
    for agent in ("bonsai", "dsh"):
        attempts = [r for r in records if r["agent"] == agent]
        valid = [r for r in attempts if r["category"] != "infrastructure_invalid"]
        successes = sum(r["reward"] == 1 for r in valid)
        times = [r["wall_seconds"] for r in attempts if r.get("wall_seconds") is not None]
        costs = {}
        for key in ("provider_billed_usd", "estimated_usd", "infrastructure_usd"):
            values = [r.get(key) for r in attempts]
            total = sum(values) if values and all(v is not None for v in values) else None
            costs[key] = {"total": total, "per_attempt": total / len(attempts) if total is not None else None,
                          "per_success": total / successes if total is not None and successes else None,
                          "missing_attempts": sum(v is None for v in values)}
        agents[agent] = {"attempts": len(attempts), "valid_attempts": len(valid), "successes": successes,
                         "mean_single_attempt_success": successes / len(valid) if valid else None,
                         "categories": dict(sorted(Counter(r["category"] for r in attempts).items())),
                         "wall_seconds": {"median": statistics.median(times) if times else None,
                                          "maximum": max(times) if times else None, "observations": times},
                         "costs": costs,
                         "usage": [r.get("usage") for r in attempts]}
    differences, task_outcomes = [], {}
    for task in sorted({t.task for t in expected.values()}):
        pairs = [[by_id.get(f"pilot/{task}/{rep}/{agent}") for agent in ("bonsai", "dsh")]
                 for rep in range(1, 4)]
        if any(r is None or r["category"] == "infrastructure_invalid" or r["reward"] is None for pair in pairs for r in pair):
            task_outcomes[task] = "incomplete"
            continue
        difference = sum(pair[0]["reward"] - pair[1]["reward"] for pair in pairs) / 3
        differences.append(difference)
        task_outcomes[task] = "win" if difference > 0 else "loss" if difference < 0 else "tie"
    interval = None
    if differences:
        rng = random.Random(manifest.sha256)
        draws = sorted(statistics.mean(rng.choices(differences, k=len(differences))) for _ in range(10_000))
        interval = [draws[249], draws[9749]]
    return {"schema_version": 1, "label": "TB4 CPU-only small pilot — not a full TB4 score",
            "manifest_digest": manifest.sha256, "provenance": manifest.body,
            "agents": agents, "missing_trials": missing, "trial_inventory": [t.to_dict() for t in expected.values()],
            "attempts": records, "task_outcomes": task_outcomes,
            "paired_task_mean_difference": statistics.mean(differences) if differences else None,
            "task_clustered_bootstrap_95_interval": interval,
            "complete_task_pairs": len(differences),
            "limitations": ["Small sample; interval is descriptive, not evidence of precise speed advantages.",
                            "CPU-only selection; smoke trials excluded; missing telemetry is null.",
                            "Incomplete pairing and infrastructure-invalid attempts are disclosed, not imputed."]}


def markdown(report: dict) -> str:
    lines = [f"# {report['label']}", "", f"Manifest: `{report['manifest_digest']}`", "",
             "| Agent | Attempts | Valid | Successes | Mean single-attempt success |",
             "|---|---:|---:|---:|---:|"]
    for name, agent in report["agents"].items():
        mean = agent["mean_single_attempt_success"]
        lines.append(f"| {name} | {agent['attempts']} | {agent['valid_attempts']} | {agent['successes']} | {mean if mean is not None else 'unavailable'} |")
    lines.extend(["", f"Complete task pairs: {report['complete_task_pairs']}; missing trials: {len(report['missing_trials'])}.",
                  f"Paired difference (Bonsai − DSH): {report['paired_task_mean_difference']}; task-clustered bootstrap 95% interval: {report['task_clustered_bootstrap_95_interval']}.", ""])
    lines.extend(f"- {line}" for line in report["limitations"])
    lines.extend(["", "See comparison.json for every attempt, verifier reward, failure category, usage/cost source, resource/profile differences, exclusions, provenance and redacted log references.", ""])
    return "\n".join(lines)
