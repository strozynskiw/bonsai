"""No-inference tests for external billing and frozen local execution."""

from datetime import datetime, timezone
import copy
import json
import os
from pathlib import Path
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, patch

from .comparison_backend import BillingFile, HarborBackend
from .comparison_manifest import FrozenManifest, digest, freeze
from .local_trial import normalize_result, tree_digest
from .test_comparison_manifest import manifest_fixture


def backend_fixture():
    body = manifest_fixture()
    body["execution"] = {
        "task_paths": {task["id"]: task["id"] for task in body["tasks"]},
        "image_ids": {task["id"]: "sha256:" + "1" * 64 for task in body["tasks"]},
        "artifact_paths": {"bonsai": "/opt/benchmark/bonsai", "dsh": "/opt/benchmark/dsh"},
        "profiles": {"bonsai": {"options": {"reasoning_effort": "high", "timeout_seconds": body["resources"]["timeout_seconds"],
                                              "network": "allow"},
                                  "environment": {"BONSAI_PROVIDER": "anthropic-compatible"}},
                     "dsh": {"options": {}, "environment": {}}},
        "request_evidence_digest": digest("request"), "external_cap_evidence_digest": digest("cap"),
        "disk_enforcement": "dedicated-runner-volume"}
    for agent, profile in body["execution"]["profiles"].items():
        body["profiles"][agent]["overlay_digest"] = digest(profile)
    return FrozenManifest.parse(freeze(body))


class BillingTests(unittest.TestCase):
    def test_fresh_monotonic_settlement_exact_bindings_and_private_file(self):
        manifest = backend_fixture()
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "billing.json"
            value = {"manifest_digest": manifest.sha256, "phase": "smoke", "sequence": 1,
                     "observed_at": datetime.now(timezone.utc).isoformat(), "settled_identity": None,
                     "provider_usd": "0", "infrastructure_usd": "0",
                     "external_cap_evidence_digest": manifest.body["execution"]["external_cap_evidence_digest"]}
            def publish(snapshot):
                path.write_text(json.dumps(snapshot), encoding="utf-8")
                path.chmod(0o600)
            publish(value)
            billing = BillingFile(path, manifest, "smoke")
            self.assertEqual(billing.read()["sequence"], 1)
            for changes in ({"phase": "pilot"}, {"manifest_digest": "wrong"},
                            {"provider_usd": "-1"}, {"sequence": 0},
                            {"observed_at": "2000-01-01T00:00:00+00:00"},
                            {"provider_usd": "1"}, {"sequence": True}):
                publish({**value, **changes})
                with self.subTest(changes=changes), self.assertRaises(ValueError):
                    billing.read()
            publish(value)
            with self.assertRaises(ValueError):
                billing.read(identity="smoke/task/1/bonsai")
            value.update(sequence=2, settled_identity="smoke/task/1/bonsai", provider_usd="0.01")
            publish(value)
            self.assertEqual(billing.read(identity=value["settled_identity"])["provider_usd"], "0.01")
            path.chmod(0o644)
            with self.assertRaises(ValueError):
                billing.read()


class BackendTests(unittest.IsolatedAsyncioTestCase):
    async def test_preflight_rejects_desktop_before_docker_or_trial(self):
        manifest = backend_fixture()
        backend = HarborBackend(manifest, task_root=Path("."), output=Path("target/unused"),
                                billing=None, credential="fake", evidence={})
        with patch("eval.adapters.harbor.comparison_backend.require_runtime"), \
                patch("eval.adapters.harbor.comparison_backend.platform.system", return_value="Darwin"), \
                patch("eval.adapters.harbor.comparison_backend.docker_json", new_callable=AsyncMock) as docker:
            with self.assertRaisesRegex(ValueError, "Linux"):
                await backend.preflight(manifest)
            docker.assert_not_called()
        with self.assertRaises(ValueError):
            await backend.run(SimpleNamespace(identity="missing-billing"))

    async def test_mismatched_evidence_rejected_before_task_effects(self):
        manifest = backend_fixture()
        backend = HarborBackend(manifest, task_root=Path("."), output=Path("target/unused"),
                                billing=None, credential="fake", evidence={
                                    "request_evidence_digest": {"forged": True},
                                    "external_cap_evidence_digest": {}})
        with patch("eval.adapters.harbor.comparison_backend.require_runtime"), \
                patch("eval.adapters.harbor.comparison_backend.platform.system", return_value="Linux"):
            with self.assertRaisesRegex(ValueError, "evidence bytes"):
                await backend.preflight(manifest)

    async def test_billing_only_settles_after_cleanup(self):
        manifest = backend_fixture()
        billing = SimpleNamespace(read=lambda **kwargs: {"provider_usd": "0.02", "infrastructure_usd": "0.01"})
        backend = HarborBackend(manifest, task_root=Path("."), output=Path("target/unused"),
                                billing=billing, credential="fake", evidence={})
        events = []
        async def stop():
            events.append("cleanup")
        backend.executor = SimpleNamespace(teardown=stop)
        backend.before = {"provider_usd": "0.01", "infrastructure_usd": "0"}
        backend.result = {"category": "verified", "reward": 1}
        result = backend.result
        await backend.teardown(SimpleNamespace(identity="trial"))
        self.assertEqual(events, ["cleanup"])
        self.assertEqual(result["provider_billed_usd"], .01)
        self.assertEqual(result["infrastructure_usd"], .01)

    async def test_full_preflight_and_container_hook_with_frozen_local_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            body = backend_fixture().body
            execution = body["execution"]
            request = {"model": body["model"], "protocol": "messages",
                       "both_agents_observed": True, "fallback_disabled": True}
            cap = {"external_hard_cap": True}
            execution["request_evidence_digest"] = digest(request)
            execution["external_cap_evidence_digest"] = digest(cap)
            for task in body["tasks"]:
                path = root / task["id"]
                path.mkdir()
                (path / "instruction.md").write_text("fixture", encoding="utf-8")
                (path / "tests").mkdir()
                (path / "tests/test.sh").write_text("echo 1", encoding="utf-8")
                (path / "task.toml").write_text(
                    'version = "1.0"\n[verifier]\nenvironment_mode = "separate"\n'
                    '[environment]\n' + f'docker_image = "local@{task["image_digest"]}"\n'
                    'network_mode = "allowlist"\nallowed_hosts = ["provider.invalid"]\n'
                    'workdir = "/app"\n', encoding="utf-8")
                task["content_digest"] = tree_digest(path)
            isolation = {"platform": "linux", "runner": "fixture-runner", "resources": body["resources"],
                         "image_ids": execution["image_ids"], **{key: True for key in (
                         "no_host_credentials", "no_host_home_or_socket", "network_enforced",
                         "disk_quota_enforced", "verifier_isolated", "cancellation_teardown_verified")}}
            manifest = FrozenManifest.parse(freeze(body))
            backend = HarborBackend(manifest, task_root=root, output=root / "private",
                billing=SimpleNamespace(read=lambda **kwargs: {"provider_usd": "0", "infrastructure_usd": "0"}),
                credential="synthetic", approval=SimpleNamespace(runner="fixture-runner", check_next=lambda *args, **kwargs: None),
                records=lambda: [], evidence={
                    "request_evidence_digest": request, "external_cap_evidence_digest": cap,
                    "isolation_evidence_digest": isolation})
            image_id = "sha256:" + "1" * 64
            checked_task = SimpleNamespace(config=SimpleNamespace(mcp_servers=[],
                verifier=SimpleNamespace(environment_mode=SimpleNamespace(value="separate"))), has_steps=False)
            with patch("eval.adapters.harbor.comparison_backend.require_runtime"), \
                    patch("eval.adapters.harbor.comparison_backend.platform.system", return_value="Linux"), \
                    patch("eval.adapters.harbor.comparison_backend.platform.node", return_value="fixture-runner"), \
                    patch("harbor.models.task.task.Task", return_value=checked_task), \
                    patch("eval.adapters.harbor.comparison_backend.docker_json", new_callable=AsyncMock,
                          return_value=[{"Id": image_id}]):
                await backend.preflight(manifest)
                backend.approval.runner = "different-runner"
                with self.assertRaisesRegex(ValueError, "approved runner"):
                    await backend.preflight(manifest)
                backend.approval.runner = "fixture-runner"
            self.assertEqual((root / "private").stat().st_mode & 0o077, 0)
            backend.active = SimpleNamespace(agent="bonsai", task=body["tasks"][0]["id"], identity="fixture")
            artifact = body["artifacts"]["bonsai"]["artifact_digest"].removeprefix("sha256:")
            environment = SimpleNamespace(session_id="tb4-fixture__env",
                exec=AsyncMock(return_value=SimpleNamespace(return_code=0, stdout=artifact)))
            backend.executor = SimpleNamespace(trial=SimpleNamespace(agent_environment=environment))
            container = {"Image": image_id, "HostConfig": {"Memory": 4096 * 1024 * 1024,
                         "NanoCpus": 2_000_000_000}, "Config": {"Env": []}, "Mounts": []}
            with patch("eval.adapters.harbor.comparison_backend.docker_json", new_callable=AsyncMock,
                       side_effect=[{"ID": "fixture"}, [container]]):
                await backend._before_agent(None)
            environment.exec.reset_mock()
            def deny(*args, **kwargs):
                raise ValueError("budget exhausted")
            backend.approval.check_next = deny
            with patch("eval.adapters.harbor.comparison_backend.docker_json", new_callable=AsyncMock) as docker:
                with self.assertRaisesRegex(ValueError, "budget exhausted"):
                    await backend._before_agent(None)
                docker.assert_not_called()
                environment.exec.assert_not_called()
            backend.approval.check_next = lambda *args, **kwargs: None
            container["HostConfig"]["Privileged"] = True
            with patch("eval.adapters.harbor.comparison_backend.docker_json", new_callable=AsyncMock,
                       side_effect=[{"ID": "fixture"}, [container]]):
                with self.assertRaisesRegex(ValueError, "confinement"):
                    await backend._before_agent(None)
                environment.exec.assert_not_called()

    async def test_tree_digest_binds_content_executable_bits_and_rejects_links(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            file = root / "instruction.md"
            file.write_text("fixture", encoding="utf-8")
            before = tree_digest(root)
            file.write_text("changed", encoding="utf-8")
            self.assertNotEqual(before, tree_digest(root))
            before = tree_digest(root)
            file.chmod(0o700)
            self.assertNotEqual(before, tree_digest(root))
            (root / "link").symlink_to(file)
            with self.assertRaises(ValueError):
                tree_digest(root)


class ResultTests(unittest.TestCase):
    def test_strict_success_and_failure_mapping(self):
        result = SimpleNamespace(
            agent_result=SimpleNamespace(metadata={"terminal_state": "completed"},
                n_input_tokens=None, n_output_tokens=None, n_cache_tokens=None, cost_usd=None),
            exception_info=None, verifier_result=SimpleNamespace(rewards={"reward": 1}),
            reward_valid=True, started_at=None, finished_at=None)
        self.assertEqual(normalize_result(result)["category"], "verified")
        self.assertIsNone(normalize_result(result)["provider_billed_usd"])
        result.exception_info = SimpleNamespace(exception_type="AgentTimeoutError")
        self.assertEqual(normalize_result(result)["category"], "timeout")
        self.assertIsNone(normalize_result(result)["reward"])
        result.exception_info = None
        result.verifier_result.rewards = {"reward": True}
        self.assertEqual(normalize_result(result)["category"], "infrastructure_invalid")
        result.verifier_result.rewards = {"reward": 1}
        result.reward_valid = False
        self.assertIsNone(normalize_result(result)["reward"])
        result.agent_result.metadata = {}
        self.assertEqual(normalize_result(result)["category"], "infrastructure_invalid")


if __name__ == "__main__":
    unittest.main()
