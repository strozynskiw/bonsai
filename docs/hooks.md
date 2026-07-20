# Hooks

A hook runs a shell command, an HTTP request, or a one-shot LLM judge around
a lifecycle event — formatting a file after every write, blocking edits to
`.env`, auditing every bash call, or gating a risky change behind a fast
model's review. Hooks are defined as `[[hooks]]` tables in
[`.bonsai/config.toml`](configuration.md).

## Definition

```toml
[[hooks]]
name = "cargo-fmt"                       # unique id (lowercase, [a-z0-9_-])
event = "PostFileWrite"
matcher = { path = "**/*.rs" }           # and/or { tool = "<glob>" }
action = { type = "shell", command = "cargo fmt -- \"$BONSAI_FILE_PATH\"" }
timeout_secs = 20                        # default 30
blocking = false                         # default
on_failure = "warn"                      # default; or "block"
enabled = true                           # default; false parks the entry
```

| Field | Notes |
| --- | --- |
| `event` | `SessionStart`, `SessionEnd`, `PreToolUse`, `PostToolUse`, `PreFileWrite`, `PostFileWrite`, `PreBash`, `PostBash` |
| `matcher` | `tool` and/or `path` glob; omitted matches every call for that event. A set matcher against a missing context field is a non-match; an invalid glob fails the hook at load. |
| `action` | `{ type = "shell", command }`, `{ type = "http", url, headers }`, or `{ type = "llm_prompt", prompt }`. `${VAR}` in headers expands at fire time. |
| `blocking` | Only meaningful on the **pre** events (`PreToolUse`/`PreFileWrite`/`PreBash`) — a blocking hook there may veto or (PreToolUse only) rewrite the call. Every other event is always non-veto. |
| `on_failure` | `warn` (fail-open: a broken hook warns and the action proceeds) or `block` (fail-closed; effective only on blocking pre-events). A failed run never disables the hook — it fires again next match. |

Execution model: matching **vetoing** hooks (blocking pre-hooks) run
sequentially, first `block` wins, last `modify` wins; all other matching
hooks run concurrently and can only contribute context. Hook-contributed
context is folded into the tool result as **untrusted data**, never a system
message.

## The wire protocol

**Shell** actions receive one JSON object on stdin; **HTTP** actions receive
it as the POST body:

```json
{
  "event": "PreFileWrite", "session_id": "…", "cwd": "/path/to/project",
  "tool_name": "write", "file_path": "src/main.rs", "tool_args": {"path": "src/main.rs"},
  "command": null, "exit_code": null, "output_excerpt": null,
  "diff": "diff --bonsai modified src/main.rs\n-old\n+new\n"
}
```

Environment variables accompany shell actions for scripts that don't parse
JSON: `BONSAI_HOOK_EVENT`, `BONSAI_PROJECT_ROOT`, `BONSAI_SESSION_ID`, and
(when applicable) `BONSAI_TOOL_NAME`, `BONSAI_FILE_PATH`, `BONSAI_COMMAND`.

**Decision semantics:**

- Shell exit `0` → continue; stdout may carry a JSON response for finer
  control (below). Exit `2` → **block**, with stderr as the reason (honored
  only on a blocking pre-event). Any other exit, or death by signal →
  failure, handled by `on_failure`.
- HTTP: 2xx → continue (+ optional JSON body); anything else → failure.
- Optional JSON response on success:
  `{"decision":"block","reason":"…"}`,
  `{"decision":"modify","tool_args":{…}}` (PreToolUse only), or
  `{"context":"…"}` (folded in as untrusted data). An empty body means
  continue; malformed non-empty JSON is a failed run and follows
  `on_failure` — including fail-closed vetoes on blocking pre-events.
- A block/modify from a hook that *cannot* veto (non-blocking, or a post
  event) is downgraded to continue with a warning.

**Limits:** the timeout covers stdin delivery, execution, and output
draining; timeout or cancellation terminates the hook's whole process group
(SIGTERM, 200 ms grace, SIGKILL). Captured stdout and stderr are each capped
at 64 KiB — an oversized JSON response fails rather than being parsed
partially. Output excerpts handed to hooks are pre-redacted; hook stderr and
diagnostics are redacted before display.

## LLM-judge actions

An `llm_prompt` action sends its prompt to a fast model and requires strict
JSON back: `{"decision":"allow"|"block","reason":"…"}`. Template markers —
`{{tool_name}}`, `{{command}}`, `{{file_path}}`, `{{diff}}`, `{{output}}` —
are each substituted **wrapped as untrusted data** first, so a hostile tool
result cannot steer the judge; the prompt template itself is trusted operator
configuration. Unparseable responses are failures under `on_failure`.

```toml
[[hooks]]
name = "migration-review"
event = "PreFileWrite"
matcher = { path = "migrations/**" }
blocking = true
timeout_secs = 45
action = { type = "llm_prompt", prompt = """
You review a proposed file change. Block it only if it drops or truncates data.
Reply with strict JSON: {"decision":"allow"|"block","reason":"..."}.
Change:
{{diff}}
""" }
```

## Diff evidence

`PreFileWrite` and `PostFileWrite` receive the same redacted per-file diff in
the JSON `diff` field (LLM hooks via `{{diff}}`). A multi-file operation runs
every pre hook before the first disk write and emits post hooks only after
the whole transaction commits. Deletes render against `/dev/null`; a move is
an explicit destination creation plus source deletion. Diff evidence is
capped at 2,000 rendered lines and 128 KiB with a `[diff truncated …]`
marker and full add/remove totals when a bound is hit. The structured diff
shown in the TUI and the hook evidence come from the same precondition and
intended content.

## Trust

- **Global-config hooks are pre-trusted** — you wrote your own home file.
- A **project** hook with a `shell` or `http` action needs a one-time
  approval before it first runs, shown as a permission-style prompt (action
  command/URL; once / session / project / deny). The trust record hashes the
  *complete* effect declaration — name, event, action, matcher, timeout,
  blocking, failure policy — so editing any of it re-prompts.
- `llm_prompt` hooks skip the gate: they only reach the model provider you
  already authorized.
- Until answered, `/hooks` shows the entry as awaiting approval and it does
  not fire. Trust is decided once per configuration at engine build, not per
  fire; denial is fail-closed.

## Commands

- **`/hooks`** — status for every configured hook (state, provenance).
- **`/hooks enable|disable <name>`** — session toggle (edit `enabled` in
  config to persist).
- **`/hooks test <name>`** — fire once with a synthetic payload and print the
  decision, without waiting for a real match.

## Interactions

- HTTP hooks respect [sandbox](sandbox.md) network denial (the denied call
  is a recorded failure, not a silent skip).
- Blocking `PreToolUse` hooks with literal tool matchers force those tools to
  serialize in [parallel batching](tools.md#concepts), since a veto/rewrite
  must be observed before anything else runs.
- Every hook decision that gates an effect is recorded in the authorization
  ledger.

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Engine, matching, decisions | `src/hooks/mod.rs` |
| Actions & wire protocol | `src/hooks/actions.rs`, `src/hooks/protocol.rs` |
| Trust gate | `src/hooks/trust.rs` |
| Config schema | `src/config/schema.rs` |
| `/hooks` command | `src/commands/hooks_cmd.rs` |
