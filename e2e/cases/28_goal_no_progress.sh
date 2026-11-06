#!/usr/bin/env bash
# Re-reading the same bytes must not keep an unfinished goal running forever.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
e2e_begin "28_goal_no_progress: repeated evidence is bounded"

ready="$E2E_HOME/provider-url"
requests="$E2E_HOME/provider-requests.jsonl"
python3 "$E2E_LIB_DIR/mock_goal_progress_provider.py" "$ready" "$requests" --repeat-read &
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
expect "unfinished work is reported" "goal response reports unfinished work"
expect_meta "stalled run returns to composer" "● Coding"
forbid "unfinished goal is not completed" "goal complete ·"

if python3 - "$requests" "$E2E_BONSAI_HOME/bonsai.db" <<'PY'
import json, sqlite3, sys
from pathlib import Path
requests = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines()]
assert 4 <= len(requests) <= 8, len(requests)
with sqlite3.connect(sys.argv[2]) as db:
    raw = db.execute('SELECT record_json FROM session_goals WHERE is_current = 1').fetchone()[0]
    assert json.loads(raw)['state'] != 'complete'
PY
then _pass "duplicate evidence did not produce completion or unlimited retries"
else _fail "goal stagnation evidence"; fi
cp "$requests" "$E2E_EVIDENCE/provider-requests.jsonl"
tui_keys "/quit" Enter
e2e_done
