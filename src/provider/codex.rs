use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use async_openai::types::chat::{ChatCompletionRequestMessage, ChatCompletionTool};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::model_catalog::{AvailableModel, LiveModelAvailability, ModelFeature, RunTarget};
use crate::output::SharedSink;
use crate::provider::reasoning::ReasoningCodec;
use crate::provider::transform::{self, ContentPart};
use crate::provider::{
    AuthInput, AuthRequirement, AuthorizeOutcome, CODEX_REASONING, NO_PARAMETERS, Protocol,
    Provider, ProviderCapabilities, ProviderFactory, ProviderMetadata, ProviderRequestDiagnostics,
    ProviderRequestPreview, ReasoningSelection, StreamedResponse, TokenCounterKind, WireField, sse,
    streaming, tool_calls, usage, wire_sections_from_body,
};
use crate::session::ProviderSession;
use crate::util::tool_args::normalize_tool_call_arguments_json;

/// Floor for the spoofed `version` header, not just an absence fallback: the
/// backend version-gates both listing and routing of newly served models. A
/// detected CLI older than this floor must not shrink `/models` or make a
/// listed Responses Lite model fail at request time (verified live: 0.158.0
/// hides GPT-6.1 Sol, while 0.159.0 lists it).
const CODEX_FALLBACK_CLIENT_VERSION: &str = "0.159.0";
const CODEX_CLIENT_VERSION_TIMEOUT: Duration = Duration::from_secs(2);
const CODEX_MODELS_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);
const CODEX_FALLBACK_MODELS: [&str; 8] = [
    "gpt-6.1-sol",
    "gpt-6-astra",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
];
const CODEX_KEYRING_SERVICE: &str = "Codex Auth";
static CODEX_CLIENT_VERSION: OnceCell<String> = OnceCell::const_new();

pub static CODEX_METADATA: LazyLock<ProviderMetadata> = LazyLock::new(|| {
    ProviderMetadata::new(
        "codex",
        "Codex",
        "gpt-5.6-sol",
        "https://chatgpt.com/backend-api/codex",
        None,
        Some("CODEX_MODEL"),
        Some("CODEX_BASE_URL"),
        &CODEX_FALLBACK_MODELS,
        Protocol::CodexResponses,
        ProviderCapabilities::new(CODEX_REASONING, NO_PARAMETERS)
            .with_prompt_cache()
            .with_vision(),
        "responses",
    )
    .with_auth_requirement(AuthRequirement::CodexCache)
    // Fallback only (used when catalog resolution misses). Keep this aligned
    // with the current Codex working window published by the live catalog.
    .with_context_window(272_000)
    .with_token_counter(TokenCounterKind::Tiktoken)
});

pub struct CodexFactory;

#[async_trait]
impl ProviderFactory for CodexFactory {
    fn metadata(&self) -> &ProviderMetadata {
        &CODEX_METADATA
    }

    async fn authorize(&self, input: AuthInput) -> Result<AuthorizeOutcome> {
        let outcome = match input {
            AuthInput::FromCodexCache => codex_cached_authorization().await?,
            AuthInput::ApiKey { .. } => {
                anyhow::bail!(
                    "Codex uses `codex login` instead of an API key. Run `codex login` then /authorize codex."
                )
            }
            AuthInput::FromEnv => {
                anyhow::bail!(
                    "Codex does not read from an env var; use `codex login` then /authorize codex."
                )
            }
            AuthInput::OpenAiCompatible { .. } => {
                anyhow::bail!(
                    "Codex does not support OpenAI-compatible endpoint setup. Run `codex login` then /authorize codex."
                )
            }
        };
        outcome.context(
            "Codex authorization requires a current Codex CLI login in CODEX_HOME (auth.json or the OS credential store). Run `codex login` externally, then retry /authorize codex.",
        )
    }

    fn is_authorized(&self, session: &ProviderSession) -> bool {
        crate::provider::auth::is_authorized(self.metadata(), session)
    }

    fn clear_authorization(&self, session: &mut ProviderSession) {
        session.api_key.clear();
        session.account_id.clear();
        session.is_fedramp_account = false;
    }

    async fn list_models(&self, session: &ProviderSession) -> Result<Vec<String>> {
        let target = crate::provider::fallback_run_target(self.metadata(), session);
        let provider = CodexProvider::new(session, &target);
        provider.list_models().await
    }

    async fn list_available_models(
        &self,
        session: &ProviderSession,
    ) -> Result<LiveModelAvailability> {
        let target = crate::provider::fallback_run_target(self.metadata(), session);
        CodexProvider::new(session, &target)
            .fetch_model_availability()
            .await
    }
}

pub struct CodexProvider {
    http: reqwest::Client,
    model: String,
    base_url: String,
    endpoint_path: String,
    access_token: String,
    account_id: String,
    is_fedramp_account: bool,
    reasoning: ReasoningSelection,
    reasoning_escalation: Option<ReasoningSelection>,
    use_responses_lite: bool,
    /// Conversation-stable `prompt_cache_key` so the Responses backend keeps a
    /// warm prefix routed to the same machine across interactive turns. Sent
    /// suffixed with a fingerprint of the turn's instructions
    /// ([`crate::provider::lane_scoped_cache_key`]) so the plan-mode and
    /// coding-mode lanes of one conversation don't collide on one cache route.
    prompt_cache_key: String,
    /// UUID-shaped conversation identity used by the official Codex routing
    /// headers. Kept separate so legacy sessions retain their original body
    /// `prompt_cache_key` while receiving standards-compliant headers.
    routing_thread_id: String,
    /// When enabled (`BONSAI_CODEX_REASONING_PERSIST`), the provider requests
    /// `reasoning.encrypted_content` and threads each turn's reasoning items
    /// back into the next request so the model does not re-reason from scratch.
    /// Off by default until validated against the live backend.
    persist_reasoning: bool,
    /// Whether the active model accepts image input. When false, image parts
    /// are downgraded to a text placeholder before serialization so an image
    /// already in history cannot 400 every later turn.
    supports_vision: bool,
    native_replay: crate::provider::NativeReplayStore,
    native_replay_scope: String,
    last_request_diagnostics: Mutex<Option<ProviderRequestDiagnostics>>,
    tool_call_identities: crate::provider::ToolCallIdentityStore,
    tool_call_scope: String,
}

impl CodexProvider {
    pub fn new(session: &ProviderSession, target: &RunTarget) -> Self {
        let prompt_cache_key = crate::provider::new_conversation_cache_key();
        let routing_thread_id = crate::provider::codex_routing_thread_id(&prompt_cache_key);
        let base_url = target.base_url.trim().trim_end_matches('/').to_string();
        let endpoint_path = target
            .endpoint_path
            .as_deref()
            .unwrap_or("responses")
            .to_string();
        let tool_call_scope = crate::provider::tool_call_scope(
            "codex-responses",
            target.connection_id.as_str(),
            &base_url,
            &endpoint_path,
        );
        let native_replay_scope = crate::provider::tool_call_scope(
            "codex-responses-native",
            &tool_call_scope,
            &target.remote_model_id,
            if target.use_responses_lite {
                "lite"
            } else {
                "responses"
            },
        );
        Self {
            http: crate::provider::http_client(),
            model: target.remote_model_id.to_string(),
            base_url,
            endpoint_path,
            access_token: session.api_key.clone(),
            account_id: session.account_id.clone(),
            is_fedramp_account: session.is_fedramp_account,
            reasoning: target.reasoning,
            reasoning_escalation: target.reasoning_escalation,
            use_responses_lite: target.use_responses_lite,
            prompt_cache_key,
            routing_thread_id,
            persist_reasoning: codex_reasoning_persistence_enabled(),
            supports_vision: target.supports_vision,
            native_replay: crate::provider::NativeReplayStore::default(),
            native_replay_scope,
            last_request_diagnostics: Mutex::new(None),
            tool_call_identities: crate::provider::ToolCallIdentityStore::default(),
            tool_call_scope,
        }
    }

    fn ensure_authorized(&self) -> Result<()> {
        if self.access_token.trim().is_empty() {
            anyhow::bail!("Codex is not authorized. Run /authorize codex.");
        }
        if self.account_id.trim().is_empty() {
            anyhow::bail!("Codex account ID is missing. Run `codex login`, then /authorize codex.");
        }
        Ok(())
    }

    fn responses_url(&self) -> String {
        transform::endpoint_url(&self.base_url, &self.endpoint_path)
    }

    fn models_url(&self, client_version: &str) -> String {
        format!(
            "{}?client_version={}",
            transform::endpoint_url(&self.base_url, "models"),
            client_version
        )
    }

    fn authorized_request(
        &self,
        builder: reqwest::RequestBuilder,
        client_version: &str,
    ) -> reqwest::RequestBuilder {
        let builder = builder
            .bearer_auth(&self.access_token)
            .header("ChatGPT-Account-ID", &self.account_id)
            .header("OAI-Product-Sku", "codex")
            .header("version", client_version)
            // Deliberate, don't drop: the backend fingerprints official
            // clients via `originator` and refuses lite-served models
            // (gpt-5.6-*) without it — the same request answers 404 "Model
            // not found" absent and 200 present (verified live). Mirrors
            // codex-rs DEFAULT_ORIGINATOR.
            .header("originator", "codex_cli_rs");

        if self.is_fedramp_account {
            builder.header("X-OpenAI-Fedramp", "true")
        } else {
            builder
        }
    }

    #[cfg(test)]
    fn reasoning_payload(&self) -> Option<Value> {
        self.reasoning_payload_for(self.reasoning)
    }

    fn reasoning_payload_for(&self, reasoning: ReasoningSelection) -> Option<Value> {
        ReasoningCodec::CodexResponses.encode_json(reasoning)
    }

    #[cfg(test)]
    fn request_body(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
    ) -> Result<Value> {
        self.request_body_with_reasoning(messages, tools, self.reasoning)
    }

    #[cfg(test)]
    fn request_body_with_reasoning(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        reasoning: ReasoningSelection,
    ) -> Result<Value> {
        self.request_body_with_context(messages, tools, reasoning, &Default::default())
    }

    fn request_body_with_context(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        reasoning: ReasoningSelection,
        context: &crate::provider::ProviderRequestContext,
    ) -> Result<Value> {
        // Match the official Codex client for the ChatGPT backend: send one
        // stable `prompt_cache_key` and keep the reusable prefix byte-stable.
        // This backend rejects the public Responses API's
        // `prompt_cache_options` and explicit content breakpoints.
        let contiguous = transform::restore_tool_result_contiguity(messages);
        let wire_messages = transform::messages_for_project_state_layout(
            contiguous.as_ref(),
            transform::ProjectStateWireLayout::AppendOnly,
        );
        // Image safety net: mirror of the OpenAI-chat/Anthropic sanitize — an
        // image already in history must not 400 every later turn, whether the
        // active model rejects image input or the image's media type is
        // unsupported everywhere (e.g. `image/svg+xml`).
        let wire_messages =
            transform::sanitize_image_parts_for_wire(wire_messages.as_ref(), self.supports_vision);
        // IMPORTANT OFFICIAL-CODEX PARITY: emit the native Responses items
        // exactly. A synthetic developer "cache checkpoint" was live-tested
        // and pinned reuse to only the original ~14k prefix instead of letting
        // history grow; the official client sends no such item. Do not add
        // cache-control prose to the model input.
        let (instructions, input) = messages_to_responses_input_with_context(
            wire_messages.as_ref(),
            &self.native_replay,
            &self.native_replay_scope,
            context,
            self.persist_reasoning,
        )?;
        let prompt_cache_key =
            crate::provider::lane_scoped_cache_key(&self.prompt_cache_key, &instructions);
        let include: Value = if self.persist_reasoning {
            json!(["reasoning.encrypted_content"])
        } else {
            json!([])
        };
        let response_tools = tools_to_responses_tools(tools)?;
        let mut body = if self.use_responses_lite {
            let mut lite_input = Vec::with_capacity(input.len().saturating_add(2));
            lite_input.push(json!({
                "type": "additional_tools",
                "role": "developer",
                "tools": response_tools,
            }));
            if !instructions.is_empty() {
                lite_input.push(json!({
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": instructions}],
                }));
            }
            lite_input.extend(input);
            json!({
                "model": self.model,
                "input": lite_input,
                "tool_choice": "auto",
                "parallel_tool_calls": false,
                "store": false,
                "stream": true,
                "include": include,
                "prompt_cache_key": prompt_cache_key,
            })
        } else {
            json!({
                "model": self.model,
                "instructions": instructions,
                "input": input,
                "tools": response_tools,
                "tool_choice": "auto",
                "parallel_tool_calls": true,
                "store": false,
                "stream": true,
                "include": include,
                "prompt_cache_key": prompt_cache_key,
            })
        };
        if let Some(reasoning) = self.reasoning_payload_for(reasoning) {
            body["reasoning"] = reasoning;
            if self.use_responses_lite {
                body["reasoning"]["context"] = json!("all_turns");
            }
        } else if self.use_responses_lite {
            // Lite validation rejects a request without `reasoning.context`
            // ("X-OpenAI-Internal-Codex-Responses-Lite requires
            // `reasoning.context` to be `all_turns`"), even for an explicit
            // reasoning-off selection, so the envelope must always ride along.
            body["reasoning"] = json!({ "context": "all_turns" });
        }
        let identity_ids = self
            .tool_call_identities
            .project_request(&self.tool_call_scope, &mut body);
        self.tool_call_identities
            .project_reuse_references(&mut body, &identity_ids);
        Ok(body)
    }

    fn request_preview_from_body(&self, body: Value) -> ProviderRequestPreview {
        let mut sections = wire_sections_from_body(
            &body,
            &[
                WireField {
                    id: "wire-instructions",
                    label: "Instructions",
                    key: "instructions",
                },
                WireField {
                    id: "wire-input",
                    label: "Input",
                    key: "input",
                },
                WireField {
                    id: "wire-tools",
                    label: "Tools",
                    key: "tools",
                },
            ],
            true,
        );
        // OpenAI caches server-side (keyed by prompt_cache_key) with no wire
        // markers; the byte-stable instructions head is the only section the
        // actual body can annotate truthfully.
        for section in &mut sections {
            if section.id == "wire-instructions" {
                section.cache = Some(crate::provider::WireCacheHint::CachedPrefix);
            }
        }
        ProviderRequestPreview::with_wire_sections("POST", self.responses_url(), body, sections)
    }

    async fn fetch_model_availability(&self) -> Result<LiveModelAvailability> {
        self.ensure_authorized()?;
        let client_version = codex_client_version().await;

        let response = timeout(
            CODEX_MODELS_REFRESH_TIMEOUT,
            self.authorized_request(
                self.http.get(self.models_url(client_version)),
                client_version,
            )
            .header("Accept", "application/json")
            .send(),
        )
        .await
        .context("Timed out listing Codex models")?
        .context("Failed to list Codex models")?;

        if !response.status().is_success() {
            return Err(sse::error_from_response(response).await.into());
        }

        let value: Value = response
            .json()
            .await
            .context("Failed to parse Codex models")?;
        let models = models_from_response(&value);
        if models.is_empty() {
            return Ok(LiveModelAvailability::from_remote_ids(
                codex_fallback_models(),
            ));
        }
        Ok(LiveModelAvailability {
            models,
            ..LiveModelAvailability::default()
        })
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn prepare_request_for_estimation(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        options: crate::provider::ProviderRequestOptions,
        context: &crate::provider::ProviderRequestContext,
    ) -> crate::provider::ProviderResult<crate::provider::PreparedProviderRequest> {
        let reasoning = self.reasoning_for_request(messages, options, context)?;
        let body = self
            .request_body_with_context(messages, tools, reasoning, context)
            .map_err(|error| {
                crate::provider::ProviderFailure::configuration(format!("{error:#}"))
            })?;
        Ok(crate::provider::PreparedProviderRequest {
            protocol: Some(Protocol::CodexResponses),
            body,
            reasoning,
            native_budget: if self.persist_reasoning {
                self.native_replay.selected_budgets(
                    &self.native_replay_scope,
                    context,
                    Protocol::CodexResponses,
                )
            } else {
                Vec::new()
            },
            count_endpoint: None,
            count_auth_header: None,
        })
    }
    fn set_native_replay_store(&mut self, store: crate::provider::NativeReplayStore) {
        self.native_replay = store;
    }
    fn set_tool_call_identity_store(&mut self, store: crate::provider::ToolCallIdentityStore) {
        self.tool_call_identities = store;
    }

    fn project_state_cache_strategy(&self) -> crate::provider::ProjectStateCacheStrategy {
        // IMPORTANT: Codex automatic caching records the complete latest input.
        // Do not fall back to the mutable system-tail strategy: rewriting or
        // relocating state caused sessions to miss most warm prefixes.
        crate::provider::ProjectStateCacheStrategy::AppendOnlyHistory
    }

    fn set_conversation_cache_key(&mut self, key: &str) {
        if !key.trim().is_empty() {
            self.prompt_cache_key = key.to_owned();
            self.routing_thread_id = crate::provider::codex_routing_thread_id(key);
        }
    }

    fn reasoning(&self) -> ReasoningSelection {
        self.reasoning
    }

    fn reasoning_escalation(&self) -> Option<ReasoningSelection> {
        self.reasoning_escalation
    }

    #[cfg(test)]
    fn preview_request(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
    ) -> Result<ProviderRequestPreview> {
        let body = self.request_body(messages, tools)?;
        Ok(self.request_preview_from_body(body))
    }

    #[cfg(test)]
    fn preview_request_with_context(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        context: &crate::provider::ProviderRequestContext,
    ) -> Result<ProviderRequestPreview> {
        let body = self.request_body_with_context(messages, tools, self.reasoning, context)?;
        Ok(self.request_preview_from_body(body))
    }

    fn take_last_request_diagnostics(&self) -> Option<ProviderRequestDiagnostics> {
        ProviderRequestDiagnostics::take(&self.last_request_diagnostics)
    }

    async fn chat_stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        self.chat_stream_with_options(
            messages,
            tools,
            crate::provider::ProviderRequestOptions::default(),
            cancellation_token,
            sink,
        )
        .await
    }

    async fn chat_stream_with_options(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        options: crate::provider::ProviderRequestOptions,
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        self.chat_stream_with_context(
            messages,
            tools,
            options,
            &Default::default(),
            cancellation_token,
            sink,
        )
        .await
    }

    async fn chat_stream_with_context(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        options: crate::provider::ProviderRequestOptions,
        context: &crate::provider::ProviderRequestContext,
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        let serialized_body = crate::provider::serialize_request_body(
            "Codex",
            || self.ensure_authorized(),
            || {
                Ok(self
                    .prepare_request_for_estimation(messages, tools, options, context)?
                    .body)
            },
            |body| self.request_preview_from_body(body),
            &self.last_request_diagnostics,
        )?;
        let client_version = codex_client_version().await;

        let mut builder = self
            .authorized_request(self.http.post(self.responses_url()), client_version)
            .header("Accept", "text/event-stream")
            .header("Content-Type", "application/json")
            // IMPORTANT CODEX CACHE/ROUTING CONTRACT: the official client
            // sends one stable thread identity in all three headers on every
            // Responses request. Keep these aligned with Bonsai's persisted
            // conversation identity; omitting them leaves HTTP turns without
            // the official client's sticky thread route.
            .header("x-client-request-id", &self.routing_thread_id)
            .header("session-id", &self.routing_thread_id)
            .header("thread-id", &self.routing_thread_id);
        if self.use_responses_lite {
            // Deliberate, don't simplify: lite-flagged models (gpt-5.6-*) are
            // routed by this internal header, not just the reshaped body —
            // without it the backend's /responses rejects them with
            // 404 "Model not found" even though /models lists them. Mirrors
            // codex-rs client.rs X_OPENAI_INTERNAL_CODEX_RESPONSES_LITE_HEADER.
            builder = builder.header("x-openai-internal-codex-responses-lite", "true");
        }
        let builder = builder.body(serialized_body);
        let Some(response) = streaming::send_json_stream(builder, &cancellation_token).await?
        else {
            return Ok(StreamedResponse::interrupted());
        };

        let parsed = parse_responses_stream(
            response,
            cancellation_token,
            sink,
            &self.tool_call_identities,
            &self.tool_call_scope,
        )
        .await?;
        if self.persist_reasoning
            && let Some(error) = parsed.replay_error
        {
            return Err(crate::provider::ProviderFailure::configuration(error));
        }
        if self.persist_reasoning
            && let Some(response_id) = &context.response_id
            && let Some(output_items) = parsed.replay_items
        {
            self.native_replay
                .insert(crate::provider::NativeReplayRecord {
                    response_id: response_id.clone(),
                    provider_scope: self.native_replay_scope.clone(),
                    payload: crate::provider::NativeReplayPayload::CodexResponses(
                        crate::provider::CodexNativeReplay {
                            output_items,
                            budget_evidence: Some(crate::provider::NativeBudgetEvidence {
                                output_tokens: parsed.reported_output_tokens,
                                output_ceiling_tokens: None,
                            }),
                        },
                    ),
                })?;
        }
        Ok(parsed.response)
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(self.fetch_model_availability().await?.remote_model_ids())
    }
}

fn tools_to_responses_tools(tools: &[ChatCompletionTool]) -> Result<Vec<Value>> {
    tools
        .iter()
        .map(|tool| {
            let func = transform::tool_function(tool)?;
            Ok(json!({
                "type": "function",
                "name": func.name,
                "description": func.description,
                "strict": func.strict,
                "parameters": func.parameters,
            }))
        })
        .collect()
}

/// Whether to request and thread back Codex reasoning items. Off unless
/// `BONSAI_CODEX_REASONING_PERSIST` is `1`/`true`. Experimental until validated
/// against the live chatgpt.com backend (item ordering + encrypted-content
/// round-trip), so it never changes the wire shape by default.
fn codex_reasoning_persistence_enabled() -> bool {
    std::env::var("BONSAI_CODEX_REASONING_PERSIST")
        .map(|value| {
            let value = value.trim();
            value == "1" || value.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

/// Input shaping is separate from lossless capture. Retain the existing Codex
/// reasoning policy under `store:false`, without dropping content or phases.
fn codex_input_replay_item(mut item: Value) -> Option<Value> {
    if !codex_replay_item_supported(&item) {
        return None;
    }
    if item["type"] == "reasoning" {
        item.as_object_mut()?.remove("status");
        if item.get("summary").is_none() {
            item["summary"] = json!([]);
        }
    }
    Some(item)
}

fn codex_replay_item_supported(item: &Value) -> bool {
    match item["type"].as_str() {
        Some("reasoning") => item["id"].is_string() && item["encrypted_content"].is_string(),
        Some("message") => item["role"] == "assistant" && item["content"].is_array(),
        Some("function_call") => {
            item["call_id"].is_string() && item["name"].is_string() && item["arguments"].is_string()
        }
        _ => false,
    }
}

#[cfg(test)]
fn messages_to_responses_input(
    messages: &[ChatCompletionRequestMessage],
) -> Result<(String, Vec<Value>)> {
    messages_to_responses_input_with_context(
        messages,
        &crate::provider::NativeReplayStore::default(),
        "",
        &Default::default(),
        false,
    )
}

fn messages_to_responses_input_with_context(
    messages: &[ChatCompletionRequestMessage],
    replay: &crate::provider::NativeReplayStore,
    scope: &str,
    context: &crate::provider::ProviderRequestContext,
    enabled: bool,
) -> Result<(String, Vec<Value>)> {
    context.validate_assistant_count(
        messages
            .iter()
            .filter(|message| matches!(message, ChatCompletionRequestMessage::Assistant(_)))
            .count(),
    )?;
    let mut response_ids = context
        .assistant_response_ids
        .as_deref()
        .unwrap_or(&[])
        .iter();
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    let mut first_instruction = true;

    for message in messages {
        let value = serde_json::to_value(message).context("Failed to serialize message")?;
        let role = value
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match role {
            "system" | "developer" => {
                if let Some(text) = transform::content_to_text(value.get("content")) {
                    if first_instruction {
                        // IMPORTANT CACHE INVARIANT: the agent has already kept
                        // volatile project state out of this stable instruction
                        // block and emitted it as append-only named user input.
                        // Do not split and re-append anything here: relocating a
                        // snapshot destroys the previous latest-message cache
                        // breakpoint on the following tool round-trip.
                        instructions.push(text);
                    } else {
                        // A later system/developer message (skill load, compaction
                        // summary, smol note). Concatenating these into
                        // `instructions` would rewrite the cached prefix from ~3k
                        // tokens on; emit them as an append-only developer input
                        // item at their historical position so the prefix stays
                        // byte-stable and the prompt cache stays warm.
                        input.push(json!({
                            "type": "message",
                            "role": "developer",
                            "content": [{"type": "input_text", "text": text}],
                        }));
                    }
                    first_instruction = false;
                }
            }
            "user" => {
                input.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": content_to_input_items(value.get("content")),
                }));
            }
            "assistant" => {
                let response_id = response_ids.next().and_then(Option::as_ref);
                if enabled
                    && let Some(crate::provider::NativeReplayPayload::CodexResponses(native)) =
                        response_id.and_then(|id| replay.payload_for(scope, id))
                {
                    let items = native
                        .output_items
                        .into_iter()
                        .map(codex_input_replay_item)
                        .collect::<Option<Vec<_>>>()
                        .context("Codex native replay contains an unreconstructable output item")?;
                    input.extend(items);
                    continue;
                }
                let tool_calls = value.get("tool_calls").and_then(Value::as_array);

                if let Some(text) = transform::content_to_text(value.get("content"))
                    && !text.is_empty()
                {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }

                if let Some(tool_calls) = tool_calls {
                    for tool_call in tool_calls {
                        let (call_id, name, arguments) = transform::tool_call_parts(tool_call);
                        let arguments = normalize_tool_call_arguments_json(arguments);

                        input.push(json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments,
                        }));
                    }
                }
            }
            "tool" => {
                let call_id = value
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let output = transform::content_to_text(value.get("content")).unwrap_or_default();
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                }));
            }
            _ => {}
        }
    }

    Ok((instructions.join("\n\n"), input))
}

fn content_to_input_items(content: Option<&Value>) -> Vec<Value> {
    transform::content_parts(content)
        .into_iter()
        .map(|part| match part {
            ContentPart::Text(text) => json!({"type": "input_text", "text": text}),
            ContentPart::ImageUrl(image_url) => {
                json!({"type": "input_image", "image_url": image_url})
            }
        })
        .collect()
}

#[derive(Debug)]
struct ParsedResponsesStream {
    response: StreamedResponse,
    replay_items: Option<Vec<Value>>,
    replay_error: Option<String>,
    reported_output_tokens: Option<u64>,
}

/// Completed indexed items and the terminal output describe one sequence.
/// Final output may enrich an item, but cannot contradict its completed data.
fn complete_responses_output(
    mut indexed: BTreeMap<usize, Value>,
    terminal_output: Option<Vec<Value>>,
) -> crate::provider::ProviderResult<Vec<Value>> {
    let Some(mut output) = terminal_output else {
        return Ok(indexed.into_values().collect());
    };
    for (index, completed) in indexed.iter_mut() {
        let Some(final_item) = output.get_mut(*index) else {
            return Err(crate::provider::ProviderFailure::transport(
                "Codex terminal output omitted a completed item",
            ));
        };
        let (Some(source), Some(target)) = (completed.as_object(), final_item.as_object_mut())
        else {
            return Err(crate::provider::ProviderFailure::transport(
                "Codex output item is not an object",
            ));
        };
        for (key, value) in source {
            if let Some(final_value) = target.get(key) {
                if final_value != value {
                    return Err(crate::provider::ProviderFailure::transport(format!(
                        "Codex terminal output contradicts completed item {index} field `{key}`",
                    )));
                }
            } else {
                target.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(output)
}

async fn parse_responses_stream(
    response: reqwest::Response,
    cancellation_token: CancellationToken,
    sink: SharedSink,
    identities: &crate::provider::ToolCallIdentityStore,
    identity_scope: &str,
) -> crate::provider::ProviderResult<ParsedResponsesStream> {
    let mut content = String::new();
    let mut tool_call_arguments: HashMap<String, String> = HashMap::new();
    let mut token_usage: Option<crate::provider::TokenUsage> = None;
    let mut reported_output_tokens = None;
    let mut completed_items: BTreeMap<usize, Value> = BTreeMap::new();
    let mut open_items = BTreeSet::new();
    let mut indexed_order = true;
    let mut terminal_output: Option<Vec<Value>> = None;
    // Whether any reasoning-summary text has streamed yet, so a new summary part
    // renders as a fresh paragraph rather than a leading blank line.
    let mut reasoning_started = false;
    let mut reasoning_chars = 0usize;
    let mut finish_reason: Option<crate::provider::FinishReason> = None;
    let mut terminal_seen = false;
    let mut protocol_completed = false;

    // Codex sends bare `data:` frames (event name is unused); the JSON payload
    // carries its own `type` discriminator.
    let interrupted = sse::drive_sse(response, cancellation_token.clone(), |_event, data| {
        let Some(json) = sse::parse_frame(data)? else {
            return Ok(());
        };

        match json.get("type").and_then(Value::as_str) {
            Some(event_type)
                if event_type.contains("reasoning_summary") && event_type.ends_with(".delta") =>
            {
                if let Some(delta) = json.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    reasoning_started = true;
                    reasoning_chars = reasoning_chars.saturating_add(delta.chars().count());
                    sink.reasoning_delta(delta);
                }
            }
            // A new summary part begins: separate it from the previous one with a
            // blank line so multi-part reasoning renders as paragraphs.
            Some(event_type)
                if event_type.contains("reasoning_summary_part")
                    && event_type.ends_with(".added") =>
            {
                if reasoning_started {
                    sink.reasoning_delta("\n\n");
                }
            }
            Some("response.output_text.delta" | "response.refusal.delta") => {
                if let Some(delta) = json.get("delta").and_then(Value::as_str) {
                    content.push_str(delta);
                    if !delta.is_empty() {
                        sink.assistant_delta(delta);
                    }
                }
            }
            Some("response.output_item.added") => {
                let Some(index) = json.get("output_index").and_then(Value::as_u64) else {
                    return Err(crate::provider::ProviderFailure::transport(
                        "Codex added an output item without output_index",
                    ));
                };
                open_items.insert(index as usize);
            }
            Some("response.function_call_arguments.delta") => {
                if let (Some(item_id), Some(delta)) = (
                    json.get("item_id").and_then(Value::as_str),
                    json.get("delta").and_then(Value::as_str),
                ) {
                    tool_call_arguments
                        .entry(item_id.to_string())
                        .or_default()
                        .push_str(delta);
                }
            }
            Some("response.output_item.done") => {
                if let Some(item) = json.get("item") {
                    let index =
                        if let Some(index) = json.get("output_index").and_then(Value::as_u64) {
                            index as usize
                        } else {
                            indexed_order = false;
                            completed_items
                                .last_key_value()
                                .map_or(0, |(index, _)| index.saturating_add(1))
                        };
                    open_items.remove(&index);
                    if let Some(previous) = completed_items.insert(index, item.clone())
                        && previous != *item
                    {
                        return Err(crate::provider::ProviderFailure::transport(
                            "Codex repeated a completed output index with conflicting data",
                        ));
                    }
                }
            }
            Some("response.completed") => {
                if terminal_seen {
                    return Err(crate::provider::ProviderFailure::transport(
                        "Codex stream contains multiple terminal events",
                    ));
                }
                terminal_seen = true;
                protocol_completed = true;
                finish_reason = Some(crate::provider::FinishReason::Stop);
                terminal_output = json
                    .pointer("/response/output")
                    .and_then(Value::as_array)
                    .cloned();
                if let Some(u) = json.pointer("/response/usage") {
                    token_usage = Some(usage::responses_usage_from_value(u));
                    reported_output_tokens = u.get("output_tokens").and_then(Value::as_u64);
                }
            }
            Some("response.incomplete") => {
                if terminal_seen {
                    return Err(crate::provider::ProviderFailure::transport(
                        "Codex stream contains multiple terminal events",
                    ));
                }
                terminal_seen = true;
                let reason = json
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("length");
                finish_reason = Some(crate::provider::FinishReason::from_openai(reason));
                if let Some(u) = json.pointer("/response/usage") {
                    token_usage = Some(usage::responses_usage_from_value(u));
                    reported_output_tokens = u.get("output_tokens").and_then(Value::as_u64);
                }
            }
            Some("response.failed") => {
                // Typed so transient overload/rate-limit failures are retried
                // instead of failing the turn as a plain error.
                return Err(sse::stream_error_from_object(
                    json.pointer("/response/error").unwrap_or(&Value::Null),
                    "Codex response failed",
                    &json,
                ));
            }
            _ => {}
        }
        Ok(())
    })
    .await?;

    if !interrupted && !terminal_seen {
        return Err(crate::provider::ProviderFailure::transport(
            "Codex stream ended before response.completed/response.incomplete",
        ));
    }

    let ordered = terminal_output.is_some() || indexed_order || completed_items.len() <= 1;
    if protocol_completed && !interrupted {
        if let Some(output) = &terminal_output {
            if open_items.iter().any(|index| *index >= output.len()) {
                return Err(crate::provider::ProviderFailure::transport(
                    "Codex terminal output omitted an unfinished output item",
                ));
            }
        } else {
            if !open_items.is_empty() {
                return Err(crate::provider::ProviderFailure::transport(
                    "Codex completed with unfinished output items",
                ));
            }
            if indexed_order && completed_items.keys().copied().ne(0..completed_items.len()) {
                return Err(crate::provider::ProviderFailure::transport(
                    "Codex completed output indexes contain missing slots",
                ));
            }
        }
    }
    let mut output = complete_responses_output(completed_items, terminal_output)?;
    let mut tool_calls = Vec::new();
    let mut call_slots = Vec::new();
    for (slot, item) in output.iter_mut().enumerate() {
        if protocol_completed
            && !interrupted
            && item
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status != "completed")
        {
            return Err(crate::provider::ProviderFailure::transport(
                "Codex completed with an unfinished output item",
            ));
        }
        if let Some(call) = tool_calls::responses_tool_call_from_item(item, &tool_call_arguments) {
            item["arguments"] = Value::String(call.arguments.clone());
            item["call_id"] = Value::String(call.id.clone());
            call_slots.push(slot);
            tool_calls.push(call);
        }
    }
    if protocol_completed && !interrupted {
        let mut completed_text = String::new();
        for item in &output {
            if item["type"] == "message"
                && item["role"] == "assistant"
                && let Some(parts) = item.get("content").and_then(Value::as_array)
            {
                for part in parts {
                    let text = match part["type"].as_str() {
                        Some("output_text") => part["text"].as_str(),
                        Some("refusal") => part["refusal"].as_str(),
                        _ => None,
                    };
                    if let Some(text) = text
                        && !text.is_empty()
                    {
                        completed_text.push_str(text);
                    }
                }
            }
        }
        if !completed_text.is_empty() {
            let Some(suffix) = completed_text.strip_prefix(&content) else {
                return Err(crate::provider::ProviderFailure::transport(
                    "Codex completed output contradicts streamed assistant text",
                ));
            };
            if !suffix.is_empty() {
                sink.assistant_delta(suffix);
            }
            content = completed_text;
        }
    }
    let _ = streaming::finish_text_stream(None, &mut content, &sink);
    let interrupted = interrupted || cancellation_token.is_cancelled();

    if matches!(finish_reason, Some(crate::provider::FinishReason::Stop)) && !tool_calls.is_empty()
    {
        finish_reason = Some(crate::provider::FinishReason::ToolCalls);
    }

    let terminal = if interrupted {
        crate::provider::StreamTerminal::Interrupted
    } else if !protocol_completed {
        crate::provider::StreamTerminal::Incomplete(
            finish_reason.unwrap_or(crate::provider::FinishReason::Length),
        )
    } else {
        crate::provider::StreamTerminal::Completed(
            finish_reason.unwrap_or(crate::provider::FinishReason::Stop),
        )
    };

    identities.assign_calls(identity_scope, &mut tool_calls);
    for (slot, call) in call_slots.into_iter().zip(&tool_calls) {
        output[slot]["call_id"] = Value::String(call.id.clone());
    }
    let supported = output.iter().all(codex_replay_item_supported);
    let replay_items =
        (protocol_completed && !interrupted && ordered && supported && !output.is_empty())
            .then_some(output);
    let replay_error = (protocol_completed && !interrupted && (!ordered || !supported))
        .then(|| "Codex native output lacks a reconstructable supported item sequence".to_string());
    Ok(ParsedResponsesStream {
        response: StreamedResponse {
            content,
            tool_calls,
            terminal,
            usage: token_usage,
            reasoning_chars,
        },
        replay_items,
        replay_error,
        reported_output_tokens,
    })
}

fn models_from_response(value: &Value) -> Vec<AvailableModel> {
    let mut models = Vec::new();
    for key in ["models", "data"] {
        let Some(items) = value.get(key).and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let visible = item
                .get("visibility")
                .and_then(Value::as_str)
                .is_none_or(|visibility| visibility == "list");
            let supported = item
                .get("supported_in_api")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if !visible || !supported {
                continue;
            }
            let Some(model_id) = item
                .get("slug")
                .or_else(|| item.get("model"))
                .or_else(|| item.get("id"))
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if models
                .iter()
                .any(|model: &AvailableModel| model.remote_model_id.as_ref() == model_id)
            {
                continue;
            }
            models.push(available_model_from_codex_item(model_id, item));
        }
    }
    models
}

fn available_model_from_codex_item(model_id: &str, item: &Value) -> AvailableModel {
    let context_window = item
        .get("context_window")
        .or_else(|| item.get("max_context_window"))
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .and_then(|value| u32::try_from(value).ok());
    let display_name = item
        .get("display_name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let supported_reasoning = item
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(reasoning_selection_from_codex_value)
        .collect::<Vec<_>>();
    let recommended_reasoning = item
        .get("default_reasoning_level")
        .and_then(reasoning_selection_from_codex_value);

    let mut features = Vec::new();
    if !supported_reasoning.is_empty() {
        features.push(ModelFeature::Reasoning);
    }
    if item
        .get("input_modalities")
        .and_then(Value::as_array)
        .is_some_and(|modalities| {
            modalities
                .iter()
                .any(|value| value.as_str() == Some("image"))
        })
    {
        features.push(ModelFeature::Attachment);
    }
    if item
        .get("supports_parallel_tool_calls")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        features.push(ModelFeature::ToolCall);
    }

    let mut model = AvailableModel::with_metadata(model_id, context_window, display_name, features)
        .with_reasoning(supported_reasoning, recommended_reasoning);
    model.use_responses_lite = item
        .get("use_responses_lite")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    model
}

fn reasoning_selection_from_codex_value(value: &Value) -> Option<ReasoningSelection> {
    let label = value
        .as_str()
        .or_else(|| value.get("effort").and_then(Value::as_str))?;
    match ReasoningSelection::parse(label)? {
        selection @ (ReasoningSelection::Off
        | ReasoningSelection::Minimal
        | ReasoningSelection::Low
        | ReasoningSelection::Medium
        | ReasoningSelection::High
        | ReasoningSelection::XHigh
        | ReasoningSelection::Max
        | ReasoningSelection::Ultra) => Some(selection),
        ReasoningSelection::Default
        | ReasoningSelection::On
        | ReasoningSelection::BudgetTokens(_) => None,
    }
}

fn codex_fallback_models() -> Vec<String> {
    CODEX_FALLBACK_MODELS
        .iter()
        .map(|model| (*model).to_string())
        .collect()
}

async fn codex_client_version() -> &'static str {
    CODEX_CLIENT_VERSION
        .get_or_init(|| async {
            // Explicit override wins verbatim — even below the floor — so the
            // version gate itself stays testable.
            if let Ok(configured) = std::env::var("BONSAI_CODEX_CLIENT_VERSION")
                && let Some(version) = parse_codex_client_version(&configured)
            {
                return version;
            }
            floored_client_version(detect_codex_client_version().await)
        })
        .await
}

/// Use the detected CLI version only when it is at or above the fallback
/// floor; an outdated local `codex` install must not shrink the served model
/// set (see [`CODEX_FALLBACK_CLIENT_VERSION`]).
fn floored_client_version(detected: Option<String>) -> String {
    let floor = version_triple(CODEX_FALLBACK_CLIENT_VERSION);
    match detected {
        Some(version) if version_triple(&version) >= floor => version,
        _ => CODEX_FALLBACK_CLIENT_VERSION.to_string(),
    }
}

fn version_triple(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(|part| part.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

async fn detect_codex_client_version() -> Option<String> {
    let mut command = tokio::process::Command::new("codex");
    command.arg("--version").kill_on_drop(true);
    let output = timeout(CODEX_CLIENT_VERSION_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout))
        .and_then(|output| parse_codex_client_version(&output))
}

fn parse_codex_client_version(output: &str) -> Option<String> {
    output.split_whitespace().find_map(|token| {
        let candidate = token.trim_start_matches('v');
        let candidate = candidate
            .split_once('-')
            .map_or(candidate, |(version, _suffix)| version);
        let mut parts = candidate.split('.');
        let valid = (0..3).all(|_| {
            parts.next().is_some_and(|part| {
                !part.is_empty() && part.chars().all(|character| character.is_ascii_digit())
            })
        }) && parts.next().is_none();
        valid.then(|| candidate.to_string())
    })
}

#[derive(Debug, Clone)]
struct CodexCredential {
    access_token: String,
    account_id: Option<String>,
    is_fedramp_account: bool,
}

fn require_codex_account_id(credential: &CodexCredential) -> Result<()> {
    if credential.account_id.as_deref().is_none_or(str::is_empty) {
        anyhow::bail!(
            "Codex auth cache is missing an account ID. Run `codex login` again, then retry /authorize codex."
        );
    }
    Ok(())
}

fn codex_home_dir() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".codex")))
}

async fn read_codex_auth_file(codex_home: &Path) -> Result<Option<CodexCredential>> {
    let auth_path = codex_home.join("auth.json");
    let content = match tokio::fs::read_to_string(&auth_path).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read Codex auth cache at {auth_path:?}"));
        }
    };
    let json: Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse Codex auth cache at {auth_path:?}"))?;
    Ok(codex_credential_from_json(&json))
}

fn codex_keyring_account(codex_home: &Path) -> String {
    let canonical = codex_home
        .canonicalize()
        .unwrap_or_else(|_| codex_home.to_path_buf());
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let short = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("cli|{short}")
}

async fn read_codex_auth_keyring(codex_home: &Path) -> Result<Option<CodexCredential>> {
    if crate::session::CredentialStore::native_access_disabled() {
        return Ok(None);
    }
    let account = codex_keyring_account(codex_home);
    let serialized = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        if crate::session::CredentialStore::native_access_disabled() {
            return Ok(None);
        }
        let entry = keyring::Entry::new(CODEX_KEYRING_SERVICE, &account)
            .context("OS credential store is unavailable for Codex")?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context("Failed to load Codex CLI credentials"),
        }
    })
    .await
    .context("Codex credential-store task failed")??;
    let Some(serialized) = serialized else {
        return Ok(None);
    };
    let json: Value = serde_json::from_str(&serialized)
        .context("Failed to parse Codex CLI credentials from the OS credential store")?;
    Ok(codex_credential_from_json(&json))
}

async fn read_codex_auth_credential() -> Result<Option<CodexCredential>> {
    let Some(codex_home) = codex_home_dir() else {
        return Ok(None);
    };
    if let Some(credential) = read_codex_auth_file(&codex_home).await? {
        return Ok(Some(credential));
    }
    read_codex_auth_keyring(&codex_home).await
}

pub(crate) async fn codex_cached_authorization() -> Result<Option<AuthorizeOutcome>> {
    let Some(credential) = read_codex_auth_credential().await? else {
        return Ok(None);
    };
    require_codex_account_id(&credential)?;
    Ok(Some(AuthorizeOutcome {
        api_key: credential.access_token,
        base_url: Some(CODEX_METADATA.default_base_url.to_string()),
        model: None,
        clear_existing_api_key: false,
        account_id: credential.account_id.unwrap_or_default(),
        is_fedramp: credential.is_fedramp_account,
    }))
}

fn codex_credential_from_json(value: &serde_json::Value) -> Option<CodexCredential> {
    let tokens = value.get("tokens").unwrap_or(value);
    let access_token = tokens
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
        .or_else(|| find_token_in_json(value))?;

    let mut account_id = tokens
        .get("account_id")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    let mut is_fedramp_account = false;

    if let Some(id_token) = tokens.get("id_token").and_then(serde_json::Value::as_str)
        && let Some(claims) = decode_jwt_payload(id_token)
        && let Some(auth) = claims.get("https://api.openai.com/auth")
    {
        if account_id.is_none() {
            account_id = auth
                .get("chatgpt_account_id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string);
        }
        is_fedramp_account = auth
            .get("chatgpt_account_is_fedramp")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
    }

    Some(CodexCredential {
        access_token,
        account_id,
        is_fedramp_account,
    })
}

fn decode_jwt_payload(jwt: &str) -> Option<serde_json::Value> {
    let mut parts = jwt.split('.');
    let (_header, payload, _signature) = (parts.next()?, parts.next()?, parts.next()?);
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn find_token_in_json(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                if matches!(
                    key.as_str(),
                    "access_token" | "accessToken" | "token" | "auth_token" | "authToken"
                ) && let Some(token) = item.as_str()
                    && !token.trim().is_empty()
                {
                    return Some(token.to_string());
                }

                if let Some(token) = find_token_in_json(item) {
                    return Some(token);
                }
            }
            None
        }
        serde_json::Value::Array(items) => items.iter().find_map(find_token_in_json),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ReasoningEffort;
    use crate::provider::test_utils::{
        named_user_message, provider_session_with_base, sample_tool, system_message,
        tool_call_message, tool_result_message, user_message,
    };

    fn make_session() -> ProviderSession {
        provider_session_with_base("", CODEX_METADATA.default_base_url.as_ref())
    }

    fn make_provider(session: &ProviderSession) -> CodexProvider {
        let target = crate::provider::fallback_run_target(&CODEX_METADATA, session);
        let mut provider = CodexProvider::new(session, &target);
        provider.persist_reasoning = false;
        provider.http = reqwest::Client::new();
        provider
    }

    fn make_provider_with_output_limit(session: &ProviderSession) -> CodexProvider {
        let mut target = crate::provider::fallback_run_target(&CODEX_METADATA, session);
        target.output_limit = Some(128_000);
        let mut provider = CodexProvider::new(session, &target);
        provider.persist_reasoning = false;
        provider.http = reqwest::Client::new();
        provider
    }

    fn make_lite_provider(session: &ProviderSession) -> CodexProvider {
        let mut target = crate::provider::fallback_run_target(&CODEX_METADATA, session);
        target.use_responses_lite = true;
        let mut provider = CodexProvider::new(session, &target);
        provider.persist_reasoning = false;
        provider.http = reqwest::Client::new();
        provider
    }

    fn loopback_codex_provider(server: &wiremock::MockServer, lite: bool) -> CodexProvider {
        let url = reqwest::Url::parse(&server.uri()).unwrap();
        assert!(
            url.host_str()
                .unwrap()
                .parse::<std::net::IpAddr>()
                .unwrap()
                .is_loopback()
        );
        let mut session = make_session();
        session.base_url = server.uri();
        session.api_key = "mock-token".into();
        session.account_id = "mock-account".into();
        if lite {
            make_lite_provider(&session)
        } else {
            make_provider(&session)
        }
    }

    fn native_responses_sse(items: &[Value], finish: &str, terminal_output: bool) -> String {
        let mut frames = Vec::new();
        for index in (0..items.len()).rev() {
            frames.push(json!({"type":"response.output_item.added","output_index":index,"item":items[index]}));
            frames.push(json!({"type":"response.output_item.done","output_index":index,"item":items[index]}));
        }
        let mut response = json!({"usage":{"input_tokens":100,"output_tokens":50}});
        if terminal_output {
            response["output"] = json!(items);
        }
        if finish == "response.incomplete" {
            response["incomplete_details"] = json!({"reason":"content_filter"});
        }
        frames.push(json!({"type":finish,"response":response}));
        frames
            .into_iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect()
    }

    async fn request_codex_sse(
        stream: &str,
        persist: bool,
        cancellation: CancellationToken,
        sink: SharedSink,
    ) -> (
        crate::provider::ProviderResult<StreamedResponse>,
        crate::provider::NativeReplayStore,
    ) {
        use crate::provider::{NativeReplayStore, ProviderRequestContext, ResponseId};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(stream.to_string()))
            .mount(&server)
            .await;
        let store = NativeReplayStore::default();
        let mut provider = loopback_codex_provider(&server, false);
        provider.persist_reasoning = persist;
        provider.set_native_replay_store(store.clone());
        let response = provider
            .chat_stream_with_context(
                &[],
                &[],
                Default::default(),
                &ProviderRequestContext {
                    response_id: Some(ResponseId::new()),
                    ..Default::default()
                },
                cancellation,
                sink,
            )
            .await;
        (response, store)
    }

    #[tokio::test]
    async fn native_codex_connection_identity_isolates_same_endpoint_model_and_account() {
        use crate::provider::{NativeReplayStore, ProviderRequestContext, ResponseId};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let items = [
            json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque=="}),
            json!({"type":"message","id":"msg_1","role":"assistant","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"same answer"}]}),
        ];
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(native_responses_sse(
                    &items,
                    "response.completed",
                    true,
                )),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut session = make_session();
        session.base_url = server.uri();
        session.api_key = "mock-token".into();
        session.account_id = "mock-account".into();
        let target = crate::provider::fallback_run_target(&CODEX_METADATA, &session);
        let mut original = CodexProvider::new(&session, &target);
        original.http = reqwest::Client::new();
        original.persist_reasoning = true;
        assert_eq!(
            original.tool_call_scope,
            crate::provider::tool_call_scope(
                "codex-responses",
                "codex",
                &server.uri(),
                target.endpoint_path.as_deref().unwrap_or("responses")
            ),
            "builtin scope remains compatible with existing snapshots"
        );
        let store = NativeReplayStore::default();
        original.set_native_replay_store(store.clone());
        let id = ResponseId::new();
        let response = original
            .chat_stream_with_context(
                &[],
                &[],
                Default::default(),
                &ProviderRequestContext {
                    response_id: Some(id.clone()),
                    ..Default::default()
                },
                CancellationToken::new(),
                std::sync::Arc::new(crate::provider::test_utils::RecordingSink::default()),
            )
            .await
            .unwrap();
        let history = [
            async_openai::types::chat::ChatCompletionRequestAssistantMessageArgs::default()
                .content(response.content)
                .build()
                .unwrap()
                .into(),
        ];
        let context = ProviderRequestContext {
            assistant_response_ids: Some(vec![Some(id)]),
            response_id: None,
        };
        let selected = original
            .prepare_request_for_estimation(&history, &[], Default::default(), &context)
            .unwrap();
        assert_eq!(selected.body["input"][0]["encrypted_content"], "opaque==");
        assert_eq!(selected.native_budget.len(), 1);
        let mut other_target = target.clone();
        other_target.connection_id = "custom-codex".parse().unwrap();
        let mut other = CodexProvider::new(&session, &other_target);
        other.persist_reasoning = true;
        other.set_native_replay_store(store.clone());
        assert_ne!(original.tool_call_scope, other.tool_call_scope);
        assert_ne!(original.native_replay_scope, other.native_replay_scope);
        let unselected = other
            .prepare_request_for_estimation(&history, &[], Default::default(), &context)
            .unwrap();
        assert!(unselected.native_budget.is_empty());
        assert!(!unselected.body.to_string().contains("opaque=="));
        assert_eq!(store.snapshot().len(), 1);
        server.verify().await;
    }

    #[tokio::test]
    async fn native_codex_summary_only_reasoning_rejects_whole_opted_in_response() {
        let items = [
            json!({"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"summary only"}]}),
            json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{}"}),
        ];
        let stream = native_responses_sse(&items, "response.completed", true);
        for persist in [true, false] {
            let (response, store) = request_codex_sse(
                &stream,
                persist,
                CancellationToken::new(),
                std::sync::Arc::new(crate::provider::test_utils::RecordingSink::default()),
            )
            .await;
            if persist {
                let error = response.unwrap_err();
                assert!(error.to_string().contains("reconstructable"), "{error}");
                assert!(!error.is_retryable());
            } else {
                assert_eq!(response.unwrap().tool_calls.len(), 1);
            }
            assert!(store.snapshot().is_empty());
        }
    }

    #[test]
    fn native_codex_projection_rejects_unusable_stored_item_atomically() {
        use crate::provider::{
            CodexNativeReplay, NativeReplayPayload, NativeReplayRecord, NativeReplayStore,
            ProviderRequestContext, ResponseId,
        };
        let store = NativeReplayStore::default();
        let id = ResponseId::new();
        store.insert(NativeReplayRecord {
            response_id: id.clone(),
            provider_scope: "test".into(),
            payload: NativeReplayPayload::CodexResponses(CodexNativeReplay { budget_evidence: None, output_items: vec![
                json!({"type":"reasoning","id":"rs_1","summary":[]}),
                json!({"type":"function_call","call_id":"call_1","name":"read","arguments":"{}"}),
            ] }),
        }).unwrap();
        let result = messages_to_responses_input_with_context(
            &[tool_call_message("call_1", "read", "{}")],
            &store,
            "test",
            &ProviderRequestContext {
                assistant_response_ids: Some(vec![Some(id)]),
                response_id: None,
            },
            true,
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unreconstructable")
        );
        assert_eq!(store.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn native_codex_completed_text_extends_streamed_prefix_without_duplication() {
        use crate::provider::test_utils::RecordingSink;
        let cases = [
            (
                "π ",
                vec![
                    json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"π complete answer"}]}),
                ],
                "π complete answer",
            ),
            (
                "checking ",
                vec![
                    json!({"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"checking "}]}),
                    json!({"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"complete"}]}),
                ],
                "checking complete",
            ),
            (
                "I cannot",
                vec![
                    json!({"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"I cannot do that"}]}),
                ],
                "I cannot do that",
            ),
        ];
        for (prefix, items, expected) in cases {
            let stream = format!(
                "data: {}\n\n{}",
                json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":prefix}),
                native_responses_sse(&items, "response.completed", true)
            );
            let sink = std::sync::Arc::new(RecordingSink::default());
            let (response, store) =
                request_codex_sse(&stream, true, CancellationToken::new(), sink.clone()).await;
            assert_eq!(response.unwrap().content, expected);
            assert_eq!(sink.assistant_deltas().concat(), expected);
            assert_eq!(store.snapshot().len(), 1);
        }
    }

    #[tokio::test]
    async fn native_codex_completed_text_cannot_contradict_streamed_prefix() {
        let items = [
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"different answer"}]}),
        ];
        let stream = format!(
            "data: {}\n\n{}",
            json!({"type":"response.output_text.delta","delta":"original "}),
            native_responses_sse(&items, "response.completed", true)
        );
        let (response, store) = request_codex_sse(
            &stream,
            true,
            CancellationToken::new(),
            std::sync::Arc::new(crate::provider::test_utils::RecordingSink::default()),
        )
        .await;
        assert!(
            response
                .unwrap_err()
                .to_string()
                .contains("contradicts streamed")
        );
        assert!(store.snapshot().is_empty());
    }

    #[tokio::test]
    async fn native_codex_completed_sequence_rejects_missing_and_unfinished_items() {
        let reasoning = json!({"type":"reasoning","id":"rs_1","status":"completed","encrypted_content":"ENC","summary":[]});
        let call = json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{}","status":"completed"});
        let mut unfinished = reasoning.clone();
        unfinished["status"] = json!("incomplete");
        let cases = [
            (
                vec![
                    json!({"type":"response.output_item.done","output_index":0,"item":unfinished}),
                ],
                json!({}),
                "unfinished output item",
            ),
            (
                vec![
                    json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
                    json!({"type":"response.output_item.done","output_index":2,"item":call}),
                ],
                json!({}),
                "missing slots",
            ),
            (
                vec![json!({"type":"response.output_item.added","output_index":1,"item":call})],
                json!({"output":[reasoning]}),
                "omitted an unfinished",
            ),
            (
                vec![json!({"type":"response.output_item.added","output_index":0,"item":call})],
                json!({}),
                "unfinished output items",
            ),
            (
                vec![json!({"type":"response.output_item.done","output_index":0,"item":call})],
                json!({"output":[reasoning]}),
                "contradicts completed",
            ),
        ];
        for (mut frames, terminal, expected) in cases {
            frames.push(json!({"type":"response.completed","response":terminal}));
            let stream: String = frames
                .into_iter()
                .map(|frame| format!("data: {frame}\n\n"))
                .collect();
            let (response, store) = request_codex_sse(
                &stream,
                true,
                CancellationToken::new(),
                std::sync::Arc::new(crate::provider::test_utils::RecordingSink::default()),
            )
            .await;
            let error = response.unwrap_err();
            assert!(error.to_string().contains(expected), "{expected}: {error}");
            assert!(store.snapshot().is_empty());
        }
    }

    #[tokio::test]
    async fn native_codex_terminal_output_can_finish_an_observed_open_item() {
        let item = json!({"type":"reasoning","id":"rs_1","status":"completed","encrypted_content":"ENC","summary":[]});
        let stream = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","status":"in_progress"}}),
            json!({"type":"response.completed","response":{"output":[item]}})
        );
        let (response, store) = request_codex_sse(
            &stream,
            true,
            CancellationToken::new(),
            std::sync::Arc::new(crate::provider::test_utils::RecordingSink::default()),
        )
        .await;
        assert!(!response.unwrap().is_interrupted());
        assert_eq!(store.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn native_codex_cancellation_in_final_text_callback_prevents_capture() {
        #[derive(Debug)]
        struct CancelOnDone(CancellationToken);
        impl crate::output::OutputSink for CancelOnDone {
            fn assistant_done(&self) {
                self.0.cancel();
            }
        }
        let cancellation = CancellationToken::new();
        let items = [
            json!({"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"finished text"}]}),
        ];
        let stream = native_responses_sse(&items, "response.completed", true);
        let (response, store) = request_codex_sse(
            &stream,
            true,
            cancellation.clone(),
            std::sync::Arc::new(CancelOnDone(cancellation)),
        )
        .await;
        assert!(response.unwrap().is_interrupted());
        assert!(store.snapshot().is_empty());
    }

    #[tokio::test]
    async fn native_codex_ordered_items_survive_snapshot_rebuild_and_both_wire_dialects() {
        use crate::provider::{
            NativeReplayRecord, NativeReplayStore, ProviderRequestContext, ResponseId,
            ToolCallIdentityStore,
        };
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let items = vec![
            json!({"type":"reasoning","id":"rs_1","status":"completed","summary":[],"content":[{"type":"reasoning_text","text":"exact π"}],"encrypted_content":"ENC_1==","extension":{"id":"call_a"}}),
            json!({"type":"message","id":"msg_1","role":"assistant","status":"completed","phase":"commentary","content":[{"type":"output_text","text":"same ","annotations":[]}]}),
            json!({"type":"function_call","id":"fc_1","call_id":"call_a","name":"read","arguments":"{malformed π","status":"completed","extension":{"id":"fc_1"}}),
            json!({"type":"reasoning","id":"rs_2","status":"completed","summary":[{"type":"summary_text","text":"second"}],"encrypted_content":"ENC_2=="}),
            json!({"type":"function_call","id":"fc_2","call_id":"call_b","name":"read","arguments":"{ \"path\": \"b\" }","status":"completed"}),
            json!({"type":"message","id":"msg_2","role":"assistant","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"answer","annotations":[]}]}),
        ];
        for lite in [false, true] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(native_responses_sse(
                        &items,
                        "response.completed",
                        true,
                    )),
                )
                .mount(&server)
                .await;
            let store = NativeReplayStore::default();
            let identities = ToolCallIdentityStore::default();
            let mut provider = loopback_codex_provider(&server, lite);
            provider.persist_reasoning = true;
            provider.set_native_replay_store(store.clone());
            provider.set_tool_call_identity_store(identities.clone());
            let id = ResponseId::new();
            let response = provider
                .chat_stream_with_context(
                    &[user_message("inspect")],
                    &[],
                    Default::default(),
                    &ProviderRequestContext {
                        response_id: Some(id.clone()),
                        ..Default::default()
                    },
                    CancellationToken::new(),
                    std::sync::Arc::new(crate::output::StdoutSink),
                )
                .await
                .unwrap();
            assert_eq!(response.content, "same answer");
            assert_eq!(response.tool_calls.len(), 2);
            assert_eq!(response.tool_calls[0].arguments, "{malformed π");
            let saved = store.snapshot();
            assert_eq!(saved.len(), 1);
            let crate::provider::NativeReplayPayload::CodexResponses(native) = &saved[0].payload
            else {
                panic!("wrong transport")
            };
            assert_eq!(native.output_items[0], items[0]);
            assert_eq!(native.output_items[3], items[3]);
            assert_eq!(native.output_items[2]["id"], "fc_1");
            assert_eq!(native.output_items[2]["call_id"], response.tool_calls[0].id);
            let restored = NativeReplayStore::default();
            restored
                .restore(
                    &serde_json::from_slice::<Vec<NativeReplayRecord>>(
                        &serde_json::to_vec(&saved).unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap();
            let rebuilt_ids = ToolCallIdentityStore::default();
            rebuilt_ids.restore(&identities.snapshot());
            let mut rebuilt = loopback_codex_provider(&server, lite);
            rebuilt.persist_reasoning = true;
            rebuilt.set_native_replay_store(restored.clone());
            rebuilt.set_tool_call_identity_store(rebuilt_ids);
            let mut assistant = tool_call_message(&response.tool_calls[0].id, "read", "{}");
            if let ChatCompletionRequestMessage::Assistant(row) = &mut assistant {
                let ChatCompletionRequestMessage::Assistant(second) =
                    tool_call_message(&response.tool_calls[1].id, "read", "{}")
                else {
                    unreachable!()
                };
                row.tool_calls
                    .as_mut()
                    .unwrap()
                    .extend(second.tool_calls.unwrap());
                row.content = Some(
                    async_openai::types::chat::ChatCompletionRequestAssistantMessageContent::Text(
                        response.content,
                    ),
                );
            }
            let history = [
                user_message("inspect"),
                assistant,
                tool_result_message(&response.tool_calls[0].id, "first"),
                tool_result_message(&response.tool_calls[1].id, "second"),
            ];
            let context = ProviderRequestContext {
                assistant_response_ids: Some(vec![Some(id.clone())]),
                response_id: None,
            };
            rebuilt
                .chat_stream_with_context(
                    &history,
                    &[],
                    Default::default(),
                    &context,
                    CancellationToken::new(),
                    std::sync::Arc::new(crate::output::StdoutSink),
                )
                .await
                .unwrap();
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 2);
            let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
            let start = if lite { 2 } else { 1 };
            let mut expected = items.clone();
            expected[0].as_object_mut().unwrap().remove("status");
            expected[3].as_object_mut().unwrap().remove("status");
            let input = body["input"].as_array().unwrap();
            assert_eq!(&input[start..start + items.len()], &expected);
            assert_eq!(input[start + items.len()]["call_id"], "call_a");
            assert_eq!(input[start + items.len() + 1]["call_id"], "call_b");
            assert_eq!(
                restored.snapshot(),
                saved,
                "preview/followup without a response UUID cannot mutate replay"
            );
            rebuilt.persist_reasoning = false;
            let disabled = rebuilt
                .preview_request_with_context(&history, &[], &context)
                .unwrap()
                .body;
            assert_eq!(disabled["include"], json!([]));
            assert!(
                disabled["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item["type"] != "reasoning" && item.get("phase").is_none())
            );
            rebuilt.persist_reasoning = true;
            let absent = rebuilt.preview_request(&history, &[]).unwrap().body;
            assert!(
                absent["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item["type"] != "reasoning")
            );
            let mut incompatible = loopback_codex_provider(&server, !lite);
            incompatible.persist_reasoning = true;
            incompatible.set_native_replay_store(restored);
            let wrong = incompatible
                .preview_request_with_context(&history, &[], &context)
                .unwrap()
                .body;
            assert!(
                wrong["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item["type"] != "reasoning")
            );
        }
    }

    #[tokio::test]
    async fn native_codex_plain_replies_select_response_uuid_with_indexed_done_fallback() {
        use crate::provider::test_utils::assistant_message;
        use crate::provider::{ProviderRequestContext, ResponseId};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        Mock::given(method("POST")).respond_with(move |_:&wiremock::Request|{
            let index=counter.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_string(native_responses_sse(&[
                json!({"type":"reasoning","id":format!("rs_{index}"),"encrypted_content":format!("ENC_{index}"),"summary":[]}),
                json!({"type":"message","id":format!("msg_{index}"),"role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"same answer"}]}),
            ],"response.completed",false))
        }).mount(&server).await;
        let mut provider = loopback_codex_provider(&server, false);
        provider.persist_reasoning = true;
        let ids = [ResponseId::new(), ResponseId::new()];
        for id in &ids {
            let response = provider
                .chat_stream_with_context(
                    &[],
                    &[],
                    Default::default(),
                    &ProviderRequestContext {
                        response_id: Some(id.clone()),
                        ..Default::default()
                    },
                    CancellationToken::new(),
                    std::sync::Arc::new(crate::output::StdoutSink),
                )
                .await
                .unwrap();
            assert_eq!(response.content, "same answer");
        }
        let history = [
            assistant_message("same answer"),
            assistant_message("same answer"),
            assistant_message("same answer"),
        ];
        let body = provider
            .preview_request_with_context(
                &history,
                &[],
                &ProviderRequestContext {
                    assistant_response_ids: Some(vec![
                        Some(ids[1].clone()),
                        None,
                        Some(ids[0].clone()),
                    ]),
                    response_id: None,
                },
            )
            .unwrap()
            .body;
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 5);
        assert_eq!(input[0]["encrypted_content"], "ENC_1");
        assert_eq!(input[3]["encrypted_content"], "ENC_0");
        assert!(input[2].get("phase").is_none());
        assert!(
            provider
                .preview_request_with_context(
                    &history,
                    &[],
                    &ProviderRequestContext {
                        assistant_response_ids: Some(vec![]),
                        response_id: None
                    }
                )
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_codex_incomplete_reasons_never_become_completed_or_persist_replay() {
        use crate::provider::{NativeReplayStore, ProviderRequestContext, ResponseId};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        for reason in [
            "max_output_tokens",
            "content_filter",
            "generation_budget",
            "unknown_partial",
        ] {
            let server = MockServer::start().await;
            let item = json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{}"});
            let stream = format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"type":"response.output_item.done","output_index":0,"item":item}),
                json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":reason},"usage":{"input_tokens":12,"output_tokens":7}}})
            );
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_string(stream))
                .mount(&server)
                .await;
            let store = NativeReplayStore::default();
            let mut provider = loopback_codex_provider(&server, false);
            provider.persist_reasoning = true;
            provider.set_native_replay_store(store.clone());
            let response = provider
                .chat_stream_with_context(
                    &[],
                    &[],
                    Default::default(),
                    &ProviderRequestContext {
                        response_id: Some(ResponseId::new()),
                        ..Default::default()
                    },
                    CancellationToken::new(),
                    std::sync::Arc::new(crate::output::StdoutSink),
                )
                .await
                .unwrap();
            assert!(matches!(
                response.terminal,
                crate::provider::StreamTerminal::Incomplete(_)
            ));
            assert_eq!(
                response.finish_reason(),
                Some(&crate::provider::FinishReason::from_openai(reason))
            );
            assert!(store.snapshot().is_empty());
        }
    }

    fn pasted_image_user_message() -> ChatCompletionRequestMessage {
        use async_openai::types::chat::{
            ChatCompletionRequestMessageContentPartImage, ChatCompletionRequestUserMessage,
            ChatCompletionRequestUserMessageContent, ChatCompletionRequestUserMessageContentPart,
            ImageUrl,
        };
        ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
            content: ChatCompletionRequestUserMessageContent::Array(vec![
                ChatCompletionRequestUserMessageContentPart::ImageUrl(
                    ChatCompletionRequestMessageContentPartImage {
                        prompt_cache_breakpoint: None,
                        image_url: ImageUrl {
                            url: "data:image/png;base64,AAAA".to_string(),
                            detail: None,
                        },
                    },
                ),
            ]),
            name: None,
        })
    }

    #[test]
    fn request_body_strips_images_for_non_vision_model() {
        // Mirror of the OpenAI-chat/Anthropic safety net: an image already in
        // history must not 400 every later turn when the active model rejects
        // image input.
        let session = make_session();
        let mut target = crate::provider::fallback_run_target(&CODEX_METADATA, &session);
        target.supports_vision = false;
        let provider = CodexProvider::new(&session, &target);

        let body = provider
            .request_body(&[user_message("hi"), pasted_image_user_message()], &[])
            .unwrap();
        let serialized = body.to_string();

        assert!(!serialized.contains("input_image"));
        assert!(serialized.contains(crate::provider::transform::IMAGE_OMITTED_PLACEHOLDER));
    }

    #[test]
    fn request_body_keeps_images_for_vision_model() {
        let provider = make_provider(&make_session());

        let body = provider
            .request_body(&[user_message("hi"), pasted_image_user_message()], &[])
            .unwrap();

        assert!(body.to_string().contains("input_image"));
    }

    #[test]
    fn metadata_is_codex() {
        assert_eq!(CODEX_METADATA.id.as_ref(), "codex");
        assert_eq!(CODEX_METADATA.default_model.as_ref(), "gpt-5.6-sol");
        assert_eq!(CODEX_METADATA.env_var_api_key.as_deref(), None);
        assert_eq!(CODEX_METADATA.auth_requirement, AuthRequirement::CodexCache);
        assert_eq!(CODEX_METADATA.protocol, Protocol::CodexResponses);
        assert_eq!(CODEX_METADATA.context_window, Some(272_000));
    }

    #[test]
    fn codex_credential_from_json_reads_account_id() {
        let value = serde_json::json!({
            "tokens": {
                "access_token": "token-123",
                "account_id": "account-123"
            }
        });

        let credential = codex_credential_from_json(&value).unwrap();
        assert_eq!(credential.access_token, "token-123");
        assert_eq!(credential.account_id.as_deref(), Some("account-123"));
        assert!(!credential.is_fedramp_account);
    }

    #[test]
    fn find_token_in_json_recurses() {
        let value = serde_json::json!({
            "account": {
                "nested": {
                    "access_token": "token-123"
                }
            }
        });
        assert_eq!(find_token_in_json(&value), Some("token-123".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn codex_keyring_account_matches_cli_path_hash_contract() {
        assert_eq!(
            codex_keyring_account(Path::new("/tmp/codex-home")),
            "cli|c790889e29f35b54"
        );
    }

    #[tokio::test]
    async fn isolated_verifier_skips_codex_native_keyring_fallback() {
        crate::util::test_env::with_var_async("BONSAI_DISABLE_KEYRING", Some("1"), async {
            let home = tempfile::tempdir().unwrap();
            assert!(
                read_codex_auth_keyring(home.path())
                    .await
                    .unwrap()
                    .is_none()
            );
            // An absent file must not escape into the native keyring, while
            // isolated file credentials remain available.
            assert!(read_codex_auth_file(home.path()).await.unwrap().is_none());
            tokio::fs::write(
                home.path().join("auth.json"),
                json!({"tokens": {"access_token": "synthetic", "account_id": "isolated"}})
                    .to_string(),
            )
            .await
            .unwrap();
            let credential = read_codex_auth_file(home.path()).await.unwrap().unwrap();
            assert_eq!(credential.access_token, "synthetic");
            assert_eq!(credential.account_id.as_deref(), Some("isolated"));
        })
        .await;
    }

    #[tokio::test]
    async fn auth_file_reader_uses_supplied_codex_home() {
        let codex_home = tempfile::tempdir().unwrap();
        tokio::fs::write(
            codex_home.path().join("auth.json"),
            json!({
                "tokens": {
                    "access_token": "token-from-custom-home",
                    "account_id": "account-from-custom-home"
                }
            })
            .to_string(),
        )
        .await
        .unwrap();

        let credential = read_codex_auth_file(codex_home.path())
            .await
            .unwrap()
            .expect("credential from custom CODEX_HOME");
        assert_eq!(credential.access_token, "token-from-custom-home");
        assert_eq!(
            credential.account_id.as_deref(),
            Some("account-from-custom-home")
        );
    }

    #[tokio::test]
    async fn malformed_auth_file_reports_parse_failure() {
        let codex_home = tempfile::tempdir().unwrap();
        tokio::fs::write(codex_home.path().join("auth.json"), "{not-json")
            .await
            .unwrap();

        let error = read_codex_auth_file(codex_home.path())
            .await
            .expect_err("malformed auth cache must not look like logged out");
        assert!(
            error
                .to_string()
                .contains("Failed to parse Codex auth cache")
        );
    }

    #[test]
    fn tools_to_responses_tools_flattens_function() {
        let tool = serde_json::from_value::<ChatCompletionTool>(json!({
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read a file",
                "parameters": {"type": "object"}
            }
        }))
        .unwrap();

        let tools = tools_to_responses_tools(&[tool]).unwrap();
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "read");
        assert_eq!(tools[0]["parameters"], json!({"type": "object"}));
    }

    #[test]
    fn request_body_sends_stable_prompt_cache_key_across_turns() {
        let mut provider = make_provider(&make_session());
        provider.set_conversation_cache_key("bonsai-persisted-session");
        let first = provider
            .request_body(&[user_message("hello")], &[])
            .unwrap();
        let second = provider
            .request_body(&[user_message("hello again")], &[])
            .unwrap();

        let key = first
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .expect("codex request must carry a prompt_cache_key");
        assert_eq!(key, "bonsai-persisted-session");
        assert_eq!(
            second.get("prompt_cache_key").and_then(Value::as_str),
            Some(key),
            "prompt_cache_key must be identical across turns of one conversation",
        );
    }

    #[test]
    fn request_body_scopes_prompt_cache_key_per_instruction_lane() {
        let provider = make_provider(&make_session());
        let planning_first = provider
            .request_body(
                &[system_message("planning persona"), user_message("hello")],
                &[],
            )
            .unwrap();
        let planning_second = provider
            .request_body(
                &[system_message("planning persona"), user_message("again")],
                &[],
            )
            .unwrap();
        let coding = provider
            .request_body(
                &[system_message("coding persona"), user_message("hello")],
                &[],
            )
            .unwrap();

        let planning_key = planning_first
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .expect("planning request must carry a prompt_cache_key");
        assert_eq!(
            planning_second
                .get("prompt_cache_key")
                .and_then(Value::as_str),
            Some(planning_key),
            "one lane must keep one key across its turns",
        );
        assert_ne!(
            coding.get("prompt_cache_key").and_then(Value::as_str),
            Some(planning_key),
            "plan-mode and coding-mode lanes must not share a cache route",
        );
    }

    #[test]
    fn gpt_5_6_uses_stable_automatic_cache_prefix() {
        let session = make_session();
        let mut target = crate::provider::fallback_run_target(&CODEX_METADATA, &session);
        target.remote_model_id = "gpt-5.6-sol".into();
        target.use_responses_lite = true;
        let provider = CodexProvider::new(&session, &target);
        let first_messages = vec![
            system_message("stable instructions"),
            user_message("inspect"),
            named_user_message(
                "bonsai_project_state",
                "Context update:\n- first dirty state",
            ),
            tool_call_message("call-1", "read", "{}"),
            tool_result_message("call-1", "first"),
        ];
        let first = provider.request_body(&first_messages, &[]).unwrap();

        let mut second_messages = first_messages.clone();
        second_messages.push(tool_call_message("call-2", "read", "{}"));
        second_messages.push(tool_result_message("call-2", "second"));
        second_messages.push(named_user_message(
            "bonsai_project_state",
            "Context update:\n- second dirty state",
        ));
        let second = provider.request_body(&second_messages, &[]).unwrap();

        assert!(
            second.get("prompt_cache_options").is_none(),
            "the ChatGPT Codex backend rejects prompt_cache_options"
        );
        assert!(
            !second.to_string().contains("prompt_cache_breakpoint"),
            "the Codex transport must not send unsupported nested cache markers"
        );
        assert_eq!(first["prompt_cache_key"], second["prompt_cache_key"]);
        let preview = provider.preview_request(&second_messages, &[]).unwrap();
        assert_eq!(
            preview.cache_mechanism().as_deref(),
            Some("prompt_cache_key")
        );
        let first_input = first["input"]
            .as_array()
            .expect("Responses input should be an array");
        let second_input = second["input"]
            .as_array()
            .expect("Responses input should be an array");
        // IMPORTANT CACHE INVARIANT: GPT automatic caching keys the exact
        // request prefix through the latest message. Do not weaken this to a
        // system-only comparison or move project-state messages to the end in
        // the provider: the complete prior request must remain an exact prefix.
        assert_eq!(
            &second_input[..first_input.len()],
            first_input,
            "new turns must retain the complete prior input byte-for-byte"
        );
    }

    #[test]
    fn responses_input_preserves_append_only_project_state_position() {
        let messages = [
            system_message("stable instructions"),
            user_message("hello"),
            named_user_message("bonsai_project_state", "Context update:\n- dirty files"),
            tool_call_message("call-1", "read", "{}"),
            tool_result_message("call-1", "file"),
        ];
        let (instructions, input) = messages_to_responses_input(&messages).unwrap();

        assert_eq!(instructions, "stable instructions");
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["role"], json!("user"));
        assert_eq!(input[0]["content"][0]["text"], json!("hello"));
        assert_eq!(input[1]["role"], json!("user"));
        assert_eq!(
            input[1]["content"][0]["text"],
            json!("Context update:\n- dirty files")
        );
        assert_eq!(input[2]["type"], json!("function_call"));
        assert_eq!(input[3]["type"], json!("function_call_output"));
    }

    #[test]
    fn native_tool_loop_preserves_complete_prefix_without_synthetic_messages() {
        let first_messages = vec![
            system_message("stable instructions"),
            user_message("inspect"),
            tool_call_message("call-1", "read", "{}"),
            tool_result_message("call-1", "first"),
        ];
        let (_, first_input) = messages_to_responses_input(&first_messages).unwrap();

        let mut second_messages = first_messages.clone();
        second_messages.push(tool_call_message("call-2", "read", "{}"));
        second_messages.push(tool_result_message("call-2", "second"));
        let (_, second_input) = messages_to_responses_input(&second_messages).unwrap();

        assert_eq!(
            &second_input[..first_input.len()],
            first_input,
            "IMPORTANT: every native item from a prior tool-loop request must stay an exact prefix",
        );
        assert_eq!(
            second_input.last().and_then(|item| item.get("type")),
            Some(&json!("function_call_output")),
            "match the official Codex request shape without synthetic messages",
        );
    }

    #[test]
    fn later_system_message_becomes_developer_input_and_keeps_instructions_stable() {
        // A single-system baseline: `instructions` is the persona head.
        let (base_instructions, base_input) =
            messages_to_responses_input(&[system_message("persona head"), user_message("hi")])
                .unwrap();
        assert_eq!(base_instructions, "persona head");
        assert_eq!(base_input.len(), 1);

        // Adding a later system message (e.g. a skill load) must NOT change
        // `instructions` — that would rewrite the cached prefix. It lands as an
        // in-position developer input item instead.
        let (instructions, input) = messages_to_responses_input(&[
            system_message("persona head"),
            user_message("hi"),
            system_message("Skill: deploy\nrun the deploy steps"),
            user_message("go"),
        ])
        .unwrap();

        assert_eq!(
            instructions, base_instructions,
            "a later system message must not perturb the cached instructions"
        );
        let developer = input
            .iter()
            .find(|item| item["role"] == json!("developer"))
            .expect("later system message should appear as a developer input item");
        assert_eq!(
            developer["content"][0]["text"],
            json!("Skill: deploy\nrun the deploy steps")
        );
        assert_eq!(developer["content"][0]["type"], json!("input_text"));
    }

    #[test]
    fn native_reasoning_input_shaping_preserves_content_and_opaque_fields() {
        let event = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "thinking"}],
                "encrypted_content": "ENC",
                "content": [{"type":"reasoning_text","text":"exact native content"}],
                "extension": {"id":"opaque"},
                "status": "completed"
            }
        });
        let item = codex_input_replay_item(event["item"].clone()).expect("reasoning item");
        assert_eq!(item["type"], json!("reasoning"));
        assert_eq!(item["id"], json!("rs_1"));
        assert_eq!(item["encrypted_content"], json!("ENC"));
        assert_eq!(item["content"], event["item"]["content"]);
        assert_eq!(item["extension"], event["item"]["extension"]);
        assert_eq!(
            event["item"]["status"], "completed",
            "capture stays unchanged"
        );
        assert!(
            item.get("status").is_none(),
            "output-only status must not be echoed back as input",
        );

        let function_call = json!({"item": {"type": "function_call", "id": "fc_1"}});
        assert!(codex_input_replay_item(function_call["item"].clone()).is_none());

        // A reasoning item without an encrypted payload can't be replayed under
        // `store:false`, so it is not projected as input.
        let summary_only = json!({
            "item": {"type": "reasoning", "id": "rs_2", "summary": []}
        });
        assert!(codex_input_replay_item(summary_only["item"].clone()).is_none());
    }

    #[test]
    fn responses_input_threads_reasoning_ahead_of_its_tool_call() {
        let messages = vec![
            user_message("do it"),
            tool_call_message("call_1", "read", "{}"),
            tool_result_message("call_1", "ok"),
        ];
        let store = crate::provider::NativeReplayStore::default();
        let response_id = crate::provider::ResponseId::new();
        store.insert(crate::provider::NativeReplayRecord {
            response_id:response_id.clone(),provider_scope:"scope".into(),
            payload:crate::provider::NativeReplayPayload::CodexResponses(
                crate::provider::CodexNativeReplay { budget_evidence: None, output_items:vec![
                    json!({"type":"reasoning","id":"rs_1","encrypted_content":"ENC"}),
                    json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{}"}),
                ] },
            ),
        }).unwrap();
        let (_instructions, input) = messages_to_responses_input_with_context(
            &messages,
            &store,
            "scope",
            &crate::provider::ProviderRequestContext {
                assistant_response_ids: Some(vec![Some(response_id)]),
                response_id: None,
            },
            true,
        )
        .unwrap();
        let reasoning_pos = input.iter().position(|i| i["type"] == json!("reasoning"));
        let call_pos = input
            .iter()
            .position(|i| i["type"] == json!("function_call"));
        assert!(
            reasoning_pos.is_some() && reasoning_pos < call_pos,
            "reasoning item must be emitted immediately before its function_call",
        );
        assert_eq!(input[reasoning_pos.unwrap()]["id"], json!("rs_1"));

        // Default without provenance emits no native reasoning items.
        let (_i, plain) = messages_to_responses_input(&messages).unwrap();
        assert!(plain.iter().all(|i| i["type"] != json!("reasoning")));
    }

    #[test]
    fn request_body_omits_reasoning_include_by_default() {
        let body = make_provider(&make_session())
            .request_body(&[user_message("hi")], &[])
            .unwrap();
        // Persistence is off by default, so encrypted reasoning is not requested.
        assert_eq!(body.get("include"), Some(&json!([])));
    }

    #[test]
    fn request_body_omits_unsupported_output_limit() {
        let provider = make_provider_with_output_limit(&make_session());
        let body = provider
            .request_body(&[user_message("hello")], &[])
            .unwrap();

        assert!(
            body.get("max_output_tokens").is_none(),
            "Codex backend rejects max_output_tokens: {body}"
        );
    }

    #[test]
    fn preview_request_wire_sections_follow_serialized_body_order() {
        let provider = make_provider(&make_session());
        let messages = vec![system_message("system prompt"), user_message("hello")];
        let preview = provider
            .preview_request(&messages, &[sample_tool()])
            .unwrap();

        let ids = preview
            .wire_sections
            .iter()
            .map(|section| section.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                "wire-include",
                "wire-input",
                "wire-instructions",
                "wire-model",
                "wire-parallel-tool-calls",
                "wire-prompt-cache-key",
                "wire-reasoning",
                "wire-store",
                "wire-stream",
                "wire-tool-choice",
                "wire-tools",
            ]
        );
        let paths = preview
            .wire_sections
            .iter()
            .map(|section| section.provider_path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            preview
                .body
                .as_object()
                .unwrap()
                .keys()
                .map(|key| format!("$.{key}"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn reasoning_payload_requests_summary_even_at_auto_effort() {
        // Auto effort still streams a reasoning summary so the user sees the
        // model think during time-to-first-token.
        let provider = make_provider(&make_session());
        assert_eq!(
            provider.reasoning_payload(),
            Some(json!({"summary": "detailed"}))
        );
    }

    #[test]
    fn reasoning_payload_emits_supported_effort_with_summary() {
        let mut session = make_session();
        session.reasoning = ReasoningSelection::from_effort(ReasoningEffort::High);
        let provider = make_provider(&session);

        assert_eq!(
            provider.reasoning_payload(),
            Some(json!({"effort": "high", "summary": "detailed"}))
        );
    }

    #[test]
    fn responses_lite_uses_header_compatible_request_shape() {
        let mut session = make_session();
        session.reasoning = ReasoningSelection::Medium;
        let provider = make_lite_provider(&session);
        let body = provider
            .request_body(
                &[system_message("system prompt"), user_message("hello")],
                &[sample_tool()],
            )
            .unwrap();

        assert!(body.get("instructions").is_none());
        assert!(body.get("tools").is_none());
        assert_eq!(
            body["parallel_tool_calls"],
            json!(false),
            "Responses Lite rejects requests unless parallel tool calls are disabled"
        );
        assert_eq!(body["input"][0]["type"], json!("additional_tools"));
        assert_eq!(body["input"][0]["role"], json!("developer"));
        assert_eq!(body["input"][0]["tools"][0]["name"], json!("read"));
        assert_eq!(body["input"][1]["role"], json!("developer"));
        assert_eq!(
            body["input"][1]["content"][0]["text"],
            json!("system prompt")
        );
        assert_eq!(body["input"][2]["role"], json!("user"));
        assert_eq!(body["reasoning"]["context"], json!("all_turns"));
    }

    #[test]
    fn responses_lite_sends_reasoning_context_even_with_reasoning_off() {
        // Lite validation 400s without `reasoning.context = "all_turns"`,
        // including when the user turned reasoning off (Off encodes to no
        // reasoning payload on the classic path).
        let mut session = make_session();
        session.reasoning = ReasoningSelection::Off;
        let provider = make_lite_provider(&session);
        let body = provider
            .request_body(&[user_message("hello")], &[])
            .unwrap();

        assert_eq!(body["reasoning"], json!({ "context": "all_turns" }));
    }

    #[test]
    fn request_local_reasoning_override_does_not_mutate_provider_default() {
        let mut session = make_session();
        session.reasoning = ReasoningSelection::High;
        let provider = make_provider(&session);

        let body = provider
            .request_body_with_reasoning(&[user_message("hello")], &[], ReasoningSelection::Medium)
            .unwrap();

        assert_eq!(body["reasoning"]["effort"], json!("medium"));
        assert_eq!(provider.reasoning, ReasoningSelection::High);
    }

    #[tokio::test]
    async fn chat_stream_captures_terminal_reason_and_reasoning_size() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thinking\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":20}}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;
        let mut session = make_session();
        session.api_key = "token".to_string();
        session.account_id = "account".to_string();
        session.base_url = server.uri();
        let provider = make_provider(&session);
        let messages = vec![user_message("hello")];

        let response = provider
            .chat_stream(
                &messages,
                &[],
                CancellationToken::new(),
                std::sync::Arc::new(crate::output::StdoutSink),
            )
            .await
            .unwrap();

        assert_eq!(response.content, "done");
        assert_eq!(
            response.finish_reason(),
            Some(&crate::provider::FinishReason::Stop)
        );
        assert_eq!(response.reasoning_chars, "thinking".chars().count());
        let diagnostics = provider.take_last_request_diagnostics().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&diagnostics.serialized_body).unwrap(),
            provider.request_body(&messages, &[]).unwrap()
        );
        assert_eq!(provider.take_last_request_diagnostics(), None);
    }

    #[tokio::test]
    async fn chat_stream_reassembles_fragmented_function_call_arguments() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"{\\\"path\\\":\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"\\\"Cargo.toml\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"read\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;
        let mut session = make_session();
        session.api_key = "token".to_string();
        session.account_id = "account".to_string();
        session.base_url = server.uri();
        let provider = make_provider(&session);

        let response = provider
            .chat_stream(
                &[user_message("read Cargo.toml")],
                &[],
                CancellationToken::new(),
                std::sync::Arc::new(crate::output::StdoutSink),
            )
            .await
            .unwrap();

        assert_eq!(response.tool_calls.len(), 1);
        crate::provider::test_utils::assert_tool_call_identity(
            &provider,
            &response.tool_calls[0],
            "call_1",
        );
        assert_eq!(response.tool_calls[0].name, "read");
        assert_eq!(
            response.tool_calls[0].arguments,
            "{\"path\":\"Cargo.toml\"}"
        );
    }

    #[tokio::test]
    async fn chat_stream_maps_incomplete_output_limit_to_length() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":100,\"output_tokens\":32}}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;
        let mut session = make_session();
        session.api_key = "token".to_string();
        session.account_id = "account".to_string();
        session.base_url = server.uri();
        let provider = make_provider(&session);

        let response = provider
            .chat_stream(
                &[user_message("hello")],
                &[],
                CancellationToken::new(),
                std::sync::Arc::new(crate::output::StdoutSink),
            )
            .await
            .unwrap();

        assert_eq!(
            response.finish_reason(),
            Some(&crate::provider::FinishReason::Length)
        );
    }

    #[test]
    fn request_body_carries_streamed_reasoning_summary() {
        let mut session = make_session();
        session.reasoning = ReasoningSelection::from_effort(ReasoningEffort::High);
        let provider = make_provider(&session);
        let body = provider
            .request_body(&[user_message("hello")], &[])
            .expect("request body builds");
        assert_eq!(body["reasoning"]["summary"], json!("detailed"));
        assert_eq!(body["reasoning"]["effort"], json!("high"));
    }

    #[test]
    fn request_body_omits_reasoning_when_reasoning_is_off() {
        let mut session = make_session();
        session.reasoning = ReasoningSelection::Off;
        let provider = make_provider(&session);
        let body = provider
            .request_body(&[user_message("hello")], &[])
            .expect("request body builds");

        assert!(
            body.get("reasoning").is_none(),
            "explicit reasoning off must not send a reasoning payload: {body}"
        );
    }

    #[test]
    fn models_from_response_filters_visible_slugs() {
        let value = json!({
            "models": [
                {"slug": "gpt-5.5", "visibility": "list"},
                {"slug": "hidden", "visibility": "hide"},
                {"model": "unsupported", "supported_in_api": false},
                {"model": "gpt-5.4"}
            ]
        });
        let models = models_from_response(&value)
            .into_iter()
            .map(|model| model.remote_model_id.to_string())
            .collect::<Vec<_>>();
        assert_eq!(models, vec!["gpt-5.5", "gpt-5.4"]);
    }

    #[test]
    fn models_from_response_accepts_openai_data_shape() {
        let value = json!({
            "data": [
                {"id": "gpt-5.3-codex-spark"},
                {"id": "gpt-5.3-codex-spark"},
                {"name": "gpt-5.3-codex"}
            ]
        });

        let models = models_from_response(&value)
            .into_iter()
            .map(|model| model.remote_model_id.to_string())
            .collect::<Vec<_>>();
        assert_eq!(models, vec!["gpt-5.3-codex-spark", "gpt-5.3-codex"]);
    }

    #[test]
    fn models_from_response_preserves_codex_reasoning_metadata() {
        let value = json!({
            "models": [{
                "slug": "gpt-5.6-sol",
                "display_name": "GPT-5.6 Sol",
                "visibility": "list",
                "context_window": 372000,
                "default_reasoning_level": "medium",
                "supported_reasoning_levels": [
                    {"effort": "low", "description": "Fast"},
                    {"effort": "medium", "description": "Balanced"},
                    {"effort": "high", "description": "Deep"},
                    {"effort": "xhigh", "description": "Deeper"},
                    {"effort": "max", "description": "Maximum"},
                    {"effort": "ultra", "description": "Ultra"}
                ],
                "input_modalities": ["text", "image"],
                "supports_parallel_tool_calls": true
                ,"use_responses_lite": true
            }]
        });

        let models = models_from_response(&value);
        let model = models.first().expect("parsed Codex model");
        assert_eq!(model.remote_model_id.as_ref(), "gpt-5.6-sol");
        assert_eq!(model.display_name.as_deref(), Some("GPT-5.6 Sol"));
        assert_eq!(model.context_window, Some(372_000));
        assert!(model.use_responses_lite);
        assert_eq!(
            model.supported_reasoning,
            vec![
                ReasoningSelection::Low,
                ReasoningSelection::Medium,
                ReasoningSelection::High,
                ReasoningSelection::XHigh,
                ReasoningSelection::Max,
                ReasoningSelection::Ultra,
            ]
        );
        assert_eq!(
            model.recommended_reasoning,
            Some(ReasoningSelection::Medium)
        );
        assert_eq!(
            model.features,
            vec![
                ModelFeature::Reasoning,
                ModelFeature::Attachment,
                ModelFeature::ToolCall,
            ]
        );
    }

    #[test]
    fn codex_model_parser_ignores_zero_context_window() {
        let model = available_model_from_codex_item(
            "gpt-zero",
            &json!({"slug": "gpt-zero", "context_window": 0}),
        );
        assert_eq!(model.context_window, None);
    }

    #[test]
    fn codex_fallback_models_excludes_unsupported_old_default() {
        assert_eq!(
            codex_fallback_models(),
            vec![
                "gpt-6.1-sol".to_string(),
                "gpt-6-astra".to_string(),
                "gpt-5.6-sol".to_string(),
                "gpt-5.6-terra".to_string(),
                "gpt-5.6-luna".to_string(),
                "gpt-5.5".to_string(),
                "gpt-5.4".to_string(),
                "gpt-5.4-mini".to_string()
            ]
        );
    }

    #[test]
    fn codex_client_version_parser_accepts_cli_output_and_prereleases() {
        assert_eq!(
            parse_codex_client_version("codex-cli 0.144.1"),
            Some("0.144.1".to_string())
        );
        assert_eq!(
            parse_codex_client_version("codex-cli v0.145.0-alpha.1"),
            Some("0.145.0".to_string())
        );
    }

    #[test]
    fn client_version_floor_ignores_outdated_detected_cli() {
        // The backend 404s lite-served models below the floor, so an old
        // local install must not win over the fallback.
        assert_eq!(
            floored_client_version(Some("0.144.1".to_string())),
            CODEX_FALLBACK_CLIENT_VERSION
        );
        assert_eq!(
            floored_client_version(Some("0.158.0".to_string())),
            CODEX_FALLBACK_CLIENT_VERSION
        );
        assert_eq!(
            floored_client_version(Some("0.159.3".to_string())),
            "0.159.3"
        );
        assert_eq!(floored_client_version(None), CODEX_FALLBACK_CLIENT_VERSION);
    }

    #[test]
    fn codex_client_version_parser_rejects_non_versions() {
        assert_eq!(parse_codex_client_version("codex-cli latest"), None);
    }

    #[tokio::test]
    async fn list_models_sends_current_codex_client_version() {
        use wiremock::matchers::{header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client_version = codex_client_version().await.to_string();
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(query_param("client_version", &client_version))
            .and(header("version", &client_version))
            .and(header("ChatGPT-Account-ID", "codex-account"))
            .and(header("OAI-Product-Sku", "codex"))
            .and(header("originator", "codex_cli_rs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"slug": "gpt-new", "visibility": "list"}]
            })))
            .mount(&server)
            .await;
        let mut session = make_session();
        session.api_key = "codex-token".to_string();
        session.account_id = "codex-account".to_string();
        session.base_url = server.uri();

        let models = make_provider(&session).list_models().await.unwrap();

        assert_eq!(models, vec!["gpt-new".to_string()]);
    }

    #[tokio::test]
    async fn list_models_uses_fallback_when_response_is_empty() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(&server)
            .await;
        let mut session = make_session();
        session.api_key = "codex-token".to_string();
        session.account_id = "codex-account".to_string();
        session.base_url = server.uri();

        let models = make_provider(&session).list_models().await.unwrap();

        assert_eq!(models, codex_fallback_models());
    }

    #[tokio::test]
    async fn responses_lite_sends_internal_routing_header() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const THREAD_ID: &str = "01990abc-1234-7def-8123-456789abcdef";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("x-openai-internal-codex-responses-lite", "true"))
            .and(header("x-client-request-id", THREAD_ID))
            .and(header("session-id", THREAD_ID))
            .and(header("thread-id", THREAD_ID))
            .respond_with(ResponseTemplate::new(200).set_body_string(concat!(
                "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
                "data: [DONE]\n\n",
            )))
            .mount(&server)
            .await;
        let mut session = make_session();
        session.api_key = "token".to_string();
        session.account_id = "account".to_string();
        session.base_url = server.uri();

        let mut provider = make_lite_provider(&session);
        provider.set_conversation_cache_key(THREAD_ID);
        provider
            .chat_stream(
                &[user_message("hello")],
                &[],
                CancellationToken::new(),
                std::sync::Arc::new(crate::output::StdoutSink),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn authorize_rejects_api_key_input() {
        let result = CodexFactory
            .authorize(AuthInput::ApiKey {
                api_key: "token-abc".to_string(),
                persistence: crate::session::CredentialPersistence::File,
                base_url: None,
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn authorize_rejects_env_input() {
        let result = CodexFactory.authorize(AuthInput::FromEnv).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn is_authorized_requires_both_token_and_account_id() {
        let factory = CodexFactory;
        let mut session = make_session();
        assert!(!factory.is_authorized(&session));
        session.api_key = "token".to_string();
        assert!(!factory.is_authorized(&session));
        session.account_id = "account".to_string();
        assert!(factory.is_authorized(&session));
    }

    #[tokio::test]
    async fn clear_authorization_wipes_credentials() {
        let factory = CodexFactory;
        let mut session = make_session();
        session.api_key = "token".to_string();
        session.account_id = "account".to_string();
        session.is_fedramp_account = true;
        factory.clear_authorization(&mut session);
        assert!(session.api_key.is_empty());
        assert!(session.account_id.is_empty());
        assert!(!session.is_fedramp_account);
    }

    #[test]
    fn require_codex_account_id_bails_on_empty() {
        let credential = CodexCredential {
            access_token: "token".to_string(),
            account_id: None,
            is_fedramp_account: false,
        };
        let result = require_codex_account_id(&credential);
        assert!(result.is_err());
    }
}
