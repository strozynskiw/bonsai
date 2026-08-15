#!/usr/bin/env bash
# Shared library for bonsai e2e TUI tests. `source` this from a case script.
#
# It provides, in order of use:
#   - a deterministic, isolated, no-network, no-real-keys runtime environment
#   - a thin tmux driver (start / keys / capture)
#   - render-aware waits + assertions that tally failures and exit non-zero
#
# The isolation is what makes these tests reproducible on any machine (unlike
# the original verifier-tui smoke script, which assumed a provider was already
# configured in your real ~/.bonsai). See e2e/README.md.

E2E_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
E2E_REPO_ROOT="$(cd "$E2E_LIB_DIR/.." && pwd)"

: "${BONSAI_BIN:=$E2E_REPO_ROOT/target/debug/bonsai}"
E2E_VERIFIER="$E2E_LIB_DIR/verifier.sh"
TMUX_BIN="$(command -v tmux || echo /opt/homebrew/bin/tmux)"

E2E_SESSION="v"
E2E_STARTUP_WAIT="${E2E_STARTUP_WAIT:-12}"   # max seconds to reach chat-ready
WAIT="${WAIT:-0.8}"                          # per-keys settle delay
E2E_ASSERT_TRIES="${E2E_ASSERT_TRIES:-20}"   # expect* polls (×0.15s ≈ 3s)

E2E_FAIL=0
E2E_NAME="?"
E2E_EVIDENCE_SEQUENCE=0
E2E_LAUNCH_SEQUENCE=0

tx() { "$TMUX_BIN" -L "$E2E_SOCK" "$@"; }

# ---- lifecycle ------------------------------------------------------------

# e2e_begin <case-name>: set up the isolated env and arrange cleanup.
e2e_begin() {
  E2E_NAME="$1"
  E2E_FAIL=0
  E2E_HOME="$(mktemp -d)"
  E2E_BONSAI_HOME="$E2E_HOME/bonsai"
  local evidence_root="${E2E_RUN_EVIDENCE_ROOT:-$E2E_REPO_ROOT/target/tui-verification/e2e}"
  mkdir -p "$evidence_root"
  E2E_EVIDENCE="$(mktemp -d "$evidence_root/case.XXXXXX")"
  printf '%s\n' "$E2E_NAME" > "$E2E_EVIDENCE/case.txt"
  E2E_EVIDENCE_SEQUENCE=0
  E2E_LAUNCH_SEQUENCE=0
  E2E_SOCK="bonsai-e2e-$$"
  trap e2e_cleanup EXIT
  printf '\n=== %s ===\n' "$E2E_NAME"
}

e2e_cleanup() {
  [ -n "${E2E_HELPER_PID:-}" ] && kill "$E2E_HELPER_PID" 2>/dev/null || true
  [ -n "${E2E_SOCK:-}" ] && tx kill-server 2>/dev/null || true
  if [ -n "${E2E_EVIDENCE:-}" ] && [ -n "${E2E_BONSAI_HOME:-}" ]; then
    mkdir -p "$E2E_EVIDENCE/database"
    local database_file
    for database_file in "$E2E_BONSAI_HOME"/bonsai.db*; do
      [ -f "$database_file" ] || continue
      cp -p "$database_file" "$E2E_EVIDENCE/database/"
    done
  fi
  [ -n "${E2E_HOME:-}" ] && rm -rf "$E2E_HOME" 2>/dev/null || true
}

# e2e_done: print the case result and exit (0 = all assertions passed).
e2e_done() {
  if [ "$E2E_FAIL" -eq 0 ]; then
    echo "PASS: $E2E_NAME"
    exit 0
  fi
  echo "FAIL: $E2E_NAME ($E2E_FAIL assertion(s) failed)"
  exit 1
}

# ---- driver ---------------------------------------------------------------

# tui_start [cols rows ready_regex]: launch bonsai in a fresh pane and block
# until `ready_regex` renders. The env is injected inline (proven to work) so
# the pane process is bonsai itself — a clean /quit then kills the pane.
tui_start() {
  local cols="${1:-140}" rows="${2:-40}" ready="${3:-(Coding|Planning) ·}"
  local provider_base_url="${E2E_PROVIDER_BASE_URL:-http://127.0.0.1:9/v1}"
  local opencode_api_key="${E2E_OPENCODE_API_KEY:-}"
  local command argument launch_evidence
  E2E_LAUNCH_SEQUENCE=$((E2E_LAUNCH_SEQUENCE + 1))
  printf -v launch_evidence '%s/launches/%04d' "$E2E_EVIDENCE" "$E2E_LAUNCH_SEQUENCE"
  mkdir -p "$launch_evidence"
  local -a launch_args=(
    --state-root "$E2E_HOME"
    --evidence-dir "$launch_evidence"
    --provider-base-url "$provider_base_url"
    --binary "$BONSAI_BIN"
  )
  if [ -n "$opencode_api_key" ]; then
    launch_args+=(--enable-opencode-test-key)
  fi
  launch_args+=(--)
  if declare -p E2E_BONSAI_ARGS >/dev/null 2>&1; then
    launch_args+=("${E2E_BONSAI_ARGS[@]}")
  fi
  printf -v command 'exec %q' "$E2E_VERIFIER"
  for argument in "${launch_args[@]}"; do
    printf -v command '%s %q' "$command" "$argument"
  done
  tx kill-server 2>/dev/null || true
  tx new-session -d -s "$E2E_SESSION" -x "$cols" -y "$rows"
  tx send-keys -t "$E2E_SESSION" "$command" Enter
  # A pristine state root opens onboarding. Surface tests exercise the chat,
  # so take its documented "later" path instead of seeding private state.
  if wait_for "Welcome to Bonsai|$ready" "$E2E_STARTUP_WAIT"; then
    if tui_text | grep -qaF "Welcome to Bonsai"; then
      tx send-keys -t "$E2E_SESSION" Escape
    fi
  fi
  if ! wait_for "$ready" "$E2E_STARTUP_WAIT"; then
    echo "  ❌ startup: '$ready' never rendered within ${E2E_STARTUP_WAIT}s"
    echo "----- pane -----"; tui_text; echo "----------------"
    E2E_FAIL=$((E2E_FAIL + 1))
    return 1
  fi
  evidence_screen
}

tui_keys() {
  printf '%04d ' "$E2E_LAUNCH_SEQUENCE" >> "$E2E_EVIDENCE/inputs.log"
  printf '%q ' "$@" >> "$E2E_EVIDENCE/inputs.log"
  printf '\n' >> "$E2E_EVIDENCE/inputs.log"
  local before
  before="$(semantic_tui_text | cksum)"
  tx send-keys -t "$E2E_SESSION" "$@"
  wait_for_screen_settle "$before"
  evidence_screen
}
tui_text() { tx capture-pane -t "$E2E_SESSION" -p 2>/dev/null; }
tui_ansi() { tx capture-pane -t "$E2E_SESSION" -p -e 2>/dev/null; }
tui_meta() { tui_text | grep -aE '(Coding|Planning) ·' | tail -1; }
# The live composer has a two-cell left inset; completion rows use one. Match
# that structural difference so popup candidates can never masquerade as input.
tui_input() { tui_text | grep -aE '│  > ' | tail -1; }
tui_alive() { tx has-session -t "$E2E_SESSION" 2>/dev/null; }

evidence_screen() {
  E2E_EVIDENCE_SEQUENCE=$((E2E_EVIDENCE_SEQUENCE + 1))
  printf -v screen_path '%s/screens/%04d.txt' "$E2E_EVIDENCE" "$E2E_EVIDENCE_SEQUENCE"
  mkdir -p "$E2E_EVIDENCE/screens"
  tui_text > "$screen_path" || true
}

# Wait internally for a semantic pane change followed by a stable render. This
# absorbs redraw timing without spending model turns on terminal polling.
semantic_tui_text() {
  tui_text | sed -E \
    -e 's/[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]/⠿/g' \
    -e 's/([0-9]+h )?([0-9]+m )?[0-9]+([.][0-9]+)?(ms|s)/<elapsed>/g' \
    -e 's/(⏱).*/\1 <elapsed>/'
}

wait_for_screen_settle() {
  local previous="$1" current changed=0 stable=0 i=0
  while [ "$i" -lt "$E2E_ASSERT_TRIES" ]; do
    current="$(semantic_tui_text | cksum)"
    if [ "$current" != "$previous" ]; then
      changed=1
      stable=0
      previous="$current"
    elif [ "$changed" -eq 1 ]; then
      stable=$((stable + 1))
      [ "$stable" -ge 2 ] && return 0
    fi
    sleep 0.05
    i=$((i + 1))
  done
  return 0
}

# wait_for <regex> [timeout_s]: poll the pane until the regex appears.
wait_for() {
  local re="$1" timeout="${2:-10}" i=0 max
  max=$(( timeout * 7 ))   # ~150ms per poll
  while [ "$i" -lt "$max" ]; do
    tui_text | grep -qaE "$re" && return 0
    sleep 0.15; i=$((i + 1))
  done
  return 1
}

# ---- assertions -----------------------------------------------------------
# expect* poll (so they tolerate render latency); forbid* check once (absence is
# immediate — sync first with a preceding expect or tui_keys settle).

_pass() { evidence_screen; echo "  ✅ $1"; }
_fail() { evidence_screen; echo "  ❌ $1"; E2E_FAIL=$((E2E_FAIL + 1)); }

expect() {        # expect <desc> <substr>  — whole pane eventually contains substr
  local desc="$1" want="$2" i=0
  while [ "$i" -lt "$E2E_ASSERT_TRIES" ]; do
    tui_text | grep -qaF -- "$want" && { _pass "$desc"; return; }
    sleep 0.15; i=$((i + 1))
  done
  _fail "$desc — missing '$want'"
}

expect_meta() {   # expect_meta <desc> <substr>  — composer meta line contains substr
  local desc="$1" want="$2" got="" i=0
  while [ "$i" -lt "$E2E_ASSERT_TRIES" ]; do
    got="$(tui_meta)"
    grep -qF -- "$want" <<<"$got" && { _pass "$desc"; return; }
    sleep 0.15; i=$((i + 1))
  done
  _fail "$desc — '$want' not in: ${got//[[:space:]]/ }"
}

forbid() {        # forbid <desc> <substr>  — whole pane does NOT contain substr
  local desc="$1" no="$2"
  if tui_text | grep -qaF -- "$no"; then _fail "$desc — '$no' present"; else _pass "$desc"; fi
}

forbid_meta() {   # forbid_meta <desc> <substr>  — meta line does NOT contain substr
  local desc="$1" no="$2" got; got="$(tui_meta)"
  if grep -qF -- "$no" <<<"$got"; then _fail "$desc — '$no' in: ${got//[[:space:]]/ }"; else _pass "$desc"; fi
}

expect_input() {  # expect_input <desc> <substr>  — composer input line contains substr
  local desc="$1" want="$2" got="" i=0
  while [ "$i" -lt "$E2E_ASSERT_TRIES" ]; do
    got="$(tui_input)"
    grep -qF -- "$want" <<<"$got" && { _pass "$desc"; return; }
    sleep 0.15; i=$((i + 1))
  done
  _fail "$desc — '$want' not in input: ${got//[[:space:]]/ }"
}

forbid_input() {  # forbid_input <desc> <substr>  — composer input line does NOT contain substr
  local desc="$1" no="$2" got; got="$(tui_input)"
  if grep -qF -- "$no" <<<"$got"; then _fail "$desc — '$no' in input: ${got//[[:space:]]/ }"; else _pass "$desc"; fi
}
