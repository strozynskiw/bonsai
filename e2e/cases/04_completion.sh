#!/usr/bin/env bash
# Tab-completion of a slash command in the composer. Exercises the live
# `complete_command_arg_from_state` path (src/tui/keymap.rs) — the wired-up
# replacement for the dead `complete_command_arg` removed from commands/.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
e2e_begin "04_completion: /mod + Tab -> /model"

tui_start 140 40 || e2e_done

echo "type a partial command:"
tui_keys "/mod"
# Check the input line only — the `/model` row above is the popup. `/mode` was
# removed, so `/model` is the sole `/mod…` command and Tab completes it fully.
forbid_input "input not yet completed" "/model"

echo "Tab completes it:"
tui_keys Tab
expect_input "Tab fills the input to /model" "/model"

e2e_done
