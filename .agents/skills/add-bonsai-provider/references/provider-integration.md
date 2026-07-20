# Provider Integration Checklist

Use this reference after the skill triggers and before touching provider code.

## Research Checklist

Verify these items against official docs or user-supplied docs:

- Provider id and display name suitable for `/provider`.
- Current recommended default coding model and fallback seed models.
- Base API URL and endpoint path for chat/messages/responses, including model-family endpoint differences.
- Auth header format and API-key env var naming.
- Live model-listing endpoint, response schema, auth requirements, and fallback behavior.
- Streaming format: SSE frame shape, event names, done marker, error frames, and usage chunks.
- Prompt caching: documented mechanism (Anthropic `cache_control` breakpoints vs an OpenAI-family `prompt_cache_key` / automatic prefix cache), supported models, any minimum cacheable prefix, and **which streaming event carries the cache/usage breakdown** (`message_start` vs `message_delta`; `prompt_tokens_details.cached_tokens` for chat-completions). Caching is gateway/vendor-specific — verify on the wire, not from the model name.
- Tool calling support: tool schema, streamed tool-call deltas, tool-result format, and limitations.
- Reasoning controls: whether effort/variants exist, accepted values, model-specific support, and defaults. Record support only when documented or present in API metadata.
- Parameter previews Bonsai should show, such as `max_tokens`.

Prefer official provider documentation. If the provider has changing model names, browse immediately before implementation.

## Code Touchpoints

Provider module:

- Add or update `src/provider/<id>.rs`.
- Define a `ProviderMetadata` constant with id, display name, default model, base URL, env vars, seed models, provider default protocol/path/capabilities, and model-specific descriptors.
- Define a factory implementing `ProviderFactory`.
- For API-key providers, use `auth::authorize_api_key`, `auth::api_key_is_authorized`, and `auth::clear_api_key`.
- For live model listing, parse the documented schema into sorted model ids when order is not meaningful; fall back to `metadata().seed_model_list()` on empty/unavailable responses.
- Prefer implementing `ProviderFactory::list_model_catalog` when live model discovery can attach documented/API-provided descriptors. Preserve raw model ids as availability, and keep capability inference separate from availability.

Provider registration:

- `src/provider/mod.rs`: add `mod <id>;`, `pub use <id>::<Factory>;`, and an `ALL_METADATA` entry in the same order as the registry.
- `src/provider/registry.rs`: import the factory and add it to `default_registry()`.
- `src/main.rs`: update help text and provider-id drift tests if they enumerate built-ins.
- `.env.example` or config docs: add API-key/model/base-url env vars when the provider exposes them.

Shared protocol changes:

- `src/provider/metadata.rs`: add only reusable metadata concepts, not one-off provider hacks.
- `src/provider/mod.rs::provider_for`: resolve the selected model's effective descriptor before mapping protocol to one shared provider implementation.
- `src/provider/transform.rs`: keep message/tool transforms shared by protocol.
- `src/provider/sse.rs`: reuse `send_cancellable`, `error_from_response`, and `drive_sse`; extend shared parsing only when needed.

Prompt caching (only when the backend caches):

- Capability gate: hand-written metadata calls `ProviderCapabilities::with_prompt_cache()`; a catalog `[[connections]]` entry sets `prompt_cache = true`, wired to the capability in `src/provider/registry.rs::build_provider_metadata`. The toml field lives on `ConnectionSpec`/`ConnectionSpecPatch` in `src/model_catalog/spec.rs`. Never gate on the model/provider name.
- `src/provider/mod.rs::provider_for`: the openai-chat path threads `supports_prompt_cache` into the provider so it conditionally emits `prompt_cache_key`; the codex path always emits it. `new_conversation_cache_key()` (same file) mints the conversation-stable key.
- `src/provider/anthropic.rs`: `cache_control` breakpoints in `request_body` (`system_field` head/tail split, `mark_last_tool_cacheable`, `mark_last_message_cacheable`); the stream handler reads usage from both `message_start` and `message_delta`.
- `src/provider/usage.rs`: chat/responses usage → `TokenUsage.input_cache`; accounting accumulates into `SessionUsage` (`src/agent/state_types.rs`) and persists to the `cache_read/creation/measured_input_token_count` DB columns.

## Existing Provider Update Rules

When the provider already exists:

- Update the existing module instead of creating a second provider id.
- Preserve provider id, env var names, and session compatibility unless the user explicitly approves a breaking rename.
- Add legacy model normalization only when an old default is known to be retired.
- Update seed models, default model, live listing parser, model descriptors, effort support, and tests from current docs.
- Remove obsolete model capabilities only when docs clearly show they no longer apply.

## Tests

Add or update inline tests near the changed module:

- Metadata values: id, default model, env vars, protocol, seed models, capabilities.
- Auth behavior: env, pasted key, unsupported auth variants, clear/is-authorized.
- Model listing: live endpoint success, empty response fallback, error fallback, parse failure context where applicable.
- Registry drift: provider appears in `ProviderRegistry::default_registry()` and `ALL_METADATA` in the same order.
- Capability normalization: unsupported effort becomes `Auto`; supported model-specific efforts survive.
- Descriptor behavior: provider default fallback, static model descriptor override, live descriptor precedence, mixed protocol/path routing when applicable, and old sessions with only `cached_models`.
- Prompt caching (when enabled): `request_body` carries the cache mechanism (`cache_control` breakpoints or a stable `prompt_cache_key`); the openai-chat key is gated on `supports_prompt_cache` and stable across turns of one provider instance; the streamed usage parser captures cache tokens from whichever event the vendor uses (add a `message_delta`-usage case if the vendor reports there). Mirror the live wire bytes in the `wiremock`/SSE fixture.

Do not add live network tests. Use `wiremock` for HTTP behavior.
