# Experimental server preview

`bonsai serve` exposes one canonical workspace over HTTP + SSE. The API is
**unstable**, version `experimental-v0`, rooted at `/api/experimental/v0`.
This is a single-operator preview, not a multi-user service. See
[server-changelog.md](server-changelog.md) for contract changes.

## Install and start

Build with the stable Rust toolchain:

```sh
cargo build --release --locked
# Run in the workspace, not the source checkout unless that is your workspace.
/path/to/bonsai serve
```

The default listener is `127.0.0.1:0`. Startup reports the actual URL and a
**token file path and ID**, never the secret. `$BONSAI_HOME` (default
`~/.bonsai`) must be outside the workspace. Do not publish the token file or
put it in source control. Its mode is 0600; SQLite stores only its hash.

Choose/authorize a provider locally first using the normal TUI. `BONSAI_PROVIDER`
can select an authorized provider at startup; normal provider environment
variables apply. Credentials cannot be supplied or edited through HTTP.
Project-owned hooks/MCP/config run only when the workspace was trusted locally
in the TUI. An untrusted workspace remains restricted: API authentication does
not grant repository trust. Server autonomy starts at Ask; HTTP cannot change it.
Create and resume never start a model or restore a goal into execution.

A second server for the same canonical workspace fails fast. The OS owner lock
is separate from persisted session claims and is retained through runtime drain.
Stop with **Ctrl-C / SIGINT** and wait for successful exit. A failed or timed-out
drain is an error, not a successful shutdown. SIGKILL is crash recovery, not
clean shutdown; after restart explicitly resume and submit a new command.

Options:

| Option | Meaning |
| --- | --- |
| `--bind IP:PORT` | Literal IP only; defaults to `127.0.0.1:0` |
| `--allow-remote` | Explicit opt-in for a non-loopback bind |
| `--tls-cert PATH --tls-key PATH` | PEM certificate/key; both required together |
| `--allow-origin https://client.example` | Repeatable, exact origin, no trailing slash |
| `--allow-host api.example:443` | Repeatable exact Host authority |

Non-loopback requires **both** remote opt-in and TLS. Prefer loopback behind a
trusted tunnel or TLS reverse proxy. Forwarded headers are not identity; configure
the exact Host the proxy sends. No wildcard CORS, cookies, query credentials or
no-auth mode. Browser preflight is public only after Host/Origin validation;
the actual resource request always requires Bearer. Browser clients need fetch
streaming with Authorization, not credential-less EventSource.

## Operator tokens

Run token commands from the same workspace and with the same BONSAI_HOME:

```sh
bonsai serve token mint --output /private/operator/read-token --ttl 3600 --scope read --session 12
bonsai serve token revoke TOKEN_ID
```

The output must be a new file outside the workspace in an existing directory.
TTL is 1–86400 seconds (default 3600). Repeat `--scope` and `--session` to narrow
access; omitting scopes grants all four; omitting sessions makes it workspace-wide.
There is no HTTP mint endpoint. Revocation/expiry stops new commands and streams,
not already accepted execution or its children. Use a different valid control
token to interrupt. Tokens do not roll back completed effects.

| Scope | Operations |
| --- | --- |
| `read` | list/get/events, including sensitive prompts and tool/file content |
| `control` | create/resume/messages/interrupt; create requires workspace-wide scope |
| `respond` | respond to a particular currently pending interaction |
| `artifacts` | list/download registered artifacts |

Read is **not** public status access. Artifact isolation does not hide information
already present in transcripts. Redaction is best-effort, not a guarantee of
finding secrets in arbitrary user/tool content.

## Requests and errors

Every response, including errors/downloads, carries
`Bonsai-API-Version: experimental-v0`. Errors have
`{"error":{"code":"scope_denied","message":"scope_denied","request_id":"..."}}`.
Use code/status, not local diagnostic text. Unknown fields in command bodies
are rejected. Message input is user text only, never system prompts or raw tool
results, arbitrary paths, safety settings or provider credentials.

All mutation bodies contain a caller-chosen `request_id` (1–128 ASCII letters,
digits, `-_.:`). Persist it before sending and reuse **only for identical retries**.
A duplicate returns the recorded result; different content under that ID conflicts.
An admitted request interrupted by crash is not replayed. An unfinished mutation
may require operator recovery/new ID; never blindly reissue effects with new IDs.
Dedupe is reauthorized, not a public result cache. Interaction responses are also
correlated to session/run/prompt and consumed once by the engine.

| Method | Relative path | Body / behavior |
| --- | --- | --- |
| POST | `/sessions` | `{request_id}` → 201 + session_id |
| GET | `/sessions?after=0&limit=50` | workspace/scope-filtered list; limit 1–100 |
| GET | `/sessions/{id}?offset=0&limit=100` | paginated transcript, state, pending, usage; loaded sessions also plan/todo/budget |
| POST | `/sessions/{id}/resume` | `{request_id}`; claims/loads only |
| POST | `/sessions/{id}/messages` | `{request_id,text}` → 202 + run_id, acceptance not completion |
| POST | `/sessions/{id}/interrupt` | `{request_id}`; cancellation requested, not yet stopped |
| GET | `/sessions/{id}/events` | SSE; requires loaded session |
| POST | `/sessions/{id}/interactions/{prompt_id}/responses` | `{request_id,run_id,answer}` |
| GET | `/sessions/{id}/artifacts` | explicitly registered artifacts |
| GET | `/sessions/{id}/artifacts/{artifact_id}` | attachment, octet-stream, nosniff |

Only one session may execute, including children/background tasks/terminals.
Busy chat conflicts rather than queueing/steering. Switching sessions may release
an idle runtime, never silently cancel an active runtime. Status, interrupt and
interaction responses remain responsive during execution.

A pending prompt exposes its kind and allowed decisions. Permission/web/extension/
hook prompts use `{"kind":"permission","decision":"allow_once"}` or `deny`;
other permission decisions are allowed only as advertised. Sandbox prompts use
`{"kind":"sandbox","decision":"allow_once"}` (or allow_for_session/deny).
Questions use `{"kind":"question","choices":[0]}`; null cancels. There is no
HTTP approve-all. Unknown/stale/wrong-run/wrong-type answers cannot grant authority.

## Curl and Python example

Use a private header file, not the secret in process arguments:

```sh
BASE=http://127.0.0.1:PORT/api/experimental/v0
TOKEN_FILE=/private/path/reported-at-startup
umask 077
AUTH=$(mktemp)
printf 'Authorization: Bearer %s\n' "$(cat "$TOKEN_FILE")" > "$AUTH"
curl -H @"$AUTH" -H 'Content-Type: application/json' -d '{"request_id":"create-1"}' "$BASE/sessions"
curl -H @"$AUTH" -H 'Content-Type: application/json' -d '{"request_id":"resume-1"}' "$BASE/sessions/12/resume"
curl -N -H @"$AUTH" "$BASE/sessions/12/events"
# In another terminal, using the same private header file:
curl -H @"$AUTH" -H 'Content-Type: application/json' -d '{"request_id":"chat-1","text":"Inspect README.md"}' "$BASE/sessions/12/messages"
curl -H @"$AUTH" -H 'Content-Type: application/json' -d '{"request_id":"answer-1","run_id":"RUN","answer":{"kind":"permission","decision":"deny"}}' "$BASE/sessions/12/interactions/1/responses"
rm "$AUTH"
```

[examples/server_client.py](../examples/server_client.py) requires Python 3.10+
and only the standard library. It refuses redirects and ambient proxies, never
auto-approves prompts and verifies the API version:

```sh
python3 examples/server_client.py --url "$BASE" --token-file "$TOKEN_FILE" list
python3 examples/server_client.py --url "$BASE" --token-file "$TOKEN_FILE" --request-id create-2 create
python3 examples/server_client.py --url "$BASE" --token-file "$TOKEN_FILE" events 12
python3 examples/server_client.py --url "$BASE" --token-file "$TOKEN_FILE" answer 12 RUN 1 '{"kind":"permission","decision":"deny"}'
```

## SSE, persistence and bounds

First connection sends a consistent snapshot with watermark, then ordered newer
events. Keep Last-Event-ID opaque: it includes process epoch, sink/load generation
and sequence. Reconnect with that header. Valid cursors replay within a bounded
in-memory window; gaps, replacement sink or restart cause explicit reset + snapshot.
Heartbeat/reset do not advance the event cursor. A resync-required reset ends a
lagging stream: reconnect without a cursor to obtain current state. A disconnected
client never cancels work or approves pending prompts. Deltas are not a durable log.
Persisted snapshots/reports are authoritative; completion is published after writes.

Body 1 MiB; pages ≤100; SSE clients 8; replay ≤1000 events/8 MiB;
download ≤64 MiB; requests 30 s; shutdown 30 s; concurrent requests 32;
request rate 120/s (process-wide fixed window); blocked SSE handoff 5 s.
Oversize/overload yields 413/429 or explicit resync, not an unbounded queue.
Existing cumulative session/child budgets still apply. Large snapshots exceeding
the SSE payload bound require paginated GET; never treat reset as complete history.

Artifacts are immutable private copies with opaque ID, hash and size. Only trusted
backend qualification can publish; no filesystem browsing or model/HTTP arbitrary
path registration. Workspace tool outputs are **not** automatically downloadable.
The server publishes an allowlisted completion summary as a JSON attachment:
status, aggregate authorization/usage, verification status and review status only.
It excludes commands, reviewer text, paths and arbitrary diagnostics; `artifacts`
does not grant `read`. Arbitrary generated deliverables require an explicit trusted adapter. Symlink, hardlink,
replacement, size and in-copy metadata changes are rejected; download rechecks hash.

## Qualification and limitations

`cargo test --locked --test server_preview` launches a real binary with isolated
HOME/DB/workspace and mock provider, runs permissions/tools/diff, a Python request,
interrupt and restart/reconnect. Inline server tests cover token matrix, every prompt
type, artifact attacks, replay gaps, slow consumers and persistence failures.
`cargo test --locked --test surface_smoke` retains headless/TUI safety regressions.
[server-contract.json](server-contract.json) fixtures are checked against actual Rust
request DTOs, decision decoding and event projections by normal `cargo test`; CI
therefore rejects fixture drift. Breaking preview changes must be recorded in
[server-changelog.md](server-changelog.md) with client migration guidance.

No uploads, steer/queue, provider/credential management, goal mutation API, ACP,
web UI, parallel workspaces/scheduler or stable v1 compatibility. Pending prompts
and work are never automatically replayed on restart. After a hard crash, resume
can return 409 until the conservative 15-second heartbeat fence expires; retry
explicitly with a new request ID after a rejected/incomplete admission (an identical
ID retrieves historical acceptance, not a new attempt). A live OS owner is never displaced, regardless of heartbeat age. #46 is not a claim of
production multi-tenant security; #77, #78, #79 and #179 remain separate work.
