#!/usr/bin/env bash
# Real terminal coverage for startup/configuration identity and task promotion.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
e2e_begin "11_session_classification: empty identities stay out of task sessions"

provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
python3 "$E2E_LIB_DIR/mock_completion_provider.py" "$provider_ready" "$provider_requests" &
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

database_assert() {
  local description="$1" query="$2" expected="$3" actual
  actual="$(python3 - "$E2E_BONSAI_HOME/bonsai.db" "$query" <<'PY'
import sqlite3
import sys
with sqlite3.connect(sys.argv[1]) as database:
    print(database.execute(sys.argv[2]).fetchone()[0])
PY
)"
  if [ "$actual" == "$expected" ]; then
    _pass "$description"
  else
    _fail "$description — expected '$expected', got '$actual'"
  fi
}

quit_and_wait() {
  tui_keys "/quit" Enter
  for _ in {1..40}; do
    tui_alive || return 0
    sleep 0.1
  done
  _fail "TUI did not exit cleanly"
}

tui_start 140 40 || e2e_done
tui_keys "/sessions" Enter
expect "normal picker excludes startup" "No prior sessions"
tui_keys Escape
tui_keys "/sessions all" Enter
expect "diagnostic view includes classification" "lifecycle_only"
database_assert "startup has no task runs" "SELECT COUNT(*) FROM task_runs" 0
database_assert "startup has no episode evidence" "SELECT COUNT(*) FROM episodes" 0
database_assert "startup has no todos" "SELECT COUNT(*) FROM todos" 0
quit_and_wait
database_assert "clean startup identity is retired" "SELECT COUNT(*) FROM sessions" 0

tui_start 140 40 || e2e_done
tui_keys "/model" Enter
expect "model picker opens" "Filter models:"
tui_keys Left End Right
tui_keys "mock-model"
expect "mock model is filtered" "OpenAI Compatible · mock-model"
tui_keys Enter
expect_meta "mock model selected" "Coding · mock-model"
tui_keys "/sessions all" Enter
expect "model configuration remains a probe" "provider_probe"
database_assert "probe has no task outcome" "SELECT COUNT(*) FROM task_runs" 0
database_assert "probe has no verification evidence" "SELECT COUNT(*) FROM verification_runs" 0
quit_and_wait
database_assert "clean provider-only identity is retired" "SELECT COUNT(*) FROM sessions" 0

tui_start 140 40 || e2e_done
identity="$(python3 - "$E2E_BONSAI_HOME/bonsai.db" <<'PY'
import sqlite3
import sys
with sqlite3.connect(sys.argv[1]) as database:
    row = database.execute("SELECT id, conversation_cache_key FROM sessions").fetchone()
    print(f"{row[0]}:{row[1]}")
PY
)"
tui_keys "Explain the project" Enter
expect "meaningful task reaches mock provider" "deterministic request-1 complete"
tui_keys "/sessions all" Enter
expect "promoted task is visible diagnostically" "Session diagnostics"
database_assert "one identity is promoted" "SELECT COUNT(*) FROM sessions WHERE kind = 'task'" 1
database_assert "id and cache key survive promotion" "SELECT id || ':' || conversation_cache_key FROM sessions" "$identity"
quit_and_wait
database_assert "task survives clean exit" "SELECT COUNT(*) FROM sessions WHERE kind = 'task'" 1

tui_start 140 40 || e2e_done
tui_keys "/sessions" Enter
expect "real task appears in normal picker" "Explain the project"
tui_keys Escape
quit_and_wait
database_assert "later empty startup does not accumulate" "SELECT COUNT(*) FROM sessions" 1
e2e_done
