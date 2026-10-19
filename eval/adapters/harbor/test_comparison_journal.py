from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from .comparison_journal import FileJournal
from .comparison_manifest import digest
from .comparison_schedule import Trial


class JournalTests(unittest.TestCase):
    def test_interrupted_staging_never_publishes_a_partial_attempt(self):
        trial = Trial("task", 1, "bonsai", "smoke")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with FileJournal(root) as journal:
                with patch.object(journal, "_write_new", side_effect=OSError("interrupted write")):
                    with self.assertRaises(OSError):
                        journal.begin(trial, digest("manifest"))
                self.assertEqual(journal.records(), [])
            with FileJournal(root) as resumed:
                resumed.begin(trial, digest("manifest"))
                self.assertEqual(resumed.records()[0]["identity"], trial.identity)
                self.assertEqual(resumed.records()[0]["category"], "infrastructure_invalid")

    def test_exclusive_lock_and_crash_reservation_never_reruns(self):
        trial = Trial("task", 1, "bonsai", "smoke")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with FileJournal(root) as journal:
                with self.assertRaises(OSError):
                    with FileJournal(root):
                        pass
                journal.begin(trial, digest("manifest"))
            with FileJournal(root) as resumed:
                record = resumed.records()[0]
                self.assertEqual(record["identity"], trial.identity)
                self.assertEqual(record["category"], "infrastructure_invalid")
                self.assertIsNone(record["provider_billed_usd"])
                with self.assertRaises(FileExistsError):
                    resumed.begin(trial, digest("manifest"))
                with self.assertRaises(ValueError):
                    resumed.finish(trial, {"manifest_digest": digest("different")})
                resumed.finish(trial, {"category": "verified", "provider_billed_usd": 0})
                self.assertEqual(resumed.records()[0]["category"], "verified")
                with self.assertRaises(FileExistsError):
                    resumed.finish(trial, {"category": "verified"})


if __name__ == "__main__":
    unittest.main()
