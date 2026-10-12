"""Offline stdlib-only import contract tests; no harness imports/downloads."""
import unittest
from copy import deepcopy

from eval.adapters.phase_e.run_canary import validate_grading


class GradingTests(unittest.TestCase):
    def report(self):
        return {
            "schema_version": 2, "total_instances": 2, "submitted_ids": ["a", "b"],
            "completed_ids": ["a", "b"], "resolved_ids": ["a"],
            "unresolved_ids": ["b"], "error_ids": [],
            "incomplete_ids": [], "empty_patch_ids": [],
        }

    def test_pass_and_ordinary_failure(self):
        self.assertEqual(validate_grading(self.report(), ["a", "b"]),
                         {"total": 2, "pass": 1, "fail": 1, "error": 0, "missing": 0})

    def test_completed_can_overlap_error(self):
        report = self.report()
        report["unresolved_ids"] = []
        report["error_ids"] = ["b"]
        self.assertEqual(validate_grading(report, ["a", "b"])["error"], 1)

    def test_missing_is_not_smaller_denominator(self):
        report = self.report()
        for key in ("submitted_ids", "completed_ids", "unresolved_ids"):
            report[key] = [v for v in report[key] if v != "b"]
        report["incomplete_ids"] = ["b"]
        result = validate_grading(report, ["a", "b"])
        self.assertEqual(result["total"], 2)
        self.assertEqual(result["missing"], 1)

    def test_foreign_duplicate_and_conflict(self):
        for key, value in (("resolved_ids", ["a", "a"]),
                           ("error_ids", ["foreign"]),
                           ("unresolved_ids", ["a", "b"])):
            with self.subTest(key=key):
                report = deepcopy(self.report())
                report[key] = value
                with self.assertRaises(ValueError):
                    validate_grading(report, ["a", "b"])

    def test_empty_and_corrupt(self):
        for report in ({}, {"schema_version": 2}, {"schema_version": 99}):
            with self.assertRaises(ValueError):
                validate_grading(report, ["a"])


if __name__ == "__main__":
    unittest.main()
