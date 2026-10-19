"""Serial orchestration seam; concrete live backends require operator validation.

Invariant: reserve an identity durably before any external effect, never rerun
it on resume, and await environment teardown before scheduling the next trial.
"""

import asyncio
from dataclasses import dataclass
from typing import Protocol

from .comparison_gate import Approval
from .comparison_manifest import FrozenManifest
from .comparison_schedule import Trial, remaining


@dataclass(frozen=True)
class Consumption:
    provider_usd: str | None
    infrastructure_usd: str | None


class Backend(Protocol):
    """Harbor owns environments/verifiers; the scheduler never runs agent loops."""

    async def preflight(self, manifest: FrozenManifest) -> None: ...
    async def consumption(self) -> Consumption: ...
    async def run(self, trial: Trial) -> dict: ...
    async def teardown(self, trial: Trial) -> None: ...


class Journal(Protocol):
    """Exclusive single-writer journal, with fsync before begin() returns.

    Records include reserved/interrupted identities. An incomplete reservation
    is recovered as infrastructure-invalid, never silently retried.
    """

    def records(self) -> list[dict]: ...
    def begin(self, trial: Trial, manifest_digest: str) -> None: ...
    def finish(self, trial: Trial, result: dict) -> None: ...


async def run_serial(manifest: FrozenManifest, phase: str, approval: Approval,
                     backend: Backend, journal: Journal, *, smoke_records: list[dict] | None = None) -> None:
    """Testable orchestration, not an executable/live-backend registration hook."""
    if approval.manifest_digest != manifest.sha256 or approval.phase != phase:
        raise ValueError("approval does not bind this manifest/phase")
    approval.check_smoke(manifest, smoke_records)
    pending = remaining(manifest, phase, journal.records())
    await backend.preflight(manifest)
    for trial in pending:
        observation = await backend.consumption()
        approval.check_next(journal.records(), observed_provider_usd=observation.provider_usd,
                            observed_infrastructure_usd=observation.infrastructure_usd)
        journal.begin(trial, manifest.sha256)
        result = {"category": "infrastructure_invalid", "provider_billed_usd": None}
        try:
            result = await backend.run(trial)
        except asyncio.CancelledError:
            result = {"category": "cancelled", "provider_billed_usd": None}
            raise
        finally:
            # Retain partial failure evidence even if teardown itself fails.
            # Shield cleanup from the cancellation that interrupted the run.
            cleanup = asyncio.create_task(backend.teardown(trial))
            interrupted = False
            try:
                while not cleanup.done():
                    try:
                        await asyncio.shield(cleanup)
                    except asyncio.CancelledError:
                        if cleanup.cancelled():
                            raise
                        interrupted = True
                cleanup.result()
            except (Exception, asyncio.CancelledError):
                result = {**result, "category": "infrastructure_invalid", "reward": None}
                raise
            finally:
                journal.finish(trial, result)
            if interrupted:
                raise asyncio.CancelledError()
        if result["category"] == "infrastructure_invalid":
            raise RuntimeError("infrastructure-invalid attempt; stop batch")
