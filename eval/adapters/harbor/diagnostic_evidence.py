"""Bounded private checkpoint evidence validation; never completion proof."""

import json
import re


def validate_receipts(raw: str, *, expected_model: str) -> dict:
    if len(raw.encode()) > 16384:
        raise ValueError("diagnostic receipt bound exceeded")
    try:
        rows = [json.loads(line) for line in raw.splitlines()]
    except (ValueError, TypeError) as error:
        raise ValueError("invalid diagnostic receipt JSON") from error
    if not 2 <= len(rows) <= 5 or not all(isinstance(row, dict) for row in rows):
        raise ValueError("incomplete diagnostic receipt stream")
    first, last = rows[0], rows[-1]
    if (first.get("type") != "identity" or first.get("schema") != 1
            or first.get("boundary") != "agent_to_provider" or first.get("request_ordinal") != 1
            or first.get("model") != expected_model or last.get("type") != "summary"
            or last.get("capture_valid") is not True or type(last.get("requests")) is not int
            or not 1 <= last["requests"] <= 200
            or not isinstance(last.get("stages"), list) or len(last["stages"]) != 3
            or not all(type(stage) is bool for stage in last["stages"])):
        raise ValueError("diagnostic identity/summary mismatch")
    seen = set()
    previous = 1
    for row in rows[1:-1]:
        stage, ordinal = row.get("stage"), row.get("request_ordinal")
        if (row.get("type") != "checkpoint" or type(stage) is not int or stage not in (1, 2, 3)
                or stage in seen or type(ordinal) is not int or not previous <= ordinal <= last["requests"]
                or not isinstance(row.get("text"), str) or len(row["text"]) > 2048
                or not row["text"].startswith(f"Harness note: Delivery checkpoint {stage}/3: ")
                or not isinstance(row.get("message_sha256"), str)
                or not re.fullmatch(r"[0-9a-f]{64}", row["message_sha256"])):
            raise ValueError("invalid diagnostic checkpoint receipt")
        seen.add(stage)
        previous = ordinal
    for row in rows[:-1]:
        if (not isinstance(row.get("request_sha256"), str)
                or not re.fullmatch(r"[0-9a-f]{64}", row["request_sha256"])):
            raise ValueError("missing diagnostic request fingerprint")
    if last["stages"] != [stage in seen for stage in (1, 2, 3)]:
        raise ValueError("diagnostic checkpoint summary mismatch")
    if last["requests"] == 200 and seen != {1, 2, 3}:
        raise ValueError("expected checkpoints missing at diagnostic hard limit")
    return {"requests": last["requests"], "stages": sorted(seen), "boundary": "agent_to_provider"}
