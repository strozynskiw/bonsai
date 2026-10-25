#!/usr/bin/env python3
"""Minimal experimental-v0 client. Python 3.10+, standard library only.

Never follows redirects with credentials; decisions are always explicit.
"""
import argparse
import json
from pathlib import Path
import sys
import urllib.error
import urllib.parse
import urllib.request
import uuid


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Do not forward Bearer material to a redirected destination."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", required=True, help="Explicit base URL including /api/experimental/v0")
    parser.add_argument("--token-file", required=True, type=Path)
    parser.add_argument("--request-id", default=None, help="Reuse for an identical mutation retry")
    subs = parser.add_subparsers(dest="operation", required=True)
    subs.add_parser("list")
    subs.add_parser("create")
    for operation in ("get", "resume", "interrupt", "artifacts"):
        subs.add_parser(operation).add_argument("session", type=int)
    message = subs.add_parser("message")
    message.add_argument("session", type=int)
    message.add_argument("text")
    events = subs.add_parser("events")
    events.add_argument("session", type=int)
    events.add_argument("--last-event-id")
    answer = subs.add_parser("answer")
    answer.add_argument("session", type=int)
    answer.add_argument("run_id")
    answer.add_argument("interaction", type=int)
    answer.add_argument("answer_json", help='e.g. {"kind":"permission","decision":"deny"}')
    args = parser.parse_args()
    url = urllib.parse.urlsplit(args.url)
    if url.scheme not in ("http", "https") or not url.netloc or url.username or url.password or url.query or url.fragment:
        parser.error("URL must be an explicit http(s) address without credentials/query/fragment")
    token = args.token_file.read_text(encoding="utf-8").strip()
    if not token or any(c.isspace() for c in token):
        parser.error("Invalid bearer file")
    headers = {"Authorization": f"Bearer {token}"}
    path = "/sessions"
    payload = None
    if args.operation == "create":
        payload = {}
    elif args.operation != "list":
        path += f"/{args.session}"
        if args.operation not in ("get",):
            suffix = {"message": "messages", "answer": f"interactions/{getattr(args, 'interaction', 0)}/responses"}.get(args.operation, args.operation)
            path += f"/{suffix}"
        if args.operation in ("resume", "interrupt", "message", "answer"):
            payload = {}
        if args.operation == "message":
            payload["text"] = args.text
        if args.operation == "answer":
            payload.update(run_id=args.run_id, answer=json.loads(args.answer_json))
        if args.operation == "events" and args.last_event_id:
            headers["Last-Event-ID"] = args.last_event_id
    if payload is not None:
        payload["request_id"] = args.request_id or str(uuid.uuid4())
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(args.url.rstrip("/") + path,
                                     data=None if payload is None else json.dumps(payload).encode("utf-8"),
                                     headers=headers)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    with opener.open(request, timeout=35) as response:
        if response.headers.get("Bonsai-API-Version") != "experimental-v0":
            raise ValueError("Unexpected API version")
        if args.operation == "events":
            # SSE frames are printed verbatim, never interpreted as instructions.
            for line in response:
                sys.stdout.write(line.decode("utf-8"))
                sys.stdout.flush()
        else:
            print(json.dumps(json.load(response), ensure_ascii=False))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, urllib.error.URLError) as error:
        print(f"Request failed: {error}", file=sys.stderr)
        sys.exit(1)
