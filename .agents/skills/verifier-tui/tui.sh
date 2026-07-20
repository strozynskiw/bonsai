#!/usr/bin/env bash
# Reusable driver for verifying the bonsai TUI at its real surface, by running
# it inside an isolated tmux session and capturing the rendered pane.
#
# See SKILL.md. Subcommands:
#   start [cols rows]   launch $BIN in a fresh tmux session (default 140x40)
#   keys <tmux-keys...> send keys, e.g.  keys "/mode auto-accept" Enter   |  keys M-m
#   text                dump the pane as plain text
#   ansi                dump the pane with ANSI escapes (for color checks)
#   line <regex>        grep the plain pane for a regex (-a, returns matching lines)
#   stop                kill the session
#
# Env overrides: BONSAI_BIN, BONSAI_TMUX_SOCK, BONSAI_STARTUP_WAIT, WAIT.
set -uo pipefail

TMUX_BIN="$(command -v tmux || echo /opt/homebrew/bin/tmux)"
SOCK="${BONSAI_TMUX_SOCK:-bonsai-verify}"
SESSION="v"
BIN="${BONSAI_BIN:-./target/debug/bonsai}"
STARTUP_WAIT="${BONSAI_STARTUP_WAIT:-3}"
WAIT="${WAIT:-1}"

tx() { "$TMUX_BIN" -L "$SOCK" "$@"; }

case "${1:-}" in
  start)
    cols="${2:-140}"; rows="${3:-40}"
    tx kill-server 2>/dev/null || true
    tx new-session -d -s "$SESSION" -x "$cols" -y "$rows"
    tx send-keys -t "$SESSION" "$BIN" Enter
    sleep "$STARTUP_WAIT"
    ;;
  keys) shift; tx send-keys -t "$SESSION" "$@"; sleep "$WAIT" ;;
  text) tx capture-pane -t "$SESSION" -p ;;
  ansi) tx capture-pane -t "$SESSION" -p -e ;;
  line) shift; tx capture-pane -t "$SESSION" -p | grep -aE "${1:-.}" || true ;;
  stop) tx kill-server 2>/dev/null || true ;;
  *)
    echo "usage: tui.sh {start [cols rows]|keys <keys...>|text|ansi|line <regex>|stop}" >&2
    exit 2
    ;;
esac
