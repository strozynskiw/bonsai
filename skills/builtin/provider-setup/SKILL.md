---
name: provider-setup
description: Add or fix bonsai model providers and models — local OpenAI-compatible endpoints (Ollama, LM Studio, vLLM), API keys and authorization, new model entries with context windows, pricing, and reasoning options, or overrides of built-in catalog entries. Load before creating or editing anything under ~/.bonsai/providers or ~/.bonsai/models, or when the user wants a new provider, a local model, or a model catalog change.
---

# Provider & Model Setup

bonsai's model catalog is compiled into the binary and overlaid by user TOML
files from two directories (created at startup with disabled, commented
example files worth reading):

- `~/.bonsai/providers/*.toml` — `[[connections]]` entries: where and how to
  reach a backend.
- `~/.bonsai/models/*.toml` — `[[targets]]` entries: which models run on a
  connection.

(`$BONSAI_HOME` relocates `~/.bonsai`.) Every `*.toml` in each directory is
read; naming a file after its connection id is convention, not requirement.
Entries merge as patches: an entry whose id matches a built-in modifies just
the fields it sets; a new id creates a new connection or target.

## Choose the path first

- **Built-in hosted provider** (`opencode`, `opencode-zen`, `codex`,
  `anthropic`, `minimax`, `minimax-coding-plan`, `zai`, `zai-coding-plan`,
  `moonshotai`, `kimi-coding-plan`, `openrouter`): nothing to author. The
  user runs `/authorize anthropic` (or another id) or sets the provider's env
  key. `codex` imports the local Codex CLI login at startup.
- **Quick local/compatible endpoint**: `/authorize openai-compatible` (or
  `anthropic-compatible`) prompts for base URL, optional key, optional model —
  no files written.
- **A named provider worth keeping**: write the two files below. (The
  `/providers add` wizard — alias `/wizard` — builds the same files
  interactively, TUI only. When you are doing the setup, write the files.)

## Connection file

```toml bonsai:providers-file
# ~/.bonsai/providers/ollama.toml
[[connections]]
id = "ollama"
display_name = "Ollama"
auth = "optional-api-key"
transport = "openai-chat"
default_base_url = "http://localhost:11434/v1"
api_key_env = "OLLAMA_API_KEY"
model_env = "OLLAMA_MODEL"
base_url_env = "OLLAMA_BASE_URL"
default_endpoint_path = "chat/completions"
default_token_counter = "heuristic"
prompt_cache = true
discovery = "ollama"
```

- Required for a **new** connection: `id`, `display_name`, `auth`,
  `transport`. Everything else defaults.
- `auth`: `api-key` (key required), `optional-api-key` (local servers),
  `codex-cache` (built-in codex only).
- `transport` picks the wire protocol: `openai-chat`, `anthropic-messages`,
  or `codex-responses`; default endpoint paths are `chat/completions`,
  `v1/messages`, `responses`.
- `discovery`: `generic` (the transport's standard model-listing endpoint),
  `ollama`, or `lm-studio` — the server-specific kinds fetch richer live
  metadata (names, context windows, capabilities).
- `prompt_cache = true` sends cache hints (`prompt_cache_key` on openai-chat,
  `cache_control` breakpoints on anthropic-messages); backends that don't
  cache simply ignore them — keep it on for local servers.
- The `*_env` names are yours to pick; the wizard derives
  `BONSAI_<ID>_API_KEY`-style names.
- **Patch a built-in** by using its id with only the fields to change (e.g.
  `default_base_url` for a proxy). A patch of just `enabled = false` disables
  it — what `/providers disable <id>` writes; refused if it would leave zero
  enabled providers.

## Model file

```toml bonsai:models-file
# ~/.bonsai/models/ollama.toml
[[targets]]
connection = "ollama"
model = "qwen3-coder"             # bare names get the connection prefix
remote_model = "qwen3-coder:30b"  # id sent on the wire
default = true
# Match the serving context (Ollama num_ctx / llama.cpp --ctx-size), not the
# model card maximum — this drives compaction budgets.
context_window = 65536
output_limit = 8192
token_counter = "heuristic"
features = ["tool-call"]

[[targets]]
connection = "ollama"
model = "ollama/deepseek-r1"
remote_model = "deepseek-r1:14b"
context_window = 32768
features = ["tool-call", "reasoning"]
pricing = { input_micros_per_million = 0, output_micros_per_million = 0 }
[[targets.reasoning_options]]
type = "effort"
values = ["none", "low", "medium", "high"]
```

- Required: `connection`, `model`. A full model id is `connection/name`
  (exactly one `/`, no whitespace); a bare name is auto-prefixed and doubles
  as the display name.
- `token_counter`: `heuristic`, `tiktoken`, `qwen3`, or
  `anthropic-count-tokens`.
- `features`: `tool-call`, `reasoning`, `structured-output`, `temperature`,
  `attachment`. Coding use needs `tool-call`.
- Reasoning: `reasoning_codec` (`openai-compatible`, `codex-responses`,
  `anthropic-thinking`, `anthropic-adaptive`, `zai-thinking`,
  `zai-reasoning-effort`, `kimi-thinking`; defaults per transport) plus
  `[[targets.reasoning_options]]` blocks — `type = "effort"` with `values`
  from `minimal`/`low`/`medium`/`high`/`xhigh`/`max`/`ultra` (include `none`
  to allow off), `type = "budget_tokens"` with `min`/`max`/`default`, or
  `type = "toggle"`.
- `pricing` is micro-USD per million tokens ($3/M ⇒ `3000000`), with optional
  `cache_read_micros_per_million` / `cache_write_micros_per_million`.
- Only one `default = true` target per connection.
- **Override a built-in model** with the same `connection` + `model` and just
  the changed fields:

```toml bonsai:models-file
# ~/.bonsai/models/tweaks.toml — adjust one field of a built-in entry
[[targets]]
connection = "anthropic"
model = "anthropic/claude-sonnet-5"
context_window = 400000
pinned = true  # deliberate divergence; silences the models.dev drift warning
```

## Keys and authorization

- Keys never go in catalog files — files carry env-var *names* and URLs only.
- Interactive: `/authorize <provider>` prompts and stores per the user's
  credential-storage choice (protected files under `~/.bonsai/credentials`,
  OS keychain, or session-only). `/unauthorize <provider>` removes it.
- Env bootstrap: whatever the connection's `api_key_env`/`model_env`/
  `base_url_env` name (built-ins: `ANTHROPIC_API_KEY`, `OPENCODE_API_KEY`,
  `MINIMAX_API_KEY`, `MINIMAX_CODING_PLAN_API_KEY`, `ZAI_API_KEY`,
  `ZAI_CODING_PLAN_API_KEY`, `MOONSHOT_API_KEY`,
  `KIMI_CODING_PLAN_API_KEY`, `OPENROUTER_API_KEY`, …). Headless runs pick
  a provider with `BONSAI_PROVIDER=<id>`.

## Metadata enrichment (models.dev)

Missing windows/pricing/display data auto-fill from models.dev (cached at
`~/.bonsai/cache/models-dev.json`, ~1 h TTL). Explicit TOML always wins; a
mismatch logs a drift warning unless the target sets `pinned = true`.
Offline/air-gapped: `BONSAI_DISABLE_MODELS_FETCH=1`; `BONSAI_MODELS_DEV_URL`,
`BONSAI_MODELS_DEV_PATH`, and `BONSAI_MODELS_DEV_TTL_SECS` override the source.

## Verify, then hand off

1. Hand-written catalog files are read at the next launch — have the user
   restart bonsai (the wizard applies its own writes immediately).
2. `/providers list` — the connection shows enabled with its base URL.
3. `/model` — the targets appear with window/pricing; picking one starts
   using it. `/refresh` re-queries the provider's live model list.
4. Startup logs name any drifted or unserved models.

## Pitfalls

- `[providers]` inside `config.toml` is reserved and **ignored** — provider
  config lives only in the catalog directories above.
- Two *user* files defining the same connection id, or the same
  connection+model pair, is a duplicate error that fails catalog load. Keep
  one file per provider.
- `/providers remove <id>` deletes only wizard-scoped files; it refuses
  built-ins and hand-authored multi-entry files (edit those manually).
- The scaffold examples (`providers/example-local.toml`,
  `models/example-local.toml`) ship disabled — copy them, or flip
  `enabled = true`, rather than adding a second `default = true`.
