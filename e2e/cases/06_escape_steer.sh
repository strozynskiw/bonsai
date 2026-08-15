#!/usr/bin/env bash
# Esc must only replace the foreground request after Enter queues a steer, and
# the interrupted request's delayed completion must not paint it as idle.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
WAIT=0.3
e2e_begin "06_escape_steer: replacement run keeps active spinner"

provider_ready="$E2E_HOME/provider-url"
python3 "$E2E_LIB_DIR/mock_streaming_provider.py" "$provider_ready" &
E2E_HELPER_PID=$!
for _ in {1..40}; do
  [ -s "$provider_ready" ] && break
  sleep 0.05
done
if [ ! -s "$provider_ready" ]; then
  _fail "mock provider did not start"
  e2e_done
fi
E2E_PROVIDER_BASE_URL="$(<"$provider_ready")"

tui_start 140 40 || e2e_done
tui_keys "/model" Enter
expect "model picker opens" "Filter models:"
tui_keys Left End Right
tui_keys "mock-model"
expect "mock model is filtered" "OpenAI Compatible · mock-model"
tui_keys Enter
expect_meta "mock model selected" "Coding · mock-model"

tui_keys "initial request" Enter
expect_meta "initial request is busy" "Coding · mock-model"
expect_meta "running hint advertises Enter queue" "Enter queue"
forbid_meta "running hint does not advertise Tab queue" "Tab queue"
forbid_meta "initial request has no idle dot" "● Coding"

tui_keys "unsent draft"
tui_keys Escape
forbid_input "Esc clears rather than sends an unqueued draft" "unsent draft"
forbid "unqueued Esc does not interrupt the request" "Run interrupted."
expect_meta "initial request remains busy after unqueued Esc" "Coding · mock-model"
forbid_meta "unqueued Esc keeps the active spinner" "● Coding"

tui_keys "urgent steering" Enter
expect "Enter stages the steering message" "urgent steering"
expect_meta "queued message enables Esc steer hint" "Esc steer"
tui_keys Escape
expect "interrupted request finishes" "Run interrupted."
expect "steer is promoted into the transcript" "urgent steering"
forbid_input "Enter-cleared steer stays out of the composer" "urgent steering"
expect_meta "replacement request is busy" "Coding · mock-model"
forbid_meta "replacement request has no idle dot" "● Coding"

e2e_done
