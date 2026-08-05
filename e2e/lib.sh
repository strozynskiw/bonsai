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
TMUX_BIN="$(command -v tmux || echo /opt/homebrew/bin/tmux)"

E2E_SESSION="v"
E2E_STARTUP_WAIT="${E2E_STARTUP_WAIT:-12}"   # max seconds to reach chat-ready
WAIT="${WAIT:-0.8}"                          # per-keys settle delay
E2E_ASSERT_TRIES="${E2E_ASSERT_TRIES:-20}"   # expect* polls (×0.15s ≈ 3s)

E2E_FAIL=0
E2E_NAME="?"

tx() { "$TMUX_BIN" -L "$E2E_SOCK" "$@"; }

# ---- lifecycle ------------------------------------------------------------

# e2e_begin <case-name>: set up the isolated env and arrange cleanup.
e2e_begin() {
  E2E_NAME="$1"
  E2E_FAIL=0
  E2E_HOME="$(mktemp -d)"
  E2E_SOCK="bonsai-e2e-$$"
  trap e2e_cleanup EXIT
  printf '\n=== %s ===\n' "$E2E_NAME"
}

e2e_cleanup() {
  [ -n "${E2E_HELPER_PID:-}" ] && kill "$E2E_HELPER_PID" 2>/dev/null || true
  [ -n "${E2E_SOCK:-}" ] && tx kill-server 2>/dev/null || true
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
  local bonsai_args="${E2E_BONSAI_ARGS:-}"
  tx kill-server 2>/dev/null || true
  tx new-session -d -s "$E2E_SESSION" -x "$cols" -y "$rows"
  tx send-keys -t "$E2E_SESSION" \
    "exec env HOME='$E2E_HOME' BONSAI_HOME='$E2E_HOME' CODEX_HOME='$E2E_HOME/codex' \
BONSAI_DISABLE_MODELS_FETCH=1 OPENCODE_API_KEY='$opencode_api_key' ANTHROPIC_API_KEY='' \
MINIMAX_API_KEY='' MINIMAX_CODING_PLAN_API_KEY='' ZAI_API_KEY='' \
ZAI_CODING_PLAN_API_KEY='' MOONSHOT_API_KEY='' KIMI_CODING_PLAN_API_KEY='' \
MIMO_API_KEY='' MIMO_CODING_PLAN_API_KEY='' OPENROUTER_API_KEY='' OPENAI_API_KEY='' \
ANTHROPIC_COMPATIBLE_API_KEY='' DEEPSEEK_API_KEY='' DASHSCOPE_API_KEY='' \
DASHSCOPE_TOKEN_PLAN_API_KEY='' GEMINI_API_KEY='' XAI_API_KEY='' MISTRAL_API_KEY='' \
HUNYUAN_API_KEY='' \
BONSAI_MEMORY_EMBEDDINGS=off OPENAI_COMPATIBLE_BASE_URL='$provider_base_url' \
OPENAI_COMPATIBLE_MODEL='mock-model' OPENAI_COMPATIBLE_API_KEY='e2e-test' \
'$BONSAI_BIN' $bonsai_args" Enter
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
}

tui_keys() { tx send-keys -t "$E2E_SESSION" "$@"; sleep "$WAIT"; }
tui_text() { tx capture-pane -t "$E2E_SESSION" -p 2>/dev/null; }
tui_ansi() { tx capture-pane -t "$E2E_SESSION" -p -e 2>/dev/null; }
tui_meta() { tui_text | grep -aE '(Coding|Planning) ·' | tail -1; }
# The live composer has a two-cell left inset; completion rows use one. Match
# that structural difference so popup candidates can never masquerade as input.
tui_input() { tui_text | grep -aE '│  > ' | tail -1; }
tui_alive() { tx has-session -t "$E2E_SESSION" 2>/dev/null; }

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

_pass() { echo "  ✅ $1"; }
_fail() { echo "  ❌ $1"; E2E_FAIL=$((E2E_FAIL + 1)); }

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
