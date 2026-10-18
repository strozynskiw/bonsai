#!/usr/bin/env bash
# Real TUI + wire + SQLite qualification for memory and repeated compaction.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=100
e2e_begin "18_context_continuity: memory provenance, compaction, and restore"

provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
mkdir -p "$E2E_BONSAI_HOME/memory"
cat > "$E2E_BONSAI_HOME/memory/partial-commits.md" <<'EOF'
---
name: partial-commits
description: partial commits need no-verify
type: preference
---
CONTEXT_MEMORY_SENTINEL: Partial commits use a focused staged diff.
EOF
python3 "$E2E_LIB_DIR/mock_context_provider.py" "$provider_ready" "$provider_requests" &
E2E_HELPER_PID=$!
for _ in {1..40}; do
  [ -s "$provider_ready" ] && break
  sleep 0.05
done
if [ ! -s "$provider_ready" ]; then
  _fail "context provider did not start"
  e2e_done
fi
E2E_PROVIDER_BASE_URL="$(<"$provider_ready")"

# The rendered answer can precede the asynchronous SQLite snapshot. Wait for
# the specific durable state instead of assuming that terminal paint is a flush.
wait_database_value() {
  python3 - "$E2E_BONSAI_HOME/bonsai.db" "$1" "$2" <<'PY'
import sqlite3, sys, time
deadline = time.monotonic() + 10
while time.monotonic() < deadline:
    with sqlite3.connect(sys.argv[1]) as db:
        value = db.execute(sys.argv[2]).fetchone()[0]
    if str(value) == sys.argv[3]:
        raise SystemExit(0)
    time.sleep(0.05)
raise SystemExit(f"snapshot did not settle: expected {sys.argv[3]}, got {value}")
PY
}

tui_start 160 48 || e2e_done
tui_keys "/model" Enter
expect "model picker opens" "Filter models:"
tui_keys Left End Right
tui_keys "mock-model"
expect "loopback model is offered" "OpenAI Compatible · mock-model"
tui_keys Enter
expect_meta "loopback model selected" "Coding · mock-model"

tui_keys "CONTEXT_TURN_1 Explain the initial example" Enter
expect "first topic finishes" "CONTEXT_RESULT_1 complete"
tui_keys "CONTEXT_TURN_2 How do I commit only part of my changes" Enter
expect "second topic with memory finishes" "CONTEXT_RESULT_2 complete"
wait_database_value "SELECT COUNT(*) FROM episodes WHERE goal LIKE 'CONTEXT_TURN_2%'" 1 \
  || _fail "second episode snapshot did not persist"

if python3 - "$E2E_BONSAI_HOME/bonsai.db" "$provider_requests" <<'PY'
import json, sqlite3, sys
requests = [json.loads(line) for line in open(sys.argv[2])]
notes = [message for request in requests for message in request["messages"]
         if "CONTEXT_MEMORY_SENTINEL" in str(message.get("content", ""))]
assert notes, "recalled memory did not reach the provider"
assert all(message.get("role") == "user" and message.get("name") == "bonsai_memory" for message in notes), notes
with sqlite3.connect(sys.argv[1]) as db:
    episodes = db.execute("SELECT goal, start_stable_id FROM episodes ORDER BY seq").fetchall()
    goal, start = next(row for row in episodes if row[0].startswith("CONTEXT_TURN_2"))
    message = json.loads(db.execute("SELECT raw_json FROM context_messages WHERE stable_id = ?", (start,)).fetchone()[0])
    assert message["content"] == goal and not message.get("name"), message
PY
then _pass "memory stays background data and the episode starts at the human request"
else _fail "memory provenance or episode boundary is incorrect"
fi

for turn in 3 4 5 6; do
  tui_keys "CONTEXT_TURN_$turn Explain another example" Enter
  expect "example $turn finishes" "CONTEXT_RESULT_$turn complete"
done
tui_keys "/compact preview target=1" Enter
expect "compaction preview renders" "Context compaction preview"
tui_keys "/compact target=1 provider" Enter
expect "first compaction renders" "Context compacted:"
wait_database_value "SELECT COUNT(*) FROM compaction_events" 1 \
  || _fail "first compaction snapshot did not persist"
for turn in 7 8 9; do
  tui_keys "CONTEXT_TURN_$turn Explain another example" Enter
  expect "example $turn finishes" "CONTEXT_RESULT_$turn complete"
done
tui_keys "/compact target=1 provider" Enter
expect "second compaction renders" "Context compacted"
wait_database_value "SELECT COUNT(*) FROM compaction_events" 2 \
  || _fail "second compaction snapshot did not persist"

if python3 - "$E2E_BONSAI_HOME/bonsai.db" "$provider_requests" <<'PY'
import json, sqlite3, sys
requests = [json.loads(line) for line in open(sys.argv[2])]
summaries = [request for request in requests if str(request["messages"][0].get("content", "")).startswith(
    ("You summarize prior chat history", "You maintain a rolling summary"))]
assert len(summaries) == 2, f"expected two hidden summaries, got {len(summaries)}"
second = json.dumps(summaries[1])
assert "UPDATE the previous summary" in second and "CTX_SUMMARY_1" in second
assert "ARCHIVE_SENTINEL_3" not in second, "second summary re-expanded old originals"
with sqlite3.connect(sys.argv[1]) as db:
    events = db.execute("SELECT before_tokens, after_tokens FROM compaction_events ORDER BY seq").fetchall()
    assert len(events) == 2, events
    assert all(after < before for before, after in events), events
    sources = " ".join(row[0] for row in db.execute("SELECT messages_json FROM context_sources"))
    assert "ARCHIVE_SENTINEL_3" in sources, "restore archive lost original bytes"
PY
then _pass "both compactions save tokens, roll the summary forward, and retain originals"
else _fail "compaction wire or archive validation failed"
fi

tui_keys "/ctx" Enter
expect "context inspector opens" "LEDGER"
# Compaction summaries are system rows; restoring that root restores its sources.
tui_keys Home r
tui_keys q
tui_keys "CONTEXT_TURN_10 Explain the restored examples" Enter
expect "conversation continues after restore" "CONTEXT_RESULT_10 complete"
if python3 - "$provider_requests" <<'PY'
import json, sys
requests = [json.loads(line) for line in open(sys.argv[1])]
last = json.dumps(requests[-1])
assert "ARCHIVE_SENTINEL_3" in last, "restored original missing from follow-up request"
assert "CTX_SUMMARY_2" not in last, "summary was not replaced by its originals"
PY
then _pass "restoring through /ctx puts original history back on the wire"
else _fail "context restore did not restore the original messages"
fi

for turn in 11 12 13; do
  tui_keys "CONTEXT_TURN_$turn Explain another example" Enter
  expect "example $turn finishes" "CONTEXT_RESULT_$turn complete"
done
tui_keys "/compact target=1" Enter
expect "oversized summary falls back visibly" "deterministic"
wait_database_value "SELECT COUNT(*) FROM compaction_events" 3 \
  || _fail "fallback compaction snapshot did not persist"

tui_keys "/quit" Enter
for _ in {1..60}; do
  tui_alive || break
  sleep 0.1
done
if tui_alive; then
  _fail "TUI did not exit cleanly before resume"
  e2e_done
fi
E2E_BONSAI_ARGS=(-c)
tui_start 160 48 || e2e_done
expect_meta "resumed session keeps the selected model" "Coding · mock-model"
tui_keys "CONTEXT_TURN_14 Explain the resumed examples" Enter
expect "resumed session continues" "CONTEXT_RESULT_14 complete"
if python3 - "$E2E_BONSAI_HOME/bonsai.db" "$provider_requests" <<'PY'
import json, sqlite3, sys
requests = [json.loads(line) for line in open(sys.argv[2])]
last = json.dumps(requests[-1])
assert "Compacted Context Summary" in last and "CONTEXT_RESULT_13" in last
assert "OVERSIZED_GENERATED_SUMMARY" not in last, "partial oversized summary leaked into context"
with sqlite3.connect(sys.argv[1]) as db:
    events = db.execute("SELECT before_tokens, after_tokens FROM compaction_events").fetchall()
    assert len(events) == 3 and all(after < before for before, after in events), events
PY
then _pass "fallback discards partial output, saves tokens, and survives real session resume"
else _fail "fallback or resumed context validation failed"
fi

cp "$provider_requests" "$E2E_EVIDENCE/provider-requests.jsonl"
tui_keys "/quit" Enter
for _ in {1..60}; do
  tui_alive || break
  sleep 0.1
done
if tui_alive; then
  _fail "resumed TUI did not exit cleanly"
fi
e2e_done
