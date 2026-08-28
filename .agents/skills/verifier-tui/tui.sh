#!/usr/bin/env bash
# Reusable driver for verifying the bonsai TUI at its real surface, by running
# it inside an isolated tmux session and capturing the rendered pane.
#
# See SKILL.md. Subcommands:
#   start [cols rows]   launch $BIN in a fresh tmux session (default 140x40);
#                       walks the first-run wizard and waits for the chat view
#   keys <tmux-keys...> send keys, e.g.  keys "/autonomy auto-accept" Enter   |  keys M-m
#   text                dump the pane as plain text
#   ansi                dump the pane with ANSI escapes (for color checks)
#   line <regex>        grep the plain pane for a regex (-a, returns matching lines)
#   stop                kill the session
#
# Env overrides: BONSAI_BIN, BONSAI_LAUNCH_ENV, BONSAI_TMUX_SOCK,
#                BONSAI_STARTUP_WAIT, WAIT.
set -uo pipefail

TMUX_BIN="$(command -v tmux || echo /opt/homebrew/bin/tmux)"
SOCK="${BONSAI_TMUX_SOCK:-bonsai-verify}"
SESSION="v"
BIN="${BONSAI_BIN:-./target/debug/bonsai}"
LAUNCH_ENV="${BONSAI_LAUNCH_ENV:-}"
STARTUP_WAIT="${BONSAI_STARTUP_WAIT:-3}"
WAIT="${WAIT:-1}"
META='(Coding|Planning) ·'   # uniquely identifies the composer meta line

tx() { "$TMUX_BIN" -L "$SOCK" "$@"; }

# tmux sends a plain `\t` for the S-Tab key name (indistinguishable from Tab),
# so translate it to the CSI Z sequence the app parses as BackTab/Shift+Tab.
send_keys() {
  local args=() a
  for a in "$@"; do
    if [ "$a" = "S-Tab" ]; then args+=($'\x1b[Z'); else args+=("$a"); fi
  done
  tx send-keys -t "$SESSION" "${args[@]}"
  sleep "$WAIT"
}

# Enter through the first-run wizard (fresh installs show 7 steps; every
# step's default choice is the recommended one) until the chat view renders.
wait_chat() {
  local i
  # Give the app time to boot before sending any keys: poll until the pane
  # shows the wizard or the chat view.
  for i in $(seq 1 20); do
    tx capture-pane -t "$SESSION" -p 2>/dev/null | grep -qaE "$META|Welcome to Bonsai|step [0-9] of 7" && break
    sleep 0.3
  done
  for i in $(seq 1 15); do
    tx capture-pane -t "$SESSION" -p 2>/dev/null | grep -qaE "$META" && return 0
    tx send-keys -t "$SESSION" Enter
    sleep 0.5
  done
  return 1
}

case "${1:-}" in
  start)
    cols="${2:-140}"; rows="${3:-40}"
    tx kill-server 2>/dev/null || true
    # Launch the binary as the tmux session command, not via the pane's login
    # shell: a login shell (zsh + oh-my-zsh update nag, etc.) can eat the first
    # sent key or stall on a prompt, mangling `exec env …` launches.
    # tmux session commands cannot parse `VAR=val` prefixes (they kill the
    # pane), so env overrides must go through `env`.
    tx new-session -d -s "$SESSION" -x "$cols" -y "$rows" "exec env ${LAUNCH_ENV}$BIN"
    if ! wait_chat; then
      echo "error: chat view ('$META') never rendered (wizard walkthrough exhausted)" >&2
      tx capture-pane -t "$SESSION" -p >&2
      tx kill-server 2>/dev/null || true
      exit 1
    fi
    sleep "$STARTUP_WAIT"
    ;;
  keys) shift; send_keys "$@" ;;
  text) tx capture-pane -t "$SESSION" -p ;;
  ansi) tx capture-pane -t "$SESSION" -p -e ;;
  line) shift; tx capture-pane -t "$SESSION" -p | grep -aE "${1:-.}" || true ;;
  stop) tx kill-server 2>/dev/null || true ;;
  *)
    echo "usage: tui.sh {start [cols rows]|keys <keys...>|text|ansi|line <regex>|stop}" >&2
    exit 2
    ;;
esac
