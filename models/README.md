# Model Catalog

Bonsai's model catalog is split into two concepts:

- **connections**: how Bonsai talks to a provider or endpoint.
- **targets**: which model ids are available on a connection, plus per-model overrides.

Built-in catalog files live in this repository under `models/builtin/`. User catalog
files live under `$BONSAI_HOME`. A trusted workspace can add a project layer under
`.bonsai/`:

```text
$BONSAI_HOME/
  providers/
    example-local.toml
    my-provider.toml
  models/
    example-local.toml
    my-models.toml
  cache/
    models-dev.json
    live-models/

<project>/.bonsai/
  providers/
    team-endpoint.toml
  models/
    team-models.toml
```

`BONSAI_HOME` defaults to `~/.bonsai`. On startup Bonsai creates
`$BONSAI_HOME/providers` and `$BONSAI_HOME/models` if they do not exist, then writes
disabled example files into them.

Project files load only after the workspace has been explicitly trusted. They are
applied after the user layer, so a project can pin its shared endpoint and model
metadata deterministically. Catalog files never contain credential values: API keys and
login sessions remain in the user's selected file, keyring, or session-only credential
store. Do not put secrets in `.bonsai/providers` or `.bonsai/models`.

`/refresh` updates Models.dev metadata and every authorized provider's live model list.
When Models.dev is offline, Bonsai keeps the last cached metadata and shows an actionable
catalog notice in `/model` and `/providers` instead of requiring log inspection.

## Quick Start

For a local OpenAI-compatible server, edit the generated examples:

`$BONSAI_HOME/providers/example-local.toml`

```toml
[[connections]]
id = "local-example"
enabled = true
display_name = "Local Example"
auth = "optional-api-key"
transport = "openai-chat"
default_base_url = "http://localhost:11434/v1"
api_key_env = "LOCAL_EXAMPLE_API_KEY"
model_env = "LOCAL_EXAMPLE_MODEL"
base_url_env = "LOCAL_EXAMPLE_BASE_URL"
default_endpoint_path = "chat/completions"
default_token_counter = "heuristic"
```

`$BONSAI_HOME/models/example-local.toml`

```toml
[[targets]]
connection = "local-example"
enabled = true
model = "example-small"
remote_model = "example-small"
default = true
context_window = 4096
output_limit = 1024
token_counter = "heuristic"
features = []
```

Then restart Bonsai and run:

```sh
/authorize local-example
/model example-small
```

For `optional-api-key` connections, `/authorize` asks for base URL, optional API key,
and optional model id.

## Maintaining built-ins

Use `scripts/update-model-catalog.sh` to compare built-in target metadata against
Models.dev:

```sh
scripts/update-model-catalog.sh --check
scripts/update-model-catalog.sh --write
```

The script validates the downloaded Models.dev JSON through Bonsai, refreshes
`context_window`, `output_limit`, `pricing`, and reasoning controls for built-in targets, and rejects targets
whose `metadata_model` cannot be resolved. It intentionally does not rewrite
transport, endpoint, auth, or default/recommended routing fields.

## Connections

Connections are stored as `[[connections]]` entries in TOML files under
`$BONSAI_HOME/providers` or, for a trusted workspace, `.bonsai/providers`.

Required fields:

- `id`: stable connection id, used by `/authorize <id>` and model targets.
- `display_name`: human-readable name in pickers.
- `auth`: one of `api-key`, `optional-api-key`, `codex-cache`.
- `transport`: one of `openai-chat`, `anthropic-messages`, `codex-responses`.

Common optional fields:

- `enabled`: defaults to `true`. Set `false` to keep a connector as a template.
- `default_base_url`: base API URL, without the endpoint path.
- `api_key_env`: environment variable Bonsai reads for the API key.
- `model_env`: environment variable Bonsai reads for the selected/default model.
- `base_url_env`: environment variable Bonsai reads for the base URL.
- `default_model`: optional canonical Bonsai model id to select by default.
- `default_endpoint_path`: request path appended to `default_base_url`.
- `default_token_counter`: `tiktoken`, `qwen3`, `anthropic-count-tokens`, or `heuristic`.
- `discovery`: `generic` uses the transport-standard model endpoint; `auto`
  additionally detects LM Studio or Ollama on loopback. Explicit native kinds include
  `lm-studio` and `ollama`.
- `prompt_cache`: sends transport-specific cache extensions. It defaults to `false` for
  compatible endpoints because unknown request fields can be rejected.
- `reasoning_content_echo`: replays OpenAI-style `reasoning_content` during tool turns.
  Enable it only when the endpoint documents that requirement.
- `auth_header`: sends the raw API key in a non-standard header instead of the transport
  default.

Use `openai-chat` for OpenAI-compatible APIs such as Ollama, LM Studio, vLLM, and
OpenRouter-style routers. Use `anthropic-messages` for Anthropic-compatible APIs.

### Compatible endpoints

The built-in `openai-compatible` and `anthropic-compatible` connections deliberately
have no fixed model lineup. Authorization, startup refresh, and `/refresh` read the
current lineup from the configured endpoint; the startup result is cached for five
minutes. OpenAI-compatible discovery calls `GET <base>/models`. Anthropic-compatible
discovery calls paginated `GET <base>/v1/models` and reuses reported display names,
input/output limits, capabilities, and reasoning controls.

For loopback URLs, `discovery = "auto"` first identifies supported local runtimes. LM
Studio uses `/api/v1/models` with `/api/v0/models` as a fallback, while Ollama uses
`/api/tags` plus bounded `/api/show` metadata probes. If no native runtime is detected,
Bonsai falls back to the standard compatible endpoint. A server without a model-list
API remains usable by entering its raw model id during authorization.

Common base URLs are:

- Ollama (OpenAI): `http://localhost:11434/v1`
- LM Studio (OpenAI): `http://localhost:1234/v1`
- LM Studio (Anthropic): `http://localhost:1234`
- vLLM (OpenAI): `http://localhost:8000/v1`
- llama.cpp (OpenAI): `http://localhost:8080/v1`

Hosted Bedrock or Vertex deployments need a gateway that exposes a standard OpenAI
Chat Completions or Anthropic Messages surface. Compatibility alone does not guarantee
prompt-cache controls or reasoning replay, so both stay off unless explicitly enabled:

```toml
[[connections]]
id = "openai-compatible"
prompt_cache = true
reasoning_content_echo = true
```

### Transport coverage and the new-transport rule

The built-in catalog currently covers every 1.0 connection with three shared wire
transports:

| Transport | Built-in use | Add another catalog connection when… |
|---|---|---|
| `openai-chat` | OpenCode, OpenAI API, Z.AI, Moonshot/Kimi, OpenRouter, generic compatible endpoints | The endpoint accepts OpenAI Chat Completions messages, streaming deltas, and function tools. |
| `anthropic-messages` | Anthropic, MiniMax API and Coding Plan, generic compatible endpoints | The endpoint accepts Anthropic Messages content blocks, SSE events, and tool results. |
| `codex-responses` | Imported Codex CLI sessions | The endpoint is the Codex Responses service with its cache/session authentication semantics. This is not the generic route for Responses-shaped vendors. |

A vendor needs a new Rust transport only when its documented request messages, streamed
events, tool-call/tool-result representation, or usage/error frames cannot be represented
by one of those protocols. Different URLs, headers represented by the existing auth modes,
model ids, endpoint paths, token counters, reasoning controls, and capability flags remain
catalog configuration.

Gemini does not require a native transport for the 1.0 provider contract: Google's
[OpenAI compatibility endpoint](https://ai.google.dev/gemini-api/docs/openai) supports
Chat Completions streaming and function calling, so it can be configured through
`openai-chat`. A future integration that needs native `generateContent`, Interactions,
grounding, or other Google-only features would justify a distinct 1.x transport after
shared request, stream, tool, usage, cancellation, and error fixtures are defined.

## Targets

Targets are stored as `[[targets]]` entries in TOML files under
`$BONSAI_HOME/models` or, for a trusted workspace, `.bonsai/models`.

Required fields:

- `connection`: connection id this target runs on.
- `model`: connection-local model name shown in Bonsai, such as `Qwen`.

Common optional fields:

- `enabled`: defaults to `true`. Set `false` to keep a model as a template.
- `remote_model`: raw model id sent to the provider. Defaults to `model`.
- `metadata_model`: Models.dev model id to use for metadata, if different from `model`.
- `default`: marks the default model for that connection.
- `recommended`: floats the model in picker/UI recommendations.
- `transport`: per-model transport override.
- `endpoint_path`: per-model endpoint path override.
- `context_window`: token context window override.
- `output_limit`: max output token metadata.
- `token_counter`: per-model token counter override.
- `max_tokens`: request parameter preview and model request cap where supported.
- `pricing`: token prices as micro-USD per million tokens. Use
  `input_micros_per_million` and `output_micros_per_million`, with optional
  `cache_read_micros_per_million` and `cache_write_micros_per_million`.
- `reasoning_options`: optional Models.dev-shaped reasoning override (`toggle`, `effort`, or `budget_tokens` with optional `min`, `max`, and `default`). Omit it to inherit Models.dev metadata.
- `features`: explicit feature list, such as `tool-call`, `reasoning`,
  `structured-output`, `temperature`, `attachment`.
- `pinned`: keeps every catalog metadata value authoritative across refreshes.
- `pinned_fields`: keeps only selected values authoritative. Valid entries are
  `display-name`, `context-window`, `output-limit`, `pricing`, `features`, and
  `reasoning`.

If a field is omitted in a user target that overrides a built-in target, Bonsai keeps
the built-in value. For local/offline models, define `reasoning_options` directly
so Bonsai does not need Models.dev to know the model's reasoning controls.

## Models.dev Metadata

Bonsai fetches Models.dev metadata into `$BONSAI_HOME/cache/models-dev.json`.
That metadata supplies context windows, pricing, display names, and capabilities when
the catalog target does not define them directly.

For unpinned fields, fresh provider metadata wins, then Models.dev, then the TOML value
as an offline fallback. `pinned = true` or `pinned_fields` deliberately moves the
selected catalog value ahead of both refreshed sources.

Use `metadata_model` when your Bonsai model id is not the same as the Models.dev id:

```toml
[[targets]]
connection = "opencode-zen"
model = "opencode-zen/grok-code"
metadata_model = "opencode/grok-code"
remote_model = "grok-code"
```

Explicit pricing overrides use an inline table:

```toml
pricing = { input_micros_per_million = 3000000, output_micros_per_million = 15000000, cache_read_micros_per_million = 300000 }
```

Environment controls:

- `BONSAI_MODELS_DEV_URL`: fetch a different Models.dev-compatible JSON URL.
- `BONSAI_MODELS_DEV_PATH`: read metadata from a local JSON file.
- `BONSAI_DISABLE_MODELS_FETCH=1`: skip network refresh and use the cached file.

## Built-In Connections

Built-ins are defined in `models/builtin/` and are loaded before user files:

- `opencode`: OpenCode Go at `https://opencode.ai/zen/go/v1`.
- `opencode-zen`: broader OpenCode Zen catalog at `https://opencode.ai/zen/v1`.
- `openai`: official OpenAI API at `https://api.openai.com/v1`.
- `openrouter`: OpenRouter at `https://openrouter.ai/api/v1`.
- `openai-compatible`: generic OpenAI-compatible endpoint.
- `anthropic-compatible`: generic Anthropic-compatible endpoint.
- `codex`: Codex session/cache based provider.
- `anthropic`: Anthropic API.
- `minimax`: MiniMax pay-as-you-go API.
- `minimax-coding-plan`: MiniMax Coding Plan Anthropic-compatible endpoint.
- `zai`: Z.AI pay-as-you-go API.
- `zai-coding-plan`: Z.AI Coding Plan endpoint with separate credentials.
- `moonshotai`: Moonshot AI pay-as-you-go API.
- `kimi-coding-plan`: Kimi Coding Plan endpoint with separate credentials.

User files can add new connections and targets, or patch built-in entries by using the
same `id` or the same `(connection, model)` pair.
