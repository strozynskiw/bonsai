#!/usr/bin/env bash
# The /model picker modal opens over the chat and Esc returns cleanly. Needs an
# authorized provider — supplied by the isolated env (OPENAI_COMPATIBLE_*).
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
e2e_begin "03_model_picker: /model open + Esc close"

tui_start 140 40 || e2e_done

echo "/model opens the picker:"
tui_keys "/model" Enter
expect "picker modal renders"     "Models"
expect "picker shows key hints"   "Esc cancel"

echo "Esc closes the picker, back to chat:"
tui_keys Escape
wait_for '(Agent|Plan) ·' 5 || true
forbid      "picker is gone"      "Esc cancel"
expect_meta "back in chat"        "Agent ·"

e2e_done
