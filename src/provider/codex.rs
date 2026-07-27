use std::collections::HashMap;
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
/// backend version-gates lite-served models (gpt-5.6-*) at roughly 0.145 —
/// below that, /responses answers 404 "Model not found" for them (and 400
/// "requires a newer version of Codex" further back) even though /models
/// lists them. A detected CLI older than this floor must not drag the header
/// down (verified live: 0.144.1 → 404, 0.150.0 → 200).
const CODEX_FALLBACK_CLIENT_VERSION: &str = "0.150.0";
const CODEX_CLIENT_VERSION_TIMEOUT: Duration = Duration::from_secs(2);
const CODEX_MODELS_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);
const CODEX_FALLBACK_MODELS: [&str; 6] = [
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
    /// Reasoning items captured per assistant turn, keyed by that turn's first
    /// tool-call id (the `call_id` that round-trips into the rebuilt assistant
    /// message). Rebuilt empty per conversation, so resumed history simply omits
    /// reasoning rather than sending orphaned items.
    reasoning_by_call_id: Mutex<HashMap<String, Vec<Value>>>,
    last_request_diagnostics: Mutex<Option<ProviderRequestDiagnostics>>,
}

impl CodexProvider {
    pub fn new(session: &ProviderSession, target: &RunTarget) -> Self {
        let prompt_cache_key = crate::provider::new_conversation_cache_key();
        let routing_thread_id = crate::provider::codex_routing_thread_id(&prompt_cache_key);
        Self {
            http: crate::provider::http_client(),
            model: target.remote_model_id.to_string(),
            base_url: target.base_url.trim().trim_end_matches('/').to_string(),
            endpoint_path: target
                .endpoint_path
                .as_deref()
                .unwrap_or("responses")
                .to_string(),
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
            reasoning_by_call_id: Mutex::new(HashMap::new()),
            last_request_diagnostics: Mutex::new(None),
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

    fn request_body_with_reasoning(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        reasoning: ReasoningSelection,
    ) -> Result<Value> {
        // Hold the reasoning map (if persisting) only for the synchronous
        // rebuild; the lock is never held across an await.
        let stored_reasoning = self.persist_reasoning.then(|| {
            self.reasoning_by_call_id
                .lock()
                .expect("reasoning map lock")
        });
        // Match the official Codex client for the ChatGPT backend: send one
        // stable `prompt_cache_key` and keep the reusable prefix byte-stable.
        // This backend rejects the public Responses API's
        // `prompt_cache_options` and explicit content breakpoints.
        let wire_messages = transform::messages_for_project_state_layout(
            messages,
            transform::ProjectStateWireLayout::AppendOnly,
        );
        // Vision safety net: mirror of the OpenAI-chat/Anthropic strip — an
        // image already in history must not 400 every later turn when the
        // active model rejects image input.
        let wire_messages = if self.supports_vision {
            wire_messages
        } else {
            transform::strip_image_parts_for_wire(wire_messages.as_ref())
        };
        // IMPORTANT OFFICIAL-CODEX PARITY: emit the native Responses items
        // exactly. A synthetic developer "cache checkpoint" was live-tested
        // and pinned reuse to only the original ~14k prefix instead of letting
        // history grow; the official client sends no such item. Do not add
        // cache-control prose to the model input.
        let (instructions, input) =
            messages_to_responses_input(wire_messages.as_ref(), stored_reasoning.as_deref())?;
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
        let serialized_body = crate::provider::serialize_request_body(
            "Codex",
            || self.ensure_authorized(),
            || {
                self.request_body_with_reasoning(
                    messages,
                    tools,
                    options.reasoning.unwrap_or(self.reasoning),
                )
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

        let reasoning_store = self.persist_reasoning.then_some(&self.reasoning_by_call_id);
        parse_responses_stream(response, cancellation_token, sink, reasoning_store).await
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

/// Extract a re-sendable reasoning item from a `response.output_item.done`
/// event as a clean Responses *input* item (`type`/`id`/`summary` plus
/// `encrypted_content` when present), so it threads into the next request
/// verbatim. `None` for any non-reasoning item.
fn reasoning_item_from_done_event(event: &Value) -> Option<Value> {
    let item = event.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return None;
    }
    let id = item.get("id").and_then(Value::as_str)?;
    // Under `store:false` a reasoning item can only be replayed with its
    // encrypted payload; a summary-only item would be rejected on the way back
    // in, so skip capturing it rather than poison the next request.
    let encrypted = item.get("encrypted_content").and_then(Value::as_str)?;
    Some(json!({
        "type": "reasoning",
        "id": id,
        "summary": item.get("summary").cloned().unwrap_or_else(|| json!([])),
        "encrypted_content": encrypted,
    }))
}

fn messages_to_responses_input(
    messages: &[ChatCompletionRequestMessage],
    reasoning_by_call_id: Option<&HashMap<String, Vec<Value>>>,
) -> Result<(String, Vec<Value>)> {
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
                let tool_calls = value.get("tool_calls").and_then(Value::as_array);

                // Reasoning precedes the turn's output items in the Responses
                // protocol. Re-emit the captured items (keyed by the turn's first
                // tool-call id) verbatim, ahead of the assistant text and calls.
                if let Some(items) =
                    transform::replay_items_for(reasoning_by_call_id, tool_calls.map(Vec::as_slice))
                {
                    input.extend(items.iter().cloned());
                }

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

async fn parse_responses_stream(
    response: reqwest::Response,
    cancellation_token: CancellationToken,
    sink: SharedSink,
    reasoning_store: Option<&Mutex<HashMap<String, Vec<Value>>>>,
) -> crate::provider::ProviderResult<StreamedResponse> {
    let mut content = String::new();
    let mut tool_call_arguments: HashMap<String, String> = HashMap::new();
    let mut tool_calls = Vec::new();
    let mut token_usage: Option<crate::provider::TokenUsage> = None;
    let mut reasoning_items: Vec<Value> = Vec::new();
    // Whether any reasoning-summary text has streamed yet, so a new summary part
    // renders as a fresh paragraph rather than a leading blank line.
    let mut reasoning_started = false;
    let mut reasoning_chars = 0usize;
    let mut finish_reason: Option<crate::provider::FinishReason> = None;
    let mut terminal_seen = false;

    // Codex sends bare `data:` frames (event name is unused); the JSON payload
    // carries its own `type` discriminator.
    let interrupted = sse::drive_sse(response, cancellation_token, |_event, data| {
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
            Some("response.output_text.delta") => {
                if let Some(delta) = json.get("delta").and_then(Value::as_str) {
                    content.push_str(delta);
                    if !delta.is_empty() {
                        sink.assistant_delta(delta);
                    }
                }
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
                if let Some(tool_call) =
                    tool_calls::responses_tool_call_from_done_event(&json, &tool_call_arguments)
                {
                    tool_calls.push(tool_call);
                } else if let Some(reasoning) = reasoning_item_from_done_event(&json) {
                    reasoning_items.push(reasoning);
                }
            }
            Some("response.completed") => {
                terminal_seen = true;
                finish_reason = Some(crate::provider::FinishReason::Stop);
                if let Some(u) = json.pointer("/response/usage") {
                    token_usage = Some(usage::responses_usage_from_value(u));
                }
            }
            Some("response.incomplete") => {
                terminal_seen = true;
                let reason = json
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("length");
                finish_reason = Some(crate::provider::FinishReason::from_openai(reason));
                if let Some(u) = json.pointer("/response/usage") {
                    token_usage = Some(usage::responses_usage_from_value(u));
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

    // Codex doesn't parse think tags, so there's no splitter tail to flush; just
    // signal end-of-message when the assistant produced text.
    let _ = streaming::finish_text_stream(None, &mut content, &sink);

    // Remember this turn's reasoning for replay ahead of its first tool call
    // (see `streaming::stash_reasoning_for_replay` for the keying contract).
    //
    // We thread reasoning only for the canonical single-reasoning turn. The
    // rebuilt assistant message has lost the original interleaving of reasoning
    // vs. function-call items, so a turn with multiple reasoning segments can't
    // be replayed in an order the Responses API (store:false) accepts — skip it
    // rather than risk an adjacency 400.
    if let Some(store) = reasoning_store {
        let single_reasoning_turn = reasoning_items.len() == 1;
        streaming::stash_reasoning_for_replay(
            store,
            &tool_calls,
            single_reasoning_turn.then_some(reasoning_items),
        )?;
    }

    if matches!(finish_reason, Some(crate::provider::FinishReason::Stop)) && !tool_calls.is_empty()
    {
        finish_reason = Some(crate::provider::FinishReason::ToolCalls);
    }

    let terminal = if interrupted {
        crate::provider::StreamTerminal::Interrupted
    } else if matches!(finish_reason, Some(crate::provider::FinishReason::Length)) {
        crate::provider::StreamTerminal::Incomplete(
            finish_reason.unwrap_or(crate::provider::FinishReason::Length),
        )
    } else {
        crate::provider::StreamTerminal::Completed(
            finish_reason.unwrap_or(crate::provider::FinishReason::Stop),
        )
    };

    Ok(StreamedResponse {
        content,
        tool_calls,
        terminal,
        usage: token_usage,
        reasoning_chars,
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
    let account = codex_keyring_account(codex_home);
    let serialized = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
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
        CodexProvider::new(session, &target)
    }

    fn make_provider_with_output_limit(session: &ProviderSession) -> CodexProvider {
        let mut target = crate::provider::fallback_run_target(&CODEX_METADATA, session);
        target.output_limit = Some(128_000);
        CodexProvider::new(session, &target)
    }

    fn make_lite_provider(session: &ProviderSession) -> CodexProvider {
        let mut target = crate::provider::fallback_run_target(&CODEX_METADATA, session);
        target.use_responses_lite = true;
        CodexProvider::new(session, &target)
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
        let (instructions, input) = messages_to_responses_input(&messages, None).unwrap();

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
        let (_, first_input) = messages_to_responses_input(&first_messages, None).unwrap();

        let mut second_messages = first_messages.clone();
        second_messages.push(tool_call_message("call-2", "read", "{}"));
        second_messages.push(tool_result_message("call-2", "second"));
        let (_, second_input) = messages_to_responses_input(&second_messages, None).unwrap();

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
        let (base_instructions, base_input) = messages_to_responses_input(
            &[system_message("persona head"), user_message("hi")],
            None,
        )
        .unwrap();
        assert_eq!(base_instructions, "persona head");
        assert_eq!(base_input.len(), 1);

        // Adding a later system message (e.g. a skill load) must NOT change
        // `instructions` — that would rewrite the cached prefix. It lands as an
        // in-position developer input item instead.
        let (instructions, input) = messages_to_responses_input(
            &[
                system_message("persona head"),
                user_message("hi"),
                system_message("Skill: deploy\nrun the deploy steps"),
                user_message("go"),
            ],
            None,
        )
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
    fn reasoning_item_from_done_event_extracts_input_shaped_item() {
        let event = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "thinking"}],
                "encrypted_content": "ENC",
                "status": "completed"
            }
        });
        let item = reasoning_item_from_done_event(&event).expect("reasoning item");
        assert_eq!(item["type"], json!("reasoning"));
        assert_eq!(item["id"], json!("rs_1"));
        assert_eq!(item["encrypted_content"], json!("ENC"));
        assert!(
            item.get("status").is_none(),
            "output-only status must not be echoed back as input",
        );

        let function_call = json!({"item": {"type": "function_call", "id": "fc_1"}});
        assert!(reasoning_item_from_done_event(&function_call).is_none());

        // A reasoning item without an encrypted payload can't be replayed under
        // `store:false`, so it is not captured.
        let summary_only = json!({
            "item": {"type": "reasoning", "id": "rs_2", "summary": []}
        });
        assert!(reasoning_item_from_done_event(&summary_only).is_none());
    }

    #[test]
    fn responses_input_threads_reasoning_ahead_of_its_tool_call() {
        let messages = vec![
            user_message("do it"),
            tool_call_message("call_1", "read", "{}"),
            tool_result_message("call_1", "ok"),
        ];
        let mut map: HashMap<String, Vec<Value>> = HashMap::new();
        map.insert(
            "call_1".to_string(),
            vec![json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "ENC"})],
        );

        let (_instructions, input) = messages_to_responses_input(&messages, Some(&map)).unwrap();
        let reasoning_pos = input.iter().position(|i| i["type"] == json!("reasoning"));
        let call_pos = input
            .iter()
            .position(|i| i["type"] == json!("function_call"));
        assert!(
            reasoning_pos.is_some() && reasoning_pos < call_pos,
            "reasoning item must be emitted immediately before its function_call",
        );
        assert_eq!(input[reasoning_pos.unwrap()]["id"], json!("rs_1"));

        // Default (no map) emits no reasoning items — wire shape is unchanged.
        let (_i, plain) = messages_to_responses_input(&messages, None).unwrap();
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
        assert_eq!(response.tool_calls[0].id, "call_1");
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
            floored_client_version(Some("0.150.0".to_string())),
            "0.150.0"
        );
        assert_eq!(
            floored_client_version(Some("0.151.2".to_string())),
            "0.151.2"
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
