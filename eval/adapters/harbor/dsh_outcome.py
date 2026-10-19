"""Read the locked DSH durable events, never infer success from `final`."""

from dataclasses import dataclass
import json
from typing import Mapping


@dataclass(frozen=True)
class DshOutcome:
    state: str
    usage: dict | None
    telemetry_complete: bool


def parse_sessions(sessions: Mapping[str, str], root_session: str, exit_code: int) -> DshOutcome:
    """Aggregate each settled model attempt across fresh parent/child logs.

    Each mapping value is a complete persisted JSONL session file. Stream usage
    belongs to one attempt; assistant/message.usage is not summed a second time.
    A truncated log, missing attempt sample or unsettled step makes totals unknown.
    """
    if root_session not in sessions:
        return DshOutcome("agent_failure", None, False)
    samples = []
    root_reason = None
    complete = True
    valid_logs = True
    for session_id, text in sessions.items():
        open_steps = set()
        last_seq = None
        try:
            events = [json.loads(line) for line in text.splitlines() if line.strip()]
            for event in events:
                if not isinstance(event, dict):
                    raise ValueError("non-object session event")
                if "seq" in event:
                    seq = event["seq"]
                    if type(seq) is not int or (last_seq is not None and seq != last_seq + 1):
                        raise ValueError("noncontiguous session sequence")
                    last_seq = seq
                kind, data = event.get("type"), event.get("data", {})
                if not isinstance(data, dict):
                    raise ValueError("non-object session event data")
                if kind == "turn/end" and session_id == root_session:
                    root_reason = data["reason"]["kind"]
                if kind == "step/start":
                    open_steps.add((data["turn"], data["step"]))
                elif kind == "step/end":
                    open_steps.discard((data["turn"], data["step"]))
                elif kind in {"assistant/message", "assistant/attempt"}:
                    stream = data.get("stream", [])
                    if not isinstance(stream, list) or any(not isinstance(item, dict) for item in stream):
                        raise ValueError("invalid durable stream")
                    usages = []
                    for item in stream:
                        if item.get("type") == "chunk":
                            chunk = item.get("chunk")
                            if not isinstance(chunk, dict):
                                raise ValueError("invalid durable chunk")
                            if chunk.get("type") == "usage":
                                if not isinstance(chunk.get("usage"), dict):
                                    raise ValueError("invalid usage sample")
                                usages.append(chunk["usage"])
                    # The final usage sample of one attempt is authoritative.
                    usage = usages[-1] if usages else None
                    if usage is None or any(type(usage.get(k)) is not int or usage[k] < 0
                                            for k in ("inputTokens", "outputTokens")):
                        complete = False
                    else:
                        samples.append(usage)
            if open_steps:
                complete = False
                valid_logs = False
        except (ValueError, KeyError, TypeError):
            complete = False
            valid_logs = False
    usage = None
    if complete and samples:
        usage = {}
        for key in ("inputTokens", "outputTokens", "cacheReadTokens", "cacheWriteTokens", "reasoningTokens", "totalTokens"):
            if all(type(sample.get(key)) is int and sample[key] >= 0 for sample in samples):
                usage[key] = sum(sample[key] for sample in samples)
    state = "completed" if valid_logs and exit_code == 0 and root_reason == "completed" else {
        "aborted": "cancelled", "interrupted": "cancelled", "max-tokens": "budget_exhausted",
    }.get(root_reason, "agent_failure")
    return DshOutcome(state, usage, complete and bool(samples))
