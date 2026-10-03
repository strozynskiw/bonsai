#!/usr/bin/env bash
# The actual TUI child must not alter a parent provider/session database.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
e2e_begin "08_state_isolation: real child leaves parent state byte-identical"

parent_fixture="$(mktemp -d)"
trap 'e2e_cleanup; rm -rf "$parent_fixture"' EXIT
parent_home="$parent_fixture/parent-bonsai"
mkdir -p "$parent_home"
printf 'parent provider/session sentinel\n' > "$parent_home/bonsai.db"
cp "$parent_home/bonsai.db" "$E2E_HOME/parent-before.db"
mkdir -p "$parent_home/credentials" "$parent_home/logs"
printf 'parent credential sentinel\n' > "$parent_home/credentials/test"
printf 'parent preference sentinel\n' > "$parent_home/config.toml"
printf 'parent log sentinel\n' > "$parent_home/logs/bonsai.log"
cp -R "$parent_home" "$E2E_HOME/parent-before"
export BONSAI_HOME="$parent_home"

tui_start 140 40 || e2e_done
tx resize-window -t "$E2E_SESSION" -x 100 -y 28
wait_for '(Coding|Planning) ·' 3 || _fail "resized child did not settle"
expect_meta "resized child is ready" "Coding ·"
tui_keys "/help" Enter
expect "key input opens help" "Commands"
tui_keys Escape
tui_keys "/quit" Enter
for _ in {1..20}; do
  tui_alive || break
  sleep 0.2
done
if tui_alive; then
  _fail "isolated child did not exit"
elif cmp -s "$E2E_HOME/parent-before.db" "$parent_home/bonsai.db"; then
  _pass "parent provider/session state is byte-identical"
else
  _fail "child TUI mutated the parent database"
fi
if diff -r "$E2E_HOME/parent-before" "$parent_home"; then
  _pass "parent database, preferences, credentials, and logs are byte-identical"
else
  _fail "child TUI mutated parent state files"
fi
manifest="$E2E_EVIDENCE/launches/0001/manifest.txt"
if grep -qF 'exit_code=0' "$manifest" && grep -qF 'database_sha256=' "$manifest" \
  && [[ -s "$E2E_EVIDENCE/inputs.log" ]] \
  && [[ -n "$(find "$E2E_EVIDENCE/screens" -name '*.txt' -print -quit)" ]]; then
  _pass "launch identity, inputs, screens, clean exit, and database evidence captured"
else
  _fail "verification replay evidence is incomplete"
fi

e2e_done
