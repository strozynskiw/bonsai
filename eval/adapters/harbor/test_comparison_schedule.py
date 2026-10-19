import json
import unittest

from .comparison_manifest import DATASET, FrozenManifest, digest, freeze
from .comparison_schedule import remaining, schedule, select_tasks
from .dsh_outcome import parse_sessions
from .test_comparison_manifest import manifest_fixture


class ScheduleTests(unittest.TestCase):
    def test_pair_order_inventory_and_resume(self):
        manifest = FrozenManifest.parse(freeze(manifest_fixture()))
        trials = schedule(manifest, "pilot")
        self.assertEqual(len(trials), 60)
        self.assertEqual(len({t.identity for t in trials}), 60)
        for i in range(0, 60, 2):
            self.assertEqual(trials[i].task, trials[i + 1].task)
            self.assertNotEqual(trials[i].agent, trials[i + 1].agent)
        records = [{"identity": trials[0].identity, "manifest_digest": manifest.sha256}]
        self.assertEqual(remaining(manifest, "pilot", records), trials[1:])
        with self.assertRaises(ValueError):
            remaining(manifest, "pilot", records * 2)

    def test_cpu_sampling_is_order_independent_and_fails_small_pool(self):
        inventory = {"dataset": DATASET, "dataset_digest": digest("dataset"), "tasks": [
            {"id": f"task-{i}", "content_digest": digest(i), "image_digest": digest(i),
             "requirements": {"gpus": int(i == 15), "cpu": 1, "ram_mb": 1, "disk_mb": 1, "unsupported": []},
             "resource_source": f"task-{i}/task.toml"} for i in range(16)]}
        resources = manifest_fixture()["resources"]
        selected = select_tasks(inventory, "seed", resources)
        inventory["tasks"].reverse()
        reversed_selection = select_tasks(inventory, "seed", resources)
        self.assertEqual(selected["tasks"], reversed_selection["tasks"])
        self.assertEqual(selected["exclusions"], reversed_selection["exclusions"])
        self.assertEqual(len(selected["tasks"]), 13)
        inventory["tasks"] = inventory["tasks"][:5]
        with self.assertRaisesRegex(ValueError, "need 13"):
            select_tasks(inventory, "seed", resources)


class DshOutcomeTests(unittest.TestCase):
    @staticmethod
    def events(reason="completed", usage=True):
        sample = {"inputTokens": 3, "outputTokens": 2, "cacheReadTokens": 1}
        return "\n".join(json.dumps(event) for event in [
            {"type": "assistant/attempt", "data": {"stream": [{"type": "chunk", "chunk": {"type": "usage", "usage": sample}}] if usage else []}},
            {"type": "assistant/message", "data": {"usage": sample, "stream": [{"type": "chunk", "chunk": {"type": "usage", "usage": sample}}]}},
            {"type": "turn/end", "data": {"reason": {"kind": reason}}},
        ])

    def test_failed_turn_and_exit_status_not_final_presence(self):
        self.assertEqual(parse_sessions({"root": self.events("error")}, "root", 0).state, "agent_failure")
        self.assertEqual(parse_sessions({"root": self.events()}, "root", 1).state, "agent_failure")
        self.assertEqual(parse_sessions({}, "root", 0).state, "agent_failure")

    def test_malformed_tail_never_preserves_completed_state(self):
        for tail in ('{"', json.dumps({"type": "assistant/message", "data": {"stream": [None]}}),
                     json.dumps({"type": "turn/end", "data": []})):
            result = parse_sessions({"root": self.events() + "\n" + tail}, "root", 0)
            self.assertEqual(result.state, "agent_failure")
            self.assertIsNone(result.usage)

    def test_retry_child_usage_and_missing_telemetry(self):
        result = parse_sessions({"root": self.events(), "child": self.events()}, "root", 0)
        self.assertEqual(result.usage["inputTokens"], 12)
        self.assertEqual(result.usage["outputTokens"], 8)
        self.assertNotIn("cacheWriteTokens", result.usage)
        self.assertIsNone(parse_sessions({"root": self.events(usage=False)}, "root", 0).usage)
        self.assertIsNone(parse_sessions({"root": self.events() + '\n{"'}, "root", 1).usage)


if __name__ == "__main__":
    unittest.main()
