#!/usr/bin/env bash
# Execution-mode prompt policy, driven through the live TUI. Ported from
# .claude/skills/verifier-tui/smoke-execution-modes.sh, now self-contained
# (isolated env, no real provider needed). Regression guard for the idle-`/yolo`
# bug: yolo must render as `yolo`, never collapse to `auto-accept`.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
e2e_begin "01_execution_modes: /autonomy, /yolo, Alt+M, Tab"

tui_start 140 40 || e2e_done

echo "fresh start (no auto-accept/yolo marker):"
expect_meta "an agent is shown"           "Agent ·"
forbid_meta "no auto-accept marker"       "auto-accept"
forbid_meta "no yolo marker"              "· yolo"

echo "/autonomy auto-accept:"
tui_keys "/autonomy auto-accept" Enter
expect_meta "auto-accept marker appears"  "· auto-accept"

echo "/yolo on (must be yolo, NOT auto-accept):"
tui_keys "/yolo on" Enter
expect_meta "yolo marker appears"         "· yolo"
forbid_meta "yolo is distinct from auto"  "auto-accept"

echo "/autonomy ask (quiet — no marker):"
tui_keys "/autonomy ask" Enter
forbid_meta "auto-accept cleared"         "· auto-accept"
forbid_meta "yolo cleared"                "· yolo"

echo "/autonomy yolo (yolo is a first-class level):"
tui_keys "/autonomy yolo" Enter
expect_meta "yolo selectable via /autonomy" "· yolo"
tui_keys "/autonomy ask" Enter
forbid_meta "back to quiet"               "· yolo"

# Alt+M walks the confined ladder ask -> conservative -> balanced -> auto-accept
# (never yolo); from `ask` the next step is `conservative`.
echo "Alt+M cycles the autonomy axis:"
tui_keys M-m
expect_meta "Alt+M -> conservative"       "· conservative"

echo "Tab switches the agent axis (orthogonal to policy):"
tui_keys Tab
expect_meta "Tab -> Plan agent"           "Plan ·"

e2e_done
