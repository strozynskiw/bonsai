#!/usr/bin/env bash
# A streamed length terminal must recover without presenting partial text as
# the final answer or replaying the user's task as a new human turn.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
WAIT=0.3
e2e_begin "13_truncated_response: recover before final completion"

provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
python3 "$E2E_LIB_DIR/mock_completion_provider.py" \
  "$provider_ready" "$provider_requests" --truncate-first &
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
expect "loopback model is offered" "OpenAI Compatible · mock-model"
tui_keys Enter
expect_meta "loopback model selected" "Coding · mock-model"

tui_keys "Explain the provider stream" Enter
expect "truncated answer is recovered" "deterministic request-2 complete"
forbid "partial answer is discarded from the visible transcript" "INCOMPLETE_ANSWER_SENTINEL"

if python3 - "$provider_requests" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    requests = [json.loads(line) for line in stream if line.strip()]
if len(requests) != 2:
    raise SystemExit(f"expected 2 provider requests, got {len(requests)}")
messages = requests[1]["messages"]
if sum(message.get("content") == "Explain the provider stream" for message in messages) != 1:
    raise SystemExit("recovery replayed or lost the human task")
if not any(message.get("role") == "assistant" and "INCOMPLETE_ANSWER_SENTINEL" in str(message.get("content")) for message in messages):
    raise SystemExit("recovery lost the partial answer context")
if "previous generation was truncated" not in json.dumps(messages):
    raise SystemExit("recovery omitted the truncation instruction")
PY
then
  _pass "recovery preserves partial context and exactly one human task"
else
  _fail "recovery request was incorrect"
fi

tui_keys "/quit" Enter
e2e_done
