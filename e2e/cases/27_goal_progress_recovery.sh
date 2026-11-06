#!/usr/bin/env bash
# A partial goal report must continue before the goal is marked complete.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
e2e_begin "27_goal_progress_recovery: partial goal work continues"

ready="$E2E_HOME/provider-url"
requests="$E2E_HOME/provider-requests.jsonl"
python3 "$E2E_LIB_DIR/mock_goal_progress_provider.py" "$ready" "$requests" &
E2E_HELPER_PID=$!
for _ in {1..40}; do [ -s "$ready" ] && break; sleep 0.05; done
[ -s "$ready" ] || { _fail "provider startup"; e2e_done; }
E2E_PROVIDER_BASE_URL="$(<"$ready")"
tui_start 150 45 || e2e_done
tui_keys "/model" Enter
expect "model picker opens" "Filter models:"
tui_keys Left End Right
tui_keys "mock-model" Enter
expect_meta "mock model selected" "Coding · mock-model"
tui_keys "/goal Review README.md, lib.sh, run.sh and verifier.sh and report findings for all four files" Enter
expect "remaining work was completed" "All four requested files were reviewed"
expect "goal completed after recovery" "goal complete ·"

if python3 - "$requests" <<'PY'
import json, sys
from pathlib import Path
requests = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines()]
assert len(requests) == 9, len(requests)
assert any(tool.get("function", {}).get("name") == "goal_complete" for tool in requests[0]["tools"])
for index in (2, 4, 6):
    assert "goal response reports unfinished work" in json.dumps(requests[index]["messages"])
tool_results = [m for m in requests[7]["messages"] if m.get("role") == "tool"]
assert len(tool_results) == 4, len(tool_results)
assert all("error" not in str(m.get("content", "")).lower()[:100] for m in tool_results)
assert any(call.get("function", {}).get("name") == "goal_complete"
           for message in requests[8]["messages"] for call in message.get("tool_calls", []))
PY
then _pass "recovery inspected the remaining files in the same run"
else _fail "goal recovery evidence"; fi
cp "$requests" "$E2E_EVIDENCE/provider-requests.jsonl"
tui_keys "/quit" Enter
e2e_done
