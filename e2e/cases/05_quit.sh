#!/usr/bin/env bash
# /quit tears down the app cleanly: the bonsai process exits, so the tmux pane
# (which exec'd it) dies. A hang or panic-on-exit would leave the pane alive.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
e2e_begin "05_quit: /quit exits the process"

tui_start 140 40 || e2e_done

echo "/quit ends the session:"
tui_keys "/quit" Enter
# Poll for the pane to disappear (clean process exit) rather than sleep-and-hope.
ended=0
for _ in $(seq 1 20); do
  if ! tui_alive; then ended=1; break; fi
  sleep 0.2
done
if [ "$ended" -eq 1 ]; then
  _pass "process exited and pane closed"
else
  _fail "process still alive after /quit"
fi

e2e_done
