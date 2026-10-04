#!/usr/bin/env bash
set -euo pipefail
trap 'printf "FAIL: verifier isolation at line %s: %s\n" "$LINENO" "$BASH_COMMAND" >&2' ERR

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
  printf 'BONSAI_DISABLE_KEYRING=%s\n' "$BONSAI_DISABLE_KEYRING"
  printf 'OPENAI_API_KEY=%s\n' "${OPENAI_API_KEY:-unset}"
  printf 'DEEPSEEK_API_KEY=%s\n' "${DEEPSEEK_API_KEY:-unset}"
  printf 'OPENAI_COMPATIBLE_API_KEY=%s\n' "$OPENAI_COMPATIBLE_API_KEY"
} > "$output"
printf 'isolated database sentinel\n' > "$BONSAI_HOME/bonsai.db"
PROBE
chmod +x "$probe"

# Ungated custom paths fail before allocation or evidence truncation.
printf 'evidence sentinel\n' > "$FIXTURE_ROOT/inputs.log"
cp "$FIXTURE_ROOT/inputs.log" "$FIXTURE_ROOT/ungated-before"
for root_option in --state-root --evidence-dir; do
  if "$VERIFIER" "$root_option" "$FIXTURE_ROOT/ungated/missing" \
    --binary "$probe" -- "$probe_output" 2>/dev/null; then
    echo "verifier accepted ungated $root_option" >&2
    exit 1
  fi
  [[ ! -e "$FIXTURE_ROOT/ungated" && ! -e "$probe_output" ]]
done
if "$VERIFIER" --evidence-dir "$FIXTURE_ROOT" --allow-shared-state \
  --binary "$probe" -- "$probe_output" 2>/dev/null; then
  echo "shared-state switch bypassed the reusable-root gate" >&2
  exit 1
fi
cmp -s "$FIXTURE_ROOT/ungated-before" "$FIXTURE_ROOT/inputs.log"

BONSAI_HOME="$parent_home" \
OPENAI_API_KEY=real-openai-secret \
DEEPSEEK_API_KEY=real-deepseek-secret \
CODEX_HOME="$FIXTURE_ROOT/real-codex" \
BONSAI_DOTENV=1 \
  "$VERIFIER" \
    --allow-reusable-roots --state-root "$state_root" \
    --evidence-dir "$evidence_dir" \
    --binary "$probe" \
    -- "$probe_output"

canonical_state_root="$(cd "$state_root" && pwd -P)"
grep -qF "HOME=$canonical_state_root/home" "$probe_output"
grep -qF "BONSAI_HOME=$canonical_state_root/bonsai" "$probe_output"
grep -qF "CODEX_HOME=$canonical_state_root/codex" "$probe_output"
grep -qF "XDG_CONFIG_HOME=$canonical_state_root/xdg/config" "$probe_output"
grep -qF 'BONSAI_DOTENV=0' "$probe_output"
grep -qF 'BONSAI_DISABLE_KEYRING=1' "$probe_output"
grep -qF 'OPENAI_API_KEY=unset' "$probe_output"
grep -qF 'DEEPSEEK_API_KEY=unset' "$probe_output"
grep -qF 'OPENAI_COMPATIBLE_API_KEY=e2e-test' "$probe_output"
cmp -s "$FIXTURE_ROOT/parent-before.db" "$parent_home/bonsai.db"
grep -qF 'isolated database sentinel' "$canonical_state_root/bonsai/bonsai.db"
grep -qF 'exit_code=0' "$evidence_dir/manifest.txt"
grep -qF 'database_sha256=' "$evidence_dir/manifest.txt"

if BONSAI_HOME="$parent_home" "$VERIFIER" \
  --allow-reusable-roots --state-root "$parent_home" \
  --evidence-dir "$FIXTURE_ROOT/refused-evidence" \
  --binary "$probe" \
  -- "$FIXTURE_ROOT/refused-output" 2>/dev/null; then
  echo "verifier accepted the parent BONSAI_HOME without explicit opt-in" >&2
  exit 1
fi

mkdir -p "$FIXTURE_ROOT/aliased-state"
ln -s "$parent_home" "$FIXTURE_ROOT/aliased-state/bonsai"
if BONSAI_HOME="$parent_home" "$VERIFIER" \
  --allow-reusable-roots --state-root "$FIXTURE_ROOT/aliased-state" --binary "$probe" \
  -- "$FIXTURE_ROOT/aliased-output" 2>/dev/null; then
  echo "verifier accepted an aliased child state root" >&2
  exit 1
fi
printf 'evidence sentinel\n' > "$parent_home/inputs.log"
cp "$parent_home/inputs.log" "$FIXTURE_ROOT/inputs-before"
if BONSAI_HOME="$parent_home" "$VERIFIER" \
  --allow-reusable-roots --state-root "$FIXTURE_ROOT/refused-child" --evidence-dir "$parent_home" \
  --binary "$probe" -- "$FIXTURE_ROOT/refused-output" 2>/dev/null; then
  echo "verifier accepted evidence inside parent state" >&2
  exit 1
fi
[[ ! -e "$FIXTURE_ROOT/refused-child" ]]
if BONSAI_HOME="$parent_home" "$VERIFIER" \
  --allow-reusable-roots --state-root "$FIXTURE_ROOT/dot-child" \
  --evidence-dir "$FIXTURE_ROOT/nonexistent/../parent-bonsai" \
  --binary "$probe" -- "$FIXTURE_ROOT/refused-output" 2>/dev/null; then
  echo "verifier accepted an unresolved dot-component parent alias" >&2
  exit 1
fi
mkdir -p "$FIXTURE_ROOT/hardlinked-evidence"
ln "$parent_home/inputs.log" "$FIXTURE_ROOT/hardlinked-evidence/inputs.log"
if BONSAI_HOME="$parent_home" "$VERIFIER" \
  --allow-reusable-roots --state-root "$FIXTURE_ROOT/link-child" \
  --evidence-dir "$FIXTURE_ROOT/hardlinked-evidence" \
  --binary "$probe" -- "$FIXTURE_ROOT/refused-output" 2>/dev/null; then
  echo "verifier accepted hard-linked evidence" >&2
  exit 1
fi
cmp -s "$FIXTURE_ROOT/inputs-before" "$parent_home/inputs.log"
cmp -s "$FIXTURE_ROOT/parent-before.db" "$parent_home/bonsai.db"

ln -s "$parent_home" "$FIXTURE_ROOT/parent-alias"
mkdir -p "$FIXTURE_ROOT/hardlinked-state/bonsai"
ln "$parent_home/bonsai.db" "$FIXTURE_ROOT/hardlinked-state/bonsai/bonsai.db"
for refused_root in "$FIXTURE_ROOT/parent-alias" \
  "$FIXTURE_ROOT/missing/../parent-bonsai" "$FIXTURE_ROOT/hardlinked-state" \
  "$FIXTURE_ROOT"; do
  if BONSAI_HOME="$parent_home" "$VERIFIER" --allow-reusable-roots \
    --state-root "$refused_root" --binary "$probe" \
    -- "$FIXTURE_ROOT/refused-output" 2>/dev/null; then
    echo "verifier accepted parent-state overlap/alias: $refused_root" >&2
    exit 1
  fi
done
cmp -s "$FIXTURE_ROOT/parent-before.db" "$parent_home/bonsai.db"

# A second trusted launch cannot enter or truncate evidence before the first
# launch and its manifest are finalized.
serial_probe="$FIXTURE_ROOT/serial-probe.sh"
cat > "$serial_probe" <<'PROBE'
#!/usr/bin/env bash
set -euo pipefail
output="$1" release="$2"
printf 'started\n' > "$output"
for (( attempt=0; attempt<100; attempt++ )); do
  [[ ! -e "$release" ]] || exit 0
  sleep 0.1
done
exit 1
PROBE
chmod +x "$serial_probe"
serial_state="$FIXTURE_ROOT/serial-state"
serial_evidence="$FIXTURE_ROOT/serial-evidence"
first_output="$FIXTURE_ROOT/serial-first"
second_output="$FIXTURE_ROOT/serial-second"
release="$FIXTURE_ROOT/release"
"$VERIFIER" --allow-reusable-roots --state-root "$serial_state" \
  --evidence-dir "$serial_evidence" --binary "$serial_probe" \
  -- "$first_output" "$release" >/dev/null 2>&1 &
first_pid=$!
for (( attempt=0; attempt<100; attempt++ )); do
  [[ ! -f "$first_output" ]] || break
  sleep 0.05
done
[[ -f "$first_output" ]]
printf 'first launch evidence\n' > "$serial_evidence/inputs.log"
"$VERIFIER" --allow-reusable-roots --state-root "$serial_state" \
  --evidence-dir "$serial_evidence" --binary "$serial_probe" \
  -- "$second_output" "$release" >/dev/null 2>&1 &
second_pid=$!
sleep 0.3
[[ ! -e "$second_output" ]]
grep -qF 'first launch evidence' "$serial_evidence/inputs.log"
touch "$release"
wait "$first_pid"
wait "$second_pid"
[[ -f "$second_output" && ! -d "${serial_state}.verifier-lock" ]]
[[ ! -d "${serial_evidence}.verifier-lock" ]]
grep -qF 'exit_code=0' "$serial_evidence/manifest.txt"

# Defaults require no opt-in and allocate fresh roots concurrently.
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

# Retention may only delete positively completed runs, never an allocation that
# has not registered .active yet or an older-sorting active run.
mkdir -p "$runs_root/000-unregistered" "$runs_root/001-active"
touch "$runs_root/001-active/.active" "$runs_root/001-active/.completed"
for index in {1..25}; do
  mkdir -p "$runs_root/retained-$index"
  touch "$runs_root/retained-$index/.completed"
done
prune_pids=()
for index in {1..6}; do
  BONSAI_VERIFIER_RUNS_ROOT="$runs_root" BONSAI_VERIFIER_BIN="$probe" \
    "$VERIFIER" -- "$FIXTURE_ROOT/prune-output-$index" >/dev/null 2>&1 &
  prune_pids+=("$!")
done
for prune_pid in "${prune_pids[@]}"; do
  wait "$prune_pid"
done
[[ -d "$runs_root/000-unregistered" && -d "$runs_root/001-active" ]]
[[ "$(find "$runs_root" -name .completed ! -path '*/001-active/*' | wc -l | tr -d ' ')" == 20 ]]
[[ ! -d "${runs_root}.verifier-prune-lock" ]]

# Reopening a completed retained run must survive pruning pressure for the
# entire child lifetime, even though its old .completed marker remains present.
retained_run="$runs_root/000-reused"
mkdir -p "$retained_run/state"
touch "$retained_run/.completed"
rm -f "$release" "$first_output"
"$VERIFIER" --allow-reusable-roots --state-root "$retained_run/state" \
  --binary "$serial_probe" -- "$first_output" "$release" >/dev/null 2>&1 &
reuse_pid=$!
for (( attempt=0; attempt<100; attempt++ )); do
  [[ ! -f "$first_output" ]] || break
  sleep 0.05
done
[[ -f "$first_output" ]]
for index in {1..25}; do
  mkdir -p "$runs_root/pressure-$index"
  touch "$runs_root/pressure-$index/.completed"
done
BONSAI_VERIFIER_RUNS_ROOT="$runs_root" BONSAI_VERIFIER_BIN="$probe" \
  "$VERIFIER" -- "$second_output" >/dev/null 2>&1
[[ -d "$retained_run/state" && -f "$retained_run/state/evidence/manifest.txt" ]]
touch "$release"
wait "$reuse_pid"
grep -qF 'exit_code=0' "$retained_run/state/evidence/manifest.txt"
[[ ! -e "${retained_run}.verifier-retention-lock" ]]

echo "PASS: verifier isolation contract"
