#!/usr/bin/env bash
# Built-ins must launch in the background even with MCP configured.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
e2e_begin "19_readonly_subagents: MCP does not widen built-in grants"

provider_ready="$E2E_HOME/provider-url"
provider_requests="$E2E_HOME/provider-requests.jsonl"
python3 "$E2E_LIB_DIR/mock_subagent_provider.py" "$provider_ready" "$provider_requests" &
E2E_HELPER_PID=$!
for _ in {1..40}; do
  [ -s "$provider_ready" ] && break
  sleep 0.05
done
[ -s "$provider_ready" ] || { _fail "provider did not start"; e2e_done; }
E2E_PROVIDER_BASE_URL="$(<"$provider_ready")"
mkdir -p "$E2E_BONSAI_HOME"
cat > "$E2E_BONSAI_HOME/config.toml" <<EOF
schema_version = 1
[mcp.servers.regression]
transport = "stdio"
command = "$(command -v python3)"
args = ["$E2E_LIB_DIR/mock_subagent_provider.py", "--mcp"]
capabilities = ["read"]
EOF

tui_start 180 55 || e2e_done
tui_keys "/model openai-compatible:mock-model" Enter
expect_meta "mock model selected" "Coding · mock-model"
tui_keys "/autonomy yolo" Enter
tui_keys "Verify background read-only delegation" Enter
expect "parent completes delegation" "Background delegation verified."
forbid "background gate does not reject built-ins" "only supported for read-only"

# Check actual wire results, not only a scripted final answer.
python3 - "$provider_requests" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    requests = [json.loads(line) for line in stream if line.strip()]
parents = [p for p in requests if any(t["function"]["name"] == "agent" for t in p.get("tools", []))]
assert any(t["function"]["name"].startswith("mcp__") for t in parents[0]["tools"]), "MCP fixture was not discovered"
results = "\n".join(str(m.get("content", "")) for p in parents for m in p["messages"] if m.get("role") == "tool")
for name in ("explore", "research", "review", "security-review"):
    assert f"({name}). I will resume when it finishes." in results, f"{name} did not launch in background: {results}"
children = [p for p in requests if not any(t["function"]["name"] == "agent" for t in p.get("tools", []))]
assert len(children) >= 4, f"only {len(children)} child requests observed"
assert all(not t["function"]["name"].startswith("mcp__") for p in children for t in p.get("tools", [])), "child inherited MCP"
print("PASS: all four built-ins launched in background without MCP grants")
PY
# Closing the pane makes the shared screen-settle helper fail under errexit.
printf '%04d /quit Enter\n' "$E2E_LAUNCH_SEQUENCE" >> "$E2E_EVIDENCE/inputs.log"
tx send-keys -t "$E2E_SESSION" "/quit" Enter
for _ in {1..60}; do
  tui_alive || break
  sleep 0.1
done
if tui_alive; then
  _fail "TUI did not exit cleanly"
fi
cp -p "$provider_requests" "$E2E_EVIDENCE/provider-requests.jsonl"
e2e_done
