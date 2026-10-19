"""Offline smoke prerequisite tests; synthetic records are not run evidence."""

import copy
from dataclasses import replace
import unittest

from .comparison_gate import Approval, smoke_digest
from .comparison_runner import run_serial
from . import test_comparison_report as fixtures
from .test_comparison_runner import FakeBackend, MemoryJournal


class SmokeGateTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        fixture = fixtures.GateTests()
        fixture.setUp()
        self.manifest = fixture.manifest
        self.value = {**fixture.approval, "phase": "pilot"}
        self.records = fixtures.records_fixture(self.manifest, 1, "smoke")
        self.value["smoke_evidence_digest"] = smoke_digest(self.manifest, self.records)

    async def test_pilot_requires_bound_smoke_before_preflight(self):
        with self.assertRaises(ValueError):
            Approval.parse(self.value, self.manifest, "pilot")
        approval = Approval.parse(self.value, self.manifest, "pilot", smoke_records=self.records)
        approval.check_smoke(self.manifest, list(reversed(self.records)))
        backend, journal = FakeBackend(), MemoryJournal()
        for records in (None, [], self.records[:-1]):
            with self.subTest(records=records), self.assertRaises(ValueError):
                await run_serial(self.manifest, "pilot", approval, backend, journal,
                                 smoke_records=records)
        with self.assertRaises(ValueError):
            await run_serial(self.manifest, "pilot", replace(approval, smoke_evidence_digest=None),
                             backend, journal, smoke_records=self.records)
        self.assertEqual(backend.events, [])
        self.assertEqual(journal.records(), [])

    async def test_failed_duplicate_changed_or_mismatched_smoke_is_rejected(self):
        for field, value in (("category", "timeout"), ("reward", 0), ("reward", True),
                             ("model", {}), ("profile_digest", "wrong"),
                             ("artifact_digest", "wrong"), ("manifest_digest", "wrong")):
            records = copy.deepcopy(self.records)
            records[0][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                Approval.parse(self.value, self.manifest, "pilot", smoke_records=records)
        with self.assertRaises(ValueError):
            smoke_digest(self.manifest, self.records + [self.records[0]])
        records = copy.deepcopy(self.records)
        records[0]["wall_seconds"] = 3
        with self.assertRaises(ValueError):
            Approval.parse(self.value, self.manifest, "pilot", smoke_records=records)


if __name__ == "__main__":
    unittest.main()
