---
name: refresh-models
description: "Audit and refresh bonsai's builtin model catalog (models/builtin/*.toml) against models.dev and live provider listings — fix drifted context windows/output limits/pricing, add newly served models, retire stale ones. Use when a provider ships new models, startup logs show drift warnings, the picker shows wrong windows/prices, or on a periodic lineup refresh. Optional argument: a connection id (any models/builtin/*.toml stem, e.g. codex, anthropic, opencode, zai, mimo-coding-plan) to scope the refresh."
---

# refresh-models

The catalog has three layers, and only the first is hand-maintained:

1. **Bundled TOML** (`models/builtin/*.toml`, compiled in via `include_str!`) —
   authoritative when it sets a value; the only layer that drifts.
2. **models.dev** (`https://models.dev/api.json`, refetched when the cache at
   `~/.bonsai/cache/models-dev.json` is older than 1h) — fills every gap TOML
   leaves; per-row tolerant parse.
3. **Live `/models`** per connection (`~/.bonsai/cache/live-models/*.json`,
   5-min TTL) — overrides context windows and (codex) reasoning levels.

At every catalog load bonsai warns about any explicit TOML value that
disagrees with its models.dev row (`log_models_dev_drift`,
`src/model_catalog/mod.rs`), unless the target sets `pinned = true`. This
skill is the maintenance loop around that warning.

## Procedure

### 1. Refresh sources and run the audit

```bash
python3 .agents/skills/refresh-models/audit.py [connection-id] --fetch
```

`--fetch` forces a fresh models.dev download; omit it to accept a cache up to
24h old. To also refresh the live caches first, run the TUI once (`/refresh`
inside it) or let startup do it, since live files older than 5 min refetch on
launch. Exit code 1 means unexplained findings.

### 2. Interpret each finding

| Finding | Action |
|---|---|
| `OK` | Nothing. |
| `PINNED` | Deliberate divergence. Only verify the comment above it is still true (e.g. a beta header still gates the bigger window, the price tier boundary still exists). |
| value mismatch | models.dev is presumed right. Fix the TOML — **unless** there is a documented reason to diverge, then keep the value and add `pinned = true` + a comment saying *why* (see Pins below). |
| `missing in toml, models.dev has N` (a price rate) | The Rust drift check compares whole pricing structs; add the missing rate (usually `cache_write_micros_per_million`) to the TOML pricing table. |
| `NOT IN models.dev` | Wrong or missing `metadata_model`. Map it to the right block (table below). A provider-invented alias (e.g. `gpt-5.5-1m`) points at its real row; a model models.dev truly lacks gets `pinned = true`. |
| lineup gap / live unmapped | The provider serves a model with no target. Add one (conventions below) — or, when exclusion is deliberate, annotate the TOML with a `# not-shipped: <remote-id> — reason` comment line: the audit reads those, stops counting the id as a gap, and labels it "(deliberately not shipped)" in the unmapped list. A skip without a reason on the same line is a bug, same rule as pins. Models listed live but absent from models.dev stay live-only — they still appear in the picker with an "(assumed)" window. An id annotated "target exists — stale cache" just needs the connection's next 5-min live refresh; a gap line marked "informational — no live cache yet" is a hint, not a finding (run the TUI once with that provider's key for precise candidates). |
| reasoning verify lines | `reasoning_options = []` is the deliberate "no thinking UI" marker; confirm it matches reality rather than trusting either side blindly. |

### models.dev block per connection (`metadata_model` prefix)

| connection | block | example |
|---|---|---|
| codex | `openai` | `metadata_model` unneeded — target ids are already `openai/...` |
| anthropic | `anthropic` | unneeded — ids are `anthropic/...` |
| opencode (Go) | `opencode-go` | `metadata_model = "opencode-go/glm-5.2"` |
| opencode-zen | `opencode` | `metadata_model = "opencode/claude-sonnet-5"` |
| minimax-coding-plan | `minimax` | `metadata_model = "minimax/MiniMax-M3"` |

### 3. Conventions when adding targets

- **codex.toml** — explicit values from the models.dev `openai` row
  **including `cache_write_micros_per_million`**; `endpoint_path =
  "responses"`, `token_counter = "tiktoken"`, toggle + effort
  `["low","medium","high","xhigh"]`. The live codex `/models` refresh overlays
  actual reasoning levels, so the static list is just the family default.
- **anthropic.toml** — generation decides the wire shape, and getting it wrong
  produces HTTP 400s, not cosmetic drift:
  - claude-sonnet-4-6 / opus-4-7 and newer / sonnet-5 / fable-5: MUST set
    `reasoning_codec = "anthropic-adaptive"` with `type = "effort"` options
    (`budget_tokens` payloads are rejected). `"none"` in the values list adds
    the Off toggle — **never give fable-5 `"none"`** (explicit disable 400s).
    Omit `pricing` (models.dev backfill tracks promo pricing). Windows are
    native 1M.
  - claude-sonnet-4-5 and older, haiku: keep `budget_tokens` options; sonnet-4-5
    stays pinned at 200000 (its 1M needs a beta header bonsai doesn't send).
  - Also update the seed list in `src/provider/anthropic.rs`
    (`ANTHROPIC_METADATA`) and its pin in
    `metadata.rs::builtin_connection_defaults_are_pinned` — though the live
    `/v1/models` discovery makes seeds an offline fallback only.
- **opencode.toml (Go)** — lean targets: `metadata_model =
  "opencode-go/<id>"`, no explicit values (backfill keeps them current).
  Per family: qwen/minimax ride `transport = "anthropic-messages"` +
  `endpoint_path = "messages"`; GLM before 5.2 uses
  `reasoning_codec = "zai-thinking"`, 5.2+ uses `"zai-reasoning-effort"`
  (5.2 rejects both fields together); everything else uses the connection's
  chat-completions default.
- **opencode-zen.toml** — explicit values from the models.dev `opencode`
  block (house style of that file).
- **minimax-coding-plan.toml** — explicit values; `-highspeed` variants bill
  2x base and usually lack their own models.dev row → `metadata_model` to the
  base row + `pinned = true`.
- **GLM 5.2+ anywhere**: keep the 12000 output clamp + rumination comment
  (session 262: 71k-token thinking turn with zero actions). `pinned = true`.

### Pins

`pinned = true` suppresses the drift warning only — TOML precedence is
unchanged. Every pin carries a comment naming the reason (working-window cap,
price-tier boundary, beta-gated limit, rumination clamp). A pin without a
comment is a bug.

### 4. After editing

1. Target-count assertions: `rg "targets.len\(\), " src/model_catalog/mod.rs`
   — two assertions pin the builtin count; adjust by the number of
   added/removed targets.
2. Bin-only crate — one filter per invocation, and confirm the literal
   `test result: ok` (piped tails can mask failures):
   ```bash
   cargo test model_catalog
   cargo test provider::
   cargo test model_resolution
   ```
3. Real-surface check (uses the verifier-tui skill's driver):
   ```bash
   cargo build
   S=.agents/skills/verifier-tui/tui.sh
   "$S" start 160 45 && sleep 2
   grep -c "drifts from models.dev" "$(ls -t ~/.bonsai/logs/bonsai-*.log | head -1)"   # must be 0
   "$S" keys "/model" Enter   # spot-check windows/prices in the picker
   "$S" stop
   ```
   The footer of the picker shows the resolved window, reasoning menu, and
   price for the selected target — compare against the audit output.
4. Offline sanity: relaunch once with `BONSAI_DISABLE_MODELS_FETCH=1` and
   confirm the picker still lists the lineup (TOML + cache only).

## Knobs

| Env | Effect |
|---|---|
| `BONSAI_MODELS_DEV_TTL_SECS` | models.dev refetch TTL (default 3600; 0 = every load) |
| `BONSAI_DISABLE_MODELS_FETCH` | never fetch; TOML + cache only |
| `BONSAI_MODELS_DEV_PATH` | read models.dev JSON from a file (fixture testing) |
| `BONSAI_MODELS_DEV_URL` | alternate catalog URL |
