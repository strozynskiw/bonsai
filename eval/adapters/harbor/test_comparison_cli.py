import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest

from .comparison import main
from .comparison_manifest import freeze
from .test_comparison_manifest import manifest_fixture


class DriverTests(unittest.TestCase):
    def test_check_dry_run_report_and_live_are_separate(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps(freeze(manifest_fixture())), encoding="utf-8")
            records = root / "records.json"
            records.write_text("[]", encoding="utf-8")
            common = ["--manifest", str(manifest)]
            live = ["--task-root", str(root), "--journal", str(root / "journal"),
                    "--raw-output", str(root / "raw"), "--billing", str(root / "billing"),
                    "--evidence", str(root / "evidence")]
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(main(["check", *common]), 0)
                self.assertEqual(main(["dry-run", *common, "--records", str(records), "--phase", "pilot"]), 0)
            self.assertEqual(main(["report", *common, "--records", str(records), "--out", str(root / "report")]), 0)
            report = json.loads((root / "report/comparison.json").read_text(encoding="utf-8"))
            self.assertEqual(len(report["missing_trials"]), 60)
            self.assertEqual(report["complete_task_pairs"], 0)
            approval = root / "approval.json"
            approval.write_text("{}", encoding="utf-8")
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(main(["live", *common, *live, "--records", str(records), "--phase", "smoke", "--approval", str(approval)]), 2)
                self.assertEqual(main(["live", *common, *live, "--records", str(records), "--phase", "smoke", "--approval", str(approval), "--execute"]), 2)


if __name__ == "__main__":
    unittest.main()
