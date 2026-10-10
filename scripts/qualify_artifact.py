#!/usr/bin/env python3
"""Fail-closed native qualification of one packed release artifact (stdlib only)."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import tarfile
import tempfile
import time

TARGETS = {
    "x86_64-unknown-linux-gnu": ("Linux", "x86_64"),
    "aarch64-unknown-linux-gnu": ("Linux", "aarch64"),
    "x86_64-apple-darwin": ("Darwin", "x86_64"),
    "aarch64-apple-darwin": ("Darwin", "arm64"),
}
REQUIRED = {"artifact-surface", "inline", "sandbox-probes", "release-eval", "continuity-eval"}
# The native probe names are shared verbatim by the Linux `bubblewrap` and macOS
# `seatbelt` modules in `src/sandbox/tests.rs`, so one target-independent set is
# correct: only the module for the running target is compiled, and a target whose
# backend cannot run them reports the names as missing (fail closed) rather than
# passing on the other platform's evidence.
REQUIRED_TESTS = {
    "artifact-surface": (
        "artifact_confines_native_writes_and_network",
        "artifact_denies_noninteractive_mutation_and_project_escapes",
        "artifact_frames_untrusted_tool_result_as_data",
        "artifact_startup_upgrades_supported_store_and_preserves_failures",
        "artifact_malformed_stream_never_reports_completed",
        "artifact_tool_loop_exhaustion_is_bounded",
        "invalid_surface_binary_override_never_falls_back",
    ),
    "sandbox-probes": (
        "blocks_write_outside_project_root",
        "denies_network_when_configured",
        "escape_runs_what_confinement_blocks",
    ),
}


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def run_check(command: list[str], env: dict[str, str], timeout: float, log: Path) -> dict:
    """Reap the entire process group on failure/timeout; raw logs stay private."""
    started = time.monotonic()
    with log.open("wb") as output:
        process = subprocess.Popen(command, env=env, stdout=output, stderr=output, start_new_session=True)
        try:
            code = process.wait(timeout=timeout)
            reason = "passed" if code == 0 else "nonzero exit"
        except subprocess.TimeoutExpired:
            code, reason = -1, "timeout"
        finally:
            # Also terminate detached descendants left behind by a successful parent.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
    return {"status": "passed" if code == 0 else "failed", "reason": reason,
            "exit_code": code, "seconds": round(time.monotonic() - started, 3)}


def extract_binary(archive: Path, directory: Path) -> Path:
    """Extract the single `bonsai` payload; nothing else in the archive is trusted."""
    with tarfile.open(archive, "r:gz") as packed:
        members = packed.getmembers()
        # macOS bsdtar packs AppleDouble `._bonsai` metadata alongside the payload,
        # so that companion member is the only other name tolerated — and it is
        # never extracted.
        names = {member.name.removeprefix("./") for member in members}
        if "bonsai" not in names or not names <= {"bonsai", "._bonsai"}:
            raise ValueError("archive must contain exactly one regular bonsai executable")
        payload = [member for member in members if member.name.removeprefix("./") == "bonsai"]
        if len(payload) != 1 or not payload[0].isfile():
            raise ValueError("archive must contain exactly one regular bonsai executable")
        if payload[0].size > 1024 * 1024 * 1024:
            raise ValueError("artifact exceeds size bound")
        packed.extract(payload[0], directory, filter="data")
    binary = directory / "bonsai"
    if not os.access(binary, os.X_OK):
        raise ValueError("artifact is not executable")
    return binary


def record_test_count(result: dict, log: Path, required_tests: tuple[str, ...] = ()) -> None:
    """Only fixed test counters escape private logs; zero tests is not success."""
    passed_tests = 0
    skipped = False
    named = set()
    with log.open(encoding="utf-8", errors="replace") as stream:
        for line in stream:
            match = re.search(r"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored", line)
            if match:
                passed_tests += int(match[1])
            skipped |= "skipping Bubblewrap integration test" in line
            ok = re.match(r"test ([\w:]+) \.\.\.", line.strip())
            if ok and "ignored" not in line:
                named.add(ok[1].rsplit("::", 1)[-1])
    result["passed_tests"] = passed_tests
    missing = sorted(name for name in required_tests if name not in named)
    result["missing_required_tests"] = missing
    if passed_tests == 0 or skipped or missing:
        result.update(status="failed", reason="required probes absent or skipped")


def validate_results(results: dict) -> None:
    if set(results) != REQUIRED or any(item["status"] != "passed" for item in results.values()):
        raise ValueError("required qualification results missing or failed")


def qualify(args: argparse.Namespace) -> int:
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    report = {"schema_version": 1, "source_revision": args.revision, "target": args.target,
              "runner": platform.platform(), "backend": "seatbelt" if platform.system() == "Darwin" else "bubblewrap",
              "status": "failed", "checks": {}}
    try:
        if TARGETS.get(args.target) != (platform.system(), platform.machine()):
            raise ValueError("qualification requires a matching native runner")
        revision = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
        if revision != args.revision:
            raise ValueError("source revision mismatch")
        report["toolchain"] = subprocess.check_output(["rustc", "--version"], text=True).strip()
        report["archive_sha256"] = sha256(args.archive)
        expected_archive = args.archive.with_name(args.archive.name + ".sha256").read_text().split()[0]
        expected_binary = args.archive.with_name(args.archive.name + ".binary-sha256").read_text().strip()
        if expected_archive != report["archive_sha256"]:
            raise ValueError("archive identity mismatch")
        with tempfile.TemporaryDirectory(prefix="bonsai-qualification-") as temporary:
            root = Path(temporary)
            binary = extract_binary(args.archive, root / "artifact")
            report["binary_sha256"] = sha256(binary)
            if expected_binary != report["binary_sha256"]:
                raise ValueError("binary identity mismatch")
            env = os.environ.copy()
            # Cargo needs the developer toolchain, but artifact children/evals must not
            # inherit provider secrets, keyrings or developer state.
            artifact_env = {key: env[key] for key in ("PATH", "LANG", "MACOSX_DEPLOYMENT_TARGET") if key in env}
            home = root / "home"
            home.mkdir()
            artifact_env.update(HOME=str(home), BONSAI_HOME=str(home / "state"), BONSAI_DOTENV="0",
                                BONSAI_DISABLE_KEYRING="1", BONSAI_DISABLE_MODELS_FETCH="1",
                                BONSAI_MEMORY_EMBEDDINGS="off", BONSAI_EPISODES="0")
            env.update(BONSAI_SURFACE_BINARY=str(binary), BONSAI_REQUIRE_NATIVE_SANDBOX="1")
            cargo = ["cargo", "test", "--release", "--locked", "--target", args.target]
            checks = [
                ("artifact-surface", cargo + ["--test", "surface_smoke", "--", "--test-threads=1"], env),
                ("inline", cargo + ["--bin", "bonsai"], env),
                ("sandbox-probes", cargo + ["--bin", "bonsai", "sandbox::tests::", "--", "--test-threads=1"], env),
                ("release-eval", [str(binary), "eval", "--mode", "mock", "--suite", "eval/suites/release_gating.toml", "--baseline", "eval/baselines/release-v1.toml", "--fail-on-task-failure"], artifact_env),
                ("continuity-eval", [str(binary), "eval", "--mode", "mock", "--suite", "eval/suites/intent_continuity.toml", "--fail-on-task-failure"], artifact_env),
            ]
            for name, command, child_env in checks:
                log = root / f"{name}.log"
                result = run_check(command, child_env, args.timeout, log)
                if name in {"artifact-surface", "inline", "sandbox-probes"}:
                    record_test_count(result, log, REQUIRED_TESTS.get(name, ()))
                report["checks"][name] = result
            if sha256(binary) != report["binary_sha256"] or sha256(args.archive) != report["archive_sha256"]:
                raise ValueError("artifact identity changed during qualification")
            validate_results(report["checks"])
            report["status"] = "passed"
    except (OSError, ValueError, subprocess.SubprocessError, tarfile.TarError) as error:
        # Never publish exception text: subprocess errors can contain credentials
        # or raw wire data. Fixed reasons plus check exit codes are allowlisted.
        report["failure_reason"] = str(error) if isinstance(error, ValueError) else type(error).__name__
    (evidence / "qualification.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"qualification: {report['status']} ({args.target})")
    return 0 if report["status"] == "passed" else 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=TARGETS)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=1800)
    args = parser.parse_args()
    if not 0 < args.timeout < float("inf"):
        parser.error("timeout must be positive")
    return qualify(args)


if __name__ == "__main__":
    raise SystemExit(main())
