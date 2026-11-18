#!/usr/bin/env bash
# A markdown file in `$BONSAI_HOME/commands/` becomes a slash command: it is
# listed in /help and the `/` popup, a built-in name clash is reported, and
# running it sends the expanded body as an ordinary prompt while the
# transcript keeps what the user typed.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
E2E_ASSERT_TRIES=80
WAIT=0.3
e2e_begin "29_custom_commands: list, complete and run a custom command"

# Global commands load without workspace trust, so the isolated (untrusted)
# launch directory does not matter here; trust gating is unit-tested.
commands_dir="$E2E_BONSAI_HOME/commands"
mkdir -p "$commands_dir"
cat > "$commands_dir/greet.md" <<'MD'
---
description: Greet someone warmly
argument-hint: <name>
---
Say hello to $ARGUMENTS in one short sentence.
MD
printf 'shadowing a built-in\n' > "$commands_dir/help.md"

provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
python3 "$E2E_LIB_DIR/mock_completion_provider.py" \
  "$provider_ready" "$provider_requests" &
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

tui_start 160 50 || e2e_done
tui_keys "/model" Enter
expect "model picker opens" "Filter models:"
tui_keys Left End Right
tui_keys "mock-model"
expect "loopback model is offered" "OpenAI Compatible · mock-model"
tui_keys Enter
expect_meta "loopback model selected" "Coding · mock-model"

tui_keys "/help" Enter
expect "/help lists the custom command with its hint" "/greet <name>"
expect "/help marks it as custom" "Greet someone warmly (custom)"
expect "/help reports the built-in clash" "! help.md"
tui_keys Escape

tui_keys "/gre"
expect "the / popup offers the custom command" "Greet someone warmly (custom)"
tui_keys C-u
forbid_input "composer cleared after the popup check" "/gre"

tui_keys "/greet Ada" Enter
expect "the custom command runs as a prompt" "deterministic request-1 complete"
expect "the transcript keeps the typed command" "/greet Ada"
forbid "the command is not reported as unknown" "Unknown command"

if python3 - "$provider_requests" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    requests = [json.loads(line) for line in stream if line.strip()]
if len(requests) != 1:
    raise SystemExit(f"expected 1 provider request, got {len(requests)}")
users = [m.get("content") for m in requests[0]["messages"] if m.get("role") == "user"]
text = json.dumps(users)
if "Say hello to Ada in one short sentence." not in text:
    raise SystemExit(f"expanded prompt missing from user messages: {text[:400]}")
if "/greet" in text:
    raise SystemExit("raw slash command leaked to the model")
PY
then
  _pass "the model received the expanded body, not the slash command"
else
  _fail "provider request did not carry the expanded prompt"
fi

tui_keys "/quit" Enter
e2e_done
