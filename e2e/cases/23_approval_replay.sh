#!/usr/bin/env bash
# Remember the whole approved command; changed commands still ask.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
e2e_begin "23_approval_replay: repeat exact approval without widening it"

provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
python3 "$E2E_LIB_DIR/mock_approval_replay_provider.py" "$provider_ready" "$provider_requests" &
E2E_HELPER_PID=$!
for _ in {1..40}; do
  [ -s "$provider_ready" ] && break
  sleep 0.05
done
[ -s "$provider_ready" ] || { _fail "provider startup"; e2e_done; }
E2E_PROVIDER_BASE_URL="$(<"$provider_ready")"
tui_start 170 48 || e2e_done
tui_keys "/model openai-compatible:mock-model" Enter
expect_meta "mock model selected" "Coding · mock-model"
tui_keys "/autonomy ask" Enter
tui_keys "Run the scripted approval replay probes without changing any files." Enter
expect "initial compound command asks" "Allow this command to run"
expect "initial command is visible" "EXACT_APPROVAL_B"
tui_keys s
expect "identical repeat finishes and changed command asks" "EXACT_APPROVAL_CHANGED"
screen="$(tui_text)"
cp "$provider_requests" "$E2E_EVIDENCE/provider-requests.jsonl"
if [[ "$screen" != *"Allow this command to run"* || "$screen" != *"EXACT_APPROVAL_CHANGED"* ]]; then
  _fail "identical command must not prompt again"
  e2e_done
fi
tui_keys d
expect "changed command denial reaches the model" "EXACT_APPROVAL_SAFE"
forbid "changed command was not silently allowed" "EXACT_APPROVAL_BROKEN"
cp "$provider_requests" "$E2E_EVIDENCE/provider-requests.jsonl"
tui_keys "/permissions" Enter
expect "approval has a readable command and directory scope" "exact command in"
tui_keys / "exact command" Enter
expect "displayed approval scope is searchable" "exact command in"
tui_keys d
expect "session approval can be revoked through the manager" "No editable rules"
tui_keys Escape
tui_keys "/quit" Enter
e2e_done
