# Agent Client Protocol (ACP)

`bonsai acp` runs bonsai as an [Agent Client Protocol](https://agentclientprotocol.com)
**v1** agent. An editor that speaks ACP, such as Zed's Agent Panel, spawns it as a
subprocess and talks JSON-RPC 2.0 over stdio, one message per line. stdout carries
only protocol traffic. Diagnostics go to stderr and the session log.

ACP is not a separate engine. Each request goes to the same session controller as
[`bonsai serve`](server.md), with the same runtime builder, workspace trust, sandbox,
budgets, storage and permission path as the TUI and headless mode. Like the server,
the adapter is experimental in 0.x.

## Zed setup

Build or install bonsai, then add it to `~/.config/zed/settings.json`:

```json
{
  "agent_servers": {
    "bonsai": {
      "type": "custom",
      "command": "/absolute/path/to/bonsai",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

Open the Agent Panel, start a new **bonsai** thread, and prompt it as usual.

- **Model.** Pick it from the model selector in the thread's toolbar. It lists
  every model of every connection authorized in bonsai, grouped by connection:
  the same list as the TUI's `/model`. A new thread starts on bonsai's current
  model, the one `/model` saved or `BONSAI_PROVIDER` selects. Switching applies
  to that thread only and is refused while a prompt is running. To add a
  provider, authorize it in bonsai (`/authorize`); Zed cannot.
- **Credentials** are bonsai's own: the keyring, or the connection's API-key
  variable in `env`. bonsai advertises no ACP auth methods.
- **Logs.** In Zed, `dev: open acp logs` from the command palette shows the raw
  traffic. bonsai's session log has the agent side; `bonsai doctor` shows where it
  is.

## Workspace and trust

The editor starts `bonsai acp` in the project directory. That launch directory is
the operator's choice, and it is always accepted as the session root, trusted or
not, exactly like running `bonsai serve` there. An untrusted directory runs in
restricted mode: no project config, hooks or project model definitions.

`session/new` and `session/load` also carry the client's `cwd`. bonsai uses a
`cwd` other than the launch directory **only if it is already a trusted bonsai
workspace**. Trust it once by opening it in the `bonsai` TUI. Otherwise the request
fails with `-32602` and nothing is created. A client cannot point bonsai at a fresh
repository and inherit trust.

Every ACP thread starts at `ask`, the level `bonsai serve` uses, so every action
the rules mark `ask` produces a permission request. The **Autonomy** selector in
the thread toolbar (the ACP `autonomy` config option, category `mode`) offers
every level from `ask` **up to the level the operator saved in the bonsai TUI**,
which defaults to `balanced` when none is saved. The editor can never select
above that ceiling. `yolo` appears only for an operator who already saved `yolo`;
it removes the sandbox, the destructive floor and read-before-edit, exactly as in
the TUI. The choice applies live to the thread's later tool calls and is not
persisted: a reloaded thread starts at `ask` again.

Sandbox posture, workspace trust and run budgets always come from bonsai's
configuration; no ACP request changes them. `session/set_mode` is not
implemented and returns `-32601`. `session/set_config_option` accepts only
`model` and `autonomy`; any other id, an unknown value, or a level above the
ceiling returns `-32602`.

## Sessions

| ACP | bonsai |
| --- | --- |
| `initialize` | Protocol v1; `loadSession: true`; prompt capabilities `embeddedContext: true`, `image: false`, `audio: false`; MCP `http: false`, `sse: false`; session capabilities `list`; no auth methods. |
| `session/new` | Controller `create` + `resume`: a durable session row and an execution claim, with no model call. The session id is bonsai's numeric session id as a string. The response carries the `model` config option (also on `session/load`). |
| `session/load` | Controller `resume` of a session in the same workspace, then the durable transcript is replayed as `session/update` (user, assistant and thought chunks, and finished tool calls with their diffs) before the response. |
| `session/list` | The workspace's sessions, newest first, as the TUI's resume list shows them (up to 200, one page). Title is the session summary or its latest prompt. A client `cwd` follows the same trust rule as `session/new`; without one, the launch directory is listed. This is Zed's thread history. |
| `session/prompt` | One controller message turn. It responds with `end_turn`, or `cancelled` after `session/cancel`. A failed run is a JSON-RPC error with a fixed, redacted message. |
| `session/cancel` | Controller `interrupt`: the run's token is cancelled and pending prompts are denied. Open tool calls end `failed` with "Interrupted", and the thread gets a short "Stopped." line. |
| `session/set_config_option` | `model`: the idle session is released and resumed with the new connection and model, the same claim path as `session/load`. `autonomy`: the session's live approval level, capped as above. The response carries both updated options. |

Editors run several threads over one agent process, so each ACP session gets its
own controller, the same one-session engine `bonsai serve` uses. Up to 8 sessions
stay loaded per connection. Past that, the least recently used **idle** session
is drained; a running one is never cancelled to make room. Prompting a drained
session returns `-32002`, and `session/load` brings it back. Each loaded session
starts bonsai's configured MCP servers.

Session persistence is unchanged from the server. A session belongs to its
workspace, outlives the connection, and can be resumed from the TUI
(`bonsai -c <id>`) or loaded again over ACP, including from a new process. Closing
stdin drains every session and exits.

Prompt content: text blocks are concatenated. `resource_link` blocks become `@uri`
mentions. Embedded text resources are inlined as fenced blocks labeled with their
URI. Images, audio and binary resources are refused with `-32602`.

**Client-supplied MCP servers are rejected.** A `session/new` or `session/load`
with a non-empty `mcpServers` fails with `-32602`. Starting processes the client
chose, under bonsai's authority, needs a trust model that does not exist yet.
Configure MCP servers in bonsai instead (see [MCP servers](mcp.md)). If Zed has
context servers configured, they are sent with every new thread, so remove them
from Zed while using bonsai.

## Event mapping

Every `SessionEvent` kind that the HTTP contract publishes
([server-contract.json](server-contract.json) `event_kinds`) has exactly one ACP
carrier below. `cargo test` fails if a kind is missing here, if this table and the
code disagree, or if the sample projections in [acp-contract.json](acp-contract.json)
drift from what bonsai emits. The two surfaces cannot diverge silently.

| `kind` | ACP carrier | notes |
| --- | --- | --- |
| `reset` | `reconcile` | The live stream lagged. Tool calls still open are settled from a fresh snapshot. |
| `user_message` | `not carried` | The client renders its own prompt. `session/load` replays it as `user_message_chunk`. |
| `message_delivery` | `not carried` | Goal follow-up queueing has no ACP equivalent. |
| `assistant_delta` | `agent_message_chunk` | |
| `assistant_done` | `not carried` | Implied by the next update or the prompt response. |
| `reasoning_delta` | `agent_thought_chunk` | |
| `attempt_started` | `not carried` | |
| `attempt_discarded` | `not carried` | ACP cannot retract streamed text. Output of a discarded provider attempt stays visible. |
| `tool_started` | `tool_call` | `kind` from the tool (read/edit/execute/search/fetch/other), `locations` from its `path`, the raw arguments as `rawInput`. |
| `tool_output` | `tool_call_update` | The accumulated output (last 64 KiB) as content, `in_progress`. |
| `tool_result` | `tool_call_update` | `completed` or `failed`. Edits become `diff` content with whole `oldText`/`newText` rebuilt from the file and hunks, or hunk fragments when that cannot be done exactly. Command output is fenced text. |
| `usage` | `not carried` | |
| `plan` | `not carried` | The plan canvas has no ACP form. |
| `todos` | `plan` | `todowrite` items become ACP plan entries. Cancelled items are dropped. |
| `goal` | `not carried` | |
| `completion` | `agent_message_chunk` | Only for a run that ends `failed`, `blocked` or `budget_exhausted`: one short notice line. The provider's error text stays in the session log. |
| `status` | `not carried` | |
| `error` | `agent_message_chunk` | The secret-redacted reason, shown as `**Error:** …` in the thread. A run that fails outright also ends the prompt with a fixed JSON-RPC error. |
| `interaction` | `session/request_permission` | See [Permissions](#permissions). |
| `interaction_resolved` | `not carried` | |
| `lifecycle` | `session/prompt response` | The run's terminal state settles the prompt. |

## Permissions

bonsai raises a permission request whenever its own policy asks. The client's
answer is the **only** thing that crosses the boundary, and it applies to one
pending prompt of the current run. A stale, forged, unknown or cancelled answer is
a denial.

| bonsai prompt | ACP options | mapping |
| --- | --- | --- |
| Command, web domain, extension tool, hook trust | `allow_once` (allow_once), `allow_session` (allow_always), `reject` (reject_once) | Allow once, allow matching **for this session only**, deny. |
| Sandbox escalation | the same three | Run once outside the sandbox, allow **this exact command** for the session, deny. |
| Single-select question | one `choice_<n>` per option (allow_once), plus `reject` | The chosen option, or no answer. |
| Multi-select question | not sent | Declined by bonsai. ACP v1 has no multi-select prompt. |

No ACP answer writes a persistent project rule (`allow for project` or `never for
project`). Those stay TUI-only. A prompt attaches to the tool call that raised it
(`edit: path [workspace-write]` to its edit call, shell prompts to the latest shell
call). A prompt without a call, such as hook trust, gets its own tool-call card.

## Not available in this client

These bonsai features are deliberately not carried over ACP v1. Nothing is
substituted through a side channel. Use the TUI or [`bonsai serve`](server.md) for
them.

- **Goals**: `/goal`, goal follow-up queueing, goal pause and resume.
- **Plan canvas**: plan mode, plan sections and tasks, plan execution. Todo lists
  *are* shown, as the ACP plan.
- **Artifacts**: the server's artifact store.
- **Slash commands**: no `available_commands_update` is sent. Text starting with
  `/` is sent to the model as a prompt.
- **Mode and settings switching**: agent/plan/review modes and reasoning
  effort. ACP sessions run in the coding agent with the model's saved reasoning
  effort. (Model and autonomy can be switched; see above.)
- **Usage, cost and budget display**, and the **completion report**.
- **Persistent permission rules** (allow or deny for the project), and
  **multi-select questions**.
- **Retraction of discarded attempts**: text already streamed from a failed
  provider attempt stays in the thread.
- **Images and audio** in prompts, and **client-supplied MCP servers**.
- **Client file system and terminal**: bonsai uses its own sandboxed tools and
  never calls `fs/*` or `terminal/*`. Unsaved editor buffers are not visible to it.

