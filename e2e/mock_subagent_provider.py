#!/usr/bin/env python3
"""Loopback provider and inert MCP server for read-only delegation coverage."""

import json
import sys
from pathlib import Path

from mock_batching_provider import BatchingServer, SCRIPT


class SubagentServer(BatchingServer):
    def record_request(self, payload):
        super().record_request(payload)
        names = [tool["function"]["name"] for tool in payload.get("tools", [])]
        if "agent" not in names:
            return 2
        if any(message.get("role") == "tool" for message in payload["messages"]):
            return 3
        return 1


def serve_mcp():
    for line in sys.stdin:
        request = json.loads(line)
        if "id" not in request:
            continue
        method = request["method"]
        if method == "initialize":
            result = {
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "readonly-regression", "version": "1"},
            }
        elif method == "tools/list":
            result = {"tools": [{
                "name": "echo",
                "description": "Inert regression fixture",
                "inputSchema": {"type": "object", "properties": {}},
            }]}
        else:
            result = {}
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)


def main():
    if sys.argv[1:] == ["--mcp"]:
        serve_mcp()
        return
    ready_file, request_log = map(Path, sys.argv[1:])
    request_log.write_text("", encoding="utf-8")
    SCRIPT.clear()
    SCRIPT[1] = ("", [
        {"name": "agent", "arguments": json.dumps({
            "agent": name,
            "prompt": "Read-only analysis: report the fixture result without tool calls.",
            "run_in_background": True,
        })}
        for name in ("explore", "research", "review", "security-review")
    ])
    SCRIPT[2] = ("Read-only subagent completed.", [])
    SCRIPT[3] = ("Background delegation verified.", [])
    with SubagentServer(("127.0.0.1", 0), request_log) as server:
        ready_file.write_text(f"http://127.0.0.1:{server.server_port}/v1", encoding="utf-8")
        server.serve_forever()


if __name__ == "__main__":
    main()
