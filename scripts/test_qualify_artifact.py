#!/usr/bin/env python3
"""Qualification runner negative controls; no provider or native backend needed."""
import argparse
import io
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import qualify_artifact as gate


class QualificationTests(unittest.TestCase):
    def test_required_results_fail_closed(self):
        with self.assertRaises(ValueError):
            gate.validate_results({})
        results = {name: {"status": "passed"} for name in gate.REQUIRED}
        gate.validate_results(results)
        results["sandbox-probes"]["status"] = "failed"
        with self.assertRaises(ValueError):
            gate.validate_results(results)

    def test_child_failure_and_timeout(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "private.log"
            self.assertEqual(gate.run_check(["/bin/sh", "-c", "exit 7"], os.environ.copy(), 2, log)["exit_code"], 7)
            result = gate.run_check(["/bin/sh", "-c", "sleep 60 & wait"], os.environ.copy(), .1, log)
            self.assertEqual(result["reason"], "timeout")
            self.assertEqual(result["status"], "failed")

    def test_missing_or_skipped_native_results_fail(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "private.log"
            for text in ("test result: ok. 0 passed; 0 failed; 0 ignored", "skipping Bubblewrap integration test: bwrap unavailable\ntest result: ok. 1 passed; 0 failed; 0 ignored"):
                log.write_text(text)
                result = {"status": "passed"}
                gate.record_test_count(result, log)
                self.assertEqual(result["status"], "failed")

    def test_missing_required_native_tests_fail(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "private.log"
            log.write_text("test artifact_confines_native_writes_and_network ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored")
            result = {"status": "passed"}
            gate.record_test_count(
                result,
                log,
                ("artifact_confines_native_writes_and_network", "artifact_malformed_stream_never_reports_completed"),
            )
            self.assertEqual(result["status"], "failed")
            self.assertEqual(result["missing_required_tests"], ["artifact_malformed_stream_never_reports_completed"])
            log.write_text("test artifact_malformed_stream_never_reports_completed ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored")
            result = {"status": "passed"}
            gate.record_test_count(result, log, ("artifact_malformed_stream_never_reports_completed",))
            self.assertEqual(result["status"], "passed")

    def test_archive_rejects_links_and_traversal(self):
        for name, kind in [
            ("../bonsai", tarfile.REGTYPE),
            ("bonsai", tarfile.SYMTYPE),
            ("other", tarfile.REGTYPE),
            ("./.hidden", tarfile.REGTYPE),
        ]:
            with tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                archive = root / "candidate.tar.gz"
                with tarfile.open(archive, "w:gz") as packed:
                    payload = tarfile.TarInfo("bonsai")
                    payload.size, payload.mode = 4, 0o755
                    packed.addfile(payload, io.BytesIO(b"true"))
                    member = tarfile.TarInfo(name)
                    member.type = kind
                    packed.addfile(member)
                with self.assertRaises(ValueError):
                    gate.extract_binary(archive, root / "out")

    def test_archive_accepts_macos_metadata_companion(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "candidate.tar.gz"
            with tarfile.open(archive, "w:gz") as packed:
                payload = tarfile.TarInfo("bonsai")
                payload.size, payload.mode = 4, 0o755
                packed.addfile(payload, io.BytesIO(b"true"))
                metadata = tarfile.TarInfo("._bonsai")
                metadata.size = 4
                packed.addfile(metadata, io.BytesIO(b"meta"))
            binary = gate.extract_binary(archive, root / "out")
            self.assertEqual(binary, root / "out" / "bonsai")
            self.assertEqual(list((root / "out").iterdir()), [binary])

    def test_identity_failures_and_evidence_allowlist(self):
        for mismatch in ("archive", "binary"):
            with tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                archive = root / "candidate.tar.gz"
                secret = b"sk-qualification-synthetic-secret-that-must-not-be-uploaded"
                with tarfile.open(archive, "w:gz") as packed:
                    member = tarfile.TarInfo("bonsai")
                    member.size, member.mode = len(secret), 0o755
                    packed.addfile(member, io.BytesIO(secret))
                Path(str(archive) + ".sha256").write_text("bad" if mismatch == "archive" else gate.sha256(archive))
                Path(str(archive) + ".binary-sha256").write_text("bad")
                args = argparse.Namespace(target="aarch64-apple-darwin", revision="revision", archive=archive, evidence=root / "evidence", timeout=1)
                with patch.object(gate.platform, "platform", return_value="fixture-runner"), patch.object(gate.platform, "system", return_value="Darwin"), patch.object(gate.platform, "machine", return_value="arm64"), patch.object(gate.subprocess, "check_output", side_effect=["revision", "rustc fixture"]):
                    self.assertEqual(gate.qualify(args), 1)
                report_text = (args.evidence / "qualification.json").read_text()
                self.assertNotIn(secret.decode(), report_text)
                report = json.loads(report_text)
                self.assertEqual(report["failure_reason"], f"{mismatch} identity mismatch")
                self.assertEqual(list(args.evidence.iterdir()), [args.evidence / "qualification.json"])


if __name__ == "__main__":
    unittest.main()
