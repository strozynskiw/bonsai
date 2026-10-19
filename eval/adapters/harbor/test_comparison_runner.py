import asyncio
import unittest

from .comparison_gate import Approval
from .comparison_manifest import FrozenManifest, freeze
from .comparison_runner import Consumption, run_serial
from .comparison_schedule import schedule
from .test_comparison_manifest import manifest_fixture
from . import test_comparison_report as report_fixtures


class MemoryJournal:
    def __init__(self):
        self.rows = []

    def records(self):
        return self.rows.copy()

    def begin(self, trial, manifest_digest):
        self.rows.append({"identity": trial.identity, "manifest_digest": manifest_digest,
                          "category": "infrastructure_invalid", "provider_billed_usd": None})

    def finish(self, trial, result):
        self.rows[-1].update(result)


class FakeBackend:
    def __init__(self, category="verified", *, cancel=False):
        self.events = []
        self.category = category
        self.cancel = cancel

    async def preflight(self, manifest):
        self.events.append("preflight")

    async def consumption(self):
        return Consumption("0", "0")

    async def run(self, trial):
        self.events.append(("run", trial.identity))
        if self.cancel:
            raise asyncio.CancelledError()
        return {"category": self.category, "provider_billed_usd": 0}

    async def teardown(self, trial):
        await asyncio.sleep(0)
        self.events.append(("teardown", trial.identity))


class RunnerTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        fixture = report_fixtures.GateTests()
        fixture.setUp()
        self.manifest = FrozenManifest.parse(freeze(manifest_fixture()))
        self.approval = Approval.parse(fixture.approval, self.manifest, "smoke")

    async def test_serial_teardown_and_resume(self):
        journal, backend = MemoryJournal(), FakeBackend()
        await run_serial(self.manifest, "smoke", self.approval, backend, journal)
        trials = schedule(self.manifest, "smoke")
        self.assertEqual(len(journal.records()), 6)
        self.assertEqual(backend.events[1:], [event for trial in trials for event in
                         (("run", trial.identity), ("teardown", trial.identity))])
        await run_serial(self.manifest, "smoke", self.approval, backend, journal)
        self.assertEqual(len(journal.records()), 6)

    async def test_cancel_tears_down_and_preserves_identity(self):
        journal, backend = MemoryJournal(), FakeBackend(cancel=True)
        with self.assertRaises(asyncio.CancelledError):
            await run_serial(self.manifest, "smoke", self.approval, backend, journal)
        self.assertEqual(journal.records()[0]["category"], "cancelled")
        self.assertEqual(backend.events[-1][0], "teardown")
        # Unknown billing after interruption forbids scheduling another attempt.
        with self.assertRaises(ValueError):
            await run_serial(self.manifest, "smoke", self.approval, FakeBackend(), journal)

    async def test_infrastructure_failure_stops_batch(self):
        journal, backend = MemoryJournal(), FakeBackend("infrastructure_invalid")
        with self.assertRaises(RuntimeError):
            await run_serial(self.manifest, "smoke", self.approval, backend, journal)
        self.assertEqual(len(journal.records()), 1)
        self.assertEqual(backend.events[-1][0], "teardown")

    async def test_cleanup_failure_invalidates_verified_reward(self):
        class FailedCleanup(FakeBackend):
            async def run(self, trial):
                return {"category": "verified", "reward": 1, "provider_billed_usd": 0}

            async def teardown(self, trial):
                raise RuntimeError("cleanup failed")

        journal = MemoryJournal()
        with self.assertRaisesRegex(RuntimeError, "cleanup failed"):
            await run_serial(self.manifest, "smoke", self.approval, FailedCleanup(), journal)
        self.assertEqual(len(journal.records()), 1)
        self.assertEqual(journal.records()[0]["category"], "infrastructure_invalid")
        self.assertIsNone(journal.records()[0]["reward"])

    async def test_repeated_cancellation_cannot_detach_cleanup(self):
        started, release, stopped = asyncio.Event(), asyncio.Event(), asyncio.Event()

        class SlowCleanup(FakeBackend):
            async def teardown(self, trial):
                started.set()
                await release.wait()
                stopped.set()

        journal = MemoryJournal()
        task = asyncio.create_task(run_serial(
            self.manifest, "smoke", self.approval, SlowCleanup(cancel=True), journal))
        await started.wait()
        for _ in range(2):
            task.cancel()
            await asyncio.sleep(0)
        self.assertFalse(task.done())
        release.set()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assertTrue(stopped.is_set())
        self.assertEqual(len(journal.records()), 1)
        self.assertEqual(journal.records()[0]["category"], "cancelled")


if __name__ == "__main__":
    unittest.main()
