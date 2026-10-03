#!/usr/bin/env bash
# The actual TUI child must not alter a parent provider/session database.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
e2e_begin "08_state_isolation: real child leaves parent state byte-identical"

parent_home="$E2E_HOME/parent-bonsai"
mkdir -p "$parent_home"
printf 'parent provider/session sentinel\n' > "$parent_home/bonsai.db"
cp "$parent_home/bonsai.db" "$E2E_HOME/parent-before.db"
export BONSAI_HOME="$parent_home"

tui_start 140 40 || e2e_done
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

e2e_done
