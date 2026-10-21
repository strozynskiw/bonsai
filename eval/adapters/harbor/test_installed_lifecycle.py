"""Explicit pinned-dependency lane; skips, never installs, when unavailable.

This exercises real Harbor base-class construction and custom-agent run methods
against a fake environment. Container/verifier/process-tree proof is a separate
Linux lane and is not claimed by these fixtures.
"""

import importlib.util
import json
from pathlib import Path
from unittest.mock import patch
import tempfile
from types import SimpleNamespace
import unittest

AVAILABLE = importlib.util.find_spec("harbor") is not None


class FakeEnvironment:
    def __init__(self, payload):
        self.payload = payload
        self.calls = []
        self.sessions = {}

    async def exec(self, **kwargs):
        self.calls.append(kwargs)
        stdout = "a" * 64 if "b3sum" in kwargs["command"] else json.dumps(self.payload)
        return SimpleNamespace(return_code=0, stdout=stdout, stderr="")

    async def upload_file(self, source, destination):
        self.overlay = Path(source).read_text(encoding="utf-8")

    async def download_file(self, source, destination):
        payload = self.payload if source.endswith("dsh.jsonl") else self.sessions
        Path(destination).write_text(json.dumps(payload), encoding="utf-8")


@unittest.skipUnless(AVAILABLE, "explicit operator setup of tb4-requirements.txt required")
class InstalledLifecycleTests(unittest.IsolatedAsyncioTestCase):
    async def test_bonsai_real_base_constructor_and_env_forwarding(self):
        from .bonsai import Bonsai
        from harbor.models.agent.context import AgentContext
        with tempfile.TemporaryDirectory() as directory:
            agent = Bonsai(logs_dir=Path(directory), model_name="deepseek/fixture-model",
                           autonomy="yolo", reasoning_effort="high", max_turns=10,
                           max_generation_seconds=20, max_output_chars=2000, max_tool_seconds=10,
                           timeout_seconds=30, network="allow", extra_env={"DEEPSEEK_API_KEY": "fixture-secret",
                           "DEEPSEEK_BASE_URL": "https://provider.invalid"})
            environment = FakeEnvironment({"status": "completed", "provider": "deepseek", "model": "fixture-model"})
            context = AgentContext()
            instruction = "quote ' \" $HOME\n雪"
            await agent.install(environment)
            await agent.run(instruction, environment, context)
            execution = next(call for call in environment.calls if "-p -" in call["command"])
            self.assertIn(instruction, execution["env"].values())
            self.assertNotIn(instruction, execution["command"])
            self.assertNotIn("fixture-secret", execution["command"])
            self.assertEqual(execution["env"]["DEEPSEEK_API_KEY"], "fixture-secret")
            self.assertIsNone(context.n_input_tokens)
            self.assertIsNone(context.cost_usd)

    async def test_diagnostic_forwards_only_explicit_private_checkpoint_path(self):
        from .bonsai import Bonsai
        from harbor.models.agent.context import AgentContext
        from harbor.models.trial.paths import EnvironmentPaths
        path = (EnvironmentPaths.agent_dir / "checkpoints.jsonl").as_posix()
        rows = [{"type": "identity", "schema": 1, "boundary": "agent_to_provider", "model": "anthropic-compatible/fixture-model",
                 "request_ordinal": 1, "request_sha256": "a" * 64},
                {"type": "summary", "requests": 1, "stages": [False] * 3, "capture_valid": True}]
        class DiagnosticEnvironment(FakeEnvironment):
            async def exec(self, **kwargs):
                if kwargs["command"] == "cat " + path:
                    return SimpleNamespace(return_code=0, stdout="\n".join(map(json.dumps, rows)), stderr="")
                return await super().exec(**kwargs)
        with tempfile.TemporaryDirectory() as directory, patch.dict("os.environ", {
                "BONSAI_TRANSCRIPT_LOG": "/tmp/ambient-full-log", "BONSAI_CHECKPOINT_LOG": "/tmp/ambient"}):
            for explicit in (None, path, "/tmp/forbidden"):
                extra = {"ANTHROPIC_COMPATIBLE_API_KEY": "fixture-secret",
                         "ANTHROPIC_COMPATIBLE_BASE_URL": "https://provider.invalid"}
                if explicit is not None:
                    extra["BONSAI_CHECKPOINT_LOG"] = explicit
                agent = Bonsai(logs_dir=Path(directory), model_name="anthropic-compatible/fixture-model",
                               autonomy="yolo", reasoning_effort="off", max_turns=200,
                               max_generation_seconds=20, max_output_chars=2000, max_tool_seconds=10,
                               timeout_seconds=30, network="allow", extra_env=extra)
                environment = DiagnosticEnvironment({"status": "completed", "provider": "anthropic-compatible",
                                                     "model": "anthropic-compatible/fixture-model" if explicit else "fixture-model"})
                if explicit == "/tmp/forbidden":
                    with self.assertRaises(ValueError):
                        await agent.run("prompt", environment, AgentContext())
                    continue
                await agent.run("prompt", environment, AgentContext())
                execution = next(call for call in environment.calls if "-p -" in call["command"])
                self.assertEqual(execution["env"].get("BONSAI_CHECKPOINT_LOG"), explicit)
                self.assertNotIn("BONSAI_TRANSCRIPT_LOG", execution["env"])
                if explicit == path:
                    for payload in ({}, {"provider": "wrong", "model": agent.model_name}):
                        context = AgentContext()
                        with self.assertRaisesRegex(RuntimeError, "diagnostic.*stop batch"):
                            await agent.run("prompt", DiagnosticEnvironment(payload), context)
                        self.assertEqual(context.metadata["bonsai_terminal_state"], "infrastructure_invalid")
                    rows[-1]["capture_valid"] = False
                    context = AgentContext()
                    with self.assertRaisesRegex(RuntimeError, "checkpoint evidence invalid"):
                        await agent.run("prompt", environment, context)
                    self.assertEqual(context.metadata["bonsai_terminal_state"], "infrastructure_invalid")
                    rows[-1]["capture_valid"] = True

    async def test_ambient_credentials_cannot_fill_missing_trial_overlay(self):
        from .bonsai import Bonsai
        from .dsh import Dsh
        from harbor.models.agent.context import AgentContext
        with tempfile.TemporaryDirectory() as directory, patch.dict("os.environ", {
                "DEEPSEEK_API_KEY": "ambient-secret", "DEEPSEEK_BASE_URL": "https://ambient.invalid",
                "TB4_REASONING": "high", "TB4_TIMEOUT_SECONDS": "30"}):
            bonsai = Bonsai(logs_dir=Path(directory), model_name="deepseek/fixture-model",
                            autonomy="yolo", reasoning_effort="high", max_turns=10,
                            max_generation_seconds=20, max_output_chars=2000, max_tool_seconds=10,
                            timeout_seconds=30, network="allow", extra_env={})
            dsh = Dsh(logs_dir=Path(directory), model_name="fixture-model", extra_env={})
            for agent in (bonsai, dsh):
                environment = FakeEnvironment({})
                with self.subTest(agent=agent.name()), self.assertRaisesRegex(ValueError, "explicit trial environment"):
                    await agent.run("prompt", environment, AgentContext())
                self.assertEqual(environment.calls, [])

    async def test_dsh_failed_final_does_not_override_persisted_failure(self):
        from .dsh import Dsh
        from harbor.models.agent.context import AgentContext
        with tempfile.TemporaryDirectory() as directory:
            agent = Dsh(logs_dir=Path(directory), model_name="fixture-model", extra_env={
                "DEEPSEEK_API_KEY": "fixture-secret", "DEEPSEEK_BASE_URL": "https://provider.invalid",
                "TB4_REASONING": "high", "TB4_TIMEOUT_SECONDS": "30"})
            environment = FakeEnvironment({"type": "session", "sessionId": "session-fixture"})
            # No durable root session: must fail, never trust stdout final.
            with self.assertRaises(RuntimeError):
                await agent.run("prompt ' \n雪", environment, AgentContext())
            execution = next(call for call in environment.calls if "--profile headless" in call["command"])
            self.assertNotIn("--session-id", execution["command"])
            self.assertEqual(execution["env"]["TB4_INSTRUCTION"], "prompt ' \n雪")
            self.assertNotIn("fixture-secret", execution["command"])
            self.assertEqual(json.loads(environment.overlay)[0]["config"]["model"], "fixture-model")

    async def test_dsh_fresh_session_is_exported_and_durable_outcome_required(self):
        from .dsh import Dsh
        from harbor.models.agent.context import AgentContext
        with tempfile.TemporaryDirectory() as directory:
            agent = Dsh(logs_dir=Path(directory), model_name="fixture-model", extra_env={
                "DEEPSEEK_API_KEY": "fixture-secret", "DEEPSEEK_BASE_URL": "https://provider.invalid",
                "TB4_REASONING": "off", "TB4_TIMEOUT_SECONDS": "30"})
            environment = FakeEnvironment({"type": "session", "sessionId": "session-fresh"})
            environment.sessions = {"session-fresh": json.dumps({"type": "turn/end",
                "data": {"reason": {"kind": "completed"}}})}
            context = AgentContext()
            await agent.run("prompt", environment, context)
            self.assertEqual(context.metadata["session_id"], "session-fresh")
            self.assertEqual(context.metadata["terminal_state"], "completed")
            export = next(call for call in environment.calls if "dsh_export.mjs" in call["command"])
            self.assertIn("session-fresh", export["command"])
            self.assertNotIn("fixture-secret", export.get("env", {}).values())

    async def test_dsh_missing_or_ambiguous_session_stops_before_export(self):
        from .dsh import fresh_session_id
        valid = json.dumps({"type": "session", "sessionId": "session-fresh"})
        for text in ('', '{}', 'null', 'invalid', valid + '\n' + valid,
                     json.dumps({"type": "session", "sessionId": "../escape"}),
                     json.dumps({"type": "session", "sessionId": 1})):
            with self.subTest(text=text), self.assertRaises(RuntimeError):
                fresh_session_id(text)
        self.assertEqual(fresh_session_id(valid + '\n{"type":"final"}'), "session-fresh")


if __name__ == "__main__":
    unittest.main()
