#!/usr/bin/env python3
"""Finite OpenAI-compatible provider that records each TUI request."""

import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class CompletionServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address, request_log):
        super().__init__(address, CompletionHandler)
        self.request_log = request_log
        self.request_count = 0
        self.request_lock = threading.Lock()

    def record_request(self, payload):
        with self.request_lock:
            self.request_count += 1
            request_id = self.request_count
            with self.request_log.open("a", encoding="utf-8") as stream:
                stream.write(json.dumps(payload, separators=(",", ":")) + "\n")
            return request_id


class CompletionHandler(BaseHTTPRequestHandler):
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
        self.close_connection = True
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        chunks = [
            {
                "choices": [
                    {
                        "delta": {
                            "content": (
                                "No change was needed; "
                                f"deterministic request-{request_id} complete."
                            )
                        }
                    }
                ]
            },
            {"choices": [{"delta": {}, "finish_reason": "stop"}]},
        ]
        try:
            for chunk in chunks:
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            return


def main():
    if len(sys.argv) != 3:
        raise SystemExit(
            "usage: mock_completion_provider.py <ready-file> <request-log>"
        )
    ready_file = Path(sys.argv[1])
    request_log = Path(sys.argv[2])
    request_log.write_text("", encoding="utf-8")
    with CompletionServer(("127.0.0.1", 0), request_log) as server:
        ready_file.write_text(
            f"http://127.0.0.1:{server.server_port}/v1", encoding="utf-8"
        )
        server.serve_forever()


if __name__ == "__main__":
    main()
