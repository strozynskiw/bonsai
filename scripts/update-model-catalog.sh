#!/usr/bin/env sh
set -eu

MODE="${1:---dry-run}"
URL="${BONSAI_MODELS_DEV_URL:-https://models.dev/api.json}"
CACHE="${BONSAI_MODELS_DEV_CACHE:-$HOME/.bonsai/cache/models-dev.json}"
# Second-opinion pricing source for cross-validation (best-effort). LiteLLM
# tracks direct-provider prices, so gross divergence flags a likely models.dev
# data error (a misplaced decimal), not resale markup.
LITELLM_URL="${BONSAI_LITELLM_URL:-https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json}"
TMP="$(mktemp)"
LITELLM_TMP="$(mktemp)"

cleanup() {
  rm -f "$TMP" "$LITELLM_TMP"
}
trap cleanup EXIT

case "$MODE" in
  --check|check|--dry-run|dry-run|--write|write)
    ;;
  *)
    printf 'Usage: scripts/update-model-catalog.sh [--check|--dry-run|--write]\n' >&2
    exit 2
    ;;
esac

curl -fsSL "$URL" -o "$TMP"
cargo run --quiet -- model-catalog check "$TMP"
# Best-effort: a fetch failure leaves an empty file and cross-validation degrades
# to sanity checks only, never blocking the refresh.
curl -fsSL "$LITELLM_URL" -o "$LITELLM_TMP" || : >"$LITELLM_TMP"

python3 - "$TMP" "$MODE" "$LITELLM_TMP" <<'PY'
import json
import pathlib
import re
import sys
from decimal import Decimal, InvalidOperation, ROUND_HALF_UP

models_dev_path = pathlib.Path(sys.argv[1])
mode = sys.argv[2]
litellm_path = pathlib.Path(sys.argv[3]) if len(sys.argv) > 3 else None
write = mode in {"--write", "write"}
check = mode in {"--check", "check"}
import os

strict_crosscheck = os.environ.get("BONSAI_STRICT_CROSSCHECK") == "1"

root = pathlib.Path("models/builtin")
data = json.loads(models_dev_path.read_text())

models = {}
skipped = 0


def positive_int(value):
    return value if isinstance(value, int) and value >= 0 else None


def micros_per_million(value):
    if isinstance(value, bool) or value is None:
        return None
    if isinstance(value, (int, float, str)):
        try:
            price = Decimal(str(value))
        except InvalidOperation:
            return None
        if price.is_nan() or price.is_infinite() or price < 0:
            return None
        return int((price * Decimal(1_000_000)).to_integral_value(rounding=ROUND_HALF_UP))
    return None


def pricing_from_cost(cost):
    if not isinstance(cost, dict):
        return None
    input_price = micros_per_million(cost.get("input"))
    output_price = micros_per_million(cost.get("output"))
    if input_price is None or output_price is None:
        return None

    pricing = {
        "input_micros_per_million": input_price,
        "output_micros_per_million": output_price,
    }
    cache_read = micros_per_million(cost.get("cache_read"))
    if cache_read is not None:
        pricing["cache_read_micros_per_million"] = cache_read
    cache_write = micros_per_million(cost.get("cache_write"))
    if cache_write is not None:
        pricing["cache_write_micros_per_million"] = cache_write
    return pricing


EFFORT_ORDER = {
    "minimal": 0,
    "low": 1,
    "medium": 2,
    "high": 3,
    "xhigh": 4,
    "max": 5,
}


def dedup(values):
    seen = set()
    out = []
    for value in values:
        if value not in seen:
            seen.add(value)
            out.append(value)
    return out


def reasoning_options_from_labels(labels):
    normalized = []
    for label in labels:
        if isinstance(label, str):
            normalized.append(label.strip().lower())

    has_toggle = any(label in {"none", "off"} for label in normalized)
    efforts = dedup(
        label
        for label in normalized
        if label in {"minimal", "low", "medium", "high", "xhigh", "max"}
    )

    options = []
    if has_toggle:
        options.append({"type": "toggle"})
    if efforts:
        options.append({"type": "effort", "values": efforts})
    return options


def normalize_reasoning_options(raw_options):
    normalized = []
    for option in raw_options:
        if not isinstance(option, dict):
            continue
        option_type = option.get("type")
        if option_type == "toggle":
            normalized.append({"type": "toggle"})
        elif option_type == "effort":
            values = option.get("values")
            if isinstance(values, list):
                labels = ["none" if value is None else value for value in values]
                normalized.extend(reasoning_options_from_labels(labels))
        elif option_type == "budget_tokens":
            budget = {"type": "budget_tokens"}
            minimum = positive_int(option.get("min"))
            if minimum is not None:
                budget["min"] = minimum
            maximum = positive_int(option.get("max"))
            if maximum is not None:
                budget["max"] = maximum
            default = positive_int(option.get("default"))
            if default is not None:
                budget["default"] = default
            normalized.append(budget)
        elif isinstance(option_type, str) and option_type:
            normalized.append({"type": option_type})
    return normalized


def reasoning_label_from_value(value):
    if isinstance(value, str):
        return value
    if isinstance(value, dict):
        for key in (
            "effort",
            "reasoningEffort",
            "reasoning_effort",
            "thinkingLevel",
            "thinking_level",
        ):
            label = reasoning_label_from_value(value.get(key))
            if label:
                return label
    return None


def reasoning_options_from_modes(raw_model):
    experimental = raw_model.get("experimental")
    modes = experimental.get("modes") if isinstance(experimental, dict) else None
    if not isinstance(modes, dict):
        return []

    labels = []
    for mode_id, mode in modes.items():
        if isinstance(mode_id, str):
            labels.append(mode_id)

        provider = mode.get("provider") if isinstance(mode, dict) else None
        body = provider.get("body") if isinstance(provider, dict) else None
        if not isinstance(body, dict):
            continue

        for key in ("reasoningEffort", "reasoning_effort", "effort"):
            label = reasoning_label_from_value(body.get(key))
            if label:
                labels.append(label)

        for key in ("reasoning", "thinking", "output_config", "outputConfig"):
            label = reasoning_label_from_value(body.get(key))
            if label:
                labels.append(label)

    return reasoning_options_from_labels(labels)


def own_reasoning_options(raw_model):
    """Reasoning options that this model's own Models.dev row declares."""
    reasoning_options = raw_model.get("reasoning_options")
    if not isinstance(reasoning_options, list):
        reasoning_options = []
    reasoning_options = normalize_reasoning_options(reasoning_options)
    if reasoning_options:
        return reasoning_options
    return reasoning_options_from_modes(raw_model)


def reasoning_options_key(options):
    """Stable, effort-order-normalized signature for a set of reasoning options."""
    parts = []
    for option in options:
        option_type = option.get("type")
        if option_type == "effort":
            values = sorted(
                set(option.get("values", [])),
                key=lambda value: EFFORT_ORDER.get(value, len(EFFORT_ORDER)),
            )
            parts.append("e:" + ",".join(values))
        elif option_type == "budget_tokens":
            parts.append(f"b:{option.get('min')}-{option.get('max')}")
        elif option_type:
            parts.append(str(option_type))
    return ";".join(parts)


def canonical_reasoning_options(options):
    """Re-emit options with efforts in canonical (ascending) order."""
    canonical_options = []
    for option in options:
        if option.get("type") == "effort":
            values = sorted(
                set(option.get("values", [])),
                key=lambda value: EFFORT_ORDER.get(value, len(EFFORT_ORDER)),
            )
            canonical_options.append({"type": "effort", "values": values})
        else:
            canonical_options.append(option)
    return canonical_options


def model_name_of(canonical):
    return canonical.split("/", 1)[1].lower()


def build_home_reasoning(data):
    """Map bare model name -> reasoning options most direct providers agree on.

    A direct provider is one that hosts the model under its own namespace (the
    Models.dev row id has no foreign `provider/` prefix). When a model's own row
    is silent, this lets us reuse the reasoning capabilities that authoritative
    providers publish for the same model, rather than inventing them.
    """
    candidates = {}
    for provider_id, provider in data.items():
        provider_models = provider.get("models") if isinstance(provider, dict) else None
        if not isinstance(provider_models, dict):
            continue
        for fallback_id, raw_model in provider_models.items():
            if not isinstance(raw_model, dict):
                continue
            raw_id = str(raw_model.get("id") or fallback_id)
            canonical = raw_id if "/" in raw_id else f"{provider_id}/{raw_id}"
            if canonical.count("/") != 1:
                continue
            if canonical.split("/", 1)[0] != provider_id:
                continue  # router/reseller copy, not an authoritative source
            options = own_reasoning_options(raw_model)
            if not options:
                continue
            name = model_name_of(canonical)
            key = reasoning_options_key(options)
            group = candidates.setdefault(name, {}).setdefault(
                key, {"options": canonical_reasoning_options(options), "providers": set()}
            )
            group["providers"].add(provider_id)

    home = {}
    for name, groups in candidates.items():
        winner = min(
            groups.items(),
            key=lambda item: (-len(item[1]["providers"]), item[0]),
        )
        home[name] = winner[1]["options"]
    return home


home_reasoning = build_home_reasoning(data)


for provider_id, provider in data.items():
    provider_models = provider.get("models") if isinstance(provider, dict) else None
    if not isinstance(provider_models, dict):
        continue
    for fallback_id, raw_model in provider_models.items():
        if not isinstance(raw_model, dict):
            continue
        raw_id = str(raw_model.get("id") or fallback_id)
        canonical = raw_id if "/" in raw_id else f"{provider_id}/{raw_id}"
        if canonical.count("/") != 1:
            skipped += 1
            continue
        limit = raw_model.get("limit") if isinstance(raw_model.get("limit"), dict) else {}
        context = limit.get("context")
        output = limit.get("output")
        reasoning_options = own_reasoning_options(raw_model)
        if not reasoning_options and raw_model.get("reasoning") is True:
            reasoning_options = home_reasoning.get(model_name_of(canonical), [])

        metadata = {
            "context_window": positive_int(context),
            "output_limit": positive_int(output),
            "pricing": pricing_from_cost(raw_model.get("cost")),
            "reasoning": raw_model.get("reasoning") is True or bool(reasoning_options),
            "reasoning_options": reasoning_options,
            "_direct": canonical.split("/", 1)[0] == provider_id,
        }
        existing = models.get(canonical)
        if existing is None or metadata["_direct"] or not existing.get("_direct", False):
            models[canonical] = metadata

target_header = re.compile(r"^\[\[targets\]\]\s*$")
string_field = re.compile(r'^(metadata_model|model)\s*=\s*"([^"]+)"\s*$')
number_field = re.compile(r"^(context_window|output_limit)\s*=\s*(\d+)\s*$")
pricing_header = re.compile(r"^\[targets\.pricing\]\s*$")
pricing_inline = re.compile(r"^pricing\s*=")
reasoning_options_header = re.compile(r"^\[\[targets\.reasoning_options\]\]\s*$")
reasoning_options_inline = re.compile(r"^reasoning_options\s*=")


def target_key(lines):
    model = None
    metadata_model = None
    for line in lines:
        match = string_field.match(line.strip())
        if not match:
            continue
        if match.group(1) == "model":
            model = match.group(2)
        elif match.group(1) == "metadata_model":
            metadata_model = match.group(2)
    return metadata_model or model


def current_numbers(lines):
    values = {}
    for line in lines:
        match = number_field.match(line.strip())
        if match:
            values[match.group(1)] = int(match.group(2))
    return values


def set_number(lines, name, value):
    line_re = re.compile(rf"^{name}\s*=")
    rendered = f"{name} = {value}\n"
    for index, line in enumerate(lines):
        if line_re.match(line):
            lines[index] = rendered
            return

    if name == "output_limit":
        anchors = ("context_window", "token_counter", "endpoint_path", "remote_model", "model")
    else:
        anchors = ("token_counter", "endpoint_path", "transport", "remote_model", "model")
    for anchor in anchors:
        for index in range(len(lines) - 1, -1, -1):
            if lines[index].startswith(f"{anchor} ="):
                lines.insert(index + 1, rendered)
                return
    lines.append(rendered)


def remove_reasoning_options(lines):
    cleaned = []
    index = 0
    while index < len(lines):
        stripped = lines[index].strip()
        if reasoning_options_inline.match(stripped):
            index += 1
            continue
        if reasoning_options_header.match(stripped):
            index += 1
            while index < len(lines):
                next_stripped = lines[index].strip()
                if target_header.match(next_stripped) or reasoning_options_header.match(next_stripped):
                    break
                index += 1
            continue
        cleaned.append(lines[index])
        index += 1
    return cleaned


def remove_pricing(lines):
    cleaned = []
    index = 0
    while index < len(lines):
        stripped = lines[index].strip()
        if pricing_inline.match(stripped):
            index += 1
            continue
        if pricing_header.match(stripped):
            index += 1
            while index < len(lines):
                next_stripped = lines[index].strip()
                if target_header.match(next_stripped) or next_stripped.startswith("["):
                    break
                index += 1
            continue
        cleaned.append(lines[index])
        index += 1
    return cleaned


def json_string(value):
    return json.dumps(str(value), ensure_ascii=False)


def render_pricing(metadata):
    pricing = metadata["pricing"]
    if not pricing:
        return None

    fields = [
        ("input_micros_per_million", pricing.get("input_micros_per_million")),
        ("output_micros_per_million", pricing.get("output_micros_per_million")),
        ("cache_read_micros_per_million", pricing.get("cache_read_micros_per_million")),
        ("cache_write_micros_per_million", pricing.get("cache_write_micros_per_million")),
    ]
    rendered = [
        f"{name} = {value}" for name, value in fields if isinstance(value, int)
    ]
    if len(rendered) < 2:
        return None
    return "pricing = { " + ", ".join(rendered) + " }\n"


def append_pricing(lines, metadata):
    rendered = render_pricing(metadata)
    if rendered is None:
        return

    while lines and not lines[-1].strip():
        lines.pop()
    lines.append(rendered)


def render_reasoning_options(metadata, trailing_blank):
    raw_options = metadata["reasoning_options"] if metadata["reasoning"] else []
    if not raw_options:
        return ["reasoning_options = []\n"]

    rendered = []
    for option in raw_options:
        if not isinstance(option, dict):
            continue
        option_type = option.get("type")
        if not isinstance(option_type, str) or not option_type:
            continue
        rendered.append("[[targets.reasoning_options]]\n")
        rendered.append(f"type = {json_string(option_type)}\n")
        values = option.get("values")
        if isinstance(values, list):
            string_values = [
                "none" if value is None else value
                for value in values
                if value is None or isinstance(value, str)
            ]
            if string_values:
                rendered.append(
                    "values = ["
                    + ", ".join(json_string(value) for value in string_values)
                    + "]\n"
                )
        minimum = positive_int(option.get("min"))
        if minimum is not None:
            rendered.append(f"min = {minimum}\n")
        maximum = positive_int(option.get("max"))
        if maximum is not None:
            rendered.append(f"max = {maximum}\n")
        default = option.get("default")
        if isinstance(default, int):
            rendered.append(f"default = {default}\n")
        rendered.append("\n")

    if not trailing_blank:
        while rendered and not rendered[-1].strip():
            rendered.pop()

    return rendered or ["reasoning_options = []\n"]


def append_reasoning_options(lines, metadata, trailing_blank):
    while lines and not lines[-1].strip():
        lines.pop()

    rendered = render_reasoning_options(metadata, trailing_blank)
    if rendered and rendered[0].startswith("[[targets.reasoning_options]]"):
        lines.append("\n")
        lines.extend(rendered)
    else:
        lines.extend(rendered)
        if trailing_blank:
            lines.append("\n")


def split_targets(content):
    chunks = []
    current = []
    for line in content.splitlines(keepends=True):
        if target_header.match(line.strip()):
            if current:
                chunks.append(current)
            current = [line]
        else:
            current.append(line)
    if current:
        chunks.append(current)
    return chunks


def litellm_price_index(path):
    """Index LiteLLM's public price sheet for cross-validation.

    Returns (full, bare): `full` keyed by the entry's own id, `bare` keyed by
    the last path segment so a models.dev `provider/model` can match LiteLLM's
    route-prefixed ids. Bare names with conflicting prices are dropped to avoid
    false matches. Prices are micro-USD per million tokens, matching models.dev.
    """
    if path is None or not path.exists():
        return {}, {}
    try:
        raw = json.loads(path.read_text())
    except (OSError, ValueError):
        return {}, {}

    def to_micros(value):
        if not isinstance(value, (int, float)) or isinstance(value, bool):
            return None
        micros = int(round(float(value) * 1_000_000 * 1_000_000))
        return micros if micros >= 0 else None

    full = {}
    bare = {}
    conflict = set()
    for key, entry in raw.items():
        if not isinstance(entry, dict):
            continue
        inp = to_micros(entry.get("input_cost_per_token"))
        out = to_micros(entry.get("output_cost_per_token"))
        if inp is None or out is None:
            continue
        price = (inp, out)
        full[key.lower()] = price
        name = key.split("/")[-1].lower()
        if name in bare and bare[name] != price:
            conflict.add(name)
        else:
            bare[name] = price
    for name in conflict:
        bare.pop(name, None)
    return full, bare


def crosscheck_shipped(shipped, litellm_full, litellm_bare):
    """Data-quality warnings for the models we actually ship a target for."""
    warnings = []
    for key in sorted(shipped):
        pricing = shipped[key].get("pricing")
        # (a) A model models.dev lists but cannot price.
        if pricing is None:
            warnings.append(f"{key}: no Models.dev price for a shipped model")
            continue
        inp = pricing["input_micros_per_million"]
        out = pricing["output_micros_per_million"]
        # (b) Output cheaper than input is almost always a data error.
        if out < inp:
            warnings.append(
                f"{key}: output ${out / 1e6:.2f}/M < input ${inp / 1e6:.2f}/M (suspicious)"
            )
        # (c) Second-opinion divergence past an order-of-magnitude-ish band —
        # a misplaced decimal, not vendor-to-vendor variation.
        name = key.split("/", 1)[1].lower()
        other = litellm_full.get(key.lower()) or litellm_bare.get(name)
        if other and inp > 0 and other[0] > 0:
            ratio = inp / other[0]
            if ratio > 2 or ratio < 0.5:
                warnings.append(
                    f"{key}: input ${inp / 1e6:.2f}/M vs LiteLLM ${other[0] / 1e6:.2f}/M "
                    f"({ratio:.1f}x)"
                )
    return warnings


changes = []
missing = []
shipped = {}
for path in sorted(root.glob("*.toml")):
    original = path.read_text()
    rewritten_chunks = []
    changed = False
    chunks = split_targets(original)
    for chunk_index, chunk in enumerate(chunks):
        if not chunk or not target_header.match(chunk[0].strip()):
            rewritten_chunks.append(chunk)
            continue

        header = chunk[:1]
        original_lines = chunk[1:]
        lines = list(original_lines)
        lines = remove_reasoning_options(lines)
        lines = remove_pricing(lines)

        key = target_key(lines)
        if key is None:
            missing.append(f"{path}: target has no model")
            rewritten_chunks.append(header + lines)
            continue
        metadata = models.get(key)
        if metadata is None:
            missing.append(f"{path}: {key} missing from Models.dev")
            rewritten_chunks.append(header + lines)
            continue
        shipped[key] = metadata

        current = current_numbers(lines)
        number_changed = False
        for field in ("context_window", "output_limit"):
            value = metadata[field]
            if value is None:
                missing.append(f"{path}: {key} missing Models.dev {field}")
                continue
            if current.get(field) != value:
                changes.append(
                    f"{path}: {key} {field} {current.get(field, 'missing')} -> {value}"
                )
                set_number(lines, field, value)
                number_changed = True
                changed = True

        append_pricing(lines, metadata)
        append_reasoning_options(lines, metadata, chunk_index + 1 < len(chunks))
        if lines != original_lines:
            if not number_changed:
                changes.append(f"{path}: {key} metadata refreshed")
            changed = True

        rewritten_chunks.append(header + lines)

    rewritten = "".join("".join(chunk) for chunk in rewritten_chunks)
    if changed and write:
        path.write_text(rewritten)

litellm_full, litellm_bare = litellm_price_index(litellm_path)
crosscheck = crosscheck_shipped(shipped, litellm_full, litellm_bare)
if crosscheck:
    print("Cross-validation warnings:", file=sys.stderr)
    for item in crosscheck:
        print(f"  {item}", file=sys.stderr)
    # Informational by default so legitimate vendor differences never block a
    # refresh; opt into a hard gate for CI with BONSAI_STRICT_CROSSCHECK=1.
    if strict_crosscheck:
        sys.exit(1)

if missing:
    for item in missing:
        print(item, file=sys.stderr)
    sys.exit(1)

if changes:
    for item in changes:
        print(item)
    if check:
        sys.exit(1)
else:
    if skipped:
        print(f"Models.dev skipped {skipped} invalid model ids")
    print("Built-in model catalog is up to date")
PY

case "$MODE" in
  --write|write)
    mkdir -p "$(dirname "$CACHE")"
    cp "$TMP" "$CACHE"
    printf 'Updated %s\n' "$CACHE"
    ;;
esac
