#!/usr/bin/env python3
"""Release a real provider stream only after the TUI stages a goal follow-up."""

import json
import sys
import time
from pathlib import Path

from mock_completion_provider import CompletionHandler, CompletionServer


class ConversationHandler(CompletionHandler):
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
                self.wfile.write(b'data: {"choices":[{"delta":{"content":"Inspecting the goal."}}]}\n\n')
                self.wfile.flush()
                deadline = time.monotonic() + 30
                while not self.server.release_file.exists() and time.monotonic() < deadline:
                    time.sleep(0.05)
            elif request == 2:
                messages = json.dumps(payload["messages"])
                valid = "GOAL_CONVERSATION" in messages and "FOLLOW_UP_TOKEN" in messages
                delta = {"choices": [{"delta": {"tool_calls": [{
                    "index": 0, "id": "goal-complete", "type": "function",
                    "function": {"name": "goal_complete", "arguments": json.dumps({
                        "evidence": "Goal and follow-ups addressed" if valid else "missing context"
                    })},
                }]}}]}
                self.wfile.write(f"data: {json.dumps(delta)}\n\n".encode())
            else:
                messages = json.dumps(payload["messages"])
                valid = "GOAL_CONVERSATION" in messages and "FOLLOW_UP_TOKEN" in messages
                content = "No change was needed; " + (
                    "goal-follow-up-received" if valid else "missing-conversation-context"
                )
                delta = {"choices": [{"delta": {"content": content}}]}
                self.wfile.write(f"data: {json.dumps(delta)}\n\n".encode())
            finish = "tool_calls" if request == 2 else "stop"
            self.wfile.write(f'data: {{"choices":[{{"delta":{{}},"finish_reason":"{finish}"}}]}}\n\ndata: [DONE]\n\n'.encode())
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == "__main__":
    ready, log, release = map(Path, sys.argv[1:])
    log.write_text("", encoding="utf-8")
    with CompletionServer(("127.0.0.1", 0), log) as server:
        server.release_file = release
        server.RequestHandlerClass = ConversationHandler
        ready.write_text(f"http://127.0.0.1:{server.server_port}/v1", encoding="utf-8")
        server.serve_forever()
