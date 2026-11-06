#!/usr/bin/env bash
# A failed native-store read must explain recovery without clearing the reference.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
e2e_begin "29_keyring_recovery_notice: unavailable credential is actionable"

tui_start 150 45 || e2e_done
tui_keys "/quit" Enter
for _ in {1..30}; do
  tui_alive || break
  sleep 0.2
done
if tui_alive; then
  _fail "initial launch did not exit"
  e2e_done
fi

if ! python3 - "$E2E_BONSAI_HOME/bonsai.db" <<'PY'
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    assert db.execute("SELECT 1 FROM provider_settings WHERE provider_id = 'anthropic'").fetchone()
    assert db.execute("UPDATE provider_credentials SET source = 'keyring', reference = '' WHERE provider_id = 'anthropic'").rowcount == 1
PY
then _fail "could not seed keyring reference"; e2e_done; fi

tui_start 150 45 || e2e_done
expect "keyring recovery is visible" "Could not read the OS credential store"
expect "affected provider is named" "anthropic"
expect "recovery action is shown" "Unlock it or restore access"
expect "saved reference is retained" "references were retained"
tui_keys "/quit" Enter
for _ in {1..30}; do
  tui_alive || break
  sleep 0.2
done
if python3 - "$E2E_BONSAI_HOME/bonsai.db" <<'PY'
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    assert db.execute("SELECT source FROM provider_credentials WHERE provider_id = 'anthropic'").fetchone() == ('keyring',)
PY
then _pass "unavailable keyring reference remains persisted"
else _fail "keyring reference changed"; fi
e2e_done
