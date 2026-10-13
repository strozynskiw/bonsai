#!/usr/bin/env bash
# Real terminal coverage for inspection batching (#143): the advisory must
# reach the wire after a singleton streak, declarations must be recorded, and
# the per-turn batch diagnostics must persist.
#
# Run via ./e2e/run.sh cases/12_inspection_batching.sh — the scripted provider
# reads repo-relative paths, so the pane's cwd must be this directory.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=120
e2e_begin "${E2E_BATCHING_CASE_NAME:-12_inspection_batching: advisory, declarations, and diagnostics}"

provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
if [ "${E2E_REPEAT_CALL_IDS:-0}" == 1 ]; then
  python3 "$E2E_LIB_DIR/mock_batching_provider.py" "$provider_ready" "$provider_requests" --repeat-call-ids &
else
  python3 "$E2E_LIB_DIR/mock_batching_provider.py" "$provider_ready" "$provider_requests" &
fi
E2E_HELPER_PID=$!
for _ in {1..40}; do
  [ -s "$provider_ready" ] && break
  sleep 0.05
done
if [ ! -s "$provider_ready" ]; then
  _fail "scripted provider did not start"
  e2e_done
fi
E2E_PROVIDER_BASE_URL="$(<"$provider_ready")"

database_assert() {
  local description="$1" query="$2" expected="$3" actual
  actual="$(python3 - "$E2E_BONSAI_HOME/bonsai.db" "$query" <<'PY'
import sqlite3
import sys
with sqlite3.connect(sys.argv[1]) as database:
    row = database.execute(sys.argv[2]).fetchone()
    print("" if row is None else row[0])
PY
)"
  if [ "$actual" == "$expected" ]; then
    _pass "$description"
  else
    _fail "$description — expected '$expected', got '$actual'"
  fi
}

# request_assert <desc> <request-index> <python-expression>
# The expression is evaluated with `payload` bound to that request's parsed body.
request_assert() {
  local description="$1" index="$2" expression="$3" actual
  actual="$(python3 - "$provider_requests" "$index" "$expression" <<'PY'
import json
import sys
rows = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8") if line.strip()]
index = int(sys.argv[2])
if len(rows) < index:
    print(f"missing request {index} (have {len(rows)})")
else:
    payload = rows[index - 1]
    print(eval(sys.argv[3], {"json": json}, {"payload": payload}))
PY
)"
  if [ "$actual" == "True" ]; then
    _pass "$description"
  else
    _fail "$description — $actual"
  fi
}

quit_and_wait() {
  tui_keys "/quit" Enter
  for _ in {1..60}; do
    tui_alive || return 0
    sleep 0.1
  done
  _fail "TUI did not exit cleanly"
}

tui_start 160 48 || e2e_done

# A pristine state root has no provider selected; choose the loopback model.
tui_keys "/model" Enter
expect "model picker opens" "Filter models:"
tui_keys Left End Right
tui_keys "mock-model"
expect "loopback model is offered" "OpenAI Compatible · mock-model"
tui_keys Enter
expect_meta "loopback model selected" "Coding · mock-model"

tui_keys "Inspect the e2e scripts and report what you found" Enter

expect "first singleton read renders" "lib.sh"
expect "batched pair renders" "01_execution_modes.sh"
expect "final answer renders" "Inspection batching verified"

# Wire evidence: the advisory is a conversation message, so it must appear in
# the payload that follows the third consecutive singleton turn.
request_assert "advisory reaches the wire after the streak" 4 \
  '"Inspection batching advisory:" in json.dumps(payload)'
request_assert "advisory carries the untrusted versions frame" 4 \
  '"inspection batching versions" in json.dumps(payload)'
request_assert "model declaration is echoed back" 4 \
  '"Inspection dependency:" in json.dumps(payload)'
request_assert "no advisory before the streak" 3 \
  '"Inspection batching advisory:" not in json.dumps(payload)'

quit_and_wait
cp -p "$provider_requests" "$E2E_EVIDENCE/provider-requests.jsonl"

# Persistence evidence: one diagnostics row per inspection turn.
database_assert "diagnostics persist for every inspection turn" \
  "SELECT COUNT(*) FROM usage_turns WHERE inspection_batch_json IS NOT NULL" 4
database_assert "the advisory is recorded exactly once" \
  "SELECT COUNT(*) FROM usage_turns WHERE inspection_batch_json LIKE '%\"advisory_emitted\":true%'" 1
database_assert "the parallel turn is classified as batched" \
  "SELECT json_extract(inspection_batch_json, '\$.classification') FROM usage_turns \
   WHERE json_extract(inspection_batch_json, '\$.classification') = 'batched'" "batched"
database_assert "the declared singleton is classified as dependent" \
  "SELECT json_extract(inspection_batch_json, '\$.dependency_reason') FROM usage_turns \
   WHERE json_extract(inspection_batch_json, '\$.classification') = 'dependent'" \
  "the request named a single file to read."
database_assert "batched turn reports a width-two plan and execution" \
  "SELECT json_extract(inspection_batch_json, '\$.planned_widths') || \
          json_extract(inspection_batch_json, '\$.actual_widths') FROM usage_turns \
   WHERE json_extract(inspection_batch_json, '\$.classification') = 'batched'" "[2][2]"

if [ "${E2E_REPEAT_CALL_IDS:-0}" == 1 ]; then
  database_assert "all tool cards survive repeated provider IDs" \
    "SELECT COUNT(*) FROM tool_calls" 5
  database_assert "tool cards have distinct canonical identities" \
    "SELECT COUNT(DISTINCT call_id) FROM tool_calls" 5
  database_assert "every tool card finished successfully" \
    "SELECT COUNT(*) FROM tool_calls WHERE status = 'succeeded'" 5
  database_assert "original provider IDs are durably retained" \
    "SELECT COUNT(*) FROM tool_call_identities, json_each(raw_json) WHERE json_extract(value, '\$.wire_id') = 'same-wire-id'" 5
  request_assert "follow-up wire IDs are unique and paired" 5 \
    '(lambda calls, results: len(calls) == 5 and len(set(calls)) == 5 and calls == results)([call["id"] for message in payload["messages"] for call in message.get("tool_calls", [])], [message["tool_call_id"] for message in payload["messages"] if message.get("role") == "tool"])'
  request_assert "read reuse points to the model-visible source ID" 3 \
    'any(message.get("role") == "tool" and "[reused previous read]" in str(message.get("content")) and "across tool(s) same-wire-id." in str(message.get("content")) and "\nsource_calls: same-wire-id" in str(message.get("content")) for message in payload["messages"])'
fi

e2e_done
