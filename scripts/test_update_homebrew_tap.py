#!/usr/bin/env python3
"""Fixture tests for update-homebrew-tap.py."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("update-homebrew-tap.py")
FIXTURES = Path(__file__).with_name("fixtures") / "homebrew"
SPEC = importlib.util.spec_from_file_location("update_homebrew_tap", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
UPDATER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = UPDATER
SPEC.loader.exec_module(UPDATER)


class UpdateHomebrewTapTests(unittest.TestCase):
    def test_rewrites_only_platform_urls_and_checksums(self) -> None:
        manifest = json.loads((FIXTURES / "release-manifest.json").read_text(encoding="utf-8"))
        formula = (FIXTURES / "bonsai.rb").read_text(encoding="utf-8")

        updated = UPDATER.update_formula(
            formula, "v1.2.3", UPDATER.parse_assets(manifest, "v1.2.3")
        )

        self.assertEqual(updated.count("releases/download/v1.2.3/"), 4)
        for digit in "1234":
            self.assertIn(f'sha256 "{digit * 64}"', updated)
        self.assertIn('bin.install "bonsai"', updated)
        self.assertIn('assert_match version.to_s, shell_output("#{bin}/bonsai --version")', updated)
        self.assertNotIn("v0.1.0", updated)

    def test_rejects_missing_target(self) -> None:
        manifest = json.loads((FIXTURES / "release-manifest.json").read_text(encoding="utf-8"))
        manifest["assets"].pop()

        with self.assertRaisesRegex(ValueError, "missing: x86_64-unknown-linux-gnu"):
            UPDATER.parse_assets(manifest, "v1.2.3")

    def test_rejects_archive_name_that_can_inject_formula(self) -> None:
        manifest = json.loads(
            (FIXTURES / "release-manifest.json").read_text(encoding="utf-8")
        )
        manifest["assets"][0]["archive"] = 'archive.tar.gz"\n  system "bad"'

        with self.assertRaisesRegex(ValueError, "archive must be"):
            UPDATER.parse_assets(manifest, "v1.2.3")

    def test_rejects_asset_without_a_supported_target(self) -> None:
        manifest = json.loads(
            (FIXTURES / "release-manifest.json").read_text(encoding="utf-8")
        )
        manifest["assets"].append({"target": None})

        with self.assertRaisesRegex(ValueError, "unsupported target: None"):
            UPDATER.parse_assets(manifest, "v1.2.3")

    def test_cli_updates_tap_checkout(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            tap = Path(temporary)
            (tap / "Formula").mkdir()
            shutil.copyfile(FIXTURES / "bonsai.rb", tap / "Formula" / "bonsai.rb")

            result = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "v1.2.3",
                    str(tap),
                    "--manifest",
                    str(FIXTURES / "release-manifest.json"),
                ],
                text=True,
                capture_output=True,
                check=False,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("bonsai-v1.2.3-x86_64-unknown-linux-gnu.tar.gz", (tap / "Formula" / "bonsai.rb").read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
