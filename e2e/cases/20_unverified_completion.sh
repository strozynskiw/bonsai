#!/usr/bin/env bash
# Missing verification remains non-success without looking like execution failed.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
e2e_begin "20_unverified_completion: missing evidence is not execution failure"

provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
python3 "$E2E_LIB_DIR/mock_completion_provider.py" "$provider_ready" "$provider_requests" --unverified &
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

tui_start 160 50 || e2e_done
tui_keys "/model openai-compatible:mock-model" Enter
expect_meta "mock model selected" "Coding · mock-model"
tui_keys "Verify the project" Enter
expect "missing evidence is explained without rejection" "Work completed; verification not confirmed"
expect "session has an unverified status" "#1 unverified"
forbid "missing evidence is not rejection" "Completion rejected"
forbid "missing evidence is not a failed report" "✗ completion"

tui_keys "/quit" Enter || true
for _ in {1..40}; do
  tui_alive || break
  sleep 0.1
done
actual="$(sqlite3 "$E2E_BONSAI_HOME/bonsai.db" "SELECT status FROM sessions WHERE kind = 'task' ORDER BY id DESC LIMIT 1;")"
if [ "$actual" == "unverified" ]; then
  _pass "unverified lifecycle survives clean exit"
else
  _fail "expected unverified session, got '$actual'"
fi
actual="$(sqlite3 "$E2E_BONSAI_HOME/bonsai.db" "SELECT outcome FROM task_runs ORDER BY id DESC LIMIT 1;")"
if [ "$actual" == "blocked" ]; then
  _pass "verification safeguard keeps task non-success"
else
  _fail "expected blocked task, got '$actual'"
fi
e2e_done
