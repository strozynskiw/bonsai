#!/usr/bin/env bash
# A terse execution approval keeps the current task identity and the complete
# Coding tool surface.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
WAIT=0.3
e2e_begin "10_action_continuation: terse approval keeps Coding tools"

provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
python3 "$E2E_LIB_DIR/mock_completion_provider.py" \
  "$provider_ready" "$provider_requests" &
E2E_HELPER_PID=$!
for _ in {1..40}; do
  [ -s "$provider_ready" ] && break
  sleep 0.05
done
if [ ! -s "$provider_ready" ]; then
  _fail "mock provider did not start"
  e2e_done
fi
E2E_PROVIDER_BASE_URL="$(<"$provider_ready")"

tui_start 140 40 || e2e_done
tui_keys "/model" Enter
expect "model picker opens" "Filter models:"
tui_keys Left End Right
tui_keys "mock-model"
expect "mock model is filtered" "OpenAI Compatible · mock-model"
tui_keys Enter
expect_meta "mock model selected" "Coding · mock-model"

tui_keys "Fix issue 173" Enter
expect "substantive task completes" "deterministic request-1 complete"

tui_keys "DO it" Enter
expect "terse execution approval completes" "deterministic request-2 complete"

if python3 - "$provider_requests" "$E2E_BONSAI_HOME/bonsai.db" <<'PY'
import json
import sqlite3
import sys

request_log, database_path = sys.argv[1:]
with open(request_log, encoding="utf-8") as stream:
    requests = [json.loads(line) for line in stream if line.strip()]
if len(requests) != 2:
    raise SystemExit(f"expected 2 provider requests, got {len(requests)}")

tools = [
    tool.get("function", {}).get("name")
    for tool in requests[-1].get("tools", [])
]
for required in ("write", "edit", "bash"):
    if required not in tools:
        raise SystemExit(f"action continuation omitted {required}: {tools}")

with sqlite3.connect(database_path) as database:
    row = database.execute(
        """
        SELECT goal, goal_id FROM task_runs
        ORDER BY started_at_ms DESC, id DESC LIMIT 1
        """
    ).fetchone()
    if row is None or row[0] != "Fix issue 173":
        raise SystemExit(f"action continuation replaced the task goal: {row}")
    attempts = database.execute(
        "SELECT COUNT(*) FROM task_runs WHERE goal_id = ?", (row[1],)
    ).fetchone()[0]
    if attempts != 2:
        raise SystemExit(f"expected 2 substantive attempts, got {attempts}")
PY
then
  _pass "terse approval retains write/edit/bash and the task identity"
else
  _fail "terse approval request or persisted task identity was incorrect"
fi

tui_keys "/quit" Enter
e2e_done
