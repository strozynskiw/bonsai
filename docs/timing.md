# Runtime timing and the active-compute budget

A run's wall clock mixes work with waiting: model execution, running tools,
build queues, permission prompts, `question` waits, and child agents. Reporting
that one number as "active time" makes a run that spent most of its hour waiting
on the user read as expensive work, and it lets queue time hide inside a failed
tool. bonsai records the split instead, and the session time budget is charged
against work rather than against the wait.

## The split

Every interval is recorded with a category, a lane (`parent` or `child`), an
owner, and — for waits — a reason.

| Label | Wire value | Kind | Begins when |
| --- | --- | --- | --- |
| `model` | `model_request` | work | a provider request is sent, streaming and retries included |
| `tools` | `tool_active` | work | a tool call is executing, local or remote |
| `queue` | `queue` | wait | the `tasks` tool waits for background work to report |
| `permission wait` | `permission_wait` | wait | a permission, sandbox, web-domain, extension-tool, or hook-trust prompt is open |
| `user wait` | `user_wait` | wait | a `question` is awaiting the user's answer |
| `child active` | `child_active` | work | a subagent is running |
| `child wait` | `child_wait` | wait | the parent is parked on a child that has not reported yet |

**Active compute** is the union of model, tool-active, and child-active time.
**Waiting** is the union of queue, permission, user, and child-wait time.
Critical-path wall time is the union of everything — what the run actually took.

## Critical path versus summed resource time

Parallel work records one interval per participant, so two numbers are reported
and they mean different things:

- **Critical path** — the union of all intervals. Concurrent tools or children
  collapse into one stretch of time, so this never double counts.
- **Summed resource time** — per category, the arithmetic total. Two 5s tools
  that ran in parallel summed to 10s of tool work inside a 5s critical path.

Reports show both, labelled. A summed row larger than the run wall is expected
for a run that did things concurrently; it is not a bug and not a wall time.

## The two clocks

| Clock | Consumes | Selected by |
| --- | --- | --- |
| active compute (default) | model + active tool + child-active | nothing — it is the default |
| wall clock | the whole run elapsed time, waits included | `BONSAI_TIMING_POLICY=wall_clock` |

The policy is read once per run, persisted with that run's timing, and reported
everywhere the numbers appear, because a sum means nothing against an unnamed
clock. Values are the wire values exactly: `active_compute` or `wall_clock`
(trimmed; case-sensitive). An unset or unreadable value keeps the default on
purpose — a typo in an opt-in must never silently start charging a run for the
time it spent waiting on its user.

`max_run_seconds` stays an independent per-run **wall** deadline under both
policies, so excluding waits can never let a run continue unbounded. Under
active compute no session wall deadline is armed at all.

## What the budget charges

- **`max_session_active_seconds`** (`/settings` → session budgets → *session
  compute*) is the cumulative session limit, and it consumes the selected
  clock. Its default reading is compute; its row labels it accordingly.
- **`alert_session_active_seconds`** (session alerts → *active time*) warns on
  the same clock, so an alert cannot fire on time the budget would not charge.
- **`max_run_seconds`** (per-run *run time*) remains wall time regardless of
  policy.

The exhaustion reason names the clock it spent — `session exhausted its 3600s
active compute budget after 3120s of active compute` — the completion report
shows it as `active compute 3120/3600s` with `waiting 197s` beside it when any
was recorded, and `used_seconds` reports the time actually spent rather than
flooring at the limit.

## Resume, restart, cancellation

- Durations come from a **monotonic** clock, so a wall-clock adjustment or an
  NTP step cannot produce a negative or inflated interval. Every interval also
  carries wall-clock anchors, used only for persistence and audit.
- Checkpoints are **additive**: a run resumed in a new process after a crash
  keeps what the interrupted segment already recorded instead of replacing it.
- Open intervals are **never invented** at startup. Restart reconciliation
  clears the dangling markers and counts them as recovered, which means the
  totals deliberately undercount a gap that may have been a dead process or a
  suspended machine — the recovered count says so.
- Cancellation closes a run's open intervals exactly once, however often the
  run is finished afterwards.
- Session counters are durable in `sessions.active_compute_ms` (the default
  policy's cumulative reading) and `sessions.active_run_ms` (the wall
  accumulator the wall-clock policy consumes). Both are restored at run start,
  before the ledger re-anchors, so a resumed session is the restored base plus
  this run's closed intervals.

## Where the split is visible

| Surface | What it shows |
| --- | --- |
| Header, while a run is active | `⏱ 12m 4s · compute 9m 12s · wait 2m 51s` — wall time beside the timer, compute and waiting only when non-zero. The two clocks disagree exactly while a prompt is open. |
| `/usage` → Activity | A runtime block: `Runtime clock <policy> · waiting <n>% of run wall`, then one aligned row per non-zero category (duration, share of run wall, and a `compute`/`wait` role), then `critical path` and `run wall`. `clock mixed` means recorded runs disagree; `clock unknown` means none recorded timing. |
| `/settings` | `session compute` and `active time` rows, labelled by the clock they charge. |
| Headless | A `timing (active compute): critical path …ms, active compute …ms, waiting …ms | model parent …ms, tools parent …ms, …` status line, one row per category and lane. Stderr in `text`/`json`, a `status` event in `stream-json`. |
| Completion report | The session time line names its clock and appends `waiting …s` when any was recorded. |
| Budget alert, before submit | `Active clock · active compute · 52m spent · 3m 17s waiting` — the clock first, then one line per reached or upcoming threshold, so continuing is an informed choice. |
| Exhaustion reason | Names the clock it charged, so an exhausted limit can never be misread as wall time. |

## What it never records

The owner and reason on an interval are bounded (200 and 512 characters) and
redacted before they are stored, and only the latest one per category survives
a checkpoint. So a permission wait keeps `permission` plus the redacted command,
and a `question` wait keeps the question prompt — never the user's answer, and
never the response body, diff, or tool arguments.

A delegated child's prompt currently lands in the parent's wait total: the
interaction service attributes waits to the parent lane, and separating them
needs the child's lane to travel with the request.

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Ledger, categories, lanes, policy, summaries | `src/timing/mod.rs` |
| Per-run checkpointing and restart reconciliation | `src/storage/timing.rs`, `migrations/0019_session_active_compute.sql` |
| Budget axes and exhaustion | `src/run_budget.rs` |
| Interval producers | `src/agent/run_loop.rs`, `src/agent/timing.rs`, `src/interaction.rs`, `src/tool/tasks.rs` |
| Headless summary line | `src/headless/mod.rs` |
| `/usage` runtime block | `src/tui/widgets/modal/usage.rs` |
| Session counters and restore | `src/tui/task.rs`, `src/storage/sessions.rs` |
