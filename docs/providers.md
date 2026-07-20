# Providers

bonsai's providers are **data, not code**. Every built-in connection is a
declarative `[[connections]]` entry in `models/builtin/connections.toml`,
bound to one of three wire transports. The catalog turns each entry into a
provider factory at startup; `/authorize` and `/model` work for a new entry
with no Rust changes. Model metadata (context windows, pricing, reasoning
support) is sourced from [models.dev](https://models.dev) and merged with
per-model TOML files — see [Models](models.md).

## Built-in connections

Nineteen connections ship in the binary, in this registry/picker order:

| id | Display name | Transport | Auth | Default model | Env vars (key / model / base URL) |
| --- | --- | --- | --- | --- | --- |
| `opencode` | OpenCode Go | openai-chat | api-key | `opencode/qwen3.7-max` | `OPENCODE_API_KEY` / `OPENCODE_MODEL` / `OPENCODE_BASE_URL` |
| `opencode-zen` | OpenCode Zen | openai-chat | api-key | `opencode-zen/grok-code` | `OPENCODE_API_KEY` / `OPENCODE_ZEN_MODEL` / `OPENCODE_ZEN_BASE_URL` |
| `codex` | Codex | codex-responses | codex-cache | `openai/gpt-5.5` | — / `CODEX_MODEL` / `CODEX_BASE_URL` |
| `anthropic` | Anthropic API | anthropic-messages | api-key | `anthropic/claude-sonnet-4-5` | `ANTHROPIC_API_KEY` / `ANTHROPIC_MODEL` / `ANTHROPIC_BASE_URL` |
| `minimax` | MiniMax API | anthropic-messages | api-key | `minimax/MiniMax-M3` | `MINIMAX_API_KEY` / `MINIMAX_MODEL` / `MINIMAX_BASE_URL` |
| `minimax-coding-plan` | MiniMax Coding Plan | anthropic-messages | api-key | `minimax-coding-plan/MiniMax-M3` | `MINIMAX_CODING_PLAN_API_KEY` / `…_MODEL` / `…_BASE_URL` |
| `zai` | Z.AI API | openai-chat | api-key | `zai/glm-5.2` | `ZAI_API_KEY` / `ZAI_MODEL` / `ZAI_BASE_URL` |
| `zai-coding-plan` | Z.AI Coding Plan | openai-chat | api-key | `zai-coding-plan/glm-5.2` | `ZAI_CODING_PLAN_API_KEY` / `…_MODEL` / `…_BASE_URL` |
| `moonshotai` | Moonshot AI API | openai-chat | api-key | `moonshotai/kimi-k2.7-code` | `MOONSHOT_API_KEY` / `MOONSHOT_MODEL` / `MOONSHOT_BASE_URL` |
| `kimi-coding-plan` | Kimi Coding Plan | openai-chat | api-key | `kimi-coding-plan/kimi-for-coding` | `KIMI_CODING_PLAN_API_KEY` / `…_MODEL` / `…_BASE_URL` |
| `mimo` | Xiaomi MiMo API | openai-chat | api-key | `mimo/mimo-v2.5-pro` | `MIMO_API_KEY` / `MIMO_MODEL` / `MIMO_BASE_URL` |
| `mimo-coding-plan` | Xiaomi MiMo Coding Plan | openai-chat | optional-api-key | `mimo-coding-plan/mimo-v2.5-pro` | `MIMO_CODING_PLAN_API_KEY` / `…_MODEL` / `…_BASE_URL` |
| `openrouter` | OpenRouter | openai-chat | api-key | `openrouter/gpt-5.2` | `OPENROUTER_API_KEY` / `OPENROUTER_MODEL` / `OPENROUTER_BASE_URL` |
| `openai` | OpenAI API | openai-chat | api-key | `openai/gpt-5.6` | `OPENAI_API_KEY` / `OPENAI_MODEL` / `OPENAI_BASE_URL` |
| `openai-compatible` | OpenAI Compatible | openai-chat | optional-api-key | — (discovered) | `OPENAI_COMPATIBLE_API_KEY` / `…_MODEL` / `…_BASE_URL` |
| `anthropic-compatible` | Anthropic Compatible | anthropic-messages | optional-api-key | — (discovered) | `ANTHROPIC_COMPATIBLE_API_KEY` / `…_MODEL` / `…_BASE_URL` |
| `deepseek` | DeepSeek API | openai-chat | api-key | `deepseek/deepseek-v4-flash` | `DEEPSEEK_API_KEY` / `DEEPSEEK_MODEL` / `DEEPSEEK_BASE_URL` |
| `qwencloud` | Qwen Cloud API | openai-chat | api-key | `qwencloud/qwen3.7-plus` | `DASHSCOPE_API_KEY` / `DASHSCOPE_MODEL` / `DASHSCOPE_BASE_URL` |
| `qwencloud-token-plan` | Qwen Cloud Token Plan | openai-chat | api-key | `qwencloud-token-plan/qwen3.7-max` | `DASHSCOPE_TOKEN_PLAN_API_KEY` / `…_MODEL` / `…_BASE_URL` |

`openai-compatible` defaults its base URL to `http://localhost:11434/v1`
(Ollama) and `anthropic-compatible` to `http://localhost:8080`, so pointing
bonsai at a local model server is a matter of authorizing with the right URL.
`BONSAI_PROVIDER` selects the provider for a new headless run; the default
provider is `opencode`.

A connection entry also carries wire-quirk flags where a vendor's adapter
needs them — for example `reasoning_content_echo` (DeepSeek/GLM/Kimi adapters
reject tool-call turns without a replayed `reasoning_content` field),
`usage_frame_ends_stream` (OpenCode Zen upstreams that close a stream with a
usage frame instead of a `finish_reason`), and `prompt_cache` /
`prompt_cache_policy` (see below). These flags live in the connection spec,
and **only** there: a capability set on internal metadata constants but not in
`connections.toml` is inert.

## Prompt-cache policies

`prompt_cache_policy` selects how bonsai keeps a provider's prompt cache warm.
It can be set on a connection (inherited by every model of that connection),
overridden per model in a catalog `[[targets]]` entry, and patched in the
[user catalog](configuration.md#layering) without rebuilding. The
default is deliberately conservative — opt a connection in only after a live
warm-request verification, because reshaping the request body invalidates every
existing cache entry for that provider.

| Policy | What it does | When to use |
| --- | --- | --- |
| `transport-default` (default) | Keeps the transport's established request shape: volatile project state (git status, read coverage) is updated in place in the system tail, and the transport places its own cache breakpoints if any. | Providers whose caching tolerates a mutable tail, or where caching is unverified. Safe default; never opt a connection out of this silently. |
| `rolling-history` | Append-only project state: volatile state is stored as immutable named history messages, so every previous request is an exact byte prefix of the next. On `anthropic-messages` the newest snapshot additionally sits behind an explicit `cache_control` breakpoint; on `openai-chat` the snapshots simply stay in place for automatic prefix caches. | Providers with strict-prefix automatic caching. Currently opted in: `opencode`, `moonshotai`, `kimi-coding-plan`, `deepseek` (a DeepSeek hit must fully match a cached prefix unit and is ~50× cheaper than a miss; verified live at 65–99% input-cache reuse), and `qwencloud`/`qwencloud-token-plan` (automatic markerless prefix cache; verified live at ~91% warm reuse, cached input bills at 20% of the input rate). |
| `open-router-anthropic` | OpenRouter's documented automatic caching for Anthropic models: top-level `cache_control: {type: ephemeral}` plus a sticky `session_id`, alongside append-only project state. | Only OpenRouter routes serving Anthropic models (set per model, e.g. `openrouter/claude-haiku-4.5`); the fields are not valid generic Chat Completions parameters. |

Related per-connection cache knobs: `prompt_cache` (send the lane-scoped
`prompt_cache_key` / enable `cache_control` breakpoints at all) and the
catalog's per-model `output_limit`, which doubles as the wire `max_tokens` —
capping it is the rumination-containment lever for thinking models (GLM 5.2
pins 12000; DeepSeek pins 32000 against an advertised 384K).

Cache accounting is provider-specific on the wire but normalized internally:
OpenAI-family backends report `prompt_tokens_details.cached_tokens`, Kimi a
top-level `cached_tokens`, DeepSeek `prompt_cache_hit_tokens` /
`prompt_cache_miss_tokens`, Anthropic `cache_read_input_tokens` — all fold
into the same cache-aware usage and pricing model (see
[Models](models.md#token-counting-and-cost)).

## The three wire transports

| Transport | Endpoint | Serves |
| --- | --- | --- |
| `openai-chat` | `chat/completions` | OpenCode Go/Zen, Z.AI, Moonshot/Kimi, MiMo, DeepSeek, Qwen Cloud (API + Token Plan), OpenRouter, OpenAI, openai-compatible |
| `anthropic-messages` | `v1/messages` | Anthropic, MiniMax (API + Coding Plan), anthropic-compatible |
| `codex-responses` | `responses` | Codex (ChatGPT backend) |

All three share one SSE driver (`src/provider/sse.rs`) that handles frame
boundaries, partial UTF-8 chunks, spaceless `event:`/`data:` lines, joined
multi-`data:` frames, `[DONE]` sentinels, keep-alives, and a 10-minute
per-chunk stall timeout. Per-provider SSE parsers are deliberately forbidden
(see [Development](development.md)).

### openai-chat

Standard Chat Completions streaming with `stream_options.include_usage`.
Notable behaviors:

- **Prompt caching** — sends `prompt_cache_key` when the connection declares
  `prompt_cache` (or the base URL is the official `api.openai.com`). The key
  is the session's conversation key plus a hash of the lane's system prompt,
  so the coding lane, plan lane, and each subagent get separate cache routes
  instead of colliding. The `openrouter-anthropic` cache policy instead sends
  a top-level `cache_control: {type: ephemeral}` and `session_id`.
- **Inline think tags** — `<think>…</think>` inside streamed content is split
  out to the reasoning channel.
- **Truncation guard** — a stream that ends without a terminal
  `finish_reason` (or a terminal usage frame, for connections that declare
  `usage_frame_ends_stream`) is treated as a retryable transport failure, not
  a completed response. Silent truncation never masquerades as success.

### anthropic-messages

Anthropic Messages streaming (`anthropic-version: 2023-06-01`):

- System/developer messages become the `system` string; consecutive
  same-role messages are merged to satisfy the alternating-turn contract.
- **Prompt caching** — up to three `cache_control` breakpoints: the last tool
  definition, the byte-stable head of the system prompt (split at the
  `## Volatile state` heading), and the newest history position (or a rolling
  history point for rolling-history connections).
- **Thinking replay** — native thinking blocks from a tool-call turn are
  captured and re-emitted ahead of the assistant content on the next request;
  dropping them stops extended thinking after the first tool round.
- Usage arrives across `message_start` and `message_delta` (MiniMax reports
  final input + cache totals in the latter); both are folded into bonsai's
  cache-inclusive usage convention.

### codex-responses

The ChatGPT-backend Responses API used by Codex subscriptions:

- Auth is imported from the local **Codex CLI login** (`~/.codex/auth.json`)
  on each startup; requests carry the account id, `originator`, and a floored
  client `version` header (the backend refuses to serve some models to older
  clients; `BONSAI_CODEX_CLIENT_VERSION` overrides).
- The first system message becomes the `instructions` field byte-stably;
  later system messages become in-position developer messages, keeping the
  cached prefix stable. Caching is automatic server-side (`prompt_cache_key`
  lane-scoped as above); explicit cache options are rejected by the backend.
- "Lite"-served models get a reshaped request (`additional_tools`, developer
  messages, `reasoning.context: all_turns`) and a marker header.

## Authentication

`/authorize` (optionally `/authorize <provider-id>`) drives one of three
flows, chosen by the connection's `auth` field:

- **`api-key`** — prompts for the key, or takes it from the connection's env
  var (`AuthInput::FromEnv`).
- **`optional-api-key`** — prompts for a base URL, optional key, and optional
  model id; used by the local/compatible connectors. bonsai probes the
  server's model list and auto-picks a sensible default model. Live discovery
  understands generic `/models` listings plus native LM Studio and Ollama
  APIs (bounded fan-out) for context-window and display-name metadata.
- **`codex-cache`** — imports the Codex CLI login: access token, account id,
  and the decoded JWT claims (including FedRAMP tenancy).

### Credential storage

You choose where secrets live; bonsai's database stores only a *reference* to
the source, never the secret:

| Store | Mechanics |
| --- | --- |
| Protected file *(default)* | `$BONSAI_HOME/credentials/`, `0600` files in a `0700` directory on Unix; user-bound DPAPI encryption on Windows |
| OS credential store | `keyring` service `dev.bonsai.provider` |
| Session-only | Process memory; cleared on exit, cannot be resumed or backed up |

The default is chosen during [onboarding](getting-started.md#first-run) and
changeable in `/settings`; `Ctrl+P` in an authorization prompt cycles the
store for that one login. Environment-sourced keys are re-read from the env
var on every start; Codex credentials are re-imported from the CLI cache on
every start. If the chosen store is unavailable at authorization time, bonsai
downgrades that credential to session-only rather than failing or writing it
somewhere less protected.

## Errors, retries, and failover

Provider failures are typed (`Http {status, retry-after}`, `Transport`,
`Decode`, `Configuration`):

- **Retryable:** 429, 500, 502, 503, 504, and 529 (Anthropic overload), plus
  all transport/decode failures — up to `MAX_PROVIDER_RETRIES` (3) with
  exponential backoff that honors a server-provided `retry-after`.
- **Not retryable:** configuration errors, and quota exhaustion (402 or
  quota-signalling messages such as `insufficient_quota`), which surface
  immediately with a next-action hint (`/authorize`, `/model`, or resume).
- A failed attempt's partially streamed output is **discarded and visibly
  retracted** in the TUI before the retry, so the transcript never contains
  half of a failed generation.
- On a *permanent* error (never on cancellation), the retry loop can activate
  a **provider fallback** — e.g. a subagent's `fallback_model` — and reset the
  attempt counter. Failover is sticky for the rest of that delegated run.
- Per-attempt meters cap streamed characters and generation time; sustained
  rate-limit or quota walls terminate the run as a typed
  [budget exhaustion](sessions.md#budgets) rather than a generic failure.

## Custom connections and endpoints

User and project catalogs layer over the built-ins:

- `$BONSAI_HOME/providers/*.toml` and `$BONSAI_HOME/models/*.toml` — global.
  bonsai scaffolds disabled `example-local.toml` starters here.
- `<project>/.bonsai/providers/` and `.bonsai/models/` — project-scoped,
  git-trackable, **only active once the workspace is trusted**. Credentials
  remain user-scoped; project files carry non-secret endpoint/model overrides
  only.

Files are partial patches: a project entry with the same id overrides fields
of a global or built-in one. Higher layer wins per field. The built-in
`provider-setup` [skill](skills.md) teaches the agent to write these files
for you ("add my Ollama models").

To add a *new vendor on an existing transport*, a `[[connections]]` TOML
entry is all it takes; a genuinely new wire format needs a transport
implementation — see [Development](development.md#adding-a-provider).

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Connection specs | `models/builtin/connections.toml`, `src/model_catalog/spec.rs` |
| Registry & catalog factory | `src/provider/registry.rs` |
| openai-chat transport | `src/provider/opencode.rs` |
| anthropic-messages transport | `src/provider/anthropic.rs` |
| codex-responses transport | `src/provider/codex.rs` |
| Shared SSE driver | `src/provider/sse.rs`, `src/provider/streaming.rs` |
| Error taxonomy & retry | `src/provider/mod.rs`, `src/agent/retry.rs` |
| Auth flows & credential stores | `src/provider/auth.rs`, `src/session/credentials.rs` |
| Live endpoint discovery | `src/provider/discovery.rs` |
