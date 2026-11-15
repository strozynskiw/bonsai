#!/usr/bin/env bash
# Popup layout at two terminal sizes: the shared modal shell must keep its title,
# its explanatory line and its hint footer on a wide and a narrow terminal, and
# Esc must return to the chat from every popup.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
e2e_begin "18_popup_layout: popups keep their chrome at 150x45 and 90x20"

# One pass per size: open each popup, assert its chrome, close it with Esc.
popup_pass() {
  local cols="$1" rows="$2" label="$3"

  echo "[$label] /mode picker:"
  tui_keys "/mode" Enter
  expect "$label mode picker title"        "Mode"
  expect "$label mode picker footer"       "Esc"
  tui_keys Escape
  wait_for '(Coding|Planning) ·' 5 || true
  expect_meta "$label back in chat after mode" "Coding ·"

  echo "[$label] /review scope choice:"
  tui_keys "/review" Enter
  expect "$label review subtitle"          "Which changes should the review cover?"
  expect "$label review footer"            "Esc"
  tui_keys Escape
  wait_for '(Coding|Planning) ·' 5 || true
  expect_meta "$label back in chat after review" "Coding ·"

  echo "[$label] /model picker:"
  tui_keys "/model" Enter
  expect "$label model picker title"       "Models"
  expect "$label model picker footer"      "Esc"
  tui_keys Escape
  wait_for '(Coding|Planning) ·' 5 || true
  expect_meta "$label back in chat after model" "Coding ·"

  echo "[$label] /help reading surface:"
  tui_keys "/help" Enter
  expect "$label command help title"       "Commands"
  expect "$label command help footer"      "Esc close"
  tui_keys Escape
  wait_for '(Coding|Planning) ·' 5 || true
  expect_meta "$label back in chat after help" "Coding ·"
}

tui_start 150 45 || e2e_done
popup_pass 150 45 "wide"

echo "--- narrow terminal ---"
tx resize-window -t "$E2E_SESSION" -x 90 -y 20 2>/dev/null || true
tx resize-pane -t "$E2E_SESSION" -x 90 -y 20 2>/dev/null || true
sleep 1
wait_for_screen_settle "$(semantic_tui_text | cksum)" || true
popup_pass 90 20 "narrow"

e2e_done
