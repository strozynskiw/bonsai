# Context management

bonsai treats the context window as a managed budget: cache-stable prefixes,
token estimation per provider family, continuous garbage collection,
graduated compaction, and default-on **task episodes** that archive finished
work into compact, recallable cards. `/ctx` shows exactly where the window
is going and gives you manual control.

## How context is assembled

The system message is: static persona prompt + a **project context** block +
optional operator suffix. Project context is split deliberately:

- **Cacheable prefix** — environment (cwd, platform, project type), steering
  files (`AGENTS.md` → `CLAUDE.md` → `.cursorrules`, nearest-first up the
  tree, 16 KiB cap each), the skills index, the subagents index, and the
  [repository map](language-intelligence.md#the-repository-map). All
  byte-stable across turns and restarts.
- **Volatile tail** — always introduced by the `## Volatile state` heading
  (providers split their cache breakpoint on it): git branch/status (with a
  run-start baseline separating pre-existing WIP from this run's changes),
  the stale-read advisory, and peer status.

Connections with rolling-history caching (and Codex always) instead receive
volatile state appended to history, keeping the entire prefix append-only.
[Memory](memory.md) is deliberately *not* in the system prompt — recalled
entries arrive as per-turn background data notes.

## Token accounting

A per-model estimator drives context-pressure decisions (billing truth stays
with provider-reported usage): local tiktoken or Qwen tokenizers, the
Anthropic `count_tokens` API, or a `chars/4` heuristic — each tagged with a
confidence. Over-budget bails only trust high-confidence estimates; image
payloads are redacted before counting. See
[Models](models.md#token-counting-and-cost).

Thresholds (percent of usable window = budget − a 16k output reserve):

| Trigger | Default | Smol |
| --- | --- | --- |
| Continuous GC reclaim | 75% | 35% |
| Automatic compaction | 95% | 50% |
| Compaction target | 50% | 35% |

## Continuous GC

Every turn, existing stubs are re-asserted and pinned rows restored —
byte-identically, so the provider prompt cache stays warm. Under pressure
(the GC trigger), a reclaim pass introduces new stubs for:

- **Superseded reads** — a later covering read, or an edit to the same path,
  makes the old read redundant (detected from typed read evidence, not tool
  names).
- **Old successful tool outputs** — bulky read/grep/glob/symbol_search/
  command outputs far from the tail.

Every rewrite must pay for itself: new stubs require ≥8,000 tokens or ≥8%
saved (rewriting history invalidates the provider cache, so small savings
are worse than nothing). Stubbed originals are restorable from `/ctx`.

**Read reuse.** A repeated read of unchanged content collapses to a compact
pointer (`[reused previous read]`) instead of resending bytes — but an
explicit re-request after that returns real bytes; reads are annotated, never
withheld. Guards watch for pathological loops: repeated identical
inspections, read storms on one target, repeated failed calls, and
implementation stalls. Read-only loop guards reject the offending call and
admit one explicit retry with a fresh budget; they do not terminate an
otherwise healthy autonomous run.

## Compaction

When the automatic trigger fires (or you run `/compact`), a graduated draft
is built:

- **Tier 1** — stub bulky tool outputs (superseded reads first, then oldest)
  until under target. No information leaves the session; originals restore
  from `/ctx`.
- **Tier 2** — only if tier 1 isn't enough: the oldest non-mandatory message
  groups are folded into a rolling summary (fixed headings: current goal,
  decisions, constraints, files touched, tool findings, open tasks, risks).
  A previous summary is *updated*, not re-summarized. This is the only tier
  that makes a hidden provider call; a deterministic fallback exists and can
  be forced.

Always protected: the system message, the last three message groups, the two
latest user messages, and read-reuse pointer targets. Summaries carry a
trust guard marking them lossy reference data. Every compaction records a
telemetry event (tokens before/after, prefix hashes, cache impact) visible
in `/ctx`.

`/compact [preview] [target=<tokens>] [fallback|provider|deterministic]`
runs or previews one manually.

## Task episodes

Enabled by default; `BONSAI_EPISODES=0` is the emergency opt-out (zero
tracking, byte-identical disabled tool arrays, no persistence).

- The parent lane is partitioned into **episodes** — contiguous topic spans.
  Boundaries are deterministic, not classifier-based: `set_session_title`
  and `plan_replace_draft` carry an `episode_action` (`new_topic` closes the
  span at the last human message; `same_topic` renames in place), and hard
  resets (`/clear`, `/start`, `/review`) close unconditionally. Tiny spans
  never close — a `new_topic` on a span without real work just renames.
- A closed episode may be **evicted**: its messages are replaced wholesale
  by one compact `[Episode archived] #N "title"` card (≤1,600 chars) with
  recall instructions. Eviction is all-or-nothing per episode and batched
  (one wire mutation per pass), and only happens when the **rewrite
  economics** clear — ≥8,000 tokens or ≥8% saved, or the pressure batch
  resolves pressure by itself. A cancelled provider call leaves context
  untouched.
- The **`recall` tool** pages an archived episode's real messages and tool
  outputs back in (`{episode: N}`, cursor-paged) or searches the transcript
  and archives (`{query: …}`) — always inside an untrusted-data frame.
  Repeat recalls of an unchanged page collapse to a pointer. Restoring from
  `/ctx` re-splices originals; restored episodes are never re-evicted.
- `/episodes` shows the ledger; resume repairs any inconsistent episode
  state conservatively (demoted to non-evictable, archives preserved).

## Smol mode

`/smol` (or the Context row in `/settings`) switches to a compact profile
for small models: lower compaction thresholds and reserve, a reduced system
prompt (bash/terminal/todowrite/set_session_title tools), no repo map or
built-in skill/subagent/memory indices, 2 KiB steering cap, deterministic
compaction summaries only, and memory recall off.

## `/ctx`

The context modal breaks the window down node by node with three views
(ledger / wire / turns): token counts, inclusion state, stub reasons,
compaction events, and episode reports, reconciled against actual provider
usage. Keys in the ledger view: `p` pin (never GC), `d` drop, `s` stub,
`r` restore, Enter/arrows expand. The header's context chip (fill bar +
percent) is the always-visible summary; click it to open `/ctx`.

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Preflight, thresholds, compaction | `src/agent/compaction/` (`mod`, `draft`, `gc`, `summary`) |
| Read evidence & reuse | `src/agent/read_persistence.rs`, `src/tool/read_evidence.rs` |
| Episodes | `src/episode.rs`, `src/agent/episodes/` |
| Project context block | `src/context.rs` |
| Estimators | `src/provider/token.rs` |
| Smol profile | `src/smol.rs`, `src/agent/controls/smol.rs` |
| `/ctx` report & modal | `src/context_view/`, `src/tui/widgets/modal/context.rs` |
