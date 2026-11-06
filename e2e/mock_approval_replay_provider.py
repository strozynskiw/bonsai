#!/usr/bin/env python3
"""Repeat one approved command, then request a different command."""

import json
import sys
from pathlib import Path

from mock_completion_provider import CompletionHandler, CompletionServer


class ApprovalReplayHandler(CompletionHandler):
    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        request = self.server.record_request(payload)
        if request <= 3:
            tail = "EXACT_APPROVAL_B" if request <= 2 else "EXACT_APPROVAL_CHANGED"
            delta = {"tool_calls": [{
                "index": 0,
                "id": f"approval-replay-{request}",
                "type": "function",
                "function": {"name": "bash", "arguments": json.dumps({
                    "command": f"echo EXACT_APPROVAL_A && echo {tail}",
                })},
            }]}
            finish = "tool_calls"
        else:
            denied = any(
                message.get("role") == "tool"
                and "Permission denied" in str(message.get("content", ""))
                for message in payload["messages"]
            )
            delta = {"content": "No change was needed; " + (
                "EXACT_APPROVAL_SAFE" if denied else "EXACT_APPROVAL_BROKEN"
            )}
            finish = "stop"
        self.close_connection = True
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            for chunk in [
                {"choices": [{"delta": delta}]},
                {"choices": [{"delta": {}, "finish_reason": finish}]},
            ]:
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == "__main__":
    ready, log = map(Path, sys.argv[1:])
    log.write_text("", encoding="utf-8")
    with CompletionServer(("127.0.0.1", 0), log) as server:
        server.RequestHandlerClass = ApprovalReplayHandler
        ready.write_text(f"http://127.0.0.1:{server.server_port}/v1", encoding="utf-8")
        server.serve_forever()
