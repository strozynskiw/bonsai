"""Real local fake-CLI process tests; no Harbor/provider dependency."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from .installed_common import (
    isolated_command, private_home, redact, required_environment, require_https_endpoint,
)


class IsolationTests(unittest.TestCase):
    def test_only_explicit_trial_credentials_and_endpoints_are_accepted(self):
        self.assertEqual(required_environment({"KEY": "explicit", "OTHER": "excluded"}, ("KEY",)),
                         {"KEY": "explicit"})
        for overlay in ({}, {"KEY": ""}, {"KEY": "a\0b"}, {"KEY": None}):
            with self.subTest(overlay=overlay), self.assertRaises(ValueError):
                required_environment(overlay, ("KEY",))
        require_https_endpoint("https://provider.invalid/anthropic")
        for endpoint in ("http://provider.invalid", "https://user:secret@provider.invalid",
                         "https://provider.invalid?key=secret", "https://provider.invalid/#secret",
                         "https://provider.invalid:99999", "https://provider.invalid/\npath"):
            with self.subTest(endpoint=endpoint), self.assertRaises(ValueError):
                require_https_endpoint(endpoint)

    def test_stdin_fidelity_minimal_keys_and_fresh_homes(self):
        instruction = "single ' double \" dollar $(touch NEVER)\n雪\n"
        with tempfile.TemporaryDirectory() as directory:
            script = Path(directory) / "fake-cli"
            script.write_text('#!/bin/bash\ntest -z "$AWS_SECRET_ACCESS_KEY" || exit 9\ncat\n', encoding="utf-8")
            script.chmod(0o700)
            command, env = isolated_command(f'printf "%s" "$INSTRUCTION" | "{script}"',
                                            {"INSTRUCTION": instruction, "DEEPSEEK_API_KEY": "fake-secret"}, directory)
            result = subprocess.run(["/bin/bash", "-c", command], env={**os.environ, **env, "AWS_SECRET_ACCESS_KEY": "ambient"},
                                    text=True, capture_output=True, check=True)
            self.assertEqual(result.stdout, instruction)
            self.assertNotIn("fake-secret", command)
            self.assertNotEqual(private_home("bonsai"), private_home("bonsai"))
            self.assertNotEqual(private_home("dsh"), private_home("dsh"))

    def test_redaction_before_publication(self):
        self.assertEqual(redact("key=fake-secret", ("fake-secret",)), "key=[REDACTED]")


if __name__ == "__main__":
    unittest.main()
