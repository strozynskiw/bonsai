"""Offline regressions for provider-scoped live-model audit coverage."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("model_audit", Path(__file__).with_name("audit.py"))
audit = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(audit)


class LiveModelCoverageTests(unittest.TestCase):
    def audit_fixture(self, target_owner=None, exclusion_owner=None, scoped=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            builtin = root / "models" / "builtin"
            builtin.mkdir(parents=True)
            live = root / "live"
            live.mkdir()
            metadata = {}
            for connection in ["alpha", "beta"]:
                name = f"{connection}-model"
                text = (
                    f'[[targets]]\nconnection = "{connection}"\n'
                    f'model = "{connection}/{name}"\ncontext_window = 1000\n'
                )
                rows = {name: {"limit": {"context": 1000}}}
                if target_owner == connection:
                    text += (
                        f'\n[[targets]]\nconnection = "{connection}"\n'
                        f'model = "{connection}/shared-model"\ncontext_window = 1000\n'
                    )
                    rows["shared-model"] = {"limit": {"context": 1000}}
                if exclusion_owner == connection:
                    text += "\n# not-shipped: shared-model — unsupported on this connection\n"
                (builtin / f"{connection}.toml").write_text(text, encoding="utf-8")
                metadata[connection] = {"models": rows}
            source = root / "models-dev.json"
            source.write_text(json.dumps(metadata), encoding="utf-8")
            (live / "alpha.json").write_text(json.dumps({"models": [
                {"remote_model_id": "shared-model", "model_id": None},
            ]}), encoding="utf-8")
            arguments = ["audit.py", *(["alpha"] if scoped else []), "--source", str(source)]
            output = io.StringIO()
            with patch.multiple(audit, REPO=str(root), BUILTIN_DIR=str(builtin),
                                LIVE_DIR=str(live), ALL_TOMLS=["alpha", "beta"]):
                with patch("sys.argv", arguments), contextlib.redirect_stdout(output):
                    status = audit.main()
            return status, output.getvalue()

    def test_foreign_target_does_not_hide_an_unmapped_model(self):
        for scoped in [False, True]:
            with self.subTest(scoped=scoped):
                status, output = self.audit_fixture(target_owner="beta", scoped=scoped)
                self.assertEqual(status, 1, output)
                self.assertIn("- `alpha`: shared-model\n", output)
                self.assertNotIn("target exists — stale cache", output)

    def test_foreign_exclusion_does_not_hide_an_unmapped_model(self):
        status, output = self.audit_fixture(exclusion_owner="beta")
        self.assertEqual(status, 1, output)
        self.assertIn("- `alpha`: shared-model\n", output)
        self.assertNotIn("deliberately not shipped", output)

    def test_own_target_identifies_a_stale_mapping(self):
        status, output = self.audit_fixture(target_owner="alpha")
        self.assertEqual(status, 0, output)
        self.assertIn("shared-model (target exists — stale cache)", output)

    def test_own_exclusion_is_honored(self):
        status, output = self.audit_fixture(exclusion_owner="alpha")
        self.assertEqual(status, 0, output)
        self.assertIn("shared-model (deliberately not shipped)", output)


if __name__ == "__main__":
    unittest.main()
