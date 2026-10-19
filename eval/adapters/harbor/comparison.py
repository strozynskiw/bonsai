"""Offline preparation, schedule and report commands. Never installs or fetches."""

import argparse
import asyncio
import json
import os
from pathlib import Path
import sys

from .comparison_gate import Approval
from .comparison_manifest import FrozenManifest, canonical_bytes, freeze
from .comparison_report import markdown, summarize
from .comparison_schedule import remaining, select_tasks


def read_json(path: Path):
    return json.loads(path.read_text(encoding="utf-8"))


def write_json(path: Path, value) -> None:
    # Immutable output prevents accidental replacement of a frozen experiment.
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as file:
        file.write(canonical_bytes(value).decode() + "\n")


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare = commands.add_parser("prepare", help="local reviewed inventory + explicit settings → frozen manifest")
    prepare.add_argument("--settings", type=Path, required=True)
    prepare.add_argument("--inventory", type=Path, required=True)
    prepare.add_argument("--seed", required=True)
    prepare.add_argument("--out", type=Path, required=True)
    for name in ("check", "dry-run", "report", "live"):
        command = commands.add_parser(name)
        command.add_argument("--manifest", type=Path, required=True)
        if name != "check":
            command.add_argument("--records", type=Path, required=True, help="local JSON array, including failures; [] for no attempts")
        if name in {"dry-run", "live"}:
            command.add_argument("--phase", choices=("smoke", "pilot"), required=True)
        if name == "report":
            command.add_argument("--out", type=Path, required=True)
        if name == "live":
            command.add_argument("--task-root", type=Path, required=True)
            command.add_argument("--journal", type=Path, required=True)
            command.add_argument("--raw-output", type=Path, required=True)
            command.add_argument("--billing", type=Path, required=True)
            command.add_argument("--evidence", type=Path, required=True)
            command.add_argument("--execute", action="store_true")
            command.add_argument("--approval", type=Path, required=True)
            command.add_argument("--smoke-records", type=Path, help="successful smoke evidence required for pilot")
    args = parser.parse_args(argv)
    try:
        if args.command == "prepare":
            body = read_json(args.settings)
            inventory = read_json(args.inventory)
            if body["dataset_digest"] != inventory["dataset_digest"]:
                raise ValueError("settings/inventory dataset digest mismatch")
            body.update(select_tasks(inventory, args.seed, body["resources"]))
            write_json(args.out, freeze(body))
            return 0
        manifest = FrozenManifest.load(args.manifest)
        if args.command == "check":
            print(manifest.sha256)
            return 0
        records = read_json(args.records)
        if args.command == "dry-run":
            print(json.dumps([trial.to_dict() for trial in remaining(manifest, args.phase, records)], indent=2))
        elif args.command == "report":
            report = summarize(manifest, records)
            write_json(args.out / "comparison.json", report)
            with (args.out / "comparison.md").open("x", encoding="utf-8") as file:
                file.write(markdown(report))
        else:
            if not args.execute:
                raise ValueError("live requires --execute and explicit approval")
            smoke = read_json(args.smoke_records) if args.smoke_records else None
            approval = Approval.parse(read_json(args.approval), manifest, args.phase, smoke_records=smoke)
            from .comparison_backend import BillingFile, HarborBackend
            from .comparison_journal import FileJournal
            from .comparison_runner import run_serial
            if approval.external_cap_evidence_digest != manifest.body["execution"]["external_cap_evidence_digest"]:
                raise ValueError("approval/backend cap evidence mismatch")
            evidence = read_json(args.evidence)
            from .comparison_manifest import digest
            if digest(evidence["isolation_evidence_digest"]) != approval.isolation_evidence_digest:
                raise ValueError("approval isolation evidence mismatch")
            with FileJournal(args.journal) as journal:
                backend = HarborBackend(manifest, task_root=args.task_root, output=args.raw_output,
                                        billing=BillingFile(args.billing, manifest, args.phase),
                                        credential=os.environ.get("TB4_PROVIDER_API_KEY", ""), evidence=evidence,
                                        approval=approval, records=journal.records)
                # --records is an explicit expectation, never an alternate
                # mutable source of truth that can erase occupied identities.
                if journal.records() != records:
                    raise ValueError("records do not match durable journal")
                asyncio.run(run_serial(manifest, args.phase, approval, backend, journal, smoke_records=smoke))
        return 0
    except (ValueError, TypeError, KeyError, OSError, RuntimeError, ImportError) as error:
        # Do not echo supplied JSON or credentials through exception reprs.
        print(f"comparison {args.command} refused ({type(error).__name__}); verify local input contracts", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
