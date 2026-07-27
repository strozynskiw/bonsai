#!/usr/bin/env python3
"""Update the Bonsai Homebrew formula from a release manifest."""

from __future__ import annotations

import argparse
import json
import re
import sys
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any

REPOSITORY = "strozynskiw/bonsai"
MANIFEST_NAME = "release-manifest.json"
TARGETS = {
    "aarch64-apple-darwin": "arm64_sonoma",
    "x86_64-apple-darwin": "sonoma",
    "aarch64-unknown-linux-gnu": "arm64_linux",
    "x86_64-unknown-linux-gnu": "x86_64_linux",
}


@dataclass(frozen=True)
class ReleaseAsset:
    target: str
    archive: str
    archive_sha256: str


def manifest_url(tag: str) -> str:
    return f"https://github.com/{REPOSITORY}/releases/download/{tag}/{MANIFEST_NAME}"


def load_manifest(source: str) -> dict[str, Any]:
    if source.startswith(("https://", "http://")):
        with urllib.request.urlopen(source, timeout=30) as response:  # noqa: S310
            return json.load(response)
    with Path(source).open(encoding="utf-8") as handle:
        return json.load(handle)


def parse_assets(manifest: dict[str, Any], tag: str) -> dict[str, ReleaseAsset]:
    if manifest.get("schema_version") != 1:
        raise ValueError("manifest schema_version must be 1")
    if manifest.get("repository") != REPOSITORY:
        raise ValueError(f"manifest repository must be {REPOSITORY}")
    if manifest.get("tag") != tag:
        raise ValueError(f"manifest tag must be {tag}")

    raw_assets = manifest.get("assets")
    if not isinstance(raw_assets, list):
        raise ValueError("manifest must contain an assets array")

    assets: dict[str, ReleaseAsset] = {}
    for raw in raw_assets:
        if not isinstance(raw, dict):
            raise ValueError("manifest asset entries must be objects")
        target = raw.get("target")
        if not isinstance(target, str) or target not in TARGETS:
            raise ValueError(f"manifest contains unsupported target: {target!r}")
        if target in assets:
            raise ValueError(f"manifest contains duplicate target: {target}")
        archive = raw.get("archive")
        sha256 = raw.get("archive_sha256")
        expected_archive = f"bonsai-{tag}-{target}.tar.gz"
        if archive != expected_archive:
            raise ValueError(
                f"manifest target {target} archive must be {expected_archive}"
            )
        if not isinstance(sha256, str) or not re.fullmatch(r"[0-9a-fA-F]{64}", sha256):
            raise ValueError(f"manifest target {target} has invalid archive_sha256")
        assets[target] = ReleaseAsset(target, archive, sha256.lower())

    missing = set(TARGETS) - set(assets)
    if missing:
        raise ValueError(
            "manifest targets must be exactly the four supported targets "
            f"(missing: {', '.join(sorted(missing))})"
        )
    return assets


def formula_block(tag: str, assets: dict[str, ReleaseAsset]) -> str:
    def stanza(target: str, indent: int) -> list[str]:
        asset = assets[target]
        prefix = " " * indent
        url = f"https://github.com/{REPOSITORY}/releases/download/{tag}/{asset.archive}"
        return [f'{prefix}url "{url}"', f'{prefix}sha256 "{asset.archive_sha256}"']

    lines = ["  on_macos do", "    if Hardware::CPU.arm?"]
    lines.extend(stanza("aarch64-apple-darwin", 6))
    lines.append("    else")
    lines.extend(stanza("x86_64-apple-darwin", 6))
    lines.extend(["    end", "  end", "", "  on_linux do", "    if Hardware::CPU.arm?"])
    lines.extend(stanza("aarch64-unknown-linux-gnu", 6))
    lines.append("    else")
    lines.extend(stanza("x86_64-unknown-linux-gnu", 6))
    lines.extend(["    end", "  end"])
    return "\n".join(lines)


def update_formula(formula: str, tag: str, assets: dict[str, ReleaseAsset]) -> str:
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?", tag):
        raise ValueError(f"invalid release tag: {tag}")
    pattern = re.compile(r"(?ms)^  on_macos do\n.*?^  end\n\n^  on_linux do\n.*?^  end$")
    updated, count = pattern.subn(formula_block(tag, assets), formula)
    if count != 1:
        raise ValueError("formula must contain one macOS and Linux platform block")
    return updated


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag", help="release tag, for example v0.2.5")
    parser.add_argument("tap_checkout", type=Path, help="path to the homebrew-bonsai checkout")
    parser.add_argument("--manifest", help="manifest path or URL (defaults to the GitHub release asset)")
    args = parser.parse_args()

    try:
        assets = parse_assets(
            load_manifest(args.manifest or manifest_url(args.tag)), args.tag
        )
        formula_path = args.tap_checkout / "Formula" / "bonsai.rb"
        original = formula_path.read_text(encoding="utf-8")
        updated = update_formula(original, args.tag, assets)
        formula_path.write_text(updated, encoding="utf-8")
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"update-homebrew-tap: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
