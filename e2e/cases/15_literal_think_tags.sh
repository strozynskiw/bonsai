#!/usr/bin/env bash
# Native reasoning and later inline examples must preserve literal answer bytes
# in the terminal and in the next provider request.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
WAIT=0.3
e2e_begin "15_literal_think_tags: preserve answer examples across turns"

provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
python3 "$E2E_LIB_DIR/mock_completion_provider.py" \
  "$provider_ready" "$provider_requests" --literal-think &
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

tui_keys "Explain the tags" Enter
expect "native answer example renders" "EXAMPLE_LITERAL"
tui_keys "Explain the tags again" Enter
expect "legacy reasoning keeps the later answer example" "Legacy request-2 complete"
expect "later literal example renders" "SECOND_LITERAL"
tui_keys "Explain the tags once more" Enter
expect "third turn completes" "Legacy request-3 complete"

if python3 - "$provider_requests" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    requests = [json.loads(line) for line in stream if line.strip()]
if len(requests) != 3:
    raise SystemExit(f"expected 3 provider requests, got {len(requests)}")
first = "<think>NATIVE_LITERAL</think>\n\nUse `<think>EXAMPLE_LITERAL</think>`."
second = "Legacy request-2 complete: `<think>SECOND_LITERAL</think>`."
for request_index, expected in [(1, first), (2, first), (2, second)]:
    if not any(message.get("role") == "assistant" and message.get("content") == expected
               for message in requests[request_index]["messages"]):
        raise SystemExit(f"literal answer changed in provider request {request_index + 1}")
if "LEGACY_REASONING_SENTINEL" in json.dumps(requests):
    raise SystemExit("leading inline reasoning leaked into model-visible answer history")
PY
then
  _pass "native and later literal tags survive exact follow-up history"
else
  _fail "literal answer history was changed"
fi
cp "$provider_requests" "$E2E_EVIDENCE/provider-requests.jsonl"

tui_keys "/quit" Enter
e2e_done
