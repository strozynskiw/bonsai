---
name: add-bonsai-provider
description: Use when adding or updating a built-in AI provider integration in the Bonsai Rust TUI agent, including researching official provider docs, listing models, deciding protocol support, modeling reasoning effort or variants, wiring auth/model metadata/registry entries, and making the provider available through /provider, /authorize, and /models. If the provider already exists, use this skill to update the existing implementation and tests instead of creating a duplicate integration.
---

# Add Bonsai Provider

## Overview

Add or update a Bonsai provider from docs to working integration. Keep changes aligned with the existing provider metadata, registry, session normalization, SSE, auth, and test patterns.

## Workflow

1. Read `AGENTS.md`, `ROADMAP.md`, and the current provider files before editing. Use the repository's Rust skill/style rules when touching Rust code.
2. Read [references/provider-integration.md](references/provider-integration.md) for the implementation checklist and file touchpoints.
3. Research only authoritative provider docs for current models, API protocol, auth, model-listing endpoint, streaming shape, tool-call support, usage reporting, prompt caching, and reasoning/effort controls. Browse because this information changes. When live `/models` output exists, preserve raw model ids as availability and do not infer capabilities from names unless official docs describe the model family.
4. Search the codebase for the provider name, id, env vars, model slugs, and base URL. If any integration already exists, update it in place and preserve compatibility unless the user explicitly wants a replacement.
5. Choose the narrowest implementation path:
   - Existing OpenAI-compatible chat protocol: add metadata and a factory that builds through `provider_for`.
   - Existing Anthropic Messages-compatible protocol: add metadata and a factory that builds through `provider_for`.
   - Existing Codex Responses protocol: reuse only when the provider is genuinely Codex-compatible.
   - New protocol: add a protocol variant and shared provider implementation; reuse `provider/sse.rs` and avoid per-provider SSE parsers.
6. Wire the provider into all user-visible registries and tests. Update env-var help/docs when the provider has API-key env vars.
7. Run formatting and focused tests. Run the full CI-equivalent command set when the provider implementation or shared protocol code changed.

## Provider Facts

Collect these facts before writing Rust:

- Canonical provider id, display name, default model, base URL, and env var names.
- Wire protocol: `OpenAiChat`, `AnthropicMessages`, `CodexResponses`, or new protocol.
- Auth method: API key from env/paste, imported local session, or another explicit flow.
- Seed model list and optional live model-listing endpoint.
- Model descriptors: matcher/model id, protocol override, endpoint path override, supported `RunEffort` values, parameter previews, context/usage caveats.
- Request differences per model or model family: headers, endpoint paths, max tokens, tool schema, tool-result schema, streaming event names, and token usage chunks.
- Prompt caching: whether the backend caches, the documented mechanism (Anthropic `cache_control` breakpoints vs an OpenAI-family `prompt_cache_key`), any minimum cacheable prefix (OpenAI ~1024 tokens), and **which streaming event carries the cache/usage breakdown** — `message_start` vs `message_delta` for Anthropic-style, `prompt_tokens_details.cached_tokens` (chat) / `input_tokens_details.cached_tokens` (responses) for OpenAI-style, which needs `stream_options.include_usage`.

Default to conservative metadata. Do not expose effort controls unless official docs or API metadata confirm the provider accepts them for the selected protocol/model family. Do not use active test-prompt probes to discover capability support.

## Implementation Rules

- Prefer adding a `src/provider/<id>.rs` file containing provider metadata, a factory, model-listing helpers, and local tests.
- Add the module, public factory export, and `ALL_METADATA` entry in `src/provider/mod.rs`.
- Add the factory to `ProviderRegistry::default_registry()` in `src/provider/registry.rs`; keep order stable and update drift-guard tests.
- Avoid `session.rs` edits unless metadata cannot represent the provider. Session defaults and env-var normalization are metadata-driven.
- Use `auth::authorize_api_key` for normal API-key providers. Reject unsupported `AuthInput` variants explicitly.
- For live model listing, return documented seed models when the endpoint is unavailable or empty. Unit-test success and fallback with `wiremock`.
- Prefer `ProviderFactory::list_model_catalog` for live discovery. Preserve raw model ids as `cached_models`, and store descriptor data separately in `cached_model_descriptors` only when docs/API metadata support it.
- Add static `ModelDescriptor`s for documented model families that need protocol/path/capability overrides; unknown live models should fall back to provider defaults with unsupported efforts disabled unless proven otherwise.
- Reuse shared transforms and `provider/sse.rs`. Preserve typed `ProviderError` details and retry behavior.
- Prompt caching is a capability, never a name-check (a model-name `is_claude` gate silently disables caching for compatible non-Claude vendors). Gate on `ProviderCapabilities::supports_prompt_cache`: a hand-written metadata calls `.with_prompt_cache()`; a catalog `[[connections]]` entry sets `prompt_cache = true` (mapped to the capability in `registry.rs::build_provider_metadata`). The transport then does the rest: `anthropic-messages` emits up to four `cache_control: {"type":"ephemeral"}` breakpoints (stable system prefix split at `VOLATILE_STATE_HEADING`, last tool, last message); `openai-chat`/`codex-responses` emit a conversation-stable `prompt_cache_key` (`provider::new_conversation_cache_key()`, minted once per provider instance). Cache/usage tokens flow `provider/usage.rs` → `TokenUsage.input_cache` → `SessionUsage` (DB `cache_*` columns) → the `/ctx` cache %. The Anthropic usage parser reads usage from **both** `message_start` and `message_delta`, because some compatible vendors (MiniMax) report the input + `cache_read/creation_input_tokens` only on `message_delta`.
- Keep tests inline near the code they cover. Update existing tests that assert provider ids, counts, or registration order.

## Validation

Minimum validation after provider edits:

- `cargo fmt --all`
- Focused provider tests, such as `cargo test --locked provider::<module_name>`
- `cargo test --locked`

Run these before finalizing shared protocol or large provider changes:

- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo build --release --locked` when request/streaming code changed

If the provider claims prompt caching, validate it **live** — not from the request shape alone (the request can be correct while the response-usage parser silently drops cache tokens). Hit the endpoint twice with an identical ≥1k-token prefix (raw `curl` cold+warm, inspecting the actual usage frames), or drive the TUI via the `verifier-tui` skill, and confirm a warm `cache_read`/`cached_tokens` returns and `/ctx` shows a rising cache % with the DB `cache_*` columns tracking `prompt_token_count`.

If network access or docs are unavailable, state the missing verification and implement only from local evidence or user-supplied docs.
