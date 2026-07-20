# MCP servers

bonsai connects to [Model Context Protocol](https://modelcontextprotocol.io)
servers over **stdio** (a spawned child process) or **Streamable HTTP**,
discovers their tools at startup, and exposes each as a namespaced tool the
model calls like a built-in — behind the same permission engine, with every
result framed as untrusted data.

## Configuration

`[mcp.servers.<name>]` entries in [`.bonsai/config.toml`](configuration.md)
(project entries replace same-named global ones wholesale; project config
requires [workspace trust](security.md#workspace-trust)):

```toml
# Local filesystem server over stdio.
[mcp.servers.filesystem]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "./docs"]
capabilities = ["read"]          # declared read-only ⇒ lower risk tier
batching = "path_scoped"         # opt out of the serialized default
allow_tools = ["read_file", "list_directory", "search_files"]
timeout_secs = 20

# Remote docs server over Streamable HTTP.
[mcp.servers.context7]
transport = "http"
url = "https://mcp.context7.com/mcp"
headers = { Authorization = "Bearer ${CONTEXT7_API_KEY}" }  # env-expanded at connect, never stored expanded
capabilities = ["network"]

# OAuth-protected remote server — no token in this file; run /mcp auth workspace.
[mcp.servers.workspace]
transport = "http"
url = "https://mcp.example.com/mcp"
# oauth_client_id = "bonsai-public-client"   # optional; may be a Client ID Metadata Document URL
# oauth_scopes = ["mcp:tools"]
capabilities = ["read", "write", "network"]

# Parked entry — kept but not connected.
[mcp.servers.github]
transport = "stdio"
command = "docker"
args = ["run", "-i", "--rm", "-e", "GITHUB_TOKEN", "ghcr.io/github/github-mcp-server"]
enabled = false
```

| Field | Required | Meaning |
| --- | --- | --- |
| `transport` | yes | `"stdio"` or `"http"` |
| `command`, `args`, `env`, `cwd` | stdio | how to spawn; `${VAR}` in `env` expands at connect time |
| `url`, `headers` | http | endpoint + headers (`${VAR}`-expanded the same way) |
| `oauth_client_id`, `oauth_scopes` | http, optional | public client id (or Client ID Metadata Document URL) and scopes for servers without dynamic registration; no client secret is ever stored |
| `enabled` | no | default `true`; `false` parks the entry |
| `allow_tools` | no | tool-name allowlist; empty exposes every discovered tool |
| `capabilities` | no | any of `read`, `write`, `network`, `shell`, `irreversible`, `untrusted_output` — see below |
| `batching` | no | `"serialized"` (default) or `"path_scoped"` |
| `timeout_secs` | no | per-call timeout, default 30 |

## Namespacing and identity

- Wire name: `mcp__<server>__<tool>` (clamped to 64 chars with a hash suffix
  when needed). Dotted id `mcp.<server>.<tool>` is the human-facing identity
  used in permission rules, hook matchers, and `/mcp` output; the model can
  address either form.
- A wire-name collision with a built-in (or an earlier server) **visibly
  degrades** the losing server instead of shadowing or crashing.
- Connection at startup is concurrent across enabled servers with a 10 s
  bound; tools register in sorted `(server, tool)` order so the prompt
  suffix stays byte-stable.

## Trust and gating

- **Always untrusted.** Every result — success or error — is wrapped in the
  [untrusted-content frame](security.md#untrusted-content-stays-data)
  before the model sees it. Non-text content blocks (images, audio,
  resources) are dropped with placeholders.
- **Capabilities drive the risk tier.** Capabilities are *your*
  declaration, never the server's: all-`read` ⇒ Low (auto-runs at
  `balanced`); any `write`/`network`/`shell`/`untrusted_output` ⇒ High;
  `irreversible` ⇒ Destructive (always-ask floor); **undeclared ⇒ High and
  serialized** — the most cautious posture. `batching = "path_scoped"` is
  honored only when capabilities are within `{read, write}`.
- **Approvals bind to the declaration.** A remembered grant is keyed to the
  server's display id + declared capabilities + the tool's full input
  schema. A config privilege change *or* a remote schema change invalidates
  it and re-prompts. Argument previews in prompts are redacted and bounded.
- Permission rules for MCP use `kind = mcp` and match dotted ids
  (`mcp.filesystem.*`) — a separate namespace from bash and domain rules.
- Stdio servers spawn inside the [sandbox](sandbox.md) wrapper when active.

## OAuth

`/mcp auth <server> [file|keyring|session]` runs the MCP
protected-resource/authorization-server discovery flow:

- Requires advertised **S256 PKCE**; every endpoint must be HTTPS or
  loopback. Sends audience-bound resource parameters.
- Uses the configured public `oauth_client_id` (including Client ID Metadata
  Document URLs) when the authorization server has no dynamic registration;
  otherwise registers bonsai dynamically.
- A loopback callback (`http://127.0.0.1:<port>/callback`) completes an
  ordinary local-browser flow automatically (10-minute window). When the
  browser is on another machine, copy its final redirect URL and run
  **`/mcp callback <server> <redirect-url>`** in the original bonsai
  process.
- Tokens are stored through the same protected-file / OS-keyring /
  session-only boundary as provider credentials — never in SQLite or config
  files — keyed per server *and* endpoint. Access-token expiry refreshes
  automatically when a refresh token exists; a failed refresh leaves only
  that server visibly failed and points back to `/mcp auth`.
- **`/mcp revoke <server>`** uses the server's RFC 7009 revocation endpoint
  when advertised, then always removes the local login.
- A static `Authorization` header remains supported and takes precedence for
  that server; remove it before switching the entry to OAuth.

## Commands

- **`/mcp`** — status for every configured server (state, provenance,
  capabilities, tool count), as an interactive modal in the TUI.
- **`/mcp tools <server>`** — discovered tool names and descriptions.
- **`/mcp enable|disable <server>`** — session toggle.
- **`/mcp reload <server>`** — drop and reconnect one server.
- **`/mcp auth | callback | revoke <server>`** — the OAuth flow above.

`bonsai doctor --online` probes enabled servers' reachability; results
report only service names, versions, and pass/fail.

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Hub, discovery, registration | `src/mcp/mod.rs` |
| Transport clients | `src/mcp/client.rs` |
| The dynamic tool wrapper | `src/mcp/tool.rs` |
| OAuth | `src/mcp/auth.rs` |
| Capabilities → policy | `src/extension/capabilities.rs` |
| Shared gate & status registry | `src/extension/gate.rs`, `src/extension/status.rs` |
| `/mcp` command | `src/commands/mcp_cmd.rs` |
