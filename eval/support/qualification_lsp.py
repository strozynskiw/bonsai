"""Network-free LSP protocol fixture; never evidence of a real language server."""

import json
import sys
from typing import BinaryIO


def read_message(stream: BinaryIO) -> dict[str, object] | None:
    length = None
    while line := stream.readline():
        if line == b"\r\n":
            break
        name, _, value = line.partition(b":")
        if name.lower() == b"content-length":
            length = int(value.strip())
    if length is None:
        return None
    message = json.loads(stream.read(length))
    if not isinstance(message, dict):
        raise ValueError("LSP message must be an object")
    return message


def serve() -> None:
    while (message := read_message(sys.stdin.buffer)) is not None:
        method = message.get("method")
        if method == "exit":
            return
        if "id" not in message:
            continue
        if method == "initialize":
            result = {"capabilities": {"workspaceSymbolProvider": True}}
        elif method == "workspace/symbol":
            result = []
        elif method == "shutdown":
            result = None
        else:
            response = {"jsonrpc": "2.0", "id": message["id"],
                        "error": {"code": -32601, "message": "Unsupported fixture method"}}
            send(response)
            continue
        send({"jsonrpc": "2.0", "id": message["id"], "result": result})


def send(message: dict[str, object]) -> None:
    payload = json.dumps(message).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(payload)}\r\n\r\n".encode("ascii"))
    sys.stdout.buffer.write(payload)
    sys.stdout.buffer.flush()


if __name__ == "__main__":
    serve()
