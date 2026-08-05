#!/usr/bin/env bash
# A persisted session must own the first resumed request even when the global
# provider and per-mode defaults point elsewhere.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
WAIT=0.3
e2e_begin "07_resume_selection: pinned model serves first resumed request"

provider_ready="$E2E_HOME/provider-url"
python3 "$E2E_LIB_DIR/mock_streaming_provider.py" "$provider_ready" &
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

# Create the target session and pin it to the loopback compatible provider.
tui_start 140 40 || e2e_done
tui_keys "/model" Enter
expect "model picker opens" "Filter models:"
tui_keys Left End Right
tui_keys "mock-model"
expect "mock model is filtered" "OpenAI Compatible · mock-model"
tui_keys Enter
expect_meta "target model selected" "Coding · mock-model"
tui_keys "/quit" Enter
for _ in {1..20}; do
  tui_alive || break
  sleep 0.2
done
if tui_alive; then
  _fail "seed session did not exit"
  e2e_done
fi

target_id="$(python3 - "$E2E_HOME/bonsai.db" <<'PY'
import sqlite3
import sys

with sqlite3.connect(sys.argv[1]) as database:
    row = database.execute("SELECT MAX(id) FROM sessions").fetchone()
    print(row[0] if row and row[0] is not None else "")
PY
)"
if [ -z "$target_id" ]; then
  _fail "seed session id was not persisted"
  e2e_done
fi

# Simulate another process changing the global default and the Coding persona
# model after the target session was saved. Both routes point at the loopback
# server so the rendered model identity, not network failure, proves routing.
python3 - "$E2E_HOME/bonsai.db" "$E2E_PROVIDER_BASE_URL" <<'PY'
import json
import sqlite3
import sys

database_path, provider_url = sys.argv[1:]
with sqlite3.connect(database_path) as database:
    preferences = {
        "current_provider": "opencode",
        "active_connection_id": "opencode",
        "active_model_id": "opencode/qwen3.7-max",
        "mode_models_json": json.dumps({"coding": "opencode:qwen3.7-max"}),
    }
    database.executemany(
        """
        INSERT INTO user_preferences(key, value) VALUES (?, ?)
        ON CONFLICT(key) DO UPDATE SET value = excluded.value
        """,
        preferences.items(),
    )
    database.execute(
        """
        INSERT INTO provider_settings(
          provider_id, base_url, model, reasoning_json, model_reasoning_json
        ) VALUES (?, ?, ?, ?, ?)
        ON CONFLICT(provider_id) DO UPDATE SET
          base_url = excluded.base_url,
          model = excluded.model,
          reasoning_json = excluded.reasoning_json,
          model_reasoning_json = excluded.model_reasoning_json
        """,
        ("opencode", provider_url, "opencode/qwen3.7-max", '"default"', "{}"),
    )
    database.commit()
PY

E2E_OPENCODE_API_KEY="global-default-only"
E2E_BONSAI_ARGS="-c $target_id"
tui_start 140 40 || e2e_done
expect_meta "resume restores target model" "Coding · mock-model"

tui_keys "continue pinned session" Enter
expect "resumed input is submitted" "continue pinned session"
expect_meta "first request stays on target model" "Coding · mock-model"
forbid_meta "global model does not replace target" "qwen3.7-max"
forbid_meta "first request is actively running" "● Coding"

e2e_done
