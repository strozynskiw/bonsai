"""Offline receipt validation independent of evaluation orchestration."""

import copy
import json
import unittest

from .diagnostic_evidence import validate_receipts


class ReceiptTests(unittest.TestCase):
    def rows(self):
        return [{"type": "identity", "schema": 1, "boundary": "agent_to_provider",
                 "model": "fixture", "request_ordinal": 1, "request_sha256": "a" * 64},
                {"type": "summary", "requests": 1, "stages": [False] * 3,
                 "capture_valid": True}]

    def validate(self, rows, **kwargs):
        return validate_receipts("\n".join(map(json.dumps, rows)),
                                 expected_model="fixture", **kwargs)

    def test_completeness_and_bounds(self):
        rows = self.rows()
        raw = "\n".join(map(json.dumps, rows))
        self.assertEqual(self.validate(rows)["requests"], 1)
        for broken in ("", json.dumps(rows[0]), raw * 100,
                       raw.replace('"capture_valid": true', '"capture_valid": false')):
            with self.assertRaises(ValueError):
                validate_receipts(broken, expected_model="fixture")
        with self.assertRaises(ValueError):
            validate_receipts(raw, expected_model="different")

    def test_rejects_zero_ordinal_and_malformed_types(self):
        rows = self.rows()
        rows.insert(1, {"type": "checkpoint", "stage": 1, "request_ordinal": 1,
                        "text": "Harness note: Delivery checkpoint 1/3: fixture",
                        "message_sha256": "b" * 64, "request_sha256": "a" * 64})
        rows[-1]["stages"] = [True, False, False]
        self.assertEqual(self.validate(rows)["stages"], [1])
        for index, field, value in ((1, "request_ordinal", 0), (2, "stages", None),
                                    (2, "stages", 3), (2, "stages", "abc"),
                                    (0, "request_sha256", 42), (1, "request_sha256", None),
                                    (1, "message_sha256", 42)):
            with self.subTest(field=field, value=value):
                broken = copy.deepcopy(rows)
                broken[index][field] = value
                with self.assertRaises(ValueError):
                    self.validate(broken)

    def test_explicit_unlimited_and_finite_limits(self):
        rows = self.rows()
        rows[-1]["requests"] = 334
        self.assertEqual(self.validate(rows, max_turns=None)["requests"], 334)
        with self.assertRaises(ValueError):
            self.validate(rows, max_turns=200)
        rows[-1]["requests"] = 200
        with self.assertRaises(ValueError):
            self.validate(rows, max_turns=200)
        for limit in (0, -1, True, "200"):
            with self.subTest(limit=limit), self.assertRaises(ValueError):
                self.validate(rows, max_turns=limit)
