"""Exclusive, durable local attempt journal for an approved serial runner."""

import fcntl
import hashlib
import json
import os
import tempfile
from pathlib import Path

from .comparison_manifest import canonical_bytes
from .comparison_schedule import Trial


class FileJournal:
    """Keep reservations forever; resume never reuses an interrupted identity.

    Call only within a context manager. The lock must remain held for the full
    scheduling run. Raw logs are not stored in this normalized journal.
    """

    def __init__(self, root: Path):
        self.root = root
        self._lock = None

    def __enter__(self):
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        lock = (self.root / ".lock").open("a", encoding="utf-8")
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            lock.close()
            raise
        self._lock = lock
        return self

    def __exit__(self, *args):
        if self._lock is not None:
            self._lock.close()
            self._lock = None

    def _require_lock(self):
        if self._lock is None:
            raise RuntimeError("journal must hold its exclusive lock")

    def _directory(self, identity: str) -> Path:
        return self.root / hashlib.sha256(identity.encode("utf-8")).hexdigest()

    @staticmethod
    def _sync_dir(path: Path):
        descriptor = os.open(path, os.O_RDONLY)
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)

    @staticmethod
    def _write_new(path: Path, value: dict):
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "wb") as file:
            file.write(canonical_bytes(value) + b"\n")
            file.flush()
            os.fsync(file.fileno())

    def records(self) -> list[dict]:
        self._require_lock()
        records = []
        for directory in sorted(path for path in self.root.iterdir()
                                if path.is_dir() and not path.name.startswith(".pending-")):
            reservation = json.loads((directory / "reservation.json").read_text(encoding="utf-8"))
            if self._directory(reservation["identity"]) != directory:
                raise ValueError("journal reservation path mismatch")
            result_path = directory / "result.json"
            if result_path.exists():
                result = json.loads(result_path.read_text(encoding="utf-8"))
                if result.get("identity") != reservation["identity"] or result.get("manifest_digest") != reservation["manifest_digest"]:
                    raise ValueError("journal result/reservation mismatch")
                records.append(result)
            else:
                records.append({**reservation, "category": "infrastructure_invalid", "provider_billed_usd": None})
        return records

    def begin(self, trial: Trial, manifest_digest: str) -> None:
        self._require_lock()
        directory = self._directory(trial.identity)
        if directory.exists():
            raise FileExistsError("trial identity already reserved")
        # Hidden staging directories cannot precede external effects: begin()
        # returns only after a complete reservation is published and fsynced.
        # Crash-orphaned staging directories are ignored, not trial attempts.
        pending = Path(tempfile.mkdtemp(prefix=".pending-", dir=self.root))
        self._write_new(pending / "reservation.json", {"identity": trial.identity, "manifest_digest": manifest_digest})
        self._sync_dir(pending)
        pending.rename(directory)
        self._sync_dir(self.root)

    def finish(self, trial: Trial, result: dict) -> None:
        self._require_lock()
        directory = self._directory(trial.identity)
        reservation = json.loads((directory / "reservation.json").read_text(encoding="utf-8"))
        for key, value in reservation.items():
            if key in result and result[key] != value:
                raise ValueError("result cannot replace reserved identity")
        self._write_new(directory / "result.json", {**result, **reservation})
        self._sync_dir(directory)
