#!/usr/bin/env bash
# Local e2e runner for the bonsai TUI.
#
#   ./e2e/run.sh                         # build once, run every case, summarize
#   ./e2e/run.sh cases/01_execution_modes.sh   # run a single case
#
# Builds the debug binary, then runs each scenario in its own isolated tmux +
# BONSAI_HOME. Exits non-zero if any case fails (CI / pre-push friendly).
set -uo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"   # the e2e/ directory
ROOT="$(cd .. && pwd)"

if ! command -v tmux >/dev/null 2>&1 && [ ! -x /opt/homebrew/bin/tmux ]; then
  echo "error: tmux not found. Install it with: brew install tmux" >&2
  exit 2
fi

echo "building debug binary…"
( cd "$ROOT" && cargo build ) || { echo "FAIL: build failed"; exit 1; }

if [ "$#" -ge 1 ]; then
  cases=("$@")
else
  cases=()
  for c in cases/*.sh; do cases+=("$c"); done
fi

pass=0; fail=0; failed=()
for c in "${cases[@]}"; do
  if bash "$c"; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1)); failed+=("$(basename "$c")")
  fi
done

echo
echo "================================================"
echo "e2e summary: $pass passed, $fail failed"
[ "$fail" -gt 0 ] && printf '  failed: %s\n' "${failed[*]}"
exit "$fail"
