#!/usr/bin/env bash
# A session approval for echo cannot authorize an unrelated external redirect.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
e2e_begin "22_compound_permissions: remembered approvals stay scoped"

provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
marker="$E2E_HOME/COMPOUND_BLOCKED_MARKER"
python3 "$E2E_LIB_DIR/mock_compound_permission_provider.py" "$provider_ready" "$provider_requests" "$marker" &
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
tui_keys "Run the scripted permission probes without changing project files." Enter
expect "first command requests permission" "Allow this command to run"
expect "first command is the echo probe" "echo COMPOUND_ALLOW_APPROVED"
tui_keys s
expect "unrelated redirect requests its own approval" "COMPOUND_BLOCKED_MARKER"
screen="$(tui_text)"
if [[ "$screen" == *"Allow this command to run"* && "$screen" == *"COMPOUND_BLOCKED_MARKER"* ]]; then
  _pass "new approval prompt shows the unrelated redirect"
else
  _fail "unrelated command must not inherit the echo approval"
fi
tui_keys d
expect "model receives denial and completes" "COMPOUND_ALLOW_DENIED"
forbid "unrelated redirect did not run" "COMPOUND_ALLOW_BROKEN"
if [ ! -e "$marker" ]; then
  _pass "external marker was never created"
else
  _fail "unapproved command wrote outside the project"
fi
cp "$provider_requests" "$E2E_EVIDENCE/provider-requests.jsonl"
tui_keys "/quit" Enter
e2e_done
