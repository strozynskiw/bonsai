#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
e2e_begin "reasoning preview and full detail"
provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
release="$E2E_HOME/release"
python3 "$E2E_LIB_DIR/mock_reasoning_preview_provider.py" "$provider_ready" "$provider_requests" "$release" &
E2E_HELPER_PID=$!
for _ in {1..40}; do [ -s "$provider_ready" ] && break; sleep 0.05; done
[ -s "$provider_ready" ] || { _fail "provider startup"; e2e_done; }
E2E_PROVIDER_BASE_URL="$(<"$provider_ready")"
tui_start 160 48 || e2e_done
tui_keys "/model openai-compatible:mock-model" Enter
expect_meta "mock model selected" "Coding · mock-model"
tui_keys "Inspect the streaming reasoning." Enter
expect "last reasoning line visible" "REASONING_LINE_08"
expect "larger preview shows additional context" "REASONING_LINE_06"
forbid "earlier reasoning is folded in transcript" "REASONING_LINE_01"
expect "fold reports hidden reasoning" "✦ thinking · +"
tui_keys Tab End
expect "selected fold advertises the detail affordance" "Enter to expand"
tui_keys Enter
expect "detail contains earlier reasoning" "REASONING_LINE_01"
expect "detail contains latest reasoning" "REASONING_LINE_08"
tui_keys Escape
touch "$release"
expect "response completes" "REASONING_FINISHED"
tui_keys "/quit" Enter
e2e_done
