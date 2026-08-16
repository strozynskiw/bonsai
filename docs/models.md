# Models

bonsai keeps model knowledge — context windows, output limits, pricing,
reasoning support — **outside the code**, in a layered model catalog. This
page covers where model metadata comes from, how a model is selected and
resolved, reasoning efforts, shortcuts, and cost accounting.

## The model catalog

Three layers, later patching earlier per field:

1. **Built-in** — `models/builtin/connections.toml` plus per-connection model
   TOMLs embedded in the binary (12 target files, ~150 model targets).
2. **User** — `$BONSAI_HOME/models/*.toml` and `$BONSAI_HOME/providers/*.toml`.
   bonsai scaffolds disabled `example-local.toml` starters.
3. **Project** — `.bonsai/models/` and `.bonsai/providers/`, git-trackable,
   active only in a [trusted workspace](security.md#workspace-trust).

Files are partial patches keyed by id, sorted by filename; a project entry
overrides fields of a user or built-in one.

### models.dev

Model facts are sourced from `https://models.dev/api.json`, cached at
`$BONSAI_HOME/cache/models-dev.json` with a one-hour TTL. The TUI serves the
cache immediately and refreshes in the background; fetch failures preserve
the existing cache. Per-row parsing is fault-isolated — one malformed model
entry is skipped, not fatal.

Merging: explicit TOML values win; models.dev backfills `output_limit`,
`context_window`, `pricing`, `features`, `reasoning_options`, and
`display_name`. Drift between an explicit TOML value and models.dev is warned
once per load unless the target sets `pinned = true`.

Env controls: `BONSAI_MODELS_DEV_URL` / `BONSAI_MODELS_DEV_PATH` (source
override), `BONSAI_DISABLE_MODELS_FETCH` (offline), and
`BONSAI_MODELS_DEV_TTL_SECS`.

### Live discovery

For endpoint connections (`openai-compatible`, `anthropic-compatible`,
LM Studio, Ollama), bonsai probes the server's model listing — including
native LM Studio (`/api/v1/models`, context length + display name) and Ollama
(`/api/tags` plus a bounded `/api/show` fan-out) APIs — and caches
availability under `$BONSAI_HOME/cache/live-models/<connection-id>.json`.

## Selecting a model

`/model` opens a three-pane picker: **Provider → Model → Reasoning**. Model
rows show pricing; the reasoning pane lists the efforts the selected model
supports, with recommended and discouraged efforts marked. The picker's data
is the resolved catalog: live-discovered models first, then the connection's
target list, then seed fallbacks.

The default provider is `opencode`. In headless mode, `--model <id>` and
`--effort <level>` override for one run without changing saved defaults; a
resumed session pins its stored provider/model/reasoning (see
[Sessions](sessions.md)).

### Per-mode models

The **coding** and **plan** modes each keep their own model selection —
`/model` (picker, typed selector, or letter shortcut) applies to whichever
mode is active when you run it. Plan on a strong reasoner while coding on a
fast implementer, and `Shift+Tab` flips both the persona and its model. In
detail:

- The composer meta line always names the active mode's model; while a run
  is in flight under the old persona it shows the pending switch
  (`Coding → Planning`) and the model changes when the run ends.
- Every run applies its persona's model **at dispatch** — queued messages,
  `/start` hand-offs, and phase advances all start on the target mode's
  model, never the one that happened to be live.
- The first time the modes diverge, the other mode keeps the model both
  were using — switching plan's model never silently drags coding along.
- Review and the built-in read-only lanes follow the current model; custom
  personas use the `model:` from their definition
  ([Agents and modes](agents.md#defining-your-own)).
- Provider-level switches (`/authorize`, the provider manager) re-link both
  modes to the new provider — per-mode entries are cleared rather than
  snapping back to the old provider on the next mode switch.
- Selections persist with the session store (`mode_models`, stored as
  `connection:model` selectors) and survive restarts.

### Reasoning efforts

The effort ladder is `minimal < low < medium < high < xhigh < max < ultra`,
plus `default`, `off`, `on`, and raw thinking-budget tokens where a model
supports them. Not every model supports every rung — the picker only offers
supported efforts. Two automatic adjustments exist:

- **Escalation** — a verification-repair turn may step one rung *up*.
- **Retry downshift** — a length-truncated response may retry one rung
  *down*.

Internal read-only lanes (self-review and the built-in `explore`, `review`,
`research`, `security-review` subagents) cap effort at `medium` — downshift
only, never upgrade — so delegated sweeps stay cheap.

Each transport encodes effort differently (top-level `reasoning_effort`,
`reasoning: {effort}` envelopes, Anthropic thinking budgets or adaptive
thinking, GLM/Kimi thinking flags); the catalog's `reasoning_codec` per
connection picks the wire form, and bonsai never sends reasoning parameters
to models that reject them.

## Model shortcuts

In `/model`, focus the Reasoning pane for a model and press any single ASCII
letter to bind that letter to the selected **model + effort** pair. Pressing
a different letter on the same pair replaces the old binding.

- Switch with `/model f` or the bare shortcut command `/f`.
- Custom-agent frontmatter can use the same selector (`model: f`) — the agent
  follows whatever the letter currently points at, resolved live at
  invocation time.
- Bindings persist in the session store (`model_shortcuts`). Legacy
  cheap/fast/smart model roles migrate automatically to the letters
  `c`/`f`/`s`.

## Fallback chains

A subagent definition may name a `fallback_model` (+ `fallback_effort`): the
resolution order is persisted primary → backup → parent model. At runtime, a
*permanent* provider error (never a cancellation) activates the fallback and
resets the retry counter; failover is sticky for the rest of that delegated
run. See [Agents and modes](agents.md).

## Token counting and cost

Each connection declares a `default_token_counter`, used for context-pressure
estimation and compaction decisions (provider-reported usage remains the
billing source of truth):

| Counter | Mechanics | Confidence |
| --- | --- | --- |
| `tiktoken` | Local BPE — `o200k_base` for gpt-4o/4.1/4.5/5/o-series, else `cl100k_base` | high |
| `qwen3` | Embedded HuggingFace tokenizer (`models/tokenizers/qwen3/`) | high |
| `anthropic-count-tokens` | Real `count_tokens` API call; degrades to heuristic on failure | high |
| `heuristic` | `chars / 4` | low |

Image data URIs are redacted to `[image]` before counting so base64 doesn't
inflate estimates or falsely trigger compaction.

**Pricing** is per-model in micro-USD per million tokens, with distinct
`input`, `output`, `cache_read`, and `cache_write` rates. Cost is
cache-aware: fresh input = prompt − cache reads − cache writes, each class
priced at its own rate. The `/usage` dashboard and session
[cost budget](sessions.md#budgets) consume these figures; cost enforcement is
exact only while every contributing turn has known usage and pricing.

Providers with time-of-day billing declare shared recurring, half-open UTC
windows on the connection; each affected target declares its own peak rates.
The base `pricing` and `pricing_tiers` remain the off-peak schedule, while
`peak_pricing` and optional `peak_pricing_tiers` apply inside a window:

```toml
[[connections]]
id = "example"
# ... auth, transport, and endpoint fields ...
peak_pricing_windows_utc = [
  { start = "01:00", end = "04:00" },
  { start = "06:00", end = "10:00" },
]

[[targets]]
connection = "example"
model = "example/model"
pricing = { input_micros_per_million = 220000, output_micros_per_million = 660000 }
peak_pricing = { input_micros_per_million = 440000, output_micros_per_million = 1320000 }
```

Windows recur daily, use `HH:MM` UTC, include their start, and exclude their
end. A start later than the end wraps across midnight. A completed response is
priced using the start time of its final provider attempt, so a long stream
that crosses a boundary keeps the rate in effect when that attempt began.

## Prompt-cache shaping

Providers differ in how a stable prefix must be presented; the catalog
declares it per connection (`prompt_cache`, `prompt_cache_policy`), and the
agent shapes context accordingly:

- **Lane-scoped cache keys** — the conversation cache key is suffixed with a
  hash of the lane's system prompt, so the coding lane, plan lane, and
  subagents never collide on one cache route. The key persists with the
  session so a resume stays warm.
- **Anthropic breakpoints** — up to three `cache_control` markers: last tool
  definition, the byte-stable system-prompt head, and the newest (or rolling)
  history position.
- **Append-only project state** — connections with rolling-history caching
  (and Codex always) receive volatile project state appended to history
  rather than mutated in the system tail, keeping the cached prefix
  byte-stable.

See [Context management](context.md) for the agent-side half of this.

## Refreshing and maintaining the catalog

Startup logs warn about drift between explicit TOML and models.dev. The
built-in `provider-setup` [skill](skills.md) can write user-catalog entries
conversationally; `/providers` manages connections in the TUI.

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Catalog load/merge, models.dev cache | `src/model_catalog/` (`mod.rs`, `io.rs`, `models_dev.rs`) |
| Connection/target specs | `src/model_catalog/spec.rs`, `models/builtin/` |
| Reasoning efforts & codecs | `src/provider/metadata.rs`, `src/provider/reasoning.rs` |
| Shortcuts & roles | `src/model_role.rs` |
| Resolution chains | `src/model_resolution.rs` |
| Token counting & pricing | `src/provider/token.rs` |
