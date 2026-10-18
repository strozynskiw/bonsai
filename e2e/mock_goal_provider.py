#!/usr/bin/env python3
"""Finite loopback question flow for the real goal TUI surface."""

import json
import sys
from pathlib import Path

from mock_completion_provider import CompletionHandler, CompletionServer


class GoalHandler(CompletionHandler):
    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        self.server.record_request(payload)
        messages = payload["messages"]
        # Each /goal replaces intent while retaining history. Script only the
        # newest human objective so an old answer cannot satisfy the next goal.
        start = max((index for index, message in enumerate(messages)
                     if message.get("role") == "user" and
                     str(message.get("content", "")).startswith("Explain cache choices")), default=0)
        messages = messages[start:]
        text = "\n".join(str(message.get("content", "")) for message in messages)
        flow = "timeout-flow" if "timeout-flow" in text else "manual-flow"
        answered = "Question #" in text and (
            "Selected:" in text or "Automatically assumed recommended option:" in text
        )
        asked = any(
            call.get("function", {}).get("name") == "question"
            for message in messages
            for call in message.get("tool_calls", [])
        )
        if not asked:
            arguments = {
                "question": f"Select cache strategy ({flow})",
                "options": [{"label": "Simple"}, {"label": "Persistent"}],
                "recommended_index": 1,
                "timeout_seconds": 2 if flow == "timeout-flow" else 30,
            }
            delta = {"tool_calls": [{
                "index": 0, "id": "goal_question", "type": "function",
                "function": {"name": "question", "arguments": json.dumps(arguments)},
            }]}
            finish = "tool_calls"
        else:
            delta = {"content": "No change was needed; " + (
                f"goal-question-answer-delivered-{flow}" if answered else "waiting on cache choice"
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
        server.RequestHandlerClass = GoalHandler
        ready.write_text(f"http://127.0.0.1:{server.server_port}/v1", encoding="utf-8")
        server.serve_forever()
