#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
e2e_begin "25_composer_emacs_keys: input movement and deletion"
tui_start 120 35 || e2e_done
tui_keys "abcde" C-b C-b C-d C-b "X" C-e "Z" C-b C-f "Q"
expect "control shortcuts edit at the intended cursor positions" "abXceZQ"
tui_keys C-a C-d
forbid "Ctrl+A still selects all and Ctrl+D deletes the selection" "abXceZQ"
tui_keys "/quit" Enter
e2e_done
