#!/usr/bin/env bash
# Enter during goal pursuit reaches the same run without requiring Escape.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
e2e_begin "21_goal_conversation: live follow-up and cumulative timer"

provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
provider_release="$E2E_HOME/provider-release"
python3 "$E2E_LIB_DIR/mock_goal_conversation_provider.py" "$provider_ready" "$provider_requests" "$provider_release" &
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
tui_keys "/goal Explain GOAL_CONVERSATION with code evidence" Enter
expect_meta "goal accepts conversation" "Enter send"
expect "goal timer is visible during pursuit" "goal active ·"
tui_keys "Discard WITHDRAWN_TOKEN" Enter
expect "withdrawable follow-up is staged" "WITHDRAWN_TOKEN"
tui_keys Up
expect "withdraw restores the draft" "Discard WITHDRAWN_TOKEN"
tui_keys C-u
tui_keys "Also explain FOLLOW_UP_TOKEN" Enter
expect "follow-up is staged" "Also explain FOLLOW_UP_TOKEN"
tui_keys "Include ADDITIONAL_TOKEN" Enter
expect "second follow-up is staged" "ADDITIONAL_TOKEN"
forbid "conversation does not interrupt the stream" "Run interrupted."
touch "$provider_release"
expect "same run consumes the follow-up" "goal-follow-up-received"
expect_meta "conversation finishes" "● Coding"
expect "completed goal keeps its timer" "goal complete ·"

if python3 - "$provider_requests" "$E2E_BONSAI_HOME/bonsai.db" <<'PY'
import json, sqlite3, sys
from pathlib import Path
requests = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines()]
assert len(requests) == 3, len(requests)
assert 'FOLLOW_UP_TOKEN' not in json.dumps(requests[0]['messages'])
assert 'FOLLOW_UP_TOKEN' in json.dumps(requests[1]['messages'])
assert 'ADDITIONAL_TOKEN' in json.dumps(requests[1]['messages'])
assert 'WITHDRAWN_TOKEN' not in json.dumps(requests[1]['messages'])
messages = json.dumps(requests[1]['messages'])
assert messages.index('FOLLOW_UP_TOKEN') < messages.index('ADDITIONAL_TOKEN')
with sqlite3.connect(sys.argv[2]) as db:
    goal_id, raw = db.execute('SELECT goal_id, record_json FROM session_goals WHERE is_current = 1').fetchone()
    assert json.loads(raw)['state'] == 'complete'
    runs = db.execute('SELECT outcome, wall_ms FROM task_runs WHERE goal_id = ?', (goal_id,)).fetchall()
    assert len(runs) == 1 and runs[0][0] == 'succeeded' and runs[0][1] > 0, runs
PY
then _pass "one goal, one run, follow-ups, and durable elapsed time"
else _fail "goal conversation evidence"; fi
cp "$provider_requests" "$E2E_EVIDENCE/provider-requests.jsonl"
tui_keys "/quit" Enter
e2e_done
