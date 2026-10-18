#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
e2e_begin "17_goal_questions: hide, reopen, answer, and recommended timeout"
provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
python3 "$E2E_LIB_DIR/mock_goal_provider.py" "$provider_ready" "$provider_requests" &
E2E_HELPER_PID=$!
for _ in {1..40}; do
  [ -s "$provider_ready" ] && break
  sleep 0.05
done
[ -s "$provider_ready" ] || { _fail "provider startup"; e2e_done; }
E2E_PROVIDER_BASE_URL="$(<"$provider_ready")"
tui_start 150 45 || e2e_done
tui_keys "/model" Enter
tui_keys Left End Right
tui_keys "mock-model" Enter
expect_meta "mock model selected" "Coding · mock-model"
tui_keys "/goal Explain cache choices for manual-flow" Enter
expect "question opens while the run remains active" "Select cache strategy"
expect "recommended choice is visible" "Persistent (Recommended)"
tui_keys Tab
expect_meta "hiding the question exposes the composer" "Coding · mock-model"
tui_keys "/questions" Enter
expect "pending question reopens" "Select cache strategy"
tui_keys Down Enter
expect "answer is consumed by the agent" "goal-question-answer-delivered-manual-flow"
expect_meta "answered goal finishes" "● Coding"
tui_keys "/goal Explain cache choices for timeout-flow" Enter
expect "timeout question opens" "Select cache strategy (timeout-flow)"
expect "timeout choice returns to the agent" "goal-question-answer-delivered-timeout-flow"
expect_meta "timed goal finishes" "● Coding"
cp "$provider_requests" "$E2E_EVIDENCE/provider-requests.jsonl"

if python3 - "$provider_requests" "$E2E_BONSAI_HOME/bonsai.db" <<'PY'
import json, sqlite3, sys
requests = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8")]
text = "\n".join(str(message.get("content", "")) for request in requests for message in request["messages"])
assert "Selected: Persistent" in text, "manual answer missing from model context"
assert "Automatically assumed recommended option: Persistent" in text, "timed recommendation missing from model context"
assert len(requests) <= 8, len(requests)
with sqlite3.connect(sys.argv[2]) as db:
    record = json.loads(db.execute("SELECT record_json FROM session_goals WHERE is_current = 1").fetchone()[0])
    assert record["state"] == "complete", record
PY
then _pass "manual and timed answers reach the model, and completion is bounded"
else _fail "question delivery or goal completion evidence"; fi
tui_keys "/quit" Enter
e2e_done
