# Headless mode

`bonsai -p "<prompt>"` runs one non-interactive coding-agent turn and exits.
The TUI and headless adapters share the same permission engine, run/session
budgets, snapshot writer, cancellation token, recovery workspace, provider
failure guidance, and completion-report classifier — the differences are
explicit and listed [below](#what-headless-does-differently).

```sh
bonsai -p "summarize the repo"
bonsai --print "run cargo test and report failures" --output-format text
bonsai -p "inspect src/main.rs" --output-format json
bonsai -p "run the smoke check" --output-format stream-json
echo "summarize stdin" | bonsai -p
bonsai -c -p "continue the latest session"
```

## CLI reference

Flags (both `--flag value` and `--flag=value` forms):

| Flag | Meaning |
| --- | --- |
| `-p`, `--print ["<text>" \| -]` | headless mode; `-`/no text/stdin-pipe reads the prompt from stdin; `--print=<text>` for prompts starting with `-` |
| `-c`, `--continue [id]` | resume the latest (or a specific) session |
| `--output-format text\|json\|stream-json` | default `text` |
| `--model <id>`, `--effort <level>` | one-run model/effort override, saved defaults untouched |
| `--autonomy <level>` / `--yolo` | raise autonomy (default `ask`; `BONSAI_AUTONOMY` also works) |
| `--max-turns <n>` | cap agent tool-call iterations |
| `--max-generation-seconds <s>` | cap each provider attempt (overrides `BONSAI_MAX_GENERATION_SECONDS`) |
| `--max-output-chars <n>` | cap streamed reasoning+output per attempt (overrides `BONSAI_MAX_STREAMED_CHARS`) |
| `--max-tool-seconds <s>` | cap each tool execution; a stalled call is cancelled without killing the run |
| `--append-system-prompt <text>` | trusted operator instructions appended before generated project context |
| `--timeout <s>` | cancel the whole run with exit 124 |
| `--isolation auto\|worktree\|off` | Git [recovery boundary](recovery.md); `auto` (default) isolates mutating runs at `balanced`+ |

Other subcommands share the binary: `bonsai eval` ([Eval](eval.md)),
`bonsai recovery …` ([Recovery](recovery.md)), `bonsai doctor [--json]
[--online]` ([Troubleshooting](troubleshooting.md)),
`bonsai completions bash|zsh|fish`, and `bonsai model-catalog check <path>`.
Interactive-only flags: `--reduced-motion`, `--screen-reader`.

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | completed successfully |
| `1` | agent/provider/tool/runtime failure, or a `budget_exhausted` result |
| `2` | CLI usage error |
| `3` | auth/config error |
| `124` | `--timeout` |
| `130` / `143` | interrupted by Ctrl+C / SIGTERM |

## Provider selection

`BONSAI_PROVIDER` selects the provider for a new run; unset, bonsai uses the
persisted current provider, then falls back to `opencode`. On `-c` resume
the session pins its stored provider/model/reasoning and ignores
`BONSAI_PROVIDER` (`--model` still overrides). Per-provider env vars
(keys/models/base URLs — full list in
[Providers](providers.md#built-in-connections)) override saved settings;
web-search env vars are read at startup too.

## Output formats

Every JSON object and stream event carries `schema_version: 1`; the
[compatibility contract](compatibility.md) governs changes. Human-readable
text and TUI layout are *not* machine-readable surfaces.

**`text`** — assistant output streams to **stdout**; reasoning (prefixed
`[reasoning]`), progress, tool activity, errors, the compact completion
report, and `Resume: bonsai -c <id>` go to **stderr**.

**`json`** — one final object on stdout:

```json
{
  "schema_version": 1,
  "status": "completed | failed | budget_exhausted | interrupted | …",
  "output": "…",
  "provider": "anthropic", "model": "…", "session_id": 42,
  "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0,
             "cost_micros": 0, "input_cache": { "read_tokens": 0, "creation_tokens": 0,
             "total_input_tokens": 0, "hit_rate_percent": 0 } },
  "persistence_duration_ms": 0,
  "budget_exhaustion": { "kind": "run_time", "limit_seconds": 300 },
  "verification": { "…": "…" },
  "completion_report": { "…": "…" }
}
```

`budget_exhaustion` appears only on `status: "budget_exhausted"` and is a
typed object (`kind` ∈ `max_turns`, `session_turns`, `session_time`,
`run_time`, `generation_time`, `provider_output`, `session_output`,
`session_cost`, `provider_generation`, `provider_rate_limit`,
`provider_quota`, with limit/used fields per kind).

**`stream-json`** — newline-delimited events, each versioned, each with a
`type`:

| Event | Payload |
| --- | --- |
| `assistant_delta`, `reasoning_delta` | `{text}` |
| `status` | `{text}` (progress/thinking) |
| `error` | `{text}` |
| `attempt_discarded` | marker: a failed attempt's streamed output was retracted |
| `tool_started` | `{id, name, arguments}` |
| `tool_output` | `{id, output}` |
| `tool_finished` | `{id, result, status, diff?}` |
| `context` | `{used_tokens, budget_tokens, last/session prompt+completion tokens, session_cost_micros, last_turn_cost_micros}` |
| `final` | the full `json`-mode object, flattened |

Consumers must tolerate new optional fields, enum values, and event types
within schema 1.

## The completion report

Derived from bonsai's ledgers, not the model's recollection: structured
`changed_files` (path, status, intent, ±lines), verification evidence,
self-review disposition, authorization counts (allowed / denied /
user-approved / policy bypasses), token/cost usage with session-budget
state, and `caveats` — appended for unresolved tool failures, missing or
failed verification after file changes, unfixed review findings, denied
authorizations, or an unenforceable cost budget. A nominally-complete run
with an unresolved tool failure is reclassified as `failed`.

## What headless does differently

The surface contract is explicit (`src/runtime.rs`):

| Axis | TUI | Headless |
| --- | --- | --- |
| Prompts | interactive | **denied immediately** — permission asks fail with guidance toward `--autonomy`/rules; the `question` tool fails instead of blocking; sandbox escapes are always denied |
| Repository map | built in the background | built eagerly before the run |
| Background subagent wake | parking + wake loop | disabled — an unresolved wait state is an error |
| Volatile context | refreshed each turn | frozen for the one-shot run |
| Sandbox preferences | persisted interactive choices restored | read from config/env only |
| Session binding | created after startup | **session id exists before runtime assembly** |
| Workspace trust | can prompt | keeps restricted posture until an interactive session trusted the workspace |

## Recovery and CI

Mutating headless runs execute in a detached [recovery
worktree](recovery.md) by default (autonomy `balanced`+); the source
checkout is untouched during the run and a recovery id prints at the end.
An automatic mutating run outside Git refuses to start unless
`--isolation off` opts out.

CI example:

```sh
BONSAI_PROVIDER=anthropic ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY" BONSAI_AUTONOMY=balanced \
  bonsai -p "run the test suite and summarize the result" \
  --max-turns 20 --max-generation-seconds 300 --max-output-chars 120000 \
  --max-tool-seconds 300 --timeout 900 --output-format json
```

`bonsai -p /test` and `bonsai -p /build` expand to the bounded
[verification workflows](configuration.md#verification); stale, failed,
interrupted, or incomplete verification exits non-zero.

## Where this lives in the code

| Concern | Location |
| --- | --- |
| CLI parser | `src/cli.rs` |
| Headless driver | `src/headless/mod.rs` |
| Output sink & schemas | `src/headless/sink.rs` |
| Surface capability contract | `src/runtime.rs` |
| Completion report | `src/completion_report.rs` |
