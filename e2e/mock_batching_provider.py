#!/usr/bin/env python3
"""Scripted OpenAI-compatible provider that exercises inspection batching.

Emits three consecutive single-call turns (which must trigger the batching
advisory), then one two-call turn, then a final answer. Each request is logged
so a case can assert on the exact wire payload the real TUI sent.
"""

import json
import socket
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

DEPENDENCY_NOTE = "Inspection dependency: the request named a single file to read."

FINAL_TEXT = "Inspection batching verified; no changes were needed."


def singleton(path):
    return [{"name": "read", "arguments": json.dumps({"path": path})}]


def paired(first, second):
    return [
        {"name": "read", "arguments": json.dumps({"path": first})},
        {"name": "read", "arguments": json.dumps({"path": second})},
    ]


# (assistant text, tool calls) per request index, 1-based.
SCRIPT = {
    1: (DEPENDENCY_NOTE, singleton("lib.sh")),
    2: ("", singleton("run.sh")),
    3: ("", singleton("verifier.sh")),
    4: ("", paired("README.md", "cases/01_execution_modes.sh")),
}


class BatchingServer(ThreadingHTTPServer):
    daemon_threads = True

    def server_bind(self):
        # http.server's bind calls socket.getfqdn(host), a reverse-DNS lookup
        # that can stall for tens of seconds on hosts without working mDNS.
        # Loopback binds never need a hostname, so bind directly.
        self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.socket.bind(self.server_address)
        self.server_address = self.socket.getsockname()
        self.server_name = "127.0.0.1"
        self.server_port = self.server_address[1]

    def __init__(self, address, request_log, repeat_call_ids=False):
        super().__init__(address, BatchingHandler)
        self.request_log = request_log
        self.request_count = 0
        self.request_lock = threading.Lock()
        self.repeat_call_ids = repeat_call_ids

    def record_request(self, payload):
        with self.request_lock:
            self.request_count += 1
            request_id = self.request_count
            with self.request_log.open("a", encoding="utf-8") as stream:
                stream.write(json.dumps(payload, separators=(",", ":")) + "\n")
            return request_id


class BatchingHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, _format, *_args):
        return

    def do_GET(self):
        payload = json.dumps({"data": [{"id": "mock-model"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)
        self.close_connection = True

    def do_POST(self):
        content_length = int(self.headers.get("Content-Length", "0"))
        payload = json.loads(self.rfile.read(content_length))
        request_id = self.server.record_request(payload)
        content, calls = SCRIPT.get(request_id, (FINAL_TEXT, []))
        if self.server.repeat_call_ids and request_id == 2:
            calls = singleton("lib.sh")
        self.close_connection = True
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        chunks = []
        if content:
            chunks.append({"choices": [{"delta": {"role": "assistant", "content": content}}]})
        for index, call in enumerate(calls):
            chunks.append(
                {
                    "choices": [
                        {
                            "delta": {
                                "tool_calls": [
                                    {
                                        "index": index,
                                        "id": "same-wire-id" if self.server.repeat_call_ids else f"call-{request_id}-{index}",
                                        "type": "function",
                                        "function": {
                                            "name": call["name"],
                                            "arguments": call["arguments"],
                                        },
                                    }
                                ]
                            }
                        }
                    ]
                }
            )
        finish = "tool_calls" if calls else "stop"
        chunks.append({"choices": [{"delta": {}, "finish_reason": finish}]})
        try:
            for chunk in chunks:
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            return


def main():
    if len(sys.argv) not in (3, 4) or (len(sys.argv) == 4 and sys.argv[3] != "--repeat-call-ids"):
        raise SystemExit("usage: mock_batching_provider.py <ready-file> <request-log> [--repeat-call-ids]")
    ready_file = Path(sys.argv[1])
    request_log = Path(sys.argv[2])
    request_log.write_text("", encoding="utf-8")
    with BatchingServer(("127.0.0.1", 0), request_log, len(sys.argv) == 4) as server:
        ready_file.write_text(
            f"http://127.0.0.1:{server.server_port}/v1", encoding="utf-8"
        )
        server.serve_forever()


if __name__ == "__main__":
    main()
