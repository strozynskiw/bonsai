#!/usr/bin/env python3
"""Emit a tool call only after the TUI has queued a human correction."""

import json
import sys
import time
from pathlib import Path

from mock_completion_provider import CompletionHandler, CompletionServer


class SteeringHandler(CompletionHandler):
    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        request = self.server.record_request(payload)
        self.close_connection = True
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            if request == 1:
                self.wfile.write(b'data: {"choices":[{"delta":{"content":"Working on it."}}]}\n\n')
                self.wfile.flush()
                deadline = time.monotonic() + 30
                while not self.server.release_file.exists() and time.monotonic() < deadline:
                    time.sleep(0.05)
                delta = {"tool_calls": [{"index": 0, "id": "stale-call", "type": "function",
                    "function": {"name": "bash", "arguments": json.dumps({"command": "echo STALE_TOOL_EXECUTED"})}}]}
                finish = "tool_calls"
            else:
                messages = payload["messages"]
                skipped = any(message.get("role") == "tool" and message.get("tool_call_id") == "stale-call"
                    and "queued user message" in str(message.get("content", "")) for message in messages)
                steered = any(message.get("role") == "user" and "STEERING_FOLLOWUP" in str(message.get("content", "")) for message in messages)
                delta = {"content": "No change was needed; " + ("STEERING_APPLIED" if skipped and steered else "STEERING_FAILED")}
                finish = "stop"
            for chunk in [{"choices": [{"delta": delta}]}, {"choices": [{"delta": {}, "finish_reason": finish}]}]:
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == "__main__":
    ready, log, release = map(Path, sys.argv[1:])
    log.write_text("", encoding="utf-8")
    with CompletionServer(("127.0.0.1", 0), log) as server:
        server.release_file = release
        server.RequestHandlerClass = SteeringHandler
        ready.write_text(f"http://127.0.0.1:{server.server_port}/v1", encoding="utf-8")
        server.serve_forever()
