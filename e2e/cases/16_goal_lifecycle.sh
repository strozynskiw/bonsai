#!/usr/bin/env bash
# Goal controls must stay responsive while a real provider stream owns Agent.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
e2e_begin "16_goal_lifecycle: busy controls, replacement, and restore"

provider_ready="$E2E_HOME/provider-url"
python3 "$E2E_LIB_DIR/mock_streaming_provider.py" "$provider_ready" &
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

tui_keys "/go" Tab
expect_input "goal command completion" "/goal"
tui_keys C-u "/goal Hold the original objective" Enter
expect "goal starts" "Goal set: Hold the original objective"
expect_meta "goal has a busy run" "Enter queue"
tui_keys "/goal status" Enter
expect "busy status preserves objective" "Hold the original objective"
expect "busy status includes limits" "Limits:"
tui_keys "/goal pause" Enter
expect "pause remains responsive" "Goal paused."
expect "foreground interruption settles" "Run interrupted."
tui_keys "/goal resume" Enter
expect "resume is explicit" "Goal resumed."
expect_meta "resume starts work" "Enter queue"
tui_keys "Prioritize the export test" Enter
tui_keys Escape
expect_meta "Esc steering starts the replacement turn" "Enter queue"
tui_keys "/goal status" Enter
expect "Esc steering preserves the active objective" "Hold the original objective"
expect "steered goal remains active" "· active"
tui_keys "Keep the regression test" Enter
tui_keys "/goal pause" Enter
expect_meta "pause holds queued steering" "● Coding"
tui_keys "/goal resume" Enter
expect_meta "resume continues with held steering" "Enter queue"
tui_keys "/goal Hold the replacement objective" Enter
expect "replacement accepted mid-stream" "Goal set: Hold the replacement objective"
expect_meta "replacement has one foreground run" "Enter queue"
tui_keys "/goal pause" Enter
expect_meta "pause leaves the app idle" "● Coding"

session_id="$(python3 - "$E2E_BONSAI_HOME/bonsai.db" <<'PY'
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    print(db.execute("SELECT session_id FROM session_goals WHERE is_current = 1").fetchone()[0])
PY
)"
tui_keys "/quit" Enter
E2E_BONSAI_ARGS=(-c "$session_id")
tui_start 150 45 || e2e_done
tui_keys "/goal status" Enter
expect "objective survives restart" "Hold the replacement objective"
expect "restored goal stays paused" "· paused"
expect_meta "restore does not start work" "● Coding"
tui_keys "/questions" Enter
expect "pending questions command exists" "No pending questions."
tui_keys "/goal clear" Enter
expect "clear retains history" "Goal cleared; history retained."
tui_keys "/goal" Enter
expect "cleared goal has no objective" "No session goal."

if python3 - "$E2E_BONSAI_HOME/bonsai.db" <<'PY'
import json, sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    rows = db.execute("SELECT goal_id, is_current, record_json FROM session_goals").fetchall()
    assert len(rows) == 2, rows
    assert not any(row[1] for row in rows), rows
    for goal_id, _, raw in rows:
        record = json.loads(raw)
        assert record['id'] == goal_id
        assert db.execute("SELECT COUNT(*) FROM task_runs WHERE goal_id = ?", (goal_id,)).fetchone()[0] > 0
    assert db.execute("SELECT COUNT(*) FROM task_runs WHERE outcome = 'superseded'").fetchone()[0] >= 1
PY
then _pass "goal history and task evidence preserve both objective identities"
else _fail "goal persistence or task identity"; fi
tui_keys "/quit" Enter
e2e_done
