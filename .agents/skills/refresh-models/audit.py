#!/usr/bin/env python3
"""Audit bonsai's builtin model TOMLs against models.dev and live caches.

Mirrors bonsai's own lookup rules:
  - models.dev key = "<provider_block>/<model>" unless the raw model id
    already contains "/" (models_dev.rs::models_dev_model_id)
  - direct-provider rows overwrite router duplicates
    (models_dev.rs::insert_with_direct_provider_precedence)
  - a target's metadata key = `metadata_model` if set, else `model`
    (mod.rs resolve())
  - `pinned = true` and `pinned_fields` mark deliberate divergences
    (spec.rs::TargetSpec)

The builtin file list and each connection's models.dev block are DISCOVERED
from the catalog, not hardcoded — every `models/builtin/<id>.toml` is audited,
and a target's block is the prefix of its `metadata_model`/`model`. Adding a
provider needs no edit here. The only hand-maintained list is DISCOVERY_FIRST
(aggregators that opt out of lineup reporting).

Usage:
  python3 audit.py [connection-id] [--fetch] [--source PATH]

  connection-id  limit the report to one builtin file stem (any
                 models/builtin/<id>.toml, e.g. gemini | xai | mistral)
  --fetch        force a fresh https://models.dev/api.json download
  --source PATH  read models.dev JSON from PATH instead of cache/network

Exit code 1 when unexplained drift or unmapped rows are found, 0 otherwise.
"""

import json
import os
import re
import sys
import tempfile
import time
import tomllib
import urllib.request

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(SCRIPT_DIR, "..", "..", ".."))
CACHE = os.path.expanduser("~/.bonsai/cache/models-dev.json")
LIVE_DIR = os.path.expanduser("~/.bonsai/cache/live-models")
MODELS_DEV_URL = "https://models.dev/api.json"
CACHE_MAX_AGE_SECS = 24 * 3600

BUILTIN_DIR = os.path.join(REPO, "models", "builtin")

# `# not-shipped: <remote-id> — reason` comment lines in a builtin TOML mark a
# served model as deliberately excluded: it stops counting as a lineup gap and
# is labeled in the unmapped-served listing. Every annotation must carry its
# reason on the same line — an unexplained skip is as bad as an unexplained pin.
NOT_SHIPPED_RE = re.compile(r"^#\s*not-shipped:\s*([\w.\-/]+)", re.MULTILINE)


def not_shipped_ids(toml_path: str) -> set[str]:
    with open(toml_path, encoding="utf-8") as f:
        return set(NOT_SHIPPED_RE.findall(f.read()))

# Connections that serve OTHER providers' models through dynamic discovery
# (aggregators, local pass-through endpoints). Their targets legitimately map
# to many foreign models.dev blocks, so lineup-gap and unmapped-served
# reporting against a single "own" block is meaningless for them and is
# skipped. This is the ONLY hand-maintained provider list — a normal
# direct-provider connection is discovered and mapped automatically (see
# discover_tomls / derive_blocks). Local `*-compatible` endpoints need no entry
# because they ship no catalog file. Add an id here only when introducing a new
# aggregator, never for a normal provider.
DISCOVERY_FIRST = {"openrouter"}


def discover_tomls() -> list[str]:
    """Every builtin catalog file stem, minus the shared connection table and
    the disabled example scaffold. Convention (relied on for live-cache and
    lineup keying): a builtin file's stem equals its connection id, which for a
    direct provider also equals its models.dev block — e.g. xai.toml holds the
    `xai` connection mapping to the `xai` block."""
    names = []
    for fn in sorted(os.listdir(BUILTIN_DIR)):
        if not fn.endswith(".toml"):
            continue
        stem = fn[:-5]
        if stem == "connections" or stem.startswith("example"):
            continue
        names.append(stem)
    return names


def load_toml(name: str) -> dict:
    with open(os.path.join(BUILTIN_DIR, f"{name}.toml"), "rb") as f:
        return tomllib.load(f)


def derive_blocks(names: list[str], md_blocks: set[str]) -> dict[str, list[str]]:
    """Map each connection to the models.dev block(s) its targets resolve to,
    read straight from the catalog: a target's block is the prefix of its
    `metadata_model` (or `model`) before the slash. Only prefixes that are real
    models.dev blocks count, so a provider-invented prefix contributes nothing.
    Discovery-first connections map to nothing by policy."""
    blocks: dict[str, list[str]] = {}
    for name in names:
        if name in DISCOVERY_FIRST:
            blocks[name] = []
            continue
        seen: list[str] = []
        for t in load_toml(name).get("targets", []):
            key = t.get("metadata_model", t["model"])
            prefix = key.split("/", 1)[0]
            if prefix in md_blocks and prefix not in seen:
                seen.append(prefix)
        blocks[name] = seen
    return blocks


ALL_TOMLS = discover_tomls()


def load_models_dev(force_fetch: bool, source: str | None) -> dict:
    if source:
        with open(source, "rb") as f:
            return json.load(f)
    if not force_fetch and os.path.exists(CACHE):
        age = time.time() - os.path.getmtime(CACHE)
        if age < CACHE_MAX_AGE_SECS:
            print(f"(using {CACHE}, {age / 3600:.1f}h old — pass --fetch to force)\n")
            with open(CACHE, "rb") as f:
                return json.load(f)
    print(f"(fetching {MODELS_DEV_URL})\n")
    # models.dev 403s the default Python-urllib User-Agent (bot filter);
    # any browser-ish or tool-ish UA passes.
    req = urllib.request.Request(
        MODELS_DEV_URL, headers={"User-Agent": "bonsai-refresh-models/1.0"}
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            data = resp.read()
    except Exception as err:
        if os.path.exists(CACHE):
            age = (time.time() - os.path.getmtime(CACHE)) / 3600
            print(f"(fetch failed: {err}; falling back to {CACHE}, {age:.1f}h old)\n")
            with open(CACHE, "rb") as f:
                return json.load(f)
        raise
    with tempfile.NamedTemporaryFile(
        "wb", suffix=".json", delete=False, prefix="models-dev-"
    ) as f:
        f.write(data)
    return json.loads(data)


def flatten(md: dict) -> dict:
    flat: dict[str, dict] = {}
    for pid, pdata in md.items():
        if not isinstance(pdata, dict):
            continue
        for mid, m in (pdata.get("models") or {}).items():
            full = mid if "/" in mid else f"{pid}/{mid}"
            if full.split("/", 1)[0] == pid:
                flat[full] = m
            else:
                flat.setdefault(full, m)
    return flat


def micros(usd):
    return None if usd is None else round(usd * 1_000_000)


def audit_target(t: dict, m: dict | None, key: str) -> list[str]:
    """Issue lines for one TOML target vs its models.dev row."""
    if t.get("pinned"):
        return ["PINNED (deliberate divergence — verify its comment still holds)"]
    if m is None:
        return [
            f"NOT IN models.dev under `{key}` — wrong/missing metadata_model, "
            "or a provider-invented model (then pin it)"
        ]
    pinned_fields = set(t.get("pinned_fields") or [])
    issues = []
    if pinned_fields:
        issues.append(f"PINNED fields {','.join(sorted(pinned_fields))}")
    lim = m.get("limit") or {}
    cost = m.get("cost") or {}
    pr = t.get("pricing") or {}
    pairs = [
        (
            "context_window",
            "context-window",
            t.get("context_window"),
            lim.get("context"),
        ),
        ("output_limit", "output-limit", t.get("output_limit"), lim.get("output")),
        (
            "price.input",
            "pricing",
            pr.get("input_micros_per_million"),
            micros(cost.get("input")),
        ),
        (
            "price.output",
            "pricing",
            pr.get("output_micros_per_million"),
            micros(cost.get("output")),
        ),
        (
            "price.cache_read",
            "pricing",
            pr.get("cache_read_micros_per_million"),
            micros(cost.get("cache_read")),
        ),
        (
            "price.cache_write",
            "pricing",
            pr.get("cache_write_micros_per_million"),
            micros(cost.get("cache_write")),
        ),
    ]
    for field, pin, tv, mv in pairs:
        if pin in pinned_fields:
            continue
        if tv is not None and mv is not None and tv != mv:
            issues.append(f"{field}: toml={tv} models.dev={mv}")
        # bonsai's Rust drift check compares whole pricing structs, so a rate
        # present in models.dev but absent in an explicit TOML pricing table
        # still warns at startup. Only applies when the TOML has ANY pricing.
        elif field.startswith("price.") and pr and tv is None and mv is not None:
            issues.append(f"{field}: missing in toml, models.dev has {mv}")
    if (
        "context-window" not in pinned_fields
        and t.get("context_window") is None
        and lim.get("context") is None
    ):
        issues.append("context_window: MISSING EVERYWHERE (falls back to 120k)")

    if "pricing" not in pinned_fields and t.get("pricing_tiers"):
        toml_tiers = {
            tier["above_input_tokens"]: tier.get("pricing") or {}
            for tier in t["pricing_tiers"]
        }
        models_dev_tiers = {
            selector["size"]: tier
            for tier in cost.get("tiers") or []
            if isinstance(tier, dict)
            and isinstance((selector := tier.get("tier")), dict)
            and selector.get("type") == "context"
            and selector.get("size")
        }
        if set(toml_tiers) != set(models_dev_tiers):
            issues.append(
                "pricing tier thresholds: "
                f"toml={sorted(toml_tiers)} models.dev={sorted(models_dev_tiers)}"
            )
        for threshold in sorted(set(toml_tiers) & set(models_dev_tiers)):
            toml_rates = toml_tiers[threshold]
            models_dev_rates = models_dev_tiers[threshold]
            for field, toml_key, models_dev_key in [
                ("input", "input_micros_per_million", "input"),
                ("output", "output_micros_per_million", "output"),
                ("cache_read", "cache_read_micros_per_million", "cache_read"),
                ("cache_write", "cache_write_micros_per_million", "cache_write"),
            ]:
                tv = toml_rates.get(toml_key)
                mv = micros(models_dev_rates.get(models_dev_key))
                if tv != mv and (tv is not None or mv is not None):
                    issues.append(
                        f"price tier >{threshold}.{field}: "
                        f"toml={tv} models.dev={mv}"
                    )

    raw_opts = t.get("reasoning_options")
    md_reasoning = bool(m.get("reasoning"))
    # `reasoning_options = []` is the deliberate "no thinking UI" marker
    # (free tiers, highspeed variants, non-thinking lanes); models.dev
    # disagreeing is expected there, so it's a note, never a counted problem.
    if "reasoning" in pinned_fields:
        pass
    elif raw_opts == [] and md_reasoning:
        issues.append("note: toml disables reasoning (explicit []); models.dev says true")
    elif isinstance(raw_opts, list) and raw_opts and not md_reasoning:
        issues.append("reasoning: toml declares options, models.dev says false — verify")
    return issues


def main() -> int:
    args = [a for a in sys.argv[1:]]
    force_fetch = "--fetch" in args
    source = None
    if "--source" in args:
        source = args[args.index("--source") + 1]
        args.remove("--source")
        args.remove(source)
    args = [a for a in args if not a.startswith("--")]
    tomls = [args[0]] if args else ALL_TOMLS
    unknown = [t for t in tomls if t not in ALL_TOMLS]
    if unknown:
        print(f"unknown connection {unknown}; choose from {ALL_TOMLS}")
        return 2

    md = load_models_dev(force_fetch, source)
    flat = flatten(md)
    # A block's OWN lineup (not router-contributed rows that happen to share
    # the provider prefix in `flat`).
    block_rows = {
        pid: set((pdata.get("models") or {}).keys())
        for pid, pdata in md.items()
        if isinstance(pdata, dict)
    }
    # Connection -> models.dev block(s), derived from the catalog (not
    # hand-maintained), so a newly added provider is audited automatically.
    blocks_of_interest = derive_blocks(ALL_TOMLS, set(block_rows))
    problems = 0
    covered: dict[str, set] = {}

    for name in tomls:
        path = os.path.join(REPO, "models", "builtin", f"{name}.toml")
        with open(path, "rb") as f:
            doc = tomllib.load(f)
        # Deliberate exclusions count as covered for lineup-gap purposes.
        covered.setdefault(name, set()).update(not_shipped_ids(path))
        print(f"## {name}.toml")
        for t in doc.get("targets", []):
            model = t["model"]
            key = t.get("metadata_model", model)
            covered.setdefault(t["connection"], set()).add(
                t.get("remote_model", model.split("/", 1)[1])
            )
            issues = audit_target(t, flat.get(key), key)
            status = "OK" if not issues else "; ".join(issues)
            counted = [
                i for i in issues if not i.startswith(("PINNED", "note:"))
            ]
            if counted:
                problems += 1
            print(f"- `{model}` (md `{key}`): {status}")
        print()

    # Lineup gaps per interesting block. A block row is only a candidate when
    # the connection actually SERVES it (live cache), otherwise the block's
    # full history (dated snapshots, embeddings, retired ids) drowns the list;
    # without a live cache, fall back to the whole block.
    live_served: dict[str, set] = {}
    if os.path.isdir(LIVE_DIR):
        for fn in os.listdir(LIVE_DIR):
            if fn.endswith(".json"):
                with open(os.path.join(LIVE_DIR, fn)) as f:
                    data = json.load(f)
                live_served[fn[:-5]] = {
                    str(lm.get("remote_model_id"))
                    for lm in data.get("models") or []
                }

    print("## lineup gaps (served + in models.dev, but no TOML target)")
    for name in tomls:
        for block in blocks_of_interest.get(name, []):
            have = covered.get(name, set())
            rows = set(block_rows.get(block, set()))
            if name in live_served:
                # High confidence: the connection demonstrably serves these.
                missing = sorted((rows & live_served[name]) - have)
                if missing:
                    problems += 1
                    print(f"- `{name}` (block `{block}`): {', '.join(missing)}")
            else:
                # No live cache (never listed with a key) — the block's full
                # lineup is only a hint, so report without counting.
                missing = sorted(rows - have)
                if missing:
                    print(
                        f"- `{name}` (block `{block}`, informational — no live "
                        f"cache yet): {', '.join(missing)}"
                    )

    # Live caches: unmapped rows = served models with no catalog target at the
    # time the cache was WRITTEN. Ids whose target exists now are stale-cache
    # artifacts (mapping recomputes on the connection's next 5-min refresh).
    all_remote_ids: set[str] = set()
    all_not_shipped: set[str] = set()
    for name in ALL_TOMLS:
        path = os.path.join(REPO, "models", "builtin", f"{name}.toml")
        all_not_shipped |= not_shipped_ids(path)
        with open(path, "rb") as f:
            for t in tomllib.load(f).get("targets", []):
                all_remote_ids.add(
                    t.get("remote_model", t["model"].split("/", 1)[1])
                )

    # Discovery-first connections (openrouter, local endpoints) serve their
    # whole catalog live and unmapped-by-design; only curated lineups are
    # worth reporting.
    curated = {name for name, bl in blocks_of_interest.items() if bl}

    print("\n## live caches: unmapped served models (curated connections)")
    if os.path.isdir(LIVE_DIR):
        for fn in sorted(os.listdir(LIVE_DIR)):
            if not fn.endswith(".json"):
                continue
            conn = fn[:-5]
            if conn not in curated:
                continue
            if args and conn != args[0]:
                continue
            with open(os.path.join(LIVE_DIR, fn)) as f:
                data = json.load(f)
            unmapped = [
                str(lm.get("remote_model_id"))
                for lm in data.get("models") or []
                if not lm.get("model_id")
            ]
            if unmapped:
                annotated = [
                    f"{mid} (target exists — stale cache)"
                    if mid in all_remote_ids
                    else f"{mid} (deliberately not shipped)"
                    if mid in all_not_shipped
                    else mid
                    for mid in unmapped
                ]
                print(f"- `{conn}`: {', '.join(annotated)}")

    print(f"\n{problems} target(s) with unexplained findings")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
