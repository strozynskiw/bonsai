"""Pure approval/spending checks. Local estimates are never hard billing caps."""

from dataclasses import dataclass
from decimal import Decimal, InvalidOperation

from .comparison_manifest import FrozenManifest, digest, require_digest, require_text
from .comparison_schedule import remaining, schedule


def smoke_digest(manifest: FrozenManifest, records: list[dict]) -> str:
    """Require six verified successes bound to the unchanged frozen configuration."""
    if remaining(manifest, "smoke", records):
        raise ValueError("all six successful smoke attempts required before pilot")
    expected = {trial.identity: trial for trial in schedule(manifest, "smoke")}
    body = manifest.body
    for record in records:
        trial = expected[record["identity"]]
        if (record.get("category") != "verified" or type(record.get("reward")) not in (int, float)
                or record["reward"] != 1 or record.get("task") != trial.task
                or record.get("agent") != trial.agent or record.get("model") != body["model"]
                or record.get("artifact_digest") != body["artifacts"][trial.agent]["artifact_digest"]
                or record.get("profile_digest") != body["profiles"][trial.agent]["overlay_digest"]):
            raise ValueError("smoke evidence failed or configuration mismatch")
    return digest(sorted(records, key=lambda record: record["identity"]))


def amount(value, name: str) -> Decimal:
    if type(value) not in (int, float, str):
        raise ValueError(f"invalid {name}")
    try:
        result = Decimal(str(value))
    except InvalidOperation as exc:
        raise ValueError(f"invalid {name}") from exc
    if not result.is_finite() or result < 0:
        raise ValueError(f"invalid {name}")
    return result


@dataclass(frozen=True)
class Approval:
    manifest_digest: str
    phase: str
    runner: str
    provider_limit_usd: Decimal
    infrastructure_limit_usd: Decimal
    per_trial_reservation_usd: Decimal
    external_cap_evidence_digest: str
    isolation_evidence_digest: str
    approver: str
    smoke_evidence_digest: str | None = None

    @classmethod
    def parse(cls, value: dict, manifest: FrozenManifest, phase: str, *,
              smoke_records: list[dict] | None = None) -> Approval:
        if phase not in {"smoke", "pilot"}:
            raise ValueError("unknown approval phase")
        evidence = None
        if phase == "pilot":
            evidence = smoke_digest(manifest, smoke_records or [])
            if value.get("smoke_evidence_digest") != evidence:
                raise ValueError("pilot approval must bind successful smoke evidence")
        if value.get("execute") is not True or value.get("manifest_digest") != manifest.sha256 or value.get("phase") != phase:
            raise ValueError("explicit execute approval for exact manifest and phase required")
        for key in ("runner", "approver"):
            require_text(value[key], key)
        if value.get("platform") != "linux":
            raise ValueError("approved Linux runner required")
        for key in ("external_cap_evidence_digest", "isolation_evidence_digest"):
            require_digest(value[key], key)
        for key in ("nested_confinement_approved", "network_approved", "external_provider_cap_enforced"):
            if value.get(key) is not True:
                raise ValueError(f"missing {key}")
        values = [amount(value[key], key) for key in ("provider_limit_usd", "infrastructure_limit_usd", "per_trial_reservation_usd")]
        if any(v <= 0 for v in values):
            raise ValueError("numeric positive spending limits and reservation required")
        return cls(manifest.sha256, phase, value["runner"], *values,
                   value["external_cap_evidence_digest"], value["isolation_evidence_digest"], value["approver"], evidence)

    def check_smoke(self, manifest: FrozenManifest, records: list[dict] | None) -> None:
        """Revalidate retained evidence before backend preflight, including resume."""
        if self.phase == "pilot" and (self.smoke_evidence_digest is None
                or smoke_digest(manifest, records or []) != self.smoke_evidence_digest):
            raise ValueError("pilot requires approval-bound successful smoke evidence")

    def check_next(self, records: list[dict], *, observed_provider_usd, observed_infrastructure_usd) -> None:
        """Fresh external billing observations must include child/retry requests."""
        if observed_provider_usd is None or observed_infrastructure_usd is None:
            raise ValueError("spending observation unavailable; stop scheduling")
        if any(r.get("provider_billed_usd") is None for r in records):
            raise ValueError("attempt billing unavailable; stop scheduling")
        provider = amount(observed_provider_usd, "provider observation")
        infrastructure = amount(observed_infrastructure_usd, "infrastructure observation")
        if provider < sum((amount(r["provider_billed_usd"], "attempt billing") for r in records), Decimal(0)):
            raise ValueError("billing observation behind persisted attempts")
        if provider + self.per_trial_reservation_usd > self.provider_limit_usd or infrastructure >= self.infrastructure_limit_usd:
            raise ValueError("approved budget exhausted; stop scheduling")
