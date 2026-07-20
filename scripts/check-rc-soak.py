#!/usr/bin/env python3
"""Validate the immutable evidence for a Bonsai release-candidate soak."""

from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
from datetime import datetime, timedelta, timezone
from typing import Any, Iterable, Mapping, Sequence


SCHEMA_VERSION = 1
RECORD_SCHEMA = (
    "https://raw.githubusercontent.com/strozynskiw/bonsai/master/"
    "release/soak/rc-soak.schema.json"
)
SUPPORTED_TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
)
SUPPORTED_TARGET_SET = frozenset(SUPPORTED_TARGETS)
RUNNER_CLASS_BY_TARGET = {
    "x86_64-unknown-linux-gnu": "ubuntu-22.04",
    "aarch64-unknown-linux-gnu": "ubuntu-22.04-arm",
    "aarch64-apple-darwin": "macos-15",
    "x86_64-apple-darwin": "macos-15-intel",
}
SURFACES = frozenset(("headless", "tui"))
INCIDENT_TYPES = frozenset(
    ("data-loss", "security", "migration", "task-completion")
)
MINIMUM_SOAK_HOURS = 336
MINIMUM_OBSERVATION_DAYS = 14
MINIMUM_SAMPLES = 3
MAXIMUM_SAMPLES = 30
MAX_RECORD_BYTES = 1024 * 1024
MAX_MANIFEST_BYTES = 1024 * 1024
MAX_REPORT_BYTES = 16 * 1024 * 1024
MAX_ARCHIVE_MEMBERS = 8
MAX_BINARY_BYTES = 512 * 1024 * 1024
GIT_CLOCK_SKEW = timedelta(minutes=15)
OBSERVATION_COMMIT_GRACE = timedelta(hours=36)
UTC = timezone.utc

_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
_GIT_OID_RE = re.compile(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$")
_RC_TAG_RE = re.compile(
    r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-rc\.(0|[1-9][0-9]*)$"
)
_REPOSITORY_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
_RECORD_PATH_RE = re.compile(
    r"^release/soak/active/rc-(v(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)-rc\."
    r"(?:0|[1-9][0-9]*))\.json$"
)
_TIMESTAMP_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")


class ValidationError(Exception):
    """A stable validation failure suitable for machine-readable output."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


class UsageError(Exception):
    """Raised for invalid CLI arguments."""


class JsonArgumentParser(argparse.ArgumentParser):
    """Argument parser that reports usage errors through the JSON protocol."""

    def error(self, message: str) -> None:
        raise UsageError(message)


def _fail(code: str, message: str) -> None:
    raise ValidationError(code, message)


def _reject_constant(value: str) -> None:
    _fail("invalid_json_number", f"JSON contains non-finite number {value}")


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            _fail("duplicate_json_key", f"JSON object repeats key {key!r}")
        result[key] = value
    return result


def _load_json_bytes(raw: bytes, label: str) -> Any:
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        _fail("invalid_json_encoding", f"{label} is not valid UTF-8: {error}")
    try:
        return json.loads(
            text,
            object_pairs_hook=_unique_object,
            parse_constant=_reject_constant,
        )
    except ValidationError:
        raise
    except (json.JSONDecodeError, RecursionError) as error:
        _fail("invalid_json", f"{label} is not valid JSON: {error}")


def _read_bounded(path: Path, maximum: int, label: str) -> bytes:
    try:
        stat = path.stat()
    except OSError as error:
        _fail("missing_evidence", f"could not inspect {label}: {error}")
    if not path.is_file():
        _fail("missing_evidence", f"{label} is not a regular file")
    if stat.st_size > maximum:
        _fail("evidence_too_large", f"{label} exceeds the {maximum}-byte limit")
    try:
        return path.read_bytes()
    except OSError as error:
        _fail("missing_evidence", f"could not read {label}: {error}")


def _load_json_file(path: Path, maximum: int, label: str) -> tuple[Any, bytes]:
    raw = _read_bounded(path, maximum, label)
    return _load_json_bytes(raw, label), raw


def _expect_object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        _fail("invalid_shape", f"{label} must be a JSON object")
    return value


def _expect_array(value: Any, label: str) -> list[Any]:
    if not isinstance(value, list):
        _fail("invalid_shape", f"{label} must be a JSON array")
    return value


def _exact_keys(value: Mapping[str, Any], expected: Iterable[str], label: str) -> None:
    expected_set = frozenset(expected)
    actual = frozenset(value)
    if actual != expected_set:
        missing = sorted(expected_set - actual)
        extra = sorted(actual - expected_set)
        _fail(
            "invalid_shape",
            f"{label} has invalid fields (missing={missing}, extra={extra})",
        )


def _expect_string(value: Any, label: str) -> str:
    if not isinstance(value, str):
        _fail("invalid_shape", f"{label} must be a string")
    return value


def _expect_bool(value: Any, label: str) -> bool:
    if type(value) is not bool:
        _fail("invalid_shape", f"{label} must be a boolean")
    return value


def _expect_u64(value: Any, label: str) -> int:
    if type(value) is not int or not 0 <= value <= (1 << 64) - 1:
        _fail("invalid_shape", f"{label} must be an unsigned 64-bit integer")
    return value


def _expect_nonnegative_float(value: Any, label: str) -> float:
    if type(value) not in (int, float):
        _fail("invalid_shape", f"{label} must be a finite non-negative number")
    result = float(value)
    if not math.isfinite(result) or result < 0:
        _fail("invalid_shape", f"{label} must be a finite non-negative number")
    return result


def _expect_sha256(value: Any, label: str) -> str:
    digest = _expect_string(value, label)
    if not _SHA256_RE.fullmatch(digest):
        _fail("invalid_digest", f"{label} must be a lowercase SHA-256 digest")
    return digest


def _expect_git_oid(value: Any, label: str) -> str:
    oid = _expect_string(value, label)
    if not _GIT_OID_RE.fullmatch(oid):
        _fail("invalid_git_oid", f"{label} must be a lowercase Git object ID")
    return oid


def _parse_timestamp(value: Any, label: str) -> datetime:
    timestamp = _expect_string(value, label)
    if not _TIMESTAMP_RE.fullmatch(timestamp):
        _fail("invalid_timestamp", f"{label} must use YYYY-MM-DDTHH:MM:SSZ")
    try:
        return datetime.strptime(timestamp, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=UTC)
    except ValueError as error:
        _fail("invalid_timestamp", f"{label} is invalid: {error}")


def _parse_git_timestamp(value: str, label: str) -> datetime:
    try:
        parsed = datetime.fromisoformat(value)
    except ValueError as error:
        _fail("invalid_git_history", f"{label} has an invalid commit timestamp: {error}")
    if parsed.tzinfo is None:
        _fail("invalid_git_history", f"{label} has a timezone-less commit timestamp")
    return parsed.astimezone(UTC)


def _normalise_now(now: datetime | None) -> datetime:
    if now is None:
        return datetime.now(UTC)
    if now.tzinfo is None or now.utcoffset() is None:
        _fail("invalid_now", "injected current time must be timezone-aware")
    return now.astimezone(UTC)


def _canonical(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True)


def _validate_digest_map(value: Any, label: str) -> dict[str, str]:
    mapping = _expect_object(value, label)
    _exact_keys(mapping, SUPPORTED_TARGETS, label)
    return {
        target: _expect_sha256(mapping[target], f"{label}.{target}")
        for target in SUPPORTED_TARGETS
    }


def _validate_record(value: Any, label: str = "record") -> dict[str, Any]:
    record = _expect_object(value, label)
    _exact_keys(
        record,
        ("$schema", "schema_version", "release", "soak", "observations", "incidents"),
        label,
    )
    if record["$schema"] != RECORD_SCHEMA:
        _fail("invalid_schema", f"{label}.$schema does not identify the RC soak schema")
    if _expect_u64(record["schema_version"], f"{label}.schema_version") != SCHEMA_VERSION:
        _fail("invalid_schema", f"{label}.schema_version is not supported")

    release = _expect_object(record["release"], f"{label}.release")
    _exact_keys(
        release,
        (
            "tag",
            "peeled_commit",
            "manifest_sha256",
            "binary_sha256_by_target",
            "performance_report_sha256_by_target",
        ),
        f"{label}.release",
    )
    tag = _expect_string(release["tag"], f"{label}.release.tag")
    if not _RC_TAG_RE.fullmatch(tag):
        _fail("invalid_release_tag", f"{label}.release.tag must be a strict semver RC tag")
    _expect_git_oid(release["peeled_commit"], f"{label}.release.peeled_commit")
    _expect_sha256(release["manifest_sha256"], f"{label}.release.manifest_sha256")
    _validate_digest_map(
        release["binary_sha256_by_target"],
        f"{label}.release.binary_sha256_by_target",
    )
    _validate_digest_map(
        release["performance_report_sha256_by_target"],
        f"{label}.release.performance_report_sha256_by_target",
    )

    soak = _expect_object(record["soak"], f"{label}.soak")
    _exact_keys(soak, ("started_at", "minimum_hours"), f"{label}.soak")
    started_at = _parse_timestamp(soak["started_at"], f"{label}.soak.started_at")
    minimum_hours = _expect_u64(soak["minimum_hours"], f"{label}.soak.minimum_hours")
    if minimum_hours < MINIMUM_SOAK_HOURS:
        _fail(
            "minimum_duration_weakened",
            f"{label}.soak.minimum_hours must be at least {MINIMUM_SOAK_HOURS}",
        )

    observations = _expect_array(record["observations"], f"{label}.observations")
    seen_observations: set[str] = set()
    for index, raw_observation in enumerate(observations):
        observation_label = f"{label}.observations[{index}]"
        observation = _expect_object(raw_observation, observation_label)
        _exact_keys(
            observation,
            ("timestamp", "target", "binary_sha256", "surface", "result"),
            observation_label,
        )
        timestamp = _parse_timestamp(observation["timestamp"], f"{observation_label}.timestamp")
        if timestamp < started_at:
            _fail("invalid_observation_time", f"{observation_label} predates the claimed soak")
        target = _expect_string(observation["target"], f"{observation_label}.target")
        if target not in SUPPORTED_TARGET_SET:
            _fail("invalid_target", f"{observation_label}.target is not supported")
        binary_sha = _expect_sha256(
            observation["binary_sha256"], f"{observation_label}.binary_sha256"
        )
        expected_binary = release["binary_sha256_by_target"][target]
        if binary_sha != expected_binary:
            _fail("binary_identity_mismatch", f"{observation_label} does not pin its target binary")
        surface = _expect_string(observation["surface"], f"{observation_label}.surface")
        if surface not in SURFACES:
            _fail("invalid_surface", f"{observation_label}.surface is not supported")
        result = _expect_string(observation["result"], f"{observation_label}.result")
        if result not in ("passed", "failed"):
            _fail("invalid_result", f"{observation_label}.result must be passed or failed")
        canonical = _canonical(observation)
        if canonical in seen_observations:
            _fail("duplicate_observation", f"{observation_label} duplicates an earlier observation")
        seen_observations.add(canonical)

    incidents = _expect_array(record["incidents"], f"{label}.incidents")
    incident_ids: set[str] = set()
    for index, raw_incident in enumerate(incidents):
        incident_label = f"{label}.incidents[{index}]"
        incident = _expect_object(raw_incident, incident_label)
        _exact_keys(
            incident,
            ("id", "timestamp", "type", "release_blocking", "resolved_at"),
            incident_label,
        )
        incident_id = _expect_string(incident["id"], f"{incident_label}.id")
        if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,63}", incident_id):
            _fail("invalid_incident_id", f"{incident_label}.id is invalid")
        if incident_id in incident_ids:
            _fail("duplicate_incident", f"{incident_label}.id is not unique")
        incident_ids.add(incident_id)
        timestamp = _parse_timestamp(incident["timestamp"], f"{incident_label}.timestamp")
        if timestamp < started_at:
            _fail("invalid_incident_time", f"{incident_label} predates the claimed soak")
        incident_type = _expect_string(incident["type"], f"{incident_label}.type")
        if incident_type not in INCIDENT_TYPES:
            _fail("invalid_incident_type", f"{incident_label}.type is not supported")
        _expect_bool(incident["release_blocking"], f"{incident_label}.release_blocking")
        resolved_at = incident["resolved_at"]
        if resolved_at is not None:
            resolved = _parse_timestamp(resolved_at, f"{incident_label}.resolved_at")
            if resolved < timestamp:
                _fail("invalid_incident_time", f"{incident_label}.resolved_at predates the incident")
    return record


def _run_git(repo: Path, args: Sequence[str], *, text: bool = True) -> str | bytes:
    try:
        completed = subprocess.run(
            ["git", "-C", os.fspath(repo), *args],
            check=False,
            capture_output=True,
            text=text,
        )
    except OSError as error:
        _fail("git_unavailable", f"could not execute Git: {error}")
    if completed.returncode != 0:
        _fail("invalid_git_history", f"Git rejected {' '.join(args[:2])}")
    return completed.stdout


def _record_repo_and_path(record_path: Path) -> tuple[Path, str]:
    resolved = record_path.resolve()
    root_output = _run_git(resolved.parent, ("rev-parse", "--show-toplevel"))
    assert isinstance(root_output, str)
    repo = Path(root_output.strip()).resolve()
    try:
        relative = resolved.relative_to(repo).as_posix()
    except ValueError:
        _fail("invalid_record_path", "record must be inside its Git repository")
    match = _RECORD_PATH_RE.fullmatch(relative)
    if match is None:
        _fail(
            "invalid_record_path",
            "active record must be release/soak/active/rc-v<semver>-rc.<n>.json",
        )
    return repo, relative


def _history_versions(
    record_path: Path, current_raw: bytes, current_record: dict[str, Any], now: datetime
) -> tuple[datetime, dict[str, datetime], set[str], bool]:
    repo, relative = _record_repo_and_path(record_path)
    path_tag = _RECORD_PATH_RE.fullmatch(relative)
    assert path_tag is not None
    if path_tag.group(1) != current_record["release"]["tag"]:
        _fail("record_tag_mismatch", "record filename does not match release.tag")

    _run_git(repo, ("ls-files", "--error-unmatch", "--", relative))
    head_raw = _run_git(repo, ("show", f"HEAD:{relative}"), text=False)
    assert isinstance(head_raw, bytes)
    if head_raw != current_raw:
        _fail("uncommitted_record", "active record bytes must exactly match HEAD")

    additions = _run_git(
        repo,
        ("log", "--format=%H", "--diff-filter=A", "--", relative),
    )
    assert isinstance(additions, str)
    add_commits = [line for line in additions.splitlines() if line]
    if len(add_commits) != 1:
        _fail("invalid_git_history", "active record must have exactly one creation commit")

    log_output = _run_git(
        repo,
        ("log", "--reverse", "--format=%H%x00%cI", "--", relative),
    )
    assert isinstance(log_output, str)
    entries: list[tuple[str, datetime]] = []
    for line in log_output.splitlines():
        try:
            commit, timestamp = line.split("\0", 1)
        except ValueError:
            _fail("invalid_git_history", "could not parse active-record history")
        commit_time = _parse_git_timestamp(timestamp, commit)
        if commit_time > now + GIT_CLOCK_SKEW:
            _fail("future_git_history", "active-record history contains a future commit")
        entries.append((commit, commit_time))
    if not entries:
        _fail("invalid_git_history", "active record has no Git history")

    versions: list[tuple[dict[str, Any], datetime]] = []
    first_seen_observations: dict[str, datetime] = {}
    historical_blocking_ids: set[str] = set()
    for commit, commit_time in entries:
        raw = _run_git(repo, ("show", f"{commit}:{relative}"), text=False)
        assert isinstance(raw, bytes)
        if len(raw) > MAX_RECORD_BYTES:
            _fail("evidence_too_large", "a historical active record exceeds the size limit")
        version = _validate_record(_load_json_bytes(raw, f"record at {commit}"), "record")
        versions.append((version, commit_time))
        for observation in version["observations"]:
            first_seen_observations.setdefault(_canonical(observation), commit_time)
        for incident in version["incidents"]:
            if incident["release_blocking"]:
                historical_blocking_ids.add(incident["id"])

    if versions[-1][0] != current_record:
        _fail("invalid_git_history", "HEAD record does not match the final parsed history version")

    immutable_keys = ("$schema", "schema_version", "release", "soak")
    immutable = {key: versions[0][0][key] for key in immutable_keys}
    history_rewritten = False
    previous = versions[0][0]
    for version, _ in versions[1:]:
        if {key: version[key] for key in immutable_keys} != immutable:
            _fail("immutable_record_changed", "immutable release or soak identity changed in Git history")
        for field in ("observations", "incidents"):
            prior_items = previous[field]
            if len(version[field]) < len(prior_items) or version[field][: len(prior_items)] != prior_items:
                history_rewritten = True
        previous = version
    if history_rewritten and not historical_blocking_ids:
        _fail("record_history_rewrite", "observations and incidents must be strict append-only prefixes")

    release_tag = current_record["release"]["tag"]
    peeled_output = _run_git(repo, ("rev-parse", f"refs/tags/{release_tag}^{{commit}}"))
    assert isinstance(peeled_output, str)
    if peeled_output.strip() != current_record["release"]["peeled_commit"]:
        _fail("moved_release_tag", "release tag no longer peels to the pinned commit")
    return entries[0][1], first_seen_observations, historical_blocking_ids, history_rewritten


def _verify_manifest_signature(
    manifest_path: Path, signature_path: Path, public_key_path: Path
) -> None:
    encoded = _read_bounded(signature_path, 4096, "release manifest signature")
    try:
        signature = base64.b64decode(encoded.strip(), validate=True)
    except (binascii.Error, ValueError):
        _fail("invalid_manifest_signature", "manifest signature is not strict base64")
    if len(signature) != 64:
        _fail("invalid_manifest_signature", "manifest signature must decode to 64 bytes")
    _read_bounded(public_key_path, 64 * 1024, "release public key")
    try:
        with tempfile.NamedTemporaryFile() as signature_file:
            signature_file.write(signature)
            signature_file.flush()
            completed = subprocess.run(
                [
                    "openssl",
                    "pkeyutl",
                    "-verify",
                    "-rawin",
                    "-pubin",
                    "-inkey",
                    os.fspath(public_key_path),
                    "-in",
                    os.fspath(manifest_path),
                    "-sigfile",
                    signature_file.name,
                ],
                check=False,
                capture_output=True,
            )
    except OSError as error:
        _fail("signature_verifier_unavailable", f"could not execute OpenSSL: {error}")
    if completed.returncode != 0:
        _fail("invalid_manifest_signature", "manifest signature verification failed")


def _validate_manifest(
    value: Any,
    record: dict[str, Any],
) -> dict[str, dict[str, str]]:
    manifest = _expect_object(value, "release manifest")
    _exact_keys(
        manifest,
        ("assets", "repository", "schema_version", "tag", "version"),
        "release manifest",
    )
    if _expect_u64(manifest["schema_version"], "release manifest.schema_version") != 1:
        _fail("invalid_manifest", "release manifest schema version is unsupported")
    tag = _expect_string(manifest["tag"], "release manifest.tag")
    if tag != record["release"]["tag"]:
        _fail("manifest_identity_mismatch", "release manifest tag does not match the record")
    if manifest["version"] != tag[1:]:
        _fail("invalid_manifest", "release manifest version does not match its tag")
    repository = _expect_string(manifest["repository"], "release manifest.repository")
    if not _REPOSITORY_RE.fullmatch(repository) or repository != "strozynskiw/bonsai":
        _fail("manifest_identity_mismatch", "release manifest repository is not strozynskiw/bonsai")

    assets = _expect_array(manifest["assets"], "release manifest.assets")
    if len(assets) != len(SUPPORTED_TARGETS):
        _fail("target_set_mismatch", "release manifest must contain exactly four target assets")
    by_target: dict[str, dict[str, str]] = {}
    for index, raw_asset in enumerate(assets):
        label = f"release manifest.assets[{index}]"
        asset = _expect_object(raw_asset, label)
        _exact_keys(asset, ("archive", "archive_sha256", "binary_sha256", "target"), label)
        target = _expect_string(asset["target"], f"{label}.target")
        if target not in SUPPORTED_TARGET_SET or target in by_target:
            _fail("target_set_mismatch", "release manifest targets must be the exact supported set")
        expected_archive = f"bonsai-{tag}-{target}.tar.gz"
        if asset["archive"] != expected_archive:
            _fail("invalid_manifest", f"{label}.archive is not the canonical release asset name")
        archive_sha = _expect_sha256(asset["archive_sha256"], f"{label}.archive_sha256")
        binary_sha = _expect_sha256(asset["binary_sha256"], f"{label}.binary_sha256")
        if binary_sha != record["release"]["binary_sha256_by_target"][target]:
            _fail("binary_identity_mismatch", f"{label} disagrees with the record binary digest")
        by_target[target] = {
            "archive": expected_archive,
            "archive_sha256": archive_sha,
            "binary_sha256": binary_sha,
        }
    if frozenset(by_target) != SUPPORTED_TARGET_SET:
        _fail("target_set_mismatch", "release manifest targets must be the exact supported set")
    return by_target


def _sha256_file(path: Path, label: str) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as source:
            while chunk := source.read(1024 * 1024):
                digest.update(chunk)
    except OSError as error:
        _fail("missing_evidence", f"could not hash {label}: {error}")
    return digest.hexdigest()


def _validate_archive(path: Path, asset: Mapping[str, str], target: str) -> int:
    if not path.is_file():
        _fail("missing_evidence", f"release archive for {target} is missing")
    if path.name != asset["archive"]:
        _fail("archive_name_mismatch", f"release archive for {target} has a non-canonical name")
    if _sha256_file(path, f"release archive for {target}") != asset["archive_sha256"]:
        _fail("archive_digest_mismatch", f"release archive digest for {target} is invalid")
    try:
        with tarfile.open(path, mode="r:gz") as archive:
            members = archive.getmembers()
            if not 1 <= len(members) <= MAX_ARCHIVE_MEMBERS:
                _fail("invalid_archive_layout", f"release archive for {target} has an invalid member count")
            binary_members: list[tarfile.TarInfo] = []
            for member in members:
                member_path = Path(member.name)
                if member_path.is_absolute() or ".." in member_path.parts:
                    _fail("invalid_archive_layout", f"release archive for {target} has an unsafe member")
                if member.isfile():
                    if member.name != "bonsai":
                        _fail("invalid_archive_layout", f"release archive for {target} has an extra file")
                    binary_members.append(member)
                elif not member.isdir():
                    _fail("invalid_archive_layout", f"release archive for {target} has a non-regular member")
            if len(binary_members) != 1:
                _fail("invalid_archive_layout", f"release archive for {target} must contain one bonsai file")
            binary = binary_members[0]
            if not 0 < binary.size <= MAX_BINARY_BYTES:
                _fail("invalid_archive_layout", f"release binary for {target} has an invalid size")
            if binary.mode & 0o111 == 0:
                _fail("invalid_archive_layout", f"release binary for {target} is not executable")
            source = archive.extractfile(binary)
            if source is None:
                _fail("invalid_archive_layout", f"release binary for {target} cannot be read")
            digest = hashlib.sha256()
            read_bytes = 0
            while chunk := source.read(1024 * 1024):
                read_bytes += len(chunk)
                if read_bytes > MAX_BINARY_BYTES:
                    _fail("invalid_archive_layout", f"release binary for {target} exceeds its size cap")
                digest.update(chunk)
            if read_bytes != binary.size:
                _fail("invalid_archive_layout", f"release binary for {target} is truncated")
    except ValidationError:
        raise
    except (tarfile.TarError, OSError, EOFError) as error:
        _fail("invalid_archive", f"could not inspect release archive for {target}: {error}")
    if digest.hexdigest() != asset["binary_sha256"]:
        _fail("binary_digest_mismatch", f"extracted release binary for {target} has the wrong digest")
    return binary.size


_RAW_ARRAY_FIELDS = (
    "fresh_startup_ms",
    "returning_startup_ms",
    "idle_cpu_percent",
    "idle_rss_bytes",
)
_HEADLESS_FIELDS = (
    "spawn_to_first_assistant_delta_ms",
    "persistence_duration_ms",
    "context_used_tokens",
    "provider_prompt_tokens",
    "prompt_tokens",
    "completion_tokens",
    "input_cache_hit_rate_percent",
    "representative_task_cost_micros",
)
_SUMMARY_FIELDS = (
    "fresh_startup_ms",
    "returning_startup_ms",
    "idle_cpu_percent",
    "idle_rss_bytes",
    "spawn_to_first_assistant_delta_ms",
    "persistence_duration_ms",
    "context_peak_tokens",
    "context_growth_tokens",
    "prompt_tokens",
    "completion_tokens",
    "input_cache_hit_rate_percent",
    "representative_task_cost_micros",
)


def _u64_distribution(values: Sequence[int]) -> dict[str, int]:
    ordered = sorted(values)
    length = len(ordered)
    middle = length // 2
    if length % 2:
        median = ordered[middle]
    else:
        median = min(ordered[middle - 1] + ordered[middle], (1 << 64) - 1) // 2
    p95_index = (length * 95 + 99) // 100 - 1
    return {
        "median": median,
        "p95": ordered[p95_index],
        "min": ordered[0],
        "max": ordered[-1],
    }


def _float_distribution(values: Sequence[float]) -> dict[str, float]:
    ordered = sorted(values)
    length = len(ordered)
    middle = length // 2
    median = (
        ordered[middle]
        if length % 2
        else (ordered[middle - 1] + ordered[middle]) / 2.0
    )
    p95_index = (length * 95 + 99) // 100 - 1
    return {
        "median": median,
        "p95": ordered[p95_index],
        "min": ordered[0],
        "max": ordered[-1],
    }


def _validate_distribution(value: Any, expected: Mapping[str, int | float], label: str) -> None:
    distribution = _expect_object(value, label)
    _exact_keys(distribution, ("median", "p95", "min", "max"), label)
    for field, expected_value in expected.items():
        actual = distribution[field]
        if isinstance(expected_value, float):
            actual_value: int | float = _expect_nonnegative_float(actual, f"{label}.{field}")
        else:
            actual_value = _expect_u64(actual, f"{label}.{field}")
        if actual_value != expected_value:
            _fail("report_summary_mismatch", f"{label}.{field} does not match raw samples")


def _validate_performance_report(
    value: Any,
    target: str,
    peeled_commit: str,
    binary_sha256: str,
    binary_bytes: int,
) -> None:
    report = _expect_object(value, f"performance report for {target}")
    _exact_keys(report, ("schema_version", "identity", "sample_count", "raw", "summary", "baseline"), f"performance report for {target}")
    if _expect_u64(report["schema_version"], "performance report.schema_version") != 1:
        _fail("invalid_report_schema", f"performance report for {target} has an unsupported schema")
    sample_count = _expect_u64(report["sample_count"], f"performance report for {target}.sample_count")
    if not MINIMUM_SAMPLES <= sample_count <= MAXIMUM_SAMPLES:
        _fail("invalid_sample_count", f"performance report for {target} has an invalid sample count")

    identity = _expect_object(report["identity"], f"performance report for {target}.identity")
    _exact_keys(
        identity,
        (
            "target",
            "profile",
            "toolchain",
            "runner_class",
            "git_commit",
            "binary_sha256",
            "binary_bytes",
        ),
        f"performance report for {target}.identity",
    )
    if identity["target"] != target:
        _fail("report_identity_mismatch", f"performance report for {target} names another target")
    if identity["profile"] != "release":
        _fail("report_identity_mismatch", f"performance report for {target} was not measured in release profile")
    toolchain = _expect_string(identity["toolchain"], f"performance report for {target}.identity.toolchain")
    if (
        not toolchain
        or len(toolchain) > 4096
        or any(
            (ord(character) < 32 and character != "\n") or ord(character) == 127
            for character in toolchain
        )
    ):
        _fail("report_identity_mismatch", f"performance report for {target} has an invalid toolchain")
    runner_class = _expect_string(
        identity["runner_class"],
        f"performance report for {target}.identity.runner_class",
    )
    if runner_class != RUNNER_CLASS_BY_TARGET[target]:
        _fail("report_identity_mismatch", f"performance report for {target} has the wrong runner class")
    if identity["git_commit"] != peeled_commit:
        _fail("report_identity_mismatch", f"performance report for {target} has the wrong commit")
    if identity["binary_sha256"] != binary_sha256:
        _fail("report_identity_mismatch", f"performance report for {target} has the wrong binary digest")
    if _expect_u64(identity["binary_bytes"], f"performance report for {target}.identity.binary_bytes") != binary_bytes:
        _fail("report_identity_mismatch", f"performance report for {target} has the wrong binary size")

    raw = _expect_object(report["raw"], f"performance report for {target}.raw")
    _exact_keys(raw, (*_RAW_ARRAY_FIELDS, "headless"), f"performance report for {target}.raw")
    u64_samples: dict[str, list[int]] = {}
    for field in ("fresh_startup_ms", "returning_startup_ms", "idle_rss_bytes"):
        samples = _expect_array(raw[field], f"performance report for {target}.raw.{field}")
        if len(samples) != sample_count:
            _fail("invalid_sample_count", f"performance report for {target}.raw.{field} has the wrong length")
        u64_samples[field] = [
            _expect_u64(item, f"performance report for {target}.raw.{field}[{index}]")
            for index, item in enumerate(samples)
        ]
    idle_cpu_raw = _expect_array(raw["idle_cpu_percent"], f"performance report for {target}.raw.idle_cpu_percent")
    if len(idle_cpu_raw) != sample_count:
        _fail("invalid_sample_count", f"performance report for {target}.raw.idle_cpu_percent has the wrong length")
    float_samples = [
        _expect_nonnegative_float(item, f"performance report for {target}.raw.idle_cpu_percent[{index}]")
        for index, item in enumerate(idle_cpu_raw)
    ]
    headless = _expect_array(raw["headless"], f"performance report for {target}.raw.headless")
    if len(headless) != sample_count:
        _fail("invalid_sample_count", f"performance report for {target}.raw.headless has the wrong length")
    flattened: dict[str, list[int]] = {field: [] for field in _HEADLESS_FIELDS if field not in ("context_used_tokens", "provider_prompt_tokens")}
    flattened["context_peak_tokens"] = []
    flattened["context_growth_tokens"] = []
    for sample_index, raw_sample in enumerate(headless):
        sample_label = f"performance report for {target}.raw.headless[{sample_index}]"
        sample = _expect_object(raw_sample, sample_label)
        _exact_keys(sample, _HEADLESS_FIELDS, sample_label)
        for field in flattened:
            if field in ("context_peak_tokens", "context_growth_tokens"):
                continue
            metric = _expect_u64(sample[field], f"{sample_label}.{field}")
            if field == "input_cache_hit_rate_percent" and metric > 100:
                _fail("invalid_report_metric", f"{sample_label}.{field} exceeds 100")
            flattened[field].append(metric)
        for field in ("context_used_tokens", "provider_prompt_tokens"):
            turns = _expect_array(sample[field], f"{sample_label}.{field}")
            if len(turns) < 4:
                _fail("invalid_report_metric", f"{sample_label}.{field} must contain at least four turns")
            values = [
                _expect_u64(item, f"{sample_label}.{field}[{index}]")
                for index, item in enumerate(turns)
            ]
            if field == "provider_prompt_tokens":
                flattened["context_peak_tokens"].append(max(values))
                flattened["context_growth_tokens"].append(max(values[-1] - values[0], 0))

    summary = _expect_object(report["summary"], f"performance report for {target}.summary")
    _exact_keys(summary, _SUMMARY_FIELDS, f"performance report for {target}.summary")
    for field, values in u64_samples.items():
        _validate_distribution(summary[field], _u64_distribution(values), f"performance report for {target}.summary.{field}")
    _validate_distribution(summary["idle_cpu_percent"], _float_distribution(float_samples), f"performance report for {target}.summary.idle_cpu_percent")
    for field, values in flattened.items():
        _validate_distribution(summary[field], _u64_distribution(values), f"performance report for {target}.summary.{field}")

    baseline = _expect_object(report["baseline"], f"performance report for {target}.baseline")
    _exact_keys(baseline, ("source", "baseline_git_commit", "passed", "violations"), f"performance report for {target}.baseline")
    expected_source = f"eval/baselines/performance/{target}.json"
    if baseline["source"] != expected_source:
        _fail("invalid_baseline", f"performance report for {target} does not name the reviewed target baseline")
    _expect_git_oid(baseline["baseline_git_commit"], f"performance report for {target}.baseline.baseline_git_commit")
    if _expect_bool(baseline["passed"], f"performance report for {target}.baseline.passed") is not True:
        _fail("performance_regression", f"performance report for {target} failed its baseline")
    violations = _expect_array(baseline["violations"], f"performance report for {target}.baseline.violations")
    if violations:
        _fail("performance_regression", f"performance report for {target} contains baseline violations")


def _validate_target_mapping(mapping: Mapping[str, Path], label: str) -> dict[str, Path]:
    if frozenset(mapping) != SUPPORTED_TARGET_SET:
        _fail("target_set_mismatch", f"{label} must contain exactly the four supported targets")
    return {target: Path(mapping[target]) for target in SUPPORTED_TARGETS}


def _failed(error: ValidationError) -> dict[str, Any]:
    return {
        "schema_version": SCHEMA_VERSION,
        "status": "failed",
        "code": error.code,
        "message": error.message,
    }


def check_rc_soak(
    record_path: Path | str,
    manifest_path: Path | str,
    manifest_signature_path: Path | str,
    release_public_key_path: Path | str,
    performance_reports: Mapping[str, Path | str],
    archives: Mapping[str, Path | str],
    release_published_at: datetime | str,
    *,
    now: datetime | None = None,
) -> dict[str, Any]:
    """Validate an RC-soak record and return its machine-readable status."""

    try:
        current_time = _normalise_now(now)
        published_at = (
            _parse_timestamp(release_published_at, "release published_at")
            if isinstance(release_published_at, str)
            else _normalise_now(release_published_at)
        )
        if published_at > current_time + GIT_CLOCK_SKEW:
            _fail("future_release", "release published_at is in the future")

        record_file = Path(record_path)
        record_value, record_raw = _load_json_file(record_file, MAX_RECORD_BYTES, "RC soak record")
        record = _validate_record(record_value)
        first_seen_at, observation_commits, blocking_ids, _ = _history_versions(
            record_file, record_raw, record, current_time
        )

        manifest_file = Path(manifest_path)
        manifest_value, manifest_raw = _load_json_file(
            manifest_file, MAX_MANIFEST_BYTES, "release manifest"
        )
        if hashlib.sha256(manifest_raw).hexdigest() != record["release"]["manifest_sha256"]:
            _fail("manifest_digest_mismatch", "release manifest digest does not match the record")
        _verify_manifest_signature(
            manifest_file, Path(manifest_signature_path), Path(release_public_key_path)
        )
        manifest_assets = _validate_manifest(manifest_value, record)

        archive_paths = _validate_target_mapping(
            {target: Path(path) for target, path in archives.items()}, "release archives"
        )
        binary_sizes = {
            target: _validate_archive(archive_paths[target], manifest_assets[target], target)
            for target in SUPPORTED_TARGETS
        }

        report_paths = _validate_target_mapping(
            {target: Path(path) for target, path in performance_reports.items()},
            "performance reports",
        )
        for target in SUPPORTED_TARGETS:
            report_value, report_raw = _load_json_file(
                report_paths[target], MAX_REPORT_BYTES, f"performance report for {target}"
            )
            expected_report_sha = record["release"]["performance_report_sha256_by_target"][target]
            if hashlib.sha256(report_raw).hexdigest() != expected_report_sha:
                _fail("report_digest_mismatch", f"performance report digest for {target} is invalid")
            _validate_performance_report(
                report_value,
                target,
                record["release"]["peeled_commit"],
                manifest_assets[target]["binary_sha256"],
                binary_sizes[target],
            )

        effective_start = max(
            _parse_timestamp(record["soak"]["started_at"], "record.soak.started_at"),
            first_seen_at,
            published_at,
        )
        if effective_start > current_time + GIT_CLOCK_SKEW:
            _fail("future_soak", "effective soak start is in the future")

        passing_days: set[str] = set()
        passing_surfaces: set[str] = set()
        for observation in record["observations"]:
            canonical = _canonical(observation)
            first_commit = observation_commits.get(canonical)
            if first_commit is None:
                _fail("invalid_git_history", "an observation has no first-seen commit")
            claimed = _parse_timestamp(observation["timestamp"], "observation.timestamp")
            if claimed > current_time + GIT_CLOCK_SKEW:
                _fail("future_observation", "an observation timestamp is in the future")
            if claimed > first_commit + GIT_CLOCK_SKEW:
                _fail("observation_future_dated", "an observation was committed before it occurred")
            if first_commit - claimed > OBSERVATION_COMMIT_GRACE:
                _fail("observation_backdated", "an observation was committed too long after its claimed time")
            effective_observed_at = max(claimed, first_commit, effective_start)
            if observation["result"] == "passed":
                passing_days.add(effective_observed_at.date().isoformat())
                passing_surfaces.add(observation["surface"])

        elapsed_seconds = max((current_time - effective_start).total_seconds(), 0.0)
        elapsed_hours = int(elapsed_seconds // 3600)
        result: dict[str, Any] = {
            "schema_version": SCHEMA_VERSION,
            "status": "pending",
            "release_tag": record["release"]["tag"],
            "peeled_commit": record["release"]["peeled_commit"],
            "effective_started_at": effective_start.strftime("%Y-%m-%dT%H:%M:%SZ"),
            "elapsed_hours": elapsed_hours,
            "minimum_hours": record["soak"]["minimum_hours"],
            "observation_days": len(passing_days),
            "required_observation_days": MINIMUM_OBSERVATION_DAYS,
            "surfaces": sorted(passing_surfaces),
            "blocking_incidents": len(blocking_ids),
            "pending_reasons": [],
        }
        if blocking_ids:
            result["status"] = "failed"
            result["code"] = "release_blocking_incident"
            result["message"] = "Git history contains a release-blocking RC incident"
            return result
        if elapsed_hours < record["soak"]["minimum_hours"]:
            result["pending_reasons"].append("minimum_duration")
        if len(passing_days) < MINIMUM_OBSERVATION_DAYS:
            result["pending_reasons"].append("observation_days")
        if passing_surfaces != SURFACES:
            result["pending_reasons"].append("surface_coverage")
        if not result["pending_reasons"]:
            result["status"] = "qualified"
        return result
    except ValidationError as error:
        return _failed(error)


def _parse_target_paths(values: Sequence[str], flag: str) -> dict[str, Path]:
    result: dict[str, Path] = {}
    for value in values:
        target, separator, raw_path = value.partition("=")
        if not separator or not target or not raw_path:
            raise UsageError(f"{flag} values must use TARGET=PATH")
        if target in result:
            raise UsageError(f"{flag} repeats target {target}")
        result[target] = Path(raw_path)
    return result


def _parser() -> JsonArgumentParser:
    parser = JsonArgumentParser(description=__doc__)
    parser.add_argument("--record", required=True, type=Path)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--manifest-signature", required=True, type=Path)
    parser.add_argument("--release-public-key", required=True, type=Path)
    parser.add_argument("--release-published-at", required=True)
    parser.add_argument("--performance-report", action="append", default=[], metavar="TARGET=PATH")
    parser.add_argument("--archive", action="append", default=[], metavar="TARGET=PATH")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    try:
        args = _parser().parse_args(argv)
        reports = _parse_target_paths(args.performance_report, "--performance-report")
        archives = _parse_target_paths(args.archive, "--archive")
    except UsageError as error:
        print(
            json.dumps(
                {
                    "schema_version": SCHEMA_VERSION,
                    "status": "failed",
                    "code": "usage",
                    "message": str(error),
                },
                sort_keys=True,
            )
        )
        return 64
    result = check_rc_soak(
        args.record,
        args.manifest,
        args.manifest_signature,
        args.release_public_key,
        reports,
        archives,
        args.release_published_at,
    )
    print(json.dumps(result, sort_keys=True))
    return {"qualified": 0, "pending": 2, "failed": 3}[result["status"]]


if __name__ == "__main__":
    sys.exit(main())
