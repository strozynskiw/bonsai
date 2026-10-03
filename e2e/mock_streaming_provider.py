#!/usr/bin/env python3
"""Loopback OpenAI-compatible stream used by real-surface TUI tests."""

import json
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class StreamingServer(ThreadingHTTPServer):
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

    def __init__(self, address):
        super().__init__(address, StreamingHandler)
        self.request_count = 0
        self.request_count_lock = threading.Lock()

    def next_request_id(self):
        with self.request_count_lock:
            self.request_count += 1
            return self.request_count


class StreamingHandler(BaseHTTPRequestHandler):
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
        self.rfile.read(content_length)
        request_id = self.server.next_request_id()
        self.close_connection = True
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        delta = {
            "choices": [{"delta": {"content": f"request-{request_id} active"}}]
        }
        try:
            self.wfile.write(f"data: {json.dumps(delta)}\n\n".encode())
            self.wfile.flush()
            while True:
                time.sleep(0.1)
                self.wfile.write(b": keepalive\n\n")
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            return


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: mock_streaming_provider.py <ready-file>")
    ready_file = Path(sys.argv[1])
    with StreamingServer(("127.0.0.1", 0)) as server:
        ready_file.write_text(f"http://127.0.0.1:{server.server_port}/v1")
        server.serve_forever()


if __name__ == "__main__":
    main()
