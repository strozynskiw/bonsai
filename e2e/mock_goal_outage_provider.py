#!/usr/bin/env python3
"""Recover after enough transient failures to exhaust the ordinary retry limit."""
import json
import sys
from pathlib import Path

from mock_completion_provider import CompletionHandler, CompletionServer


class Handler(CompletionHandler):
    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        request = self.server.record_request(payload)
        self.close_connection = True
        if request <= 4:
            body = b'{"error":{"message":"temporary outage"}}'
            self.send_response(500)
            self.send_header("Retry-After", "1")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        messages = json.dumps(payload["messages"])
        if request == 5:
            delta, finish = {"content": "old answer"}, "stop"
        elif request == 6 and "OUTAGE_FOLLOWUP" in messages:
            delta = {"tool_calls": [{"index": 0, "id": "goal-complete", "type": "function",
                     "function": {"name": "goal_complete", "arguments": json.dumps({"evidence": "The goal and follow-up were addressed."})}}]}
            finish = "tool_calls"
        else:
            delta, finish = {"content": "OUTAGE_RECOVERED" if "OUTAGE_FOLLOWUP" in messages else "OUTAGE_FOLLOWUP_MISSING"}, "stop"
        try:
            for chunk in [{"choices": [{"delta": delta}]}, {"choices": [{"delta": {}, "finish_reason": finish}]}]:
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


ready, log = map(Path, sys.argv[1:])
log.write_text("", encoding="utf-8")
with CompletionServer(("127.0.0.1", 0), log) as server:
    server.RequestHandlerClass = Handler
    ready.write_text(f"http://127.0.0.1:{server.server_port}/v1", encoding="utf-8")
    server.serve_forever()
