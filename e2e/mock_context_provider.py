#!/usr/bin/env python3
"""Finite loopback provider for context continuity through the real terminal."""

import json
import re
import sys
from pathlib import Path

from mock_completion_provider import CompletionHandler, CompletionServer


class ContextHandler(CompletionHandler):
    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        self.server.record_request(payload)
        messages = payload["messages"]
        system = str(messages[0].get("content", ""))
        if system.startswith(("You summarize prior chat history", "You maintain a rolling summary")):
            self.server.summary_count += 1
            answer = (
                "# Compacted Context Summary\n\n## Current goal\n"
                "Explain the context examples.\n\n## Decisions\n"
                f"CTX_SUMMARY_{self.server.summary_count}: preserve the user's request."
            )
            if self.server.summary_count == 3:
                answer = "OVERSIZED_GENERATED_SUMMARY " * 1_000
            delta = {"content": answer}
            finish = "stop"
        elif system.startswith("You write a compact completion card"):
            delta = {"content": "## Episode card\nOutcome: example explained.\nDecisions: preserve user intent."}
            finish = "stop"
        else:
            user_index, user = next(
                (index, message)
                for index, message in reversed(list(enumerate(messages)))
                if message.get("role") == "user" and "CONTEXT_TURN_" in str(message.get("content", ""))
                and not message.get("name")
            )
            turn = int(re.search(r"CONTEXT_TURN_(\d+)", str(user["content"]))[1])
            has_result = any(message.get("role") == "tool" for message in messages[user_index + 1:])
            if turn in (1, 2) and not has_result:
                delta = {"tool_calls": [
                    {
                        "index": 0, "id": f"context-title-{turn}", "type": "function",
                        "function": {"name": "set_session_title", "arguments": json.dumps({
                            "title": "Explain initial example" if turn == 1 else "Explain partial commits",
                            "episode_action": "new_topic",
                        })},
                    },
                    {
                        "index": 1, "id": f"context-read-{turn}", "type": "function",
                        "function": {"name": "read", "arguments": json.dumps({
                            "path": "lib.sh", "offset": turn, "limit": 5,
                        })},
                    },
                ]}
                finish = "tool_calls"
            else:
                delta = {"content": (
                    f"ARCHIVE_SENTINEL_{turn}\n\n"
                    + "Historical explanation of the example and its assumptions. " * 100
                    + f"\n\nNo change was needed; CONTEXT_RESULT_{turn} complete."
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


def main():
    ready, log = map(Path, sys.argv[1:])
    log.write_text("", encoding="utf-8")
    with CompletionServer(("127.0.0.1", 0), log) as server:
        server.RequestHandlerClass = ContextHandler
        server.summary_count = 0
        ready.write_text(f"http://127.0.0.1:{server.server_port}/v1", encoding="utf-8")
        server.serve_forever()


if __name__ == "__main__":
    main()
