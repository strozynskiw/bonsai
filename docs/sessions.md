# Sessions

Every conversation is a durable **session**: transcript, model-facing
context, tool calls, usage ledger, plan, todos, and episode state persist in
SQLite and survive restarts. This page covers the session lifecycle,
persistence, resume, concurrent sessions, and budgets.

## Where sessions live

`$BONSAI_HOME/bonsai.db` (default `~/.bonsai/bonsai.db`) — SQLite in WAL
mode, permissions hardened (`0700` home, `0600` database). Ordered
migrations run at open; before applying pending migrations to an existing
database, bonsai snapshots it to `$BONSAI_HOME/backups/` and a failed
migration restores the snapshot automatically. See
[Compatibility](compatibility.md) for the upgrade/rollback contract.

What a session snapshot holds: the rendered transcript, the model-facing
context messages (resume-authoritative), tool calls with args/results/diffs,
compaction and episode records, read evidence, the per-turn usage ledger,
verification and self-review runs, plan and todos, and a full-text-searchable
copy of plain messages (powering `/search` and `recall`'s query mode).
Secrets never enter the database — credentials are stored by
[reference only](security.md#credentials).

## Lifecycle

- **Start** — a session row is created per project with a fresh
  conversation cache key (the prompt-cache route; see
  [Models](models.md#prompt-cache-shaping)).
- **Persistence cadence** — the TUI flushes changed snapshot groups every
  500 ms off the input thread; each group (transcript, context, usage, plan,
  …) has a content signature so unchanged groups are skipped, and all
  changed groups commit in one transaction. A final synchronous flush runs
  at shutdown. Headless persists at run end (and on failure paths).
- **Heartbeat** — a background task stamps liveness every 3 s, separate from
  content updates so `/resume` ordering stays content-recency-based. An
  **active-run timer** accumulates foreground execution time for the
  session-time budget; a crashed segment is recovered only up to its last
  heartbeat.
- **Titles** — the agent sets one via `set_session_title` (also an
  [episode](context.md#task-episodes) boundary hint); otherwise the first
  user prompt seeds a derived title.
- **End** — `/quit`, `/clear`, or `/new` complete the session.
  `/clear`/`/new` rotate to a fresh row (new cache key, cleared episodes and
  plan canvas); crash leftovers are promoted to *interrupted* at next boot,
  excluding rows with a fresh heartbeat (that's what makes concurrent
  sessions safe). Terminal reasons — including typed budget exhaustion — are
  recorded on the row, not reported as generic failures.

## Resume

- `bonsai -c` resumes the latest session for the project; `bonsai -c <id>`
  a specific one. In the TUI, `/sessions` lists, `/resume [id]` resumes,
  `/forget <id>` deletes, `/search <query>` searches message history.
- A resumed session **pins** its stored provider, model, and reasoning
  (headless ignores `BONSAI_PROVIDER` on resume; `--model` can still
  override for that run), and restores its conversation cache key so the
  provider-side prompt cache stays warm.
- Resume refuses cross-project targets and **live** targets (a session
  another process is actively heartbeating). The full target runtime is
  prepared before any durable mutation, so an invalid resume has no side
  effects; the switch itself is one atomic transaction that also re-attaches
  any active recovery workspace.
- Interactive PTYs and background tasks are process-local: a resumed session
  marks previously running ones as lost rather than pretending to reattach.

## Peer sessions

Multiple bonsai processes can work in one project root concurrently:

- **Identity and liveness** — each process is identified by its active
  session; peers are the other live sessions of the same project (heartbeat
  within 15 s). The status line shows waiting peers.
- **Reads are optimistic** — they never wait for a lease. **Mutations
  serialize** through a project-wide write lease (30 s, background-renewed)
  with the lease row id as a fencing token: a stale owner fails its final
  commit with a re-read-and-retry hint instead of clobbering newer work.
- **Coordination** — the `peers` tool lists live peers, sends bounded,
  redacted, rate-limited messages (≤6,000 chars, ≤30/hour), registers a
  one-shot **wake-when-done** on another session (mutual waits are refused;
  auto-wakes are hop- and rate-limited to prevent loops), and takes advisory
  **claims** on work areas. Message delivery uses durable per-consumer
  leases, so a crash never drops or double-delivers a message.
- `/peers` shows a live read-only view. Per-session changed-file records
  attribute concurrent edits correctly (the self-review pass subtracts
  peer-attributed paths from its diff).

## Budgets

All budgets are **off by default**; enable presets in `/settings` or pass
headless flags (flags override saved values for that run).

**Per-run** (reset each run): `max turns`, `run time`, `generation` (per
provider attempt), `output` (streamed chars per attempt), `tool time` (per
tool call — a stalled call is cancelled without killing the run).

**Per-session** (cumulative across resumes, consuming the persisted ledger —
including output from failed provider retries): `session turns`,
`session output`, `session time` (active run time), `session cost`.

- Session budgets are checked before each provider call; the cost limit uses
  cache-aware pricing and stops before the next call **only while every
  contributing turn has exact usage and pricing** — otherwise the completion
  report discloses that exact enforcement was unavailable.
- Exhaustion preserves partial work and the resumable session, records the
  typed reason (`max_turns`, `session_cost`, `provider_rate_limit`, …) in
  SQLite, and — in headless JSON — emits `status: "budget_exhausted"` with a
  typed `budget_exhaustion` object. See [Headless mode](headless.md).

## The usage ledger and `/usage`

Every provider response is one ledger turn: lane (parent / subagent /
self-review / compaction), tokens (prompt, completion, cache read/write),
cost and a no-cache baseline cost (their difference is the cache saving),
finish reason, latency and time-to-first-token, cache-route telemetry
(expected vs actual cache-read percent, prefix hashes, rewrite events), and
inspection counters. Subagent turns keep their own lane and per-turn model
identity while folding into session totals.

- `/usage` — the cross-project analytics dashboard (activity, models, cost,
  sessions, tools, cache tabs plus a heatmap).
- `/perf` (alias `/cost`) — the current session's performance/usage report.
- `/ctx` — where the context window itself is going; see
  [Context management](context.md).

Cost math is honest about unknowns: turns without pricing flag the session's
cost as inexact, and dashboard totals are floors, with unknown-cost turns
counted separately.

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Database, migrations, backups | `src/storage/mod.rs`, `migrations/` |
| Session rows & resume | `src/storage/sessions.rs`, `src/tui/run/persistence.rs` |
| Snapshot writer | `src/session_persist.rs` |
| Peers & workspace leases | `src/peer.rs`, `src/storage/peers.rs`, `src/storage/workspace_locks.rs` |
| Budgets | `src/run_budget.rs` |
| Usage ledger & dashboard | `src/agent/usage_ledger.rs`, `src/storage/usage_stats.rs` |
