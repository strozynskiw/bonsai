import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "e2e"))
from mock_completion_provider import CompletionHandler, CompletionServer


class Handler(CompletionHandler):
    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.record_request(payload)
        self.close_connection = True
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            text = "\n\n".join(f"REASONING_LINE_{i:02d}" for i in range(1, 9))
            data = {"choices": [{"delta": {"reasoning_content": text}}]}
            self.wfile.write(f"data: {json.dumps(data)}\n\n".encode())
            self.wfile.flush()
            deadline = time.monotonic() + 30
            while not self.server.release.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            self.wfile.write(b'data: {"choices":[{"delta":{"content":"REASONING_FINISHED"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n')
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


ready, log, release = map(Path, sys.argv[1:])
log.write_text("")
with CompletionServer(("127.0.0.1", 0), log) as server:
    server.release = release
    server.RequestHandlerClass = Handler
    ready.write_text(f"http://127.0.0.1:{server.server_port}/v1")
    server.serve_forever()
