#!/usr/bin/env python3
"""Finite OpenAI-compatible provider that records each TUI request."""

import json
import socket
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class CompletionServer(ThreadingHTTPServer):
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

    def __init__(self, address, request_log, truncate_first=False, literal_think=False):
        super().__init__(address, CompletionHandler)
        self.request_log = request_log
        self.request_count = 0
        self.request_lock = threading.Lock()
        self.truncate_first = truncate_first
        self.literal_think = literal_think

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
        truncated = self.server.truncate_first and request_id == 1
        answer = (
            "INCOMPLETE_ANSWER_SENTINEL"
            if truncated
            else "No change was needed; "
            f"deterministic request-{request_id} complete."
        )
        chunks = [
            {
                "choices": [
                    {
                        "delta": {
                            "content": answer
                        }
                    }
                ]
            },
            {"choices": [{"delta": {}, "finish_reason": "length" if truncated else "stop"}]},
        ]
        if self.server.literal_think:
            if request_id == 1:
                deltas = [
                    {"reasoning_content": "native reasoning", "content": "<thi"},
                    {"content": "nk>NATIVE_LITERAL</think>\n\nUse `<think>EXAMPLE_LITERAL</think>`."},
                ]
            else:
                deltas = [
                    {"content": "<thi"},
                    {"content": "nk>LEGACY_REASONING_SENTINEL</think>"
                     f"Legacy request-{request_id} complete: `<think>SECOND_LITERAL</think>`."},
                ]
            chunks = [{"choices": [{"delta": delta}]} for delta in deltas]
            chunks.append({"choices": [{"delta": {}, "finish_reason": "stop"}]})
        try:
            for chunk in chunks:
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            return


def main():
    if len(sys.argv) not in (3, 4) or (
        len(sys.argv) == 4 and sys.argv[3] not in ("--truncate-first", "--literal-think")
    ):
        raise SystemExit(
            "usage: mock_completion_provider.py <ready-file> <request-log> "
            "[--truncate-first|--literal-think]"
        )
    ready_file = Path(sys.argv[1])
    request_log = Path(sys.argv[2])
    request_log.write_text("", encoding="utf-8")
    mode = sys.argv[3] if len(sys.argv) == 4 else None
    with CompletionServer(
        ("127.0.0.1", 0), request_log,
        truncate_first=mode == "--truncate-first",
        literal_think=mode == "--literal-think",
    ) as server:
        ready_file.write_text(
            f"http://127.0.0.1:{server.server_port}/v1", encoding="utf-8"
        )
        server.serve_forever()


if __name__ == "__main__":
    main()
