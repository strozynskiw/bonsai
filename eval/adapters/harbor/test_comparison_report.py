import copy
import unittest

from .comparison_gate import Approval
from .comparison_manifest import FrozenManifest, digest, freeze
from .comparison_report import markdown, summarize
from .comparison_schedule import schedule
from .test_comparison_manifest import manifest_fixture


def records_fixture(manifest, reward=0, phase="pilot"):
    return [{"identity": trial.identity, "task": trial.task, "agent": trial.agent,
             "manifest_digest": manifest.sha256, "model": manifest.body["model"],
             "artifact_digest": manifest.body["artifacts"][trial.agent]["artifact_digest"],
             "profile_digest": manifest.body["profiles"][trial.agent]["overlay_digest"],
             "reward": reward, "category": "verified", "wall_seconds": 2,
             "provider_billed_usd": 0.01, "estimated_usd": None,
             "infrastructure_usd": None, "usage": None, "redacted_log": None}
            for trial in schedule(manifest, phase)]


class ReportTests(unittest.TestCase):
    def setUp(self):
        self.manifest = FrozenManifest.parse(freeze(manifest_fixture()))

    def test_all_failure_cost_and_deterministic_intervals(self):
        records = records_fixture(self.manifest)
        report = summarize(self.manifest, records)
        self.assertEqual(report, summarize(self.manifest, records))
        self.assertEqual(report["task_clustered_bootstrap_95_interval"], [0, 0])
        self.assertEqual(report["complete_task_pairs"], 10)
        costs = report["agents"]["bonsai"]["costs"]
        self.assertIsNone(costs["provider_billed_usd"]["per_success"])
        self.assertIsNone(costs["estimated_usd"]["total"])
        self.assertIn("not a full TB4 score", markdown(report))

    def test_failed_categories_cannot_keep_stale_success(self):
        for category in ("agent_failure", "timeout", "cancelled", "budget_exhausted", "infrastructure_invalid"):
            records = records_fixture(self.manifest, 1)
            records[0]["category"] = category
            with self.subTest(category=category), self.assertRaises(ValueError):
                summarize(self.manifest, records)

    def test_missing_invalid_duplicate_and_mismatch(self):
        records = records_fixture(self.manifest, 1)
        records[0]["category"] = "infrastructure_invalid"
        records[0]["reward"] = None
        report = summarize(self.manifest, records[:-1])
        self.assertEqual(len(report["missing_trials"]), 1)
        self.assertEqual(report["complete_task_pairs"], 8)
        with self.assertRaises(ValueError):
            summarize(self.manifest, records + records[:1])
        records[0]["profile_digest"] = digest("wrong")
        with self.assertRaisesRegex(ValueError, "profile mismatch"):
            summarize(self.manifest, records)


class GateTests(unittest.TestCase):
    def setUp(self):
        self.manifest = FrozenManifest.parse(freeze(manifest_fixture()))
        self.approval = {"execute": True, "manifest_digest": self.manifest.sha256, "phase": "smoke",
                         "runner": "fixture-linux", "platform": "linux", "approver": "fixture-human",
                         "provider_limit_usd": "1", "infrastructure_limit_usd": "2", "per_trial_reservation_usd": "0.1",
                         "external_cap_evidence_digest": digest("external cap"), "isolation_evidence_digest": digest("isolation"),
                         "nested_confinement_approved": True, "network_approved": True, "external_provider_cap_enforced": True}

    def test_requires_explicit_limits_and_cap(self):
        for key, value in (("execute", False), ("manifest_digest", "changed"), ("provider_limit_usd", 0),
                           ("external_provider_cap_enforced", False), ("platform", "darwin")):
            changed = copy.deepcopy(self.approval)
            changed[key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                Approval.parse(changed, self.manifest, "smoke")

    def test_exhausted_unknown_or_stale_consumption_stops(self):
        approval = Approval.parse(self.approval, self.manifest, "smoke")
        approval.check_next([], observed_provider_usd=0, observed_infrastructure_usd=0)
        for provider, infrastructure in ((None, 0), (0, None), (0.95, 0), (0, 2)):
            with self.assertRaises(ValueError):
                approval.check_next([], observed_provider_usd=provider, observed_infrastructure_usd=infrastructure)
        with self.assertRaises(ValueError):
            approval.check_next([{"provider_billed_usd": None}], observed_provider_usd=0, observed_infrastructure_usd=0)


if __name__ == "__main__":
    unittest.main()
