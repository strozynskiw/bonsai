#!/usr/bin/env python3
"""Adversarial tests for check-rc-soak.py."""

from __future__ import annotations

import base64
import copy
from datetime import datetime, timedelta, timezone
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from typing import Any, Callable


SCRIPT = Path(__file__).with_name("check-rc-soak.py")
SPEC = importlib.util.spec_from_file_location("check_rc_soak", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
CHECKER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECKER)

UTC = timezone.utc
START = datetime(2026, 1, 1, 12, 0, tzinfo=UTC)
TAG = "v1.2.3-rc.1"
TARGETS = CHECKER.SUPPORTED_TARGETS
ReportMutator = Callable[[str, dict[str, Any]], None]
ManifestMutator = Callable[[dict[str, Any]], None]
RecordMutator = Callable[[dict[str, Any]], None]


def timestamp(value: datetime) -> str:
    return value.astimezone(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def u64_distribution(values: list[int]) -> dict[str, int]:
    ordered = sorted(values)
    middle = len(ordered) // 2
    median = (
        ordered[middle]
        if len(ordered) % 2
        else min(ordered[middle - 1] + ordered[middle], (1 << 64) - 1) // 2
    )
    p95 = ordered[(len(ordered) * 95 + 99) // 100 - 1]
    return {"median": median, "p95": p95, "min": ordered[0], "max": ordered[-1]}


def float_distribution(values: list[float]) -> dict[str, float]:
    ordered = sorted(values)
    middle = len(ordered) // 2
    median = (
        ordered[middle]
        if len(ordered) % 2
        else (ordered[middle - 1] + ordered[middle]) / 2.0
    )
    p95 = ordered[(len(ordered) * 95 + 99) // 100 - 1]
    return {"median": median, "p95": p95, "min": ordered[0], "max": ordered[-1]}


class EvidenceFixture:
    def __init__(
        self,
        *,
        observation_days: int = 14,
        surfaces: tuple[str, ...] = ("headless", "tui"),
        annotated_tag: bool = False,
        start_claim: datetime = START,
        first_seen: datetime = START,
        published_at: datetime = START,
        now: datetime | None = None,
        report_mutator: ReportMutator | None = None,
        manifest_mutator: ManifestMutator | None = None,
        record_mutator: RecordMutator | None = None,
        wrong_binary_target: str | None = None,
        archive_extra_file_target: str | None = None,
        archive_non_executable_target: str | None = None,
    ) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.published_at = published_at
        self.now = now or first_seen + timedelta(days=15)
        self._git("init", "-q")
        self._git("config", "user.name", "RC Test")
        self._git("config", "user.email", "rc-test@example.invalid")
        (self.repo / "seed.txt").write_text("release source\n", encoding="utf-8")
        self._git("add", "seed.txt")
        self._commit("seed release", first_seen - timedelta(hours=1))
        self.peeled_commit = self._git("rev-parse", "HEAD").stdout.strip()
        tag_args = ["tag"]
        if annotated_tag:
            tag_args.extend(("-a", "-m", "release candidate"))
        tag_args.append(TAG)
        self._git(*tag_args, when=first_seen - timedelta(minutes=30))

        self.private_key = self.root / "private.pem"
        self.public_key = self.root / "public.pem"
        subprocess.run(
            ["openssl", "genpkey", "-algorithm", "Ed25519", "-out", self.private_key],
            check=True,
            capture_output=True,
        )
        subprocess.run(
            ["openssl", "pkey", "-in", self.private_key, "-pubout", "-out", self.public_key],
            check=True,
            capture_output=True,
        )

        self.archives: dict[str, Path] = {}
        self.binary_hashes: dict[str, str] = {}
        self.binary_sizes: dict[str, int] = {}
        archive_hashes: dict[str, str] = {}
        for target in TARGETS:
            binary = f"bonsai release binary for {target}\n".encode()
            self.binary_sizes[target] = len(binary)
            self.binary_hashes[target] = hashlib.sha256(binary).hexdigest()
            archive_path = self.root / f"bonsai-{TAG}-{target}.tar.gz"
            with tarfile.open(archive_path, "w:gz") as archive:
                info = tarfile.TarInfo("bonsai")
                info.size = len(binary)
                info.mode = 0o644 if target == archive_non_executable_target else 0o755
                archive.addfile(info, io.BytesIO(binary))
                if target == archive_extra_file_target:
                    extra = b"unexpected"
                    extra_info = tarfile.TarInfo("notes.txt")
                    extra_info.size = len(extra)
                    archive.addfile(extra_info, io.BytesIO(extra))
            self.archives[target] = archive_path
            archive_hashes[target] = hashlib.sha256(archive_path.read_bytes()).hexdigest()

        declared_binary_hashes = dict(self.binary_hashes)
        if wrong_binary_target is not None:
            declared_binary_hashes[wrong_binary_target] = "f" * 64
        manifest = {
            "assets": [
                {
                    "archive": f"bonsai-{TAG}-{target}.tar.gz",
                    "archive_sha256": archive_hashes[target],
                    "binary_sha256": declared_binary_hashes[target],
                    "target": target,
                }
                for target in TARGETS
            ],
            "repository": "strozynskiw/bonsai",
            "schema_version": 1,
            "tag": TAG,
            "version": TAG[1:],
        }
        if manifest_mutator is not None:
            manifest_mutator(manifest)
        self.manifest = self.root / "release-manifest.json"
        self.manifest.write_text(
            json.dumps(manifest, sort_keys=True, separators=(",", ":")) + "\n",
            encoding="utf-8",
        )
        self.signature = self.root / "release-manifest.json.sig"
        self._sign_manifest()

        self.reports: dict[str, Path] = {}
        report_hashes: dict[str, str] = {}
        for target in TARGETS:
            report = self._report(
                target,
                declared_binary_hashes[target],
                self.binary_sizes[target],
            )
            if report_mutator is not None:
                report_mutator(target, report)
            report_path = self.root / f"bonsai-{TAG}-{target}.performance.json"
            report_path.write_text(
                json.dumps(report, sort_keys=True, separators=(",", ":")) + "\n",
                encoding="utf-8",
            )
            self.reports[target] = report_path
            report_hashes[target] = hashlib.sha256(report_path.read_bytes()).hexdigest()

        self.record: dict[str, Any] = {
            "$schema": CHECKER.RECORD_SCHEMA,
            "schema_version": 1,
            "release": {
                "tag": TAG,
                "peeled_commit": self.peeled_commit,
                "manifest_sha256": hashlib.sha256(self.manifest.read_bytes()).hexdigest(),
                "binary_sha256_by_target": declared_binary_hashes,
                "performance_report_sha256_by_target": report_hashes,
            },
            "soak": {
                "started_at": timestamp(start_claim),
                "minimum_hours": 336,
            },
            "observations": [],
            "incidents": [],
        }
        if record_mutator is not None:
            record_mutator(self.record)
        self.record_path = (
            self.repo / "release" / "soak" / "active" / f"rc-{TAG}.json"
        )
        self.record_path.parent.mkdir(parents=True)

        if observation_days:
            self.record["observations"].append(
                self._observation(first_seen, 0, surfaces)
            )
        self._write_record()
        self._git("add", self.record_path.relative_to(self.repo).as_posix())
        self._commit("start RC soak", first_seen)
        for day in range(1, observation_days):
            observed_at = first_seen + timedelta(days=day)
            self.record["observations"].append(
                self._observation(observed_at, day, surfaces)
            )
            self.commit_record(f"record observation day {day + 1}", observed_at)

    def close(self) -> None:
        self.temporary.cleanup()

    def _git(
        self, *args: str, when: datetime | None = None
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        if when is not None:
            env["GIT_AUTHOR_DATE"] = timestamp(when)
            env["GIT_COMMITTER_DATE"] = timestamp(when)
        return subprocess.run(
            ["git", "-C", self.repo, *args],
            check=True,
            capture_output=True,
            text=True,
            env=env,
        )

    def _commit(self, message: str, when: datetime) -> None:
        self._git("commit", "-q", "-m", message, when=when)

    def _sign_manifest(self) -> None:
        raw_signature = self.root / "manifest.sig.raw"
        subprocess.run(
            [
                "openssl",
                "pkeyutl",
                "-sign",
                "-rawin",
                "-inkey",
                self.private_key,
                "-in",
                self.manifest,
                "-out",
                raw_signature,
            ],
            check=True,
            capture_output=True,
        )
        self.signature.write_bytes(base64.b64encode(raw_signature.read_bytes()) + b"\n")

    def _report(self, target: str, binary_hash: str, binary_size: int) -> dict[str, Any]:
        fresh = [10, 12, 11]
        returning = [6, 5, 7]
        idle_cpu = [125.5, 130.0, 127.25]
        idle_rss = [1000, 1100, 1050]
        headless = []
        for sample in range(3):
            headless.append(
                {
                    "spawn_to_first_assistant_delta_ms": 20 + sample,
                    "persistence_duration_ms": 2 + sample,
                    "context_used_tokens": [80, 90, 100, 110],
                    "provider_prompt_tokens": [100 + sample, 110 + sample, 125 + sample, 140 + sample],
                    "prompt_tokens": 200 + sample,
                    "completion_tokens": 50 + sample,
                    "input_cache_hit_rate_percent": 80 + sample,
                    "representative_task_cost_micros": 1000 + sample,
                }
            )
        summary: dict[str, Any] = {
            "fresh_startup_ms": u64_distribution(fresh),
            "returning_startup_ms": u64_distribution(returning),
            "idle_cpu_percent": float_distribution(idle_cpu),
            "idle_rss_bytes": u64_distribution(idle_rss),
        }
        scalar_fields = (
            "spawn_to_first_assistant_delta_ms",
            "persistence_duration_ms",
            "prompt_tokens",
            "completion_tokens",
            "input_cache_hit_rate_percent",
            "representative_task_cost_micros",
        )
        for field in scalar_fields:
            summary[field] = u64_distribution([sample[field] for sample in headless])
        peaks = [max(sample["provider_prompt_tokens"]) for sample in headless]
        growth = [
            sample["provider_prompt_tokens"][-1] - sample["provider_prompt_tokens"][0]
            for sample in headless
        ]
        summary["context_peak_tokens"] = u64_distribution(peaks)
        summary["context_growth_tokens"] = u64_distribution(growth)
        return {
            "schema_version": 1,
            "identity": {
                "target": target,
                "profile": "release",
                "toolchain": "rustc 1.90.0 (test)\nbinary: rustc\ncommit-hash: abcdef",
                "runner_class": CHECKER.RUNNER_CLASS_BY_TARGET[target],
                "git_commit": self.peeled_commit,
                "binary_sha256": binary_hash,
                "binary_bytes": binary_size,
            },
            "sample_count": 3,
            "raw": {
                "fresh_startup_ms": fresh,
                "returning_startup_ms": returning,
                "idle_cpu_percent": idle_cpu,
                "idle_rss_bytes": idle_rss,
                "headless": headless,
            },
            "summary": summary,
            "baseline": {
                "source": f"eval/baselines/performance/{target}.json",
                "baseline_git_commit": self.peeled_commit,
                "passed": True,
                "violations": [],
            },
        }

    def _observation(
        self, observed_at: datetime, day: int, surfaces: tuple[str, ...]
    ) -> dict[str, Any]:
        target = TARGETS[day % len(TARGETS)]
        return {
            "timestamp": timestamp(observed_at),
            "target": target,
            "binary_sha256": self.record["release"]["binary_sha256_by_target"][target],
            "surface": surfaces[day % len(surfaces)],
            "result": "passed",
        }

    def _write_record(self) -> None:
        self.record_path.write_text(
            json.dumps(self.record, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )

    def commit_record(self, message: str, when: datetime) -> None:
        self._write_record()
        self._git("add", self.record_path.relative_to(self.repo).as_posix())
        self._commit(message, when)

    def check(self) -> dict[str, Any]:
        return CHECKER.check_rc_soak(
            self.record_path,
            self.manifest,
            self.signature,
            self.public_key,
            self.reports,
            self.archives,
            timestamp(self.published_at),
            now=self.now,
        )


class RcSoakValidatorTests(unittest.TestCase):
    def fixture(self, **kwargs: Any) -> EvidenceFixture:
        fixture = EvidenceFixture(**kwargs)
        self.addCleanup(fixture.close)
        return fixture

    def assert_failed(self, result: dict[str, Any], code: str) -> None:
        self.assertEqual(result["status"], "failed", result)
        self.assertEqual(result.get("code"), code, result)

    def test_qualifies_lightweight_and_annotated_tags(self) -> None:
        for annotated in (False, True):
            with self.subTest(annotated=annotated):
                fixture = self.fixture(annotated_tag=annotated)
                result = fixture.check()
                self.assertEqual(result["status"], "qualified", result)
                self.assertEqual(result["observation_days"], 14)
                self.assertEqual(result["surfaces"], ["headless", "tui"])

    def test_pending_duration_days_and_surface_coverage(self) -> None:
        duration = self.fixture(observation_days=5, now=START + timedelta(days=5)).check()
        self.assertEqual(duration["status"], "pending")
        self.assertIn("minimum_duration", duration["pending_reasons"])
        days = self.fixture(observation_days=5).check()
        self.assertEqual(days["status"], "pending")
        self.assertIn("observation_days", days["pending_reasons"])
        surface = self.fixture(surfaces=("headless",)).check()
        self.assertEqual(surface["status"], "pending")
        self.assertEqual(surface["pending_reasons"], ["surface_coverage"])

    def test_effective_start_anchors_backdated_claim_to_first_seen_commit(self) -> None:
        first_seen = START + timedelta(days=30)
        fixture = self.fixture(
            observation_days=0,
            start_claim=START,
            first_seen=first_seen,
            now=first_seen + timedelta(days=1),
        )
        result = fixture.check()
        self.assertEqual(result["status"], "pending", result)
        self.assertEqual(result["effective_started_at"], timestamp(first_seen))
        self.assertEqual(result["elapsed_hours"], 24)

    def test_batch_of_backdated_observations_is_rejected(self) -> None:
        fixture = self.fixture(observation_days=0)
        for day in range(14):
            fixture.record["observations"].append(
                fixture._observation(START + timedelta(days=day), day, ("headless", "tui"))
            )
        fixture.commit_record("backfill observations", START + timedelta(days=14))
        self.assert_failed(fixture.check(), "observation_backdated")

    def test_observation_history_rewrite_is_rejected(self) -> None:
        fixture = self.fixture()
        fixture.record["observations"][0]["result"] = "failed"
        fixture.commit_record("rewrite observation", START + timedelta(days=14))
        self.assert_failed(fixture.check(), "record_history_rewrite")

    def test_uncommitted_record_is_rejected(self) -> None:
        fixture = self.fixture()
        fixture.record["observations"][0]["result"] = "failed"
        fixture._write_record()
        self.assert_failed(fixture.check(), "uncommitted_record")

    def test_historical_blocking_incident_survives_deletion(self) -> None:
        fixture = self.fixture()
        incident = {
            "id": "task-loss-1",
            "timestamp": timestamp(START + timedelta(days=14)),
            "type": "task-completion",
            "release_blocking": True,
            "resolved_at": None,
        }
        fixture.record["incidents"].append(incident)
        fixture.commit_record("record blocking incident", START + timedelta(days=14))
        fixture.record["incidents"].clear()
        fixture.commit_record("delete incident", START + timedelta(days=14, hours=1))
        self.assert_failed(fixture.check(), "release_blocking_incident")

    def test_resolved_blocking_incident_still_fails(self) -> None:
        def mutate(record: dict[str, Any]) -> None:
            record["incidents"].append(
                {
                    "id": "migration-1",
                    "timestamp": timestamp(START),
                    "type": "migration",
                    "release_blocking": True,
                    "resolved_at": timestamp(START + timedelta(hours=1)),
                }
            )

        self.assert_failed(self.fixture(record_mutator=mutate).check(), "release_blocking_incident")

    def test_manifest_digest_and_signature_are_verified(self) -> None:
        digest_fixture = self.fixture()
        digest_fixture.manifest.write_bytes(digest_fixture.manifest.read_bytes() + b" ")
        self.assert_failed(digest_fixture.check(), "manifest_digest_mismatch")
        signature_fixture = self.fixture()
        signature_fixture.signature.write_bytes(base64.b64encode(b"\0" * 64) + b"\n")
        self.assert_failed(signature_fixture.check(), "invalid_manifest_signature")

    def test_manifest_repository_and_target_set_are_pinned(self) -> None:
        wrong_repo = self.fixture(
            manifest_mutator=lambda manifest: manifest.__setitem__("repository", "other/project")
        )
        self.assert_failed(wrong_repo.check(), "manifest_identity_mismatch")
        missing_target = self.fixture(
            manifest_mutator=lambda manifest: manifest["assets"].pop()
        )
        self.assert_failed(missing_target.check(), "target_set_mismatch")

    def test_archive_hash_binary_layout_and_mode_are_verified(self) -> None:
        raw = self.fixture()
        raw.archives[TARGETS[0]].write_bytes(raw.archives[TARGETS[0]].read_bytes() + b"tamper")
        self.assert_failed(raw.check(), "archive_digest_mismatch")
        wrong_binary = self.fixture(wrong_binary_target=TARGETS[0])
        self.assert_failed(wrong_binary.check(), "binary_digest_mismatch")
        extra = self.fixture(archive_extra_file_target=TARGETS[0])
        self.assert_failed(extra.check(), "invalid_archive_layout")
        non_executable = self.fixture(archive_non_executable_target=TARGETS[0])
        self.assert_failed(non_executable.check(), "invalid_archive_layout")

    def test_report_hash_and_recursive_shape_are_verified(self) -> None:
        raw = self.fixture()
        raw.reports[TARGETS[0]].write_bytes(raw.reports[TARGETS[0]].read_bytes() + b" ")
        self.assert_failed(raw.check(), "report_digest_mismatch")

        def add_extra(target: str, report: dict[str, Any]) -> None:
            if target == TARGETS[0]:
                report["raw"]["headless"][0]["transcript"] = "must not be accepted"

        extra = self.fixture(report_mutator=add_extra)
        self.assert_failed(extra.check(), "invalid_shape")

    def test_report_identity_summary_and_baseline_are_verified(self) -> None:
        def wrong_identity(target: str, report: dict[str, Any]) -> None:
            if target == TARGETS[0]:
                report["identity"]["runner_class"] = "self-hosted"

        self.assert_failed(
            self.fixture(report_mutator=wrong_identity).check(), "report_identity_mismatch"
        )

        def wrong_summary(target: str, report: dict[str, Any]) -> None:
            if target == TARGETS[0]:
                report["summary"]["fresh_startup_ms"]["median"] += 1

        self.assert_failed(
            self.fixture(report_mutator=wrong_summary).check(), "report_summary_mismatch"
        )

        def failed_baseline(target: str, report: dict[str, Any]) -> None:
            if target == TARGETS[0]:
                report["baseline"]["passed"] = False

        self.assert_failed(
            self.fixture(report_mutator=failed_baseline).check(), "performance_regression"
        )

        def absolute_baseline(target: str, report: dict[str, Any]) -> None:
            if target == TARGETS[0]:
                report["baseline"]["source"] = "/tmp/baseline.json"

        self.assert_failed(
            self.fixture(report_mutator=absolute_baseline).check(), "invalid_baseline"
        )

    def test_exact_external_target_maps_are_required(self) -> None:
        fixture = self.fixture()
        reports = dict(fixture.reports)
        reports.pop(TARGETS[-1])
        result = CHECKER.check_rc_soak(
            fixture.record_path,
            fixture.manifest,
            fixture.signature,
            fixture.public_key,
            reports,
            fixture.archives,
            timestamp(fixture.published_at),
            now=fixture.now,
        )
        self.assert_failed(result, "target_set_mismatch")

    def test_minimum_duration_incident_type_and_moved_tag_are_rejected(self) -> None:
        weakened = self.fixture(
            record_mutator=lambda record: record["soak"].__setitem__("minimum_hours", 335)
        )
        self.assert_failed(weakened.check(), "minimum_duration_weakened")

        def invalid_incident(record: dict[str, Any]) -> None:
            record["incidents"].append(
                {
                    "id": "availability-1",
                    "timestamp": timestamp(START),
                    "type": "availability",
                    "release_blocking": False,
                    "resolved_at": None,
                }
            )

        incident = self.fixture(record_mutator=invalid_incident)
        self.assert_failed(incident.check(), "invalid_incident_type")
        moved = self.fixture()
        moved._git("tag", "-f", TAG, "HEAD")
        self.assert_failed(moved.check(), "moved_release_tag")

    def test_duplicate_keys_nonfinite_numbers_and_naive_clock_are_rejected(self) -> None:
        with self.assertRaises(CHECKER.ValidationError) as duplicate:
            CHECKER._load_json_bytes(b'{"a":1,"a":2}', "test")
        self.assertEqual(duplicate.exception.code, "duplicate_json_key")
        with self.assertRaises(CHECKER.ValidationError) as nonfinite:
            CHECKER._load_json_bytes(b'{"a":NaN}', "test")
        self.assertEqual(nonfinite.exception.code, "invalid_json_number")
        fixture = self.fixture()
        result = CHECKER.check_rc_soak(
            fixture.record_path,
            fixture.manifest,
            fixture.signature,
            fixture.public_key,
            fixture.reports,
            fixture.archives,
            timestamp(fixture.published_at),
            now=datetime(2026, 1, 20),
        )
        self.assert_failed(result, "invalid_now")

    def test_cli_usage_is_json_and_exit_64(self) -> None:
        completed = subprocess.run(
            [sys.executable, SCRIPT], capture_output=True, text=True, check=False
        )
        self.assertEqual(completed.returncode, 64)
        self.assertEqual(json.loads(completed.stdout)["code"], "usage")


if __name__ == "__main__":
    unittest.main()
