# Server preview changelog

The `/api/experimental/v0` contract is unstable. Record incompatible changes
here before updating its checked fixtures and clients. This does not promise v1
compatibility or durable token-delta replay.

## Initial preview

- One canonical workspace and one execution slot; explicit session resume.
- Bearer scopes, operator-only token management, exact Host/Origin policy,
  bounded mutation admission, SSE and artifact downloads.
- SSE IDs are opaque `process-epoch:sink-generation:sequence` cursors. Clients
  must preserve them verbatim and replace local state after `reset` + `snapshot`.
- Completion reports are explicitly published by the trusted backend as private,
  immutable JSON attachments. Tool-produced workspace paths are not published.
- Restart never repeats admitted model work or restores approvals. A new request
  ID is required for a genuinely new command, including an explicit reload.
- Production binary qualification and Python stdlib example live in
  `tests/server_preview.rs` and `examples/server_client.py`.
