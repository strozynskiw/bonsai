#!/usr/bin/env bash
# Goals survive transient provider outages and retain follow-up messages.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=160
e2e_begin "31_goal_outage_recovery: retry and retain user input"
ready="$E2E_HOME/provider-url"
requests="$E2E_HOME/provider-requests.jsonl"
python3 "$E2E_LIB_DIR/mock_goal_outage_provider.py" "$ready" "$requests" &
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
tui_keys "/goal Explain the outage recovery" Enter
expect "provider retry is visible" "retrying"
tui_keys "Also explain OUTAGE_FOLLOWUP" Enter
expect "follow-up is retained" "OUTAGE_FOLLOWUP"
expect "goal recovers after the ordinary retry limit" "OUTAGE_RECOVERED"
expect "goal finishes after recovery" "goal complete ·"
if python3 - "$requests" <<'PY'
import json, sys
from pathlib import Path
requests = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines()]
assert len(requests) >= 7, len(requests)
assert "OUTAGE_FOLLOWUP" in json.dumps(requests[5]["messages"])
PY
then _pass "recovered goal received the queued follow-up"
else _fail "goal outage recovery evidence"; fi
cp "$requests" "$E2E_EVIDENCE/provider-requests.jsonl"
tui_keys "/quit" Enter
e2e_done
