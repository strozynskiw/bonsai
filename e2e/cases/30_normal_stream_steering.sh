#!/usr/bin/env bash
# A normal run must also receive follow-ups before stale tools execute.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
e2e_begin "30_normal_stream_steering: mid-run input reaches the active run"
provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
provider_release="$E2E_HOME/provider-release"
python3 "$E2E_LIB_DIR/mock_stream_steering_provider.py" "$provider_ready" "$provider_requests" "$provider_release" &
E2E_HELPER_PID=$!
for _ in {1..40}; do
  [ -s "$provider_ready" ] && break
  sleep 0.05
done
[ -s "$provider_ready" ] || { _fail "provider startup"; e2e_done; }
E2E_PROVIDER_BASE_URL="$(<"$provider_ready")"
tui_start 150 45 || e2e_done
tui_keys "/model" Enter
expect "model picker opens" "Filter models:"
tui_keys Left End Right
tui_keys "mock-model" Enter
expect_meta "mock model selected" "Coding · mock-model"
tui_keys "/yolo on" Enter
tui_keys "Explain STEERING_ORIGINAL" Enter
expect_meta "first response is streaming" "Enter send"
tui_keys "Also mention STEERING_FOLLOWUP." Enter
expect "follow-up is staged" "STEERING_FOLLOWUP"
touch "$provider_release"
expect "follow-up preempts the tool" "STEERING_APPLIED"
if python3 - "$provider_requests" <<'PY'
import json, sys
from pathlib import Path
requests = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines()]
assert len(requests) >= 2
messages = requests[1]["messages"]
assert any(message.get("role") == "user" and "STEERING_FOLLOWUP" in str(message.get("content", "")) for message in messages)
results = [message for message in messages if message.get("role") == "tool" and message.get("tool_call_id") == "stale-call"]
assert len(results) == 1 and "queued user message" in str(results[0].get("content", "")), results
assert "STALE_TOOL_EXECUTED" not in str(results[0].get("content", "")), results
PY
then _pass "normal follow-up reached the active run before the stale tool"
else _fail "normal follow-up delivery"; fi
cp "$provider_requests" "$E2E_EVIDENCE/provider-requests.jsonl"
tui_keys "/quit" Enter
e2e_done
