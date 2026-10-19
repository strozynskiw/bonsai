"""Explicit real pinned-CLI contract test: no credential, inference or downloads."""

import asyncio
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import unittest

RUNTIME = os.environ.get("TB4_DSH_RUNTIME")
AVAILABLE = bool(RUNTIME and importlib.util.find_spec("harbor"))


@unittest.skipUnless(AVAILABLE, "set TB4_DSH_RUNTIME to the prebuilt pinned DSH workspace")
class PinnedCliTests(unittest.IsolatedAsyncioTestCase):
    async def test_fresh_session_without_resume_flag_exports_real_durable_failure(self):
        from .comparison_manifest import DSH_COMMIT
        from .dsh import fresh_session_id
        from .dsh_outcome import parse_sessions
        runtime = Path(RUNTIME).resolve()
        revision = subprocess.run(["git", "-C", str(runtime), "rev-parse", "HEAD"],
                                  check=True, capture_output=True, text=True).stdout.strip()
        self.assertEqual(revision, DSH_COMMIT)
        node = shutil.which("node")
        self.assertIsNotNone(node)
        with tempfile.TemporaryDirectory(prefix="tb4-real-dsh-") as directory:
            root = Path(directory)
            # No credential reaches this child; the endpoint is also loopback.
            # The real CLI must emit and persist a fresh session even when the
            # request cannot start. No local imitation of its argv parser.
            env = {"PATH": os.defpath, "HOME": directory, "DSH_HOME": directory,
                   "XDG_CONFIG_HOME": str(root / "config"),
                   "XDG_CACHE_HOME": str(root / "cache"),
                   "DEEPSEEK_BASE_URL": "https://127.0.0.1:1", "NO_COLOR": "1"}
            process = await asyncio.create_subprocess_exec(
                node, "--import", "tsx/esm", "apps/cli/src/bin.ts", "--profile", "headless", "--json", "-",
                cwd=runtime, env=env, stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
                start_new_session=True)
            try:
                stdout, stderr = await asyncio.wait_for(process.communicate(b"Reply briefly. Do not use tools."), 30)
            finally:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                await process.wait()
            session = fresh_session_id(stdout.decode())
            self.assertNotEqual(process.returncode, 0, stderr.decode())
            output = root / "sessions.json"
            exporter = Path(__file__).with_name("dsh_export.mjs").resolve()
            exported = await asyncio.create_subprocess_exec(
                node, str(exporter), str(runtime), str(root / "sessions"), session, str(output),
                env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
            _, error = await asyncio.wait_for(exported.communicate(), 30)
            self.assertEqual(exported.returncode, 0, error.decode())
            sessions = json.loads(output.read_text(encoding="utf-8"))
            self.assertIn(session, sessions)
            self.assertEqual(parse_sessions(sessions, session, process.returncode).state, "agent_failure")


if __name__ == "__main__":
    unittest.main()
