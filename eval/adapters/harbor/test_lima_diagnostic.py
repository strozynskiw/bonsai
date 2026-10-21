"""Keyless diagnostic schedule, approval and occupied-slot boundaries."""

import copy
import json
import unittest

from .comparison_manifest import canonical_bytes, digest
from .lima_diagnostic import POLICY, schedule, remaining, check_approval, require_request
from .lima_manifest import LimaManifest
from .lima_paid import check_approval as paid_approval, require_success, schedule as paid_schedule
from .diagnostic_evidence import validate_receipts


class DiagnosticTests(unittest.TestCase):
    def setUp(self):
        self.body = {"diagnostic_policy": copy.deepcopy(POLICY),
                     "model": {"id": "deepseek-v4-flash", "reasoning": "off", "fallback": False}, "preparation": {"tasks": [
            {"id": "rs-archive-clone", "candidate": True}, {"id": "other", "candidate": True}]}}
        self.manifest = LimaManifest(canonical_bytes(self.body), digest(self.body))
        self.expected = schedule(self.body)

    def record(self, trial, category="verified"):
        return {"identity": trial.identity, "manifest_digest": self.manifest.sha256,
                "category": category, "reward": 0, "raw_sha256": "sha256:" + "a" * 64}

    def test_fixed_fresh_serial_pair(self):
        self.assertEqual([trial.agent for trial in self.expected], ["dsh", "bonsai"])
        self.assertEqual({trial.task for trial in self.expected}, {"rs-archive-clone"})
        self.assertEqual(len({trial.identity for trial in self.expected}), 2)
        self.assertTrue(all("diagnostic" in trial.identity for trial in self.expected))
        with self.assertRaises(ValueError):
            schedule({**self.body, "diagnostic_policy": {}})

    def test_ordinary_failure_continues_but_occupied_identity_is_not_retried(self):
        for category in ("verified", "agent_failure", "verifier_failure"):
            records = [self.record(self.expected[0], category)]
            self.assertEqual(remaining(records, self.expected, self.manifest.sha256), self.expected[1:])
        records = [self.record(trial) for trial in self.expected]
        self.assertEqual(remaining(records, self.expected, self.manifest.sha256), [])
        for broken in (records + records[:1], [records[1]],
                       [{**records[0], "manifest_digest": "other"}],
                       [{**records[0], "raw_sha256": None}]):
            with self.assertRaises(ValueError):
                remaining(broken, self.expected, self.manifest.sha256)

    def test_infrastructure_and_interruption_stop_dispatch(self):
        for category in ("infrastructure_invalid", "cancelled", "timeout", "budget_exhausted"):
            with self.assertRaises(ValueError):
                remaining([self.record(self.expected[0], category)], self.expected, self.manifest.sha256)

    def test_approvals_are_phase_manifest_policy_and_prerequisite_bound(self):
        approval = {"schema": "tb4-lima-approval-v1", "manifest_digest": self.manifest.sha256,
                    "policy_digest": digest(POLICY), "phase": "diagnostic", "prerequisite_digest": "proof",
                    "execute_approved": True, "uncapped_approved": True}
        check_approval(approval, self.manifest, "diagnostic", "proof")
        for key in approval:
            with self.assertRaises(ValueError):
                check_approval({**approval, key: None}, self.manifest, "diagnostic", "proof")
        with self.assertRaises(ValueError):
            paid_approval(approval, self.manifest, "smoke", "proof")
        with self.assertRaises(ValueError):
            check_approval(approval, self.manifest, "diagnostic-request", "proof")

    def test_fresh_request_binding_and_no_smoke_scored_credit(self):
        evidence = {"manifest_digest": self.manifest.sha256, "qualified": True,
                    "qualification_phase": "diagnostic-request", "policy_digest": digest(POLICY),
                    "model": "deepseek-v4-flash"}
        require_request(evidence, self.manifest, digest(evidence))
        for key in evidence:
            broken = {**evidence, key: None}
            with self.assertRaises(ValueError):
                require_request(broken, self.manifest, digest(broken))
        records = [{**self.record(trial), "reward": 1} for trial in self.expected]
        with self.assertRaises(ValueError):
            require_success(records, paid_schedule(self.body, "smoke"), self.manifest.sha256)

    def test_receipt_completeness_and_bounds(self):
        rows = [{"type": "identity", "schema": 1, "boundary": "agent_to_provider", "model": "fixture",
                 "request_ordinal": 1, "request_sha256": "a" * 64},
                {"type": "summary", "requests": 1, "stages": [False] * 3, "capture_valid": True}]
        raw = "\n".join(map(json.dumps, rows))
        self.assertEqual(validate_receipts(raw, expected_model="fixture")["requests"], 1)
        for broken in ("", json.dumps(rows[0]), raw * 100, raw.replace('"capture_valid": true', '"capture_valid": false')):
            with self.assertRaises(ValueError):
                validate_receipts(broken, expected_model="fixture")
        with self.assertRaises(ValueError):
            validate_receipts(raw, expected_model="different")


class DiagnosticDispatchTests(unittest.IsolatedAsyncioTestCase):
    async def test_failure_continuation_stop_and_resume(self):
        from pathlib import Path
        from types import SimpleNamespace
        import tempfile
        from unittest.mock import AsyncMock, patch
        from .comparison_journal import FileJournal
        from .lima_diagnostic import execute
        for category in ("agent_failure", "verified", "infrastructure_invalid"):
            with self.subTest(category=category), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                body = {"schema": "tb4-lima-phase-v1", "launch_authorized": False,
                        "diagnostic_policy": POLICY, "model": {"id": "deepseek-v4-flash", "endpoint": "https://provider.invalid", "reasoning": "off", "fallback": False},
                        "preparation": {"tasks": [{"id": "rs-archive-clone", "candidate": True, "content_digest": "fixture"}]}}
                manifest = LimaManifest(canonical_bytes(body), digest(body))
                (root / "manifest.json").write_text(json.dumps({"manifest": body, "digest": manifest.sha256}))
                evidence = {"manifest_digest": manifest.sha256, "qualified": True,
                            "qualification_phase": "diagnostic-request", "policy_digest": digest(POLICY),
                    "model": "deepseek-v4-flash"}
                (root / "request.json").write_text(json.dumps(evidence))
                approval = {"schema": "tb4-lima-approval-v1", "manifest_digest": manifest.sha256,
                            "policy_digest": digest(POLICY), "phase": "diagnostic", "prerequisite_digest": digest(evidence),
                            "execute_approved": True, "uncapped_approved": True}
                (root / "approval.json").write_text(json.dumps(approval))
                args = SimpleNamespace(manifest=root / "manifest.json", artifacts=root, tasks=root, dependencies=root,
                                       results=root, approval=root / "approval.json", prerequisite=digest(evidence))
                calls = []
                async def retained(spec, trial, binding, journal, executor, **kwargs):
                    calls.append(trial.agent)
                    journal.begin(trial, binding)
                    result = {"category": category if trial.agent == "dsh" else "verified", "raw_sha256": "sha256:" + "a" * 64}
                    journal.finish(trial, result)
                    return result
                with patch("eval.adapters.harbor.lima_diagnostic.validate_runtime", new=AsyncMock()), patch(
                        "eval.adapters.harbor.lima_diagnostic.os.environ", {"TB4_PROVIDER_KEY": "inert"}), patch(
                        "eval.adapters.harbor.lima_diagnostic.shutil.disk_usage", return_value=SimpleNamespace(free=100 * 1024**3)), patch(
                        "eval.adapters.harbor.lima_diagnostic.run_retained", side_effect=retained):
                    if category == "infrastructure_invalid":
                        with self.assertRaises(ValueError):
                            await execute(args)
                        with self.assertRaises(ValueError):
                            await execute(args)
                        self.assertEqual(calls, ["dsh"])
                    else:
                        await execute(args)
                        await execute(args)
                        self.assertEqual(calls, ["dsh", "bonsai"])


if __name__ == "__main__":
    unittest.main()
