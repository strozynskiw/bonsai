"""Pinned Harbor + explicit local Docker fixture lane, never a paid model run."""

import asyncio
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
import uuid

from .local_trial import LocalExecutor, LocalTrial, tree_digest


@unittest.skipUnless(importlib.util.find_spec("harbor") and os.environ.get("TB4_FIXTURE_IMAGE"),
                     "set TB4_FIXTURE_IMAGE to the explicitly built synthetic CLI image")
class DockerLifecycleTests(unittest.IsolatedAsyncioTestCase):
    async def exercise(self, agent, instruction, expected, *, cancel=False, reject_setup=False):
        with tempfile.TemporaryDirectory(prefix="tb4-fixture-") as directory:
            root = Path(directory)
            task = root / "task"
            task.mkdir()
            (task / "tests").mkdir()
            (task / "environment").mkdir()
            (task / "instruction.md").write_text(instruction, encoding="utf-8")
            image = os.environ["TB4_FIXTURE_IMAGE"]
            (task / "task.toml").write_text(
                'version = "1.0"\n[agent]\ntimeout_sec = 20\n'
                '[verifier]\ntimeout_sec = 20\nenvironment_mode = "shared"\n'
                '[environment]\n' + f'docker_image = "{image}"\n'
                'cpus = 1\nmemory_mb = 256\nstorage_mb = 512\n'
                'workdir = "/workspace"\n', encoding="utf-8")
            # Darwin cannot use Harbor's Linux-kernel egress sidecar. This
            # synthetic-only compose definition enforces Docker network=none.
            (task / "environment/docker-compose.yaml").write_text(
                'services:\n  main:\n' + f'    image: "{image}"\n'
                '    entrypoint: []\n    command: ["sh", "-c", "sleep infinity"]\n'
                '    network_mode: none\n    working_dir: /workspace\n', encoding="utf-8")
            (task / "tests/test.sh").write_text(
                '#!/bin/bash\nmkdir -p /logs/verifier\n'
                'test "$(cat /workspace/answer)" = fixture && echo 1 >/logs/verifier/reward.txt\n',
                encoding="utf-8")
            env = {"DEEPSEEK_API_KEY": "synthetic-not-a-credential",
                   "DEEPSEEK_BASE_URL": "https://fixture.invalid", "TB4_REASONING": "high",
                   "TB4_TIMEOUT_SECONDS": "20", "BONSAI_BIN": "/opt/benchmark/bin/fake-bonsai",
                   "DSH_BIN": "/opt/benchmark/bin/fake-dsh"}
            options = {} if agent == "dsh" else {
                "autonomy": "yolo", "reasoning_effort": "high", "max_turns": 10,
                "max_generation_seconds": 20, "max_output_chars": 2000,
                "max_tool_seconds": 10, "timeout_seconds": 20, "network": "deny"}
            specification = LocalTrial(task, tree_digest(task), "tb4-" + uuid.uuid4().hex,
                                       root / "results", agent,
                                       "deepseek/fixture-model" if agent == "bonsai" else "fixture-model",
                                       env, options, 2 if instruction == "hang" else 20, 1, 256, 512)
            stages = []
            async def setup_guard(environment):
                stages.append("setup")
                self.assertNotIn("DEEPSEEK_API_KEY", executor.trial.agent.extra_env)
                if reject_setup:
                    raise ValueError("fixture setup guard refused")
            async def authorize(event):
                stages.append("authorize")
                self.assertEqual(stages, ["setup", "authorize"])
                self.assertNotIn("DEEPSEEK_API_KEY", executor.trial.agent.extra_env)
            executor = LocalExecutor(before_setup=setup_guard, before_agent=authorize)
            try:
                running = asyncio.create_task(executor.run(specification))
                if cancel:
                    for _ in range(200):
                        if running.done():
                            await running
                        if (root / "results" / specification.name / "agent" / "dsh.jsonl").exists():
                            break
                        await asyncio.sleep(.1)
                    running.cancel()
                    with self.assertRaises(asyncio.CancelledError):
                        await running
                else:
                    result = await running
                    self.assertEqual(result["category"], expected,
                                     executor.trial.result.exception_info)
                    self.assertEqual(stages, ["setup"] if reject_setup else ["setup", "authorize"])
                    if reject_setup:
                        self.assertIsNone(executor.trial.agent._version)
                        self.assertNotIn("DEEPSEEK_API_KEY", executor.trial.agent.extra_env)
                    if expected == "verified":
                        self.assertEqual(result["reward"], 1)
                        self.assertIsNone(result["provider_billed_usd"])
            finally:
                await executor.teardown()

    async def test_both_adapters_verified_by_real_harbor(self):
        for agent in ("bonsai", "dsh"):
            with self.subTest(agent=agent):
                await self.exercise(agent, "fixture", "verified")

    async def test_timeout_removes_descendants_and_container(self):
        await self.exercise("dsh", "hang", "timeout")

    async def test_cancel_removes_descendants_and_container(self):
        await self.exercise("dsh", "hang", "cancelled", cancel=True)

    async def test_cli_failure_cannot_retain_verifier_success(self):
        await self.exercise("bonsai", "fail", "agent_failure")

    async def test_setup_guard_refuses_before_version_or_secret_release(self):
        for agent in ("bonsai", "dsh"):
            with self.subTest(agent=agent):
                await self.exercise(agent, "fixture", "infrastructure_invalid", reject_setup=True)


if __name__ == "__main__":
    unittest.main()
