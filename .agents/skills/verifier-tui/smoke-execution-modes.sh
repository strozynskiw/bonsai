#!/usr/bin/env bash
# Smoke test: the unified `/autonomy` approval axis, driven through the live TUI.
#
# Doubles as the worked example for the verifier-tui protocol and as a
# regression guard for two bugs that green unit tests missed:
#   1. the idle-`/yolo` bug (yolo must render as `yolo`, never `auto-accept`);
#   2. the command→status wiring for `/autonomy` and Alt+M.
# Exits non-zero if any assertion fails.
#
# Assumes a provider is already configured so the TUI opens to the chat view.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"
export PATH="/opt/homebrew/bin:$PATH"

HERE="$(cd "$(dirname "$0")" && pwd)"
TUI="$HERE/tui.sh"
META='(Agent|Plan) ·'   # uniquely identifies the composer meta line
fail=0

meta() { "$TUI" line "$META" | tail -1; }

check() { # desc, expected-substring
  local desc="$1" want="$2" got; got="$(meta)"
  if grep -qF "$want" <<<"$got"; then
    echo "  ✅ $desc"
  else
    echo "  ❌ $desc — wanted '$want' in: ${got//[[:space:]]/ }"; fail=1
  fi
}
refute() { # desc, forbidden-substring
  local desc="$1" no="$2" got; got="$(meta)"
  if grep -qF "$no" <<<"$got"; then
    echo "  ❌ $desc — '$no' unexpectedly present in: ${got//[[:space:]]/ }"; fail=1
  else
    echo "  ✅ $desc"
  fi
}

echo "building debug binary…"
cargo build >/dev/null 2>&1 || { echo "FAIL: build failed"; exit 1; }

echo "launching TUI…"
"$TUI" start 140 40
trap '"$TUI" stop' EXIT

echo "fresh start (default level is 'balanced', shown in the bar):"
check  "an agent is shown"            "Agent ·"
check  "balanced marker shown"        "· balanced"
refute "no auto-accept marker"        "auto-accept"
refute "no yolo marker"               "· yolo"

echo "/autonomy ask (quiet — no marker):"
"$TUI" keys "/autonomy ask" Enter
refute "ask is quiet"                 "· balanced"
refute "no auto-accept"               "auto-accept"
refute "no yolo"                      "· yolo"

echo "/autonomy auto-accept:"
"$TUI" keys "/autonomy auto-accept" Enter
check  "auto-accept marker appears"   "· auto-accept"

echo "/yolo on (must be yolo, NOT auto-accept):"
"$TUI" keys "/yolo on" Enter
check  "yolo marker appears"          "· yolo"
refute "yolo is distinct from auto"   "auto-accept"

echo "/yolo off (returns to ask, quiet):"
"$TUI" keys "/yolo off" Enter
refute "yolo cleared"                 "· yolo"
refute "auto-accept cleared"          "auto-accept"

echo "/autonomy yolo (yolo is a first-class level):"
"$TUI" keys "/autonomy yolo" Enter
check  "yolo selectable via /autonomy" "· yolo"
"$TUI" keys "/autonomy ask" Enter
refute "back to ask"                  "· yolo"

echo "Alt+M cycles the confined ladder (ask -> conservative), never yolo:"
"$TUI" keys M-m
check  "Alt+M -> conservative"        "· conservative"
refute "Alt+M never lands on yolo"    "· yolo"

echo "Tab switches the agent axis (orthogonal to autonomy):"
"$TUI" keys Tab
check  "Tab -> Plan agent"            "Plan ·"

echo
if [ "$fail" = 0 ]; then echo "PASS"; else echo "FAIL"; fi
exit "$fail"
