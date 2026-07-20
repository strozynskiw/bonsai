# Architecture

bonsai is a single Rust binary (edition 2024, tokio, ratatui) — deliberately
a binary crate with no `lib.rs`. This page maps the modules, the startup
sequence, the agent loop, and the abstractions that hold the system
together.

## Module map

```
src/
  main.rs               thin entry; parses CLI, dispatches mode
  cli.rs                hand-rolled arg parser (Run / Print / eval / recovery / doctor / completions)
  bootstrap.rs          TUI startup + tool-registry construction
  runtime.rs            RuntimeBuilder — surface-neutral assembly + the TUI/headless capability contract
  agent/                turn coordinator, batching, retry, compaction, episodes, guards, self-review
  tool/                 one file per tool; Tool trait, registries, risk classifier, action plans
  provider/             Provider trait, three wire transports, SSE driver, errors, token counting
  model_catalog/        connections + model TOMLs, models.dev merge, live discovery
  session/              provider/session state, credential stores
  storage/              SQLite (sqlx), migrations, snapshots, peers, leases, usage
  commands/             slash-command metadata + shared TUI/headless handlers
  tui/                  event loop (run/), reducer (app/), views, widgets, keymap, themes
  headless/             bonsai -p driver + versioned output sink
  eval/                 deterministic eval runner, graders, mock provider, adapters
  hooks/ mcp/ extension/ resource/   extensibility: hooks, MCP, gates, file-based resources
  lsp/ symbol/          language servers; tree-sitter extractors
  memory/               persistent memory stores + hybrid retrieval
  sandbox/              Seatbelt/Bubblewrap confinement
  context.rs episode.rs peer.rs recovery.rs redact.rs
  permissions.rs interaction.rs run_budget.rs workspace_trust.rs
  self_review.rs completion_report.rs verification.rs
```

Support directories: `models/` (builtin catalog), `migrations/` (SQLx),
`skills/builtin/`, `eval/` (suites/baselines), `e2e/` (tmux TUI smoke
tests), `scripts/` (release tooling), `site/`.

## Startup

`main.rs` loads `.env`, parses the CLI, and builds a tokio runtime per mode.
The TUI path (`bootstrap.rs`): open storage (run migrations, taking a
pre-upgrade backup) → resolve the project → load the **workspace trust**
gate *before* any project-owned config → load the model catalog (project
layer only if trusted; background models.dev refresh) → build the provider
registry from the catalog → load session state → build the interaction
channel → `RuntimeBuilder::build`.

`RuntimeBuilder` (`runtime.rs`) is the surface-neutral assembly used by both
TUI and headless. It wires, in order: permissions, config (project layer
trust-gated), sandbox, task/terminal/subagent registries, the authorization
ledger, skills + custom agents + memory, project context (repo map eager for
headless), the peer bus, MCP discovery, the hook engine, the episode store,
the four **tool registries**, the provider + prompt estimator, and finally
the `Agent`. The intentional TUI/headless differences are a frozen
capability table on the surface kind — prompts, repo-map timing, subagent
wake, volatile-context refresh, sandbox preference source, and session
binding — so parity is a checked contract, not a convention (see
[Headless mode](headless.md#what-headless-does-differently)).

## The agent loop

One run = one `TurnCoordinator` driving up to `max_iterations` turns
(default 375, budget-overridable):

1. **Preflight** — refresh volatile project state, drain queued user
   messages, then `prepare_context_for_model`: episode evictions, continuous
   GC, token estimation, and compaction when thresholds fire
   ([Context management](context.md)).
2. **Model call** — `chat_stream_with_retry`: up to 3 retries with
   `retry-after`-aware backoff, per-attempt generation-time and
   streamed-char meters, visible retraction of a failed attempt's output,
   and provider-fallback activation on permanent errors
   ([Providers](providers.md#errors-retries-and-failover)).
3. **Tool execution** — calls are classified
   (`ReadPath`/`WritePath`/`GlobalWrite`/`ParallelBash`/`Independent`),
   packed into conflict-free batches (path-prefix overlap detection), and
   each batch runs under `join_all` with cancellation, per-tool timeouts,
   Pre/PostToolUse hooks, and post-batch context updates
   ([Tools](tools.md#concepts)).
4. **Completion** — a no-tool response first offers the turn to
   [self-review](agents.md#self-review) and after-edit
   [verification](configuration.md#verification); only when both decline is
   the run complete. A completion report is assembled from ledgers
   ([Headless](headless.md#the-completion-report)).

Turn-level guards watch for pathological patterns — repeated identical
inspections, read storms, repeated failed calls, empty responses,
implementation stalls, planning rumination — each with escalating nudges
and, where warranted, a hard stop.

## Key abstractions

- **`Tool` trait** (`src/tool/mod.rs`) — name, description, JSON schema,
  a mandatory `ToolEffectPolicy`, a `ParallelPolicy`, and `execute`.
  Registries are byte-stable in wire order to keep the prompt prefix
  cacheable.
- **`ToolOutput`** — typed results (`Text`, `Read` + evidence, `Edit` +
  diff, `Command`, `Image`, …) plus the two context forms: `TrustedContext`
  (skills only; may become a system message) and `UntrustedContext` (framed
  external data; never promoted). All text is redacted at one choke point.
- **`Provider` trait** (`src/provider/mod.rs`) — `chat_stream` with a
  cancellation token, typed `ProviderFailure`, model listing, and a
  `project_state_cache_strategy`. Three transports implement it; the shared
  SSE driver is the only stream parser.
- **`SharedSink` / `OutputSink`** (`src/output.rs`) — how the agent reports
  progress without knowing about ratatui: `assistant_delta`,
  `tool_started/finished`, `attempt_started/discarded`, `status`, `context`.
  The TUI forwards events into its reducer; headless writes stdout/stderr or
  JSON events; tests use a stdout sink.
- **`InteractionService`** (`src/interaction.rs`) — the prompt channel
  (permission, sandbox escalation, web domain, MCP tool, hook trust,
  question) with oneshot responders; the noninteractive variant fails
  immediately, which is what makes headless denial uniform.
- **Action plans + risk tiers** (`src/tool/action_policy.rs`,
  `src/tool/risk.rs`) — every effect is typed, tiered, and passed through
  one verdict combiner; every decision lands in the authorization ledger.
- **`ReadTracker` / `ReadEvidence`** — digested, coverage-aware read records
  that gate writes, drive stale-read advisories, and let GC collapse
  superseded reads.
- **Two-part project context** — a byte-stable cacheable prefix and a
  `## Volatile state` tail, so prompt caches survive across turns
  ([Context](context.md#how-context-is-assembled)).

## Persistence and concurrency

SQLite (WAL) holds sessions, context, transcripts, tool calls, usage,
permissions, episodes, and peer state; snapshot groups are
signature-diffed and flushed off the render thread. Concurrent sessions in
one repository coexist through heartbeats, a fenced project-wide write
lease, per-session change attribution, and the peer message bus
([Sessions](sessions.md)). Mutating runs can be isolated in managed
[recovery worktrees](recovery.md).

## Design rules that shape the code

From `AGENTS.md`, enforced in review:

- No `lib.rs`; tokio is the only async runtime; tools never print — they
  return `ToolOutput` and the sink decides rendering.
- No per-provider SSE parsers; reuse `provider/sse.rs`.
- Untrusted content must use `ToolOutput::untrusted_context` — never a
  parallel framing path.
- Errors keep their detail (the malformed-tool-arguments path returns the
  tool name, required fields, bad payload, and parse error to the model).
- Tests live inline in `#[cfg(test)]` modules next to the code; the batching
  and agent tests are the spec for their subsystems.

See [Development](development.md) for build/test workflow and extension
recipes.
