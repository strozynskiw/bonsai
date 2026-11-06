#!/usr/bin/env python3
"""Exercise a partial goal report followed by real remaining inspection."""

import json
import sys
from pathlib import Path

from mock_completion_provider import CompletionHandler, CompletionServer


class GoalProgressHandler(CompletionHandler):
    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        request = self.server.record_request(payload)
        self.close_connection = True
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        if request == 8 and not self.server.repeat_read:
            chunks = [
                {"choices": [{"delta": {"tool_calls": [{
                    "index": 0, "id": "goal-complete", "type": "function",
                    "function": {"name": "goal_complete", "arguments": json.dumps({
                        "evidence": "All four requested files were inspected."
                    })},
                }]}}]},
                {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]},
            ]
        elif request in (1, 3, 5, 7):
            paths = [
                "README.md"
                if self.server.repeat_read
                else {1: "README.md", 3: "lib.sh", 5: "run.sh", 7: "verifier.sh"}[request]
            ]
            calls = [
                {
                    "index": index,
                    "id": f"read-{request}-{index}",
                    "type": "function",
                    "function": {"name": "read", "arguments": json.dumps({"path": path})},
                }
                for index, path in enumerate(paths)
            ]
            chunks = [
                {"choices": [{"delta": {"tool_calls": calls}}]},
                {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]},
            ]
        else:
            answer = {
                2: "I reviewed README.md. The other three files remain unreviewed.",
                4: "I reviewed lib.sh. Two files remain unreviewed.",
                6: "I reviewed run.sh. One file remains unreviewed.",
            }.get(request, "All four requested files were reviewed.")
            if self.server.repeat_read:
                answer = "I reviewed README.md. The other three files remain unreviewed."
            chunks = [
                {"choices": [{"delta": {"content": answer}}]},
                {"choices": [{"delta": {}, "finish_reason": "stop"}]},
            ]
        try:
            for chunk in chunks:
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == "__main__":
    ready, log = map(Path, sys.argv[1:3])
    log.write_text("", encoding="utf-8")
    with CompletionServer(("127.0.0.1", 0), log) as server:
        server.RequestHandlerClass = GoalProgressHandler
        server.repeat_read = "--repeat-read" in sys.argv[3:]
        ready.write_text(f"http://127.0.0.1:{server.server_port}/v1", encoding="utf-8")
        server.serve_forever()
