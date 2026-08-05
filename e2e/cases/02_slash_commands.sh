#!/usr/bin/env bash
# Slash commands that open overlays / emit status, plus a safe no-op for a
# bogus argument — proving the command dispatch is wired to the live UI.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
e2e_begin "02_slash_commands: /help, /theme, bad arg"

tui_start 140 40 || e2e_done

echo "/help opens the command modal:"
tui_keys "/help" Enter
expect "command modal renders"     "Commands"
expect "lists a known command"     "List commands"
expect "lists a saved-session cmd" "Resume a saved session"
tui_keys Escape

echo "/theme opens the picker:"
tui_keys "/theme" Enter
expect "theme picker renders"      "Themes"
tui_keys Escape

echo "unknown subcommand is a safe no-op (no crash, chat survives):"
tui_keys "/help totally-bogus" Enter
forbid      "no panic"             "panicked"
expect_meta "chat still rendered"  "Coding ·"

e2e_done
