#!/usr/bin/env bash
set -euo pipefail

E2E_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERIFIER="$E2E_DIR/verifier.sh"
FIXTURE_ROOT="$(mktemp -d)"
trap 'rm -rf "$FIXTURE_ROOT"' EXIT

parent_home="$FIXTURE_ROOT/parent-bonsai"
state_root="$FIXTURE_ROOT/child-state"
evidence_dir="$FIXTURE_ROOT/evidence"
probe_output="$FIXTURE_ROOT/probe-output"
probe="$FIXTURE_ROOT/probe.sh"
mkdir -p "$parent_home"
printf 'parent database sentinel\n' > "$parent_home/bonsai.db"
cp "$parent_home/bonsai.db" "$FIXTURE_ROOT/parent-before.db"

cat > "$probe" <<'PROBE'
#!/usr/bin/env bash
set -euo pipefail
output="$1"
{
  printf 'HOME=%s\n' "$HOME"
  printf 'BONSAI_HOME=%s\n' "$BONSAI_HOME"
  printf 'CODEX_HOME=%s\n' "$CODEX_HOME"
  printf 'XDG_CONFIG_HOME=%s\n' "$XDG_CONFIG_HOME"
  printf 'BONSAI_DOTENV=%s\n' "$BONSAI_DOTENV"
  printf 'OPENAI_API_KEY=%s\n' "${OPENAI_API_KEY:-unset}"
  printf 'DEEPSEEK_API_KEY=%s\n' "${DEEPSEEK_API_KEY:-unset}"
  printf 'OPENAI_COMPATIBLE_API_KEY=%s\n' "$OPENAI_COMPATIBLE_API_KEY"
} > "$output"
printf 'isolated database sentinel\n' > "$BONSAI_HOME/bonsai.db"
PROBE
chmod +x "$probe"

BONSAI_HOME="$parent_home" \
OPENAI_API_KEY=real-openai-secret \
DEEPSEEK_API_KEY=real-deepseek-secret \
CODEX_HOME="$FIXTURE_ROOT/real-codex" \
BONSAI_DOTENV=1 \
  "$VERIFIER" \
    --state-root "$state_root" \
    --evidence-dir "$evidence_dir" \
    --binary "$probe" \
    -- "$probe_output"

canonical_state_root="$(cd "$state_root" && pwd -P)"
grep -qF "HOME=$canonical_state_root/home" "$probe_output"
grep -qF "BONSAI_HOME=$canonical_state_root/bonsai" "$probe_output"
grep -qF "CODEX_HOME=$canonical_state_root/codex" "$probe_output"
grep -qF "XDG_CONFIG_HOME=$canonical_state_root/xdg/config" "$probe_output"
grep -qF 'BONSAI_DOTENV=0' "$probe_output"
grep -qF 'OPENAI_API_KEY=unset' "$probe_output"
grep -qF 'DEEPSEEK_API_KEY=unset' "$probe_output"
grep -qF 'OPENAI_COMPATIBLE_API_KEY=e2e-test' "$probe_output"
cmp -s "$FIXTURE_ROOT/parent-before.db" "$parent_home/bonsai.db"
grep -qF 'isolated database sentinel' "$canonical_state_root/bonsai/bonsai.db"
grep -qF 'exit_code=0' "$evidence_dir/manifest.txt"
grep -qF 'database_sha256=' "$evidence_dir/manifest.txt"

if BONSAI_HOME="$parent_home" "$VERIFIER" \
  --state-root "$parent_home" \
  --evidence-dir "$FIXTURE_ROOT/refused-evidence" \
  --binary "$probe" \
  -- "$FIXTURE_ROOT/refused-output" 2>/dev/null; then
  echo "verifier accepted the parent BONSAI_HOME without explicit opt-in" >&2
  exit 1
fi

runs_root="$FIXTURE_ROOT/concurrent-runs"
first_output="$FIXTURE_ROOT/first-output"
second_output="$FIXTURE_ROOT/second-output"
BONSAI_VERIFIER_RUNS_ROOT="$runs_root" BONSAI_VERIFIER_BIN="$probe" \
  "$VERIFIER" -- "$first_output" >/dev/null 2>&1 &
first_pid=$!
BONSAI_VERIFIER_RUNS_ROOT="$runs_root" BONSAI_VERIFIER_BIN="$probe" \
  "$VERIFIER" -- "$second_output" >/dev/null 2>&1 &
second_pid=$!
wait "$first_pid"
wait "$second_pid"
first_home="$(grep '^BONSAI_HOME=' "$first_output")"
second_home="$(grep '^BONSAI_HOME=' "$second_output")"
[[ "$first_home" != "$second_home" ]]
[[ "$(find "$runs_root" -name manifest.txt | wc -l | tr -d ' ')" == 2 ]]

echo "PASS: verifier isolation contract"
