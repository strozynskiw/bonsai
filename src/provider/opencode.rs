use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionTool, ChatCompletionTools,
    CreateChatCompletionRequestArgs,
};
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::model_catalog::{LiveModelAvailability, RunTarget};
use crate::output::SharedSink;
use crate::provider::openai_catalog::{model_ids_from_response, normalize_openai_base_url};
use crate::provider::reasoning::ReasoningCodec;
use crate::provider::think_tags::ThinkTagSplitter;
use crate::provider::transform;
use crate::provider::{
    NO_PARAMETERS, NO_REASONING, Protocol, Provider, ProviderCapabilities, ProviderFactory,
    ProviderMetadata, ProviderRequestDiagnostics, ProviderRequestPreview, StreamedResponse,
    TokenCounterKind, WireField, sse, streaming, tool_calls, usage, wire_sections_from_body,
};
use crate::session::ProviderSession;
use crate::util::tool_args::normalize_tool_call_arguments_json;

pub static OPENCODE_METADATA: LazyLock<ProviderMetadata> = LazyLock::new(|| {
    ProviderMetadata::new(
        "opencode",
        "OpenCode Go",
        "qwen3.7-max",
        "https://opencode.ai/zen/go/v1",
        Some("OPENCODE_API_KEY"),
        Some("OPENCODE_MODEL"),
        Some("OPENCODE_BASE_URL"),
        &["qwen3.7-max", "glm-5.2"],
        Protocol::OpenAiChat,
        // IMPORTANT CACHE INVARIANT: the zen backends cache prompts server-side
        // (usage frames report cache_read tokens for qwen and GLM alike —
        // 1.69M cache-read tokens were measured live with this flag OFF).
        // Do not remove this capability: it activates Bonsai's stable cache key,
        // while the Chat Completions wire policy keeps mutable project state
        // after stable history. Keeping that state inside the system head made
        // 20 of 91 turns re-tokenize 40k+ prompts from byte 0.
        // The Go gateway's DeepSeek adapter additionally requires `reasoning_content`
        // on assistant tool-call messages — see
        // [`ensure_reasoning_content_on_tool_call_messages`].
        ProviderCapabilities::new(NO_REASONING, NO_PARAMETERS)
            .with_prompt_cache()
            .with_reasoning_content_echo(),
        "chat/completions",
    )
    .with_context_window(128_000)
    .with_token_counter(TokenCounterKind::Qwen3)
});

pub struct OpenCodeFactory;

#[async_trait]
impl ProviderFactory for OpenCodeFactory {
    fn metadata(&self) -> &ProviderMetadata {
        &OPENCODE_METADATA
    }

    async fn list_models(&self, session: &ProviderSession) -> Result<Vec<String>> {
        Ok(self
            .list_available_models(session)
            .await?
            .remote_model_ids())
    }

    async fn list_available_models(
        &self,
        session: &ProviderSession,
    ) -> Result<LiveModelAvailability> {
        Ok(LiveModelAvailability::from_remote_ids(
            fetch_opencode_model_catalog(session).await?,
        ))
    }
}

async fn fetch_opencode_model_catalog(session: &ProviderSession) -> Result<Vec<String>> {
    let base_url = if session.base_url.trim().is_empty() {
        OPENCODE_METADATA.default_base_url.as_ref()
    } else {
        session.base_url.trim().trim_end_matches('/')
    };
    let endpoint = format!("{base_url}/models");
    let response = crate::provider::http_client()
        .get(endpoint)
        .bearer_auth(&session.api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .context("Failed to fetch OpenCode models")?;
    if !response.status().is_success() {
        return Err(sse::error_from_response(response).await.into());
    }
    let value: Value = response
        .json()
        .await
        .context("Failed to parse OpenCode /models response")?;
    let mut models = opencode_catalog_from_response(&value);
    models.sort();
    if models.is_empty() {
        models = OPENCODE_METADATA.seed_model_list();
    }
    Ok(models)
}

fn opencode_catalog_from_response(value: &Value) -> Vec<String> {
    model_ids_from_response(value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiKeyPolicy {
    Required,
    Optional,
}

pub struct OpenAiCompatibleProvider {
    provider_id: String,
    http: reqwest::Client,
    model: String,
    base_url: String,
    api_key: String,
    endpoint_path: String,
    output_limit: Option<u32>,
    reasoning: crate::provider::ReasoningSelection,
    reasoning_escalation: Option<crate::provider::ReasoningSelection>,
    reasoning_codec: ReasoningCodec,
    api_key_policy: ApiKeyPolicy,
    /// When the backend supports OpenAI automatic prompt caching, a
    /// conversation-stable `prompt_cache_key` is sent to keep the warm prefix
    /// pinned across turns. Off for backends we have not verified accept the
    /// field (e.g. local servers), so it is never sent there. Sent suffixed
    /// with a fingerprint of the system head
    /// ([`crate::provider::lane_scoped_cache_key`]) so one conversation's
    /// prompt lanes don't collide on one cache route.
    supports_prompt_cache: bool,
    /// Whether assistant tool-call messages must carry a `reasoning_content`
    /// field. Off for every backend we have not verified wants it, since the
    /// field is not part of the Chat Completions schema.
    echoes_reasoning_content: bool,
    /// Whether a terminal usage frame is accepted as the end of a stream that
    /// never sent a `finish_reason`. Off everywhere else, so a truncated stream
    /// still fails loudly instead of returning a cut-off turn as complete.
    usage_frame_is_stream_terminal: bool,
    /// Connection/model-selected placement of volatile project state. The
    /// default preserves every compatible provider's historic wire body;
    /// OpenCode opts into rolling append-only history explicitly.
    prompt_cache_policy: crate::model_catalog::PromptCachePolicy,
    prompt_cache_key: String,
    /// Native reasoning captured for assistant tool-call turns, keyed by the
    /// first call id so it can be restored when that turn re-enters history.
    reasoning_by_call_id: std::sync::Mutex<HashMap<String, String>>,
    last_request_diagnostics: std::sync::Mutex<Option<ProviderRequestDiagnostics>>,
}

impl OpenAiCompatibleProvider {
    pub(crate) fn with_api_key_policy(
        provider_id: impl Into<String>,
        session: &ProviderSession,
        target: &RunTarget,
        api_key_policy: ApiKeyPolicy,
        capabilities: ProviderCapabilities,
    ) -> Self {
        let api_key = session.api_key.clone();
        let base_url = normalized_base_url_for_policy(&target.base_url, api_key_policy);
        let supports_prompt_cache =
            capabilities.supports_prompt_cache || is_official_openai_api_base_url(&base_url);
        let model = target.remote_model_id.to_string();

        Self {
            provider_id: provider_id.into(),
            http: crate::provider::http_client(),
            model,
            base_url,
            api_key,
            endpoint_path: target
                .endpoint_path
                .as_deref()
                .unwrap_or("chat/completions")
                .to_string(),
            output_limit: target.output_limit,
            reasoning: target.reasoning,
            reasoning_escalation: target.reasoning_escalation,
            reasoning_codec: target.reasoning_codec,
            api_key_policy,
            supports_prompt_cache,
            echoes_reasoning_content: capabilities.echoes_reasoning_content,
            usage_frame_is_stream_terminal: capabilities.usage_frame_is_stream_terminal,
            prompt_cache_policy: target.prompt_cache_policy,
            prompt_cache_key: crate::provider::new_conversation_cache_key(),
            reasoning_by_call_id: std::sync::Mutex::new(HashMap::new()),
            last_request_diagnostics: std::sync::Mutex::new(None),
        }
    }

    fn ensure_authorized(&self) -> Result<()> {
        if self.api_key_policy == ApiKeyPolicy::Required && self.api_key.trim().is_empty() {
            anyhow::bail!(
                "{} is not authorized. Run /authorize {}.",
                self.provider_id,
                self.provider_id
            );
        }
        if self.base_url.trim().is_empty() {
            anyhow::bail!("{} has no base URL configured", self.provider_id);
        }
        if self.model.trim().is_empty() {
            anyhow::bail!("{} has no model configured", self.provider_id);
        }
        Ok(())
    }

    #[cfg(test)]
    fn reasoning_payload(&self) -> Option<Value> {
        self.reasoning_codec.encode_json(self.reasoning)
    }

    fn chat_completions_url(&self) -> String {
        crate::provider::transform::endpoint_url(&self.base_url, &self.endpoint_path)
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
        reasoning: crate::provider::ReasoningSelection,
    ) -> Result<Value> {
        let mut request_builder = CreateChatCompletionRequestArgs::default();
        // IMPORTANT CACHE COMPATIBILITY: automatic Chat Completions caching
        // needs each previous request to remain a literal prefix, so OpenCode's
        // verified rolling-history policy preserves immutable snapshots in
        // place. Every other compatible provider retains its historic
        // latest-state-at-end body unless its connection/model opts in. Do not
        // globalize the OpenCode choice.
        let project_state_layout = if self.prompt_cache_policy.uses_append_only_project_state() {
            transform::ProjectStateWireLayout::AppendOnly
        } else {
            transform::ProjectStateWireLayout::LatestAtEnd
        };
        let staged = transform::messages_for_project_state_layout(messages, project_state_layout);
        // Strict OpenAI-compatible chat templates (Qwen3 and other Jinja
        // templates) reject a `system` message anywhere but index 0. Bonsai
        // injects trusted harness notes (e.g. the self-review nudge) mid-stream
        // as system messages, so fold any non-leading system turn into the user
        // stream before it hits the wire. Leaves the leading system prompt — and
        // therefore the cache prefix — byte-stable.
        let request_messages = transform::demote_non_leading_system_messages(staged.as_ref());

        request_builder
            .model(&self.model)
            .messages(request_messages.as_ref())
            .stream(true);

        if !tools.is_empty() {
            let wrapped_tools: Vec<ChatCompletionTools> = tools
                .iter()
                .map(|tool| ChatCompletionTools::Function(tool.clone()))
                .collect();
            request_builder.tools(wrapped_tools);
        }

        let request = request_builder
            .build()
            .context("Failed to build chat completion request")?;

        let mut body = serde_json::to_value(&request)?;
        normalize_request_tool_call_arguments(&mut body);
        if self.echoes_reasoning_content {
            let reasoning_by_call_id = self
                .reasoning_by_call_id
                .lock()
                .map_err(|_| anyhow::anyhow!("reasoning replay state lock is poisoned"))?;
            ensure_reasoning_content_on_tool_call_messages(&mut body, &reasoning_by_call_id);
        }
        // After the reasoning echo: `reasoning_by_call_id` is keyed by original
        // wire ids, so the lookup must run before duplicates are renamed.
        dedupe_request_tool_call_ids(&mut body);
        // Ask for a final usage chunk; servers that don't support it ignore it.
        body["stream_options"] = serde_json::json!({ "include_usage": true });
        // IMPORTANT CACHE INVARIANT: pin the warm prefix to one backend across
        // turns. Do not remove or derive this key from mutable project state.
        // The transport policy emits a byte-stable first system message, so
        // hashing it keeps plan-mode and coding-mode turns on separate cache
        // routes. Only verified backends receive the field, so strict servers
        // never see it.
        if self.supports_prompt_cache {
            let lane = request_messages
                .first()
                .and_then(|message| serde_json::to_value(message).ok())
                .filter(|value| value.get("role").and_then(Value::as_str) == Some("system"))
                .and_then(|value| transform::content_to_text(value.get("content")))
                .unwrap_or_default();
            body["prompt_cache_key"] = serde_json::json!(crate::provider::lane_scoped_cache_key(
                &self.prompt_cache_key,
                &lane
            ));
        }
        if self.prompt_cache_policy.uses_openrouter_anthropic_control() {
            // IMPORTANT OPENROUTER CACHE CONTRACT: Claude models require the
            // documented top-level control; `prompt_cache_key` alone does not
            // activate caching. `session_id` pins every turn to the same
            // upstream before the first hit. Keep this model-selectable: these
            // fields are not valid universal Chat Completions parameters.
            body["cache_control"] = serde_json::json!({ "type": "ephemeral" });
            body["session_id"] = serde_json::json!(&self.prompt_cache_key);
        }
        if let Some(output_limit) = self.output_limit {
            let field = if self.reasoning_codec == ReasoningCodec::OpenAiChatCompletions {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            body[field] = serde_json::json!(output_limit);
        }
        self.reasoning_codec
            .apply_chat_completions_fields(&mut body, reasoning);
        if self.reasoning_codec == ReasoningCodec::OpenAiChatCompletions && !tools.is_empty() {
            body["parallel_tool_calls"] = serde_json::json!(true);
        }
        Ok(body)
    }

    fn request_preview_from_body(&self, body: Value) -> ProviderRequestPreview {
        let mut sections = wire_sections_from_body(
            &body,
            &[
                WireField {
                    id: "wire-messages",
                    label: "Messages",
                    key: "messages",
                },
                WireField {
                    id: "wire-tools",
                    label: "Tools",
                    key: "tools",
                },
            ],
            true,
        );
        crate::provider::annotate_cache_control_sections(&mut sections, &body);
        ProviderRequestPreview::with_wire_sections(
            "POST",
            self.chat_completions_url(),
            body,
            sections,
        )
    }
}

/// Give every assistant message that carries `tool_calls` its captured
/// `reasoning_content`, defaulting to an empty string when the provider did not
/// return one (including history restored from disk).
///
/// The OpenCode Go gateway's DeepSeek adapter rejects a request whose assistant
/// tool-call messages lack this field: the call fails with HTTP 400 `Error from
/// provider (Console Go): Upstream request failed`, which names neither the
/// field nor the message. Every agent turn after the first tool call replays
/// those messages, so without this the deepseek-v4 models are unusable for tool
/// work on that connection — the very first tool result 400s.
///
/// Deliberate, don't simplify:
/// - The empty-string fallback is a real fix, not a placeholder. DeepSeek's
///   adapter checks that the key is present even when no native trace exists.
///   Kimi and GLM instead receive the exact trace captured during this provider
///   instance's live tool round.
/// - It must go on *every* assistant tool-call message in the history, not just
///   the newest: a single message missing it 400s the whole request.
/// - It stays gated on [`ProviderCapabilities::echoes_reasoning_content`]
///   rather than being sent everywhere. The field is off-schema, and strict
///   servers reject unknown message fields.
fn ensure_reasoning_content_on_tool_call_messages(
    body: &mut Value,
    reasoning_by_call_id: &HashMap<String, String>,
) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        let Some(message) = message.as_object_mut() else {
            continue;
        };
        // Presence of the key is not enough: a plain assistant message can
        // serialize as `"tool_calls": null`, and those must stay untouched.
        let tool_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .filter(|tool_calls| !tool_calls.is_empty());
        let Some(tool_calls) = tool_calls else {
            continue;
        };
        let reasoning = tool_calls
            .first()
            .and_then(|tool_call| tool_call.get("id"))
            .and_then(Value::as_str)
            .and_then(|call_id| reasoning_by_call_id.get(call_id))
            .cloned()
            .unwrap_or_default();
        message
            .entry("reasoning_content")
            .or_insert_with(|| Value::String(reasoning));
    }
}

/// Kimi K2 mints deterministic `name:index` tool-call ids, so a replayed
/// history can carry the same id twice — verified live against
/// api.moonshot.ai: two parallel calls sharing an id inside ONE assistant
/// message get HTTP 400 "tool call id ... is duplicated", wedging the session
/// on every retry. (Cross-turn repeats are accepted today, but the rewrite
/// keeps ids request-unique anyway — the error message doesn't promise where
/// the server checks.) Chat Completions is stateless: ids only need
/// request-internal consistency, so keep each first occurrence and rename
/// later duplicates. Every original id maps to a queue of assigned wire ids
/// in call order, and the tool results that follow the assistant message
/// consume that queue positionally, so result N always follows call N even
/// when both replayed the same original id. The rewrite is a deterministic
/// function of message order, keeping repeated requests byte-stable for
/// prefix caching. Deliberate mixed-model seam — do not "simplify" by
/// trusting wire ids to be unique.
fn dedupe_request_tool_call_ids(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let mut seen: HashSet<String> = HashSet::new();
    // Current assistant turn only: original id -> queue of assigned wire ids.
    let mut assigned: HashMap<String, std::collections::VecDeque<String>> = HashMap::new();
    for message in messages {
        let Some(message) = message.as_object_mut() else {
            continue;
        };
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                // A new assistant turn owns the tool results that follow it;
                // assignments from earlier turns must not leak onto them.
                assigned.clear();
                let Some(tool_calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut)
                else {
                    continue;
                };
                for tool_call in tool_calls {
                    let Some(id) = tool_call.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let id = id.to_string();
                    let unique = if seen.insert(id.clone()) {
                        id.clone()
                    } else {
                        let mut suffix = 2usize;
                        loop {
                            // `-dup2` over `#2`: strict gateways are pickier
                            // about id charsets than about length.
                            let candidate = format!("{id}-dup{suffix}");
                            if seen.insert(candidate.clone()) {
                                break candidate;
                            }
                            suffix += 1;
                        }
                    };
                    if unique != id {
                        tool_call["id"] = Value::String(unique.clone());
                    }
                    assigned.entry(id).or_default().push_back(unique);
                }
            }
            Some("tool") => {
                let next = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .and_then(|id| assigned.get_mut(id))
                    .and_then(std::collections::VecDeque::pop_front);
                // An exhausted queue means more results than calls — leave the
                // extra result untouched rather than guess a target.
                if let Some(next) = next {
                    message.insert("tool_call_id".to_string(), Value::String(next));
                }
            }
            _ => {}
        }
    }
}

fn normalize_request_tool_call_arguments(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        let Some(tool_calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) else {
            continue;
        };
        for tool_call in tool_calls {
            let Some(function) = tool_call.get_mut("function").and_then(Value::as_object_mut)
            else {
                continue;
            };
            let arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            function.insert(
                "arguments".to_string(),
                Value::String(normalize_tool_call_arguments_json(arguments)),
            );
        }
    }
}

fn normalized_base_url_for_policy(base_url: &str, api_key_policy: ApiKeyPolicy) -> String {
    if base_url.trim().is_empty() {
        return String::new();
    }
    match api_key_policy {
        ApiKeyPolicy::Required => base_url.trim().trim_end_matches('/').to_string(),
        ApiKeyPolicy::Optional => normalize_openai_base_url(base_url)
            .unwrap_or_else(|_| base_url.trim().trim_end_matches('/').to_string()),
    }
}

fn is_official_openai_api_base_url(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("api.openai.com"))
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn project_state_cache_strategy(&self) -> crate::provider::ProjectStateCacheStrategy {
        if self.prompt_cache_policy.uses_append_only_project_state() {
            // IMPORTANT OPENCODE CACHE CONTRACT: Chat Completions has no
            // explicit breakpoint that can exclude a moving state tail.
            // Immutable snapshots make every prior request an exact prefix;
            // Qwen's Anthropic transport reshapes the same stored history
            // behind its explicit breakpoint.
            crate::provider::ProjectStateCacheStrategy::AppendOnlyHistory
        } else {
            crate::provider::ProjectStateCacheStrategy::MutableSystemTail
        }
    }

    fn set_conversation_cache_key(&mut self, key: &str) {
        if !key.trim().is_empty() {
            self.prompt_cache_key = key.to_owned();
        }
    }

    fn reasoning(&self) -> crate::provider::ReasoningSelection {
        self.reasoning
    }

    fn reasoning_escalation(&self) -> Option<crate::provider::ReasoningSelection> {
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
        self.ensure_authorized()
            .map_err(|error| crate::provider::ProviderFailure::configuration(error.to_string()))?;

        let body = self
            .request_body_with_reasoning(
                messages,
                tools,
                options.reasoning.unwrap_or(self.reasoning),
            )
            .map_err(|error| crate::provider::ProviderFailure::configuration(error.to_string()))?;
        let request_preview = self.request_preview_from_body(body);
        let serialized_body =
            ProviderRequestDiagnostics::capture(request_preview, &self.last_request_diagnostics)
                .map_err(|error| {
                    crate::provider::ProviderFailure::configuration(format!(
                        "Failed to serialize OpenAI-compatible request body: {error}"
                    ))
                })?;

        let mut builder = self
            .http
            .post(self.chat_completions_url())
            .header("Content-Type", "application/json")
            .body(serialized_body);
        if !self.api_key.trim().is_empty() {
            builder = builder.header("Authorization", format!("Bearer {}", self.api_key));
        }
        let Some(response) = streaming::send_json_stream(builder, &cancellation_token).await?
        else {
            return Ok(StreamedResponse::interrupted());
        };

        let mut content = String::new();
        let mut tool_call_accumulator = tool_calls::ChatToolCallAccumulator::default();
        let mut token_usage: Option<crate::provider::TokenUsage> = None;
        // Whether any choice carried a terminal `finish_reason`. A stream that
        // ends without one (and without a usage frame) did not complete — the
        // connection dropped mid-response. Indistinguishable from success
        // without this check: an observed final turn streamed ~53k tokens of
        // GLM reasoning, the connection died at a frame boundary, and the
        // empty remainder was treated as a clean "task finished".
        let mut finish_reason: Option<crate::provider::FinishReason> = None;
        let mut reasoning_chars = 0usize;
        let mut reasoning_for_replay = String::new();
        // Open models on this transport (DeepSeek, Qwen, GLM, …) emit reasoning
        // inline as `<think>…</think>` in the content channel; route it to the
        // reasoning channel rather than letting the tags leak into the answer.
        // Official OpenAI-style models never put `<think>` in content, so this is
        // inert for them.
        let mut splitter = ThinkTagSplitter::new();

        // OpenAI-compatible chat completions send bare `data:` frames (event
        // name unused) whose JSON carries the streamed delta.
        let interrupted = sse::drive_sse(response, cancellation_token, |_event, data| {
            let Some(json) = sse::parse_frame(data)? else {
                return Ok(());
            };

            // An OpenAI-compatible stream can deliver an error as a data frame
            // (e.g. a mid-stream rate-limit/overload). It matches neither the
            // usage nor the choices branch below, so without this it is silently
            // dropped and the turn completes as an empty success. Type it so
            // transient classes are retried instead of swallowed. Guard on a
            // non-empty object/string so a benign `error: false`/`""`/`{}` on a
            // normal chunk doesn't fail the turn.
            if let Some(error) = json.get("error").filter(|error| {
                error.as_object().is_some_and(|object| !object.is_empty())
                    || error.as_str().is_some_and(|text| !text.is_empty())
            }) {
                return Err(sse::stream_error_from_object(
                    error,
                    "OpenAI-compatible stream error",
                    error,
                ));
            }

            // The final chunk (with include_usage) carries totals and no choices.
            if let Some(u) = json.get("usage").filter(|u| !u.is_null()) {
                token_usage = Some(usage::chat_completions_usage_from_value(u));
            }

            if let Some(choices) = json["choices"].as_array() {
                for choice in choices {
                    if let Some(reason) = choice
                        .get("finish_reason")
                        .and_then(Value::as_str)
                        .filter(|reason| !reason.is_empty())
                    {
                        finish_reason = Some(crate::provider::FinishReason::from_openai(reason));
                    }
                    let delta = &choice["delta"];

                    if let Some(text) = delta["content"].as_str() {
                        let split = splitter.push(text);
                        if !split.visible.is_empty() {
                            content.push_str(&split.visible);
                            sink.assistant_delta(&split.visible);
                        }
                        if !split.reasoning.is_empty() {
                            reasoning_chars =
                                reasoning_chars.saturating_add(split.reasoning.chars().count());
                            reasoning_for_replay.push_str(&split.reasoning);
                            sink.reasoning_delta(&split.reasoning);
                        }
                    }

                    // Providers surface streamed reasoning under different keys:
                    // OpenAI Responses uses reasoning_summary / reasoning_summary_text,
                    // DeepSeek uses reasoning_content, and OpenRouter/vLLM use
                    // reasoning. Route whichever is present (first non-empty wins)
                    // into the reasoning sink.
                    if let Some(text) = [
                        "reasoning_summary",
                        "reasoning_summary_text",
                        "reasoning_content",
                        "reasoning",
                    ]
                    .into_iter()
                    .find_map(|key| delta[key].as_str().filter(|text| !text.is_empty()))
                    {
                        reasoning_chars = reasoning_chars.saturating_add(text.chars().count());
                        if delta["reasoning_content"].as_str() == Some(text)
                            || delta["reasoning"].as_str() == Some(text)
                        {
                            reasoning_for_replay.push_str(text);
                        }
                        sink.reasoning_delta(text);
                    }

                    if let Some(tool_calls) = delta["tool_calls"].as_array() {
                        for tc in tool_calls {
                            tool_call_accumulator.push_delta(tc);
                        }
                    }
                }
            }
            Ok(())
        })
        .await?;

        // A non-cancelled stream that never delivered a finish_reason broke
        // mid-response (connection drop at a frame boundary). Surface it as a
        // retryable transport error instead of returning the partial as a clean
        // success — an empty partial would otherwise end the whole agent run as
        // "completed" with nothing.
        //
        // `usage_frame_is_stream_terminal` backends are the sole exception, and
        // it is a per-backend opt-in rather than a global weakening: OpenCode
        // Zen's OpenAI- and Anthropic-upstream adapters (every gpt-5.x,
        // claude-opus-4-8) stream a complete answer and then close without ever
        // sending a finish_reason or `[DONE]` — their last frames are
        // `{"choices":[],"usage":{…}}` and a cost frame. Requiring finish_reason
        // there rejected complete turns. Everywhere else a missing finish_reason
        // still fails, and even on those backends a genuinely truncated stream
        // fails, because usage only arrives once the upstream finished
        // generating.
        let terminal_seen = finish_reason.is_some()
            || (self.usage_frame_is_stream_terminal && token_usage.is_some());
        if !interrupted && !terminal_seen {
            return Err(crate::provider::ProviderFailure::transport(
                "OpenAI-compatible stream ended before a terminal finish_reason",
            ));
        }

        // Flush any text held back at a possible `<think>` boundary, then signal
        // end-of-message.
        reasoning_chars = reasoning_chars.saturating_add(streaming::finish_text_stream(
            Some(&mut splitter),
            &mut content,
            &sink,
        ));

        let tool_calls = tool_call_accumulator.into_tool_calls();
        // Only `reasoning_content`-echoing backends replay reasoning (see
        // `streaming::stash_reasoning_for_replay` for the keying contract).
        if self.echoes_reasoning_content && !interrupted {
            streaming::stash_reasoning_for_replay(
                &self.reasoning_by_call_id,
                &tool_calls,
                (!reasoning_for_replay.is_empty()).then_some(reasoning_for_replay),
            )?;
        }
        let terminal = if interrupted {
            crate::provider::StreamTerminal::Interrupted
        } else {
            // A backend that ends on a usage frame alone (see above) leaves the
            // finish reason to be inferred. Infer it from what the stream
            // actually produced rather than defaulting to `Stop`, so a turn that
            // streamed tool calls is not recorded as a plain text stop.
            crate::provider::StreamTerminal::from_finish_reason(finish_reason.unwrap_or(
                if tool_calls.is_empty() {
                    crate::provider::FinishReason::Stop
                } else {
                    crate::provider::FinishReason::ToolCalls
                },
            ))
        };

        Ok(StreamedResponse {
            content,
            tool_calls,
            terminal,
            usage: token_usage,
            reasoning_chars,
        })
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        self.ensure_authorized()?;

        let mut builder = self
            .http
            .get(format!("{}/models", self.base_url.trim_end_matches('/')))
            .header("Accept", "application/json");
        if !self.api_key.trim().is_empty() {
            builder = builder.header("Authorization", format!("Bearer {}", self.api_key));
        }
        let response = builder.send().await.context("Failed to list models")?;
        if !response.status().is_success() {
            return Err(sse::error_from_response(response).await.into());
        }
        let value: Value = response
            .json()
            .await
            .context("Failed to parse /models response")?;
        let mut models = value
            .get("data")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("id").and_then(Value::as_str))
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        models.sort();
        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        unsafe_code,
        reason = "edition-2024 requires `unsafe` around std::env::{set_var,remove_var}; test-only"
    )]
    use super::*;
    use crate::provider::test_utils::{
        named_user_message, provider_session, sample_tool, system_message, tool_call_message,
        tool_result_message, user_message,
    };
    use crate::provider::{AuthInput, AuthRequirement};

    /// A backend with none of the optional wire quirks enabled.
    const PLAIN: ProviderCapabilities = ProviderCapabilities::new(NO_REASONING, NO_PARAMETERS);
    /// A backend that accepts the `prompt_cache_key` routing hint.
    const CACHING: ProviderCapabilities = PLAIN.with_prompt_cache();

    fn resolved_opencode_provider(session: &ProviderSession) -> OpenAiCompatibleProvider {
        let catalog =
            crate::model_catalog::ModelCatalog::load_builtin().expect("built-in catalog loads");
        let connection_id = OPENCODE_METADATA
            .id
            .parse::<crate::model_catalog::ConnectionId>()
            .expect("metadata id is a connection id");
        let resolved = catalog
            .resolve_connection_model(&connection_id, &session.model)
            .expect("opencode test model resolves");
        let target = resolved.run_target(
            OPENCODE_METADATA.default_base_url.as_ref().into(),
            session.reasoning,
        );
        OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN,
        )
    }

    #[test]
    fn request_body_demotes_mid_stream_system_messages_for_strict_templates() {
        // Regression (observed live): a strict OpenAI-compatible chat
        // template — Qwen3's Jinja `raise_exception('System message must be at
        // the beginning')` — rejects a `system` message anywhere but index 0.
        // Bonsai arms a self-review harness note mid-conversation as a system
        // message, so the wire body must fold it into the user stream.
        let session = provider_session("sk", "http://localhost/v1", "qwen-qwen3.6-35b-a3b");
        let target = crate::provider::fallback_run_target(&OPENCODE_METADATA, &session);
        let provider = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN,
        );

        let harness_note = ChatCompletionRequestMessage::System(
            async_openai::types::chat::ChatCompletionRequestSystemMessage {
                content: "Harness note: Self-review before finishing."
                    .to_string()
                    .into(),
                name: Some("bonsai_harness".to_string()),
            },
        );
        let body = provider
            .request_body(
                &[
                    system_message("system prompt"),
                    user_message("do the thing"),
                    harness_note,
                    user_message("continue"),
                ],
                &[],
            )
            .unwrap();

        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages[0]["role"], "system", "leading system prompt stays");
        assert_eq!(
            messages[2]["role"], "user",
            "the mid-stream system harness note is demoted to a user turn"
        );
        assert_eq!(
            messages[2]["name"], "bonsai_harness",
            "demotion preserves the harness provenance name"
        );
        assert!(
            messages
                .iter()
                .skip(1)
                .all(|message| message["role"] != "system"),
            "no system message may reach the wire past index 0"
        );
    }

    #[test]
    fn opencode_rolling_history_keeps_project_snapshots_in_place() {
        let session = provider_session(
            "sk-oc",
            OPENCODE_METADATA.default_base_url.as_ref(),
            "deepseek-v4-flash",
        );
        let provider = resolved_opencode_provider(&session);
        assert_eq!(
            provider.project_state_cache_strategy(),
            crate::provider::ProjectStateCacheStrategy::AppendOnlyHistory,
            "OpenCode's declared rolling-history policy must reach the agent"
        );
        let history = [
            system_message("stable"),
            user_message("first"),
            // Deliberately the legacy envelope: resumed pre-Harness-note
            // sessions must keep flowing through the append-only layout.
            named_user_message(
                crate::context::PROJECT_STATE_MESSAGE_NAME,
                "Context update for the request above:\n\nstate one",
            ),
            user_message("second"),
            named_user_message(
                crate::context::PROJECT_STATE_MESSAGE_NAME,
                "Context update for the request above:\n\nstate two",
            ),
        ];

        let body = provider.request_body(&history, &[]).unwrap();
        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages.len(), history.len());
        assert_eq!(
            messages[2]["name"],
            serde_json::json!(crate::context::PROJECT_STATE_MESSAGE_NAME)
        );
        assert_eq!(
            messages[4]["name"],
            serde_json::json!(crate::context::PROJECT_STATE_MESSAGE_NAME)
        );
        assert_eq!(messages[3]["content"], serde_json::json!("second"));
    }

    #[test]
    fn openrouter_anthropic_policy_activates_cache_and_sticky_routing() {
        let catalog =
            crate::model_catalog::ModelCatalog::load_builtin().expect("built-in catalog loads");
        let connection_id = "openrouter"
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();
        let resolved = catalog
            .resolve_connection_model(&connection_id, "openrouter/claude-haiku-4.5")
            .expect("OpenRouter Haiku target resolves");
        let session = provider_session(
            "sk-or",
            "https://openrouter.ai/api/v1",
            "openrouter/claude-haiku-4.5",
        );
        let target = resolved.run_target("https://openrouter.ai/api/v1".into(), session.reasoning);
        let mut provider = OpenAiCompatibleProvider::with_api_key_policy(
            "openrouter",
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN,
        );
        let thread_id = "01990abc-1234-7def-8123-456789abcdef";
        provider.set_conversation_cache_key(thread_id);

        let body = provider
            .request_body(&[system_message("stable"), user_message("hello")], &[])
            .unwrap();

        assert_eq!(
            body["model"],
            serde_json::json!("anthropic/claude-haiku-4.5")
        );
        assert_eq!(
            body["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
        assert_eq!(body["session_id"], serde_json::json!(thread_id));
        assert!(body.get("prompt_cache_key").is_none());
        assert_eq!(
            provider.project_state_cache_strategy(),
            crate::provider::ProjectStateCacheStrategy::AppendOnlyHistory
        );

        let default_resolved = catalog
            .resolve_connection_model(&connection_id, "openrouter/gpt-5.2")
            .expect("OpenRouter GPT target resolves");
        let default_target =
            default_resolved.run_target("https://openrouter.ai/api/v1".into(), session.reasoning);
        let default_provider = OpenAiCompatibleProvider::with_api_key_policy(
            "openrouter",
            &session,
            &default_target,
            ApiKeyPolicy::Required,
            PLAIN,
        );
        let default_body = default_provider
            .request_body(&[system_message("stable"), user_message("hello")], &[])
            .unwrap();
        assert!(default_body.get("cache_control").is_none());
        assert!(default_body.get("session_id").is_none());
        assert_eq!(
            default_provider.project_state_cache_strategy(),
            crate::provider::ProjectStateCacheStrategy::MutableSystemTail,
            "OpenRouter's Anthropic cache policy must never leak to other models"
        );
    }

    #[test]
    fn prompt_cache_key_is_gated_on_support_and_stable_across_turns() {
        let session = provider_session("sk", "http://localhost/v1", "qwen3.7-max");
        let target = crate::provider::fallback_run_target(&OPENCODE_METADATA, &session);

        let unsupported = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN,
        );
        assert_eq!(
            unsupported.project_state_cache_strategy(),
            crate::provider::ProjectStateCacheStrategy::MutableSystemTail,
            "compatible providers must not inherit OpenCode's opt-in policy"
        );
        let body = unsupported
            .request_body(&[user_message("hi")], &[])
            .unwrap();
        assert!(
            body.get("prompt_cache_key").is_none(),
            "no prompt_cache_key when the backend is not marked cache-capable",
        );

        let mut supported = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            CACHING,
        );
        supported.set_conversation_cache_key("bonsai-persisted-session");
        let first = supported.request_body(&[user_message("hi")], &[]).unwrap();
        let second = supported
            .request_body(&[user_message("hi again")], &[])
            .unwrap();
        let key = first
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .expect("prompt_cache_key present when the backend supports caching");
        assert_eq!(key, "bonsai-persisted-session");
        assert_eq!(
            second.get("prompt_cache_key").and_then(Value::as_str),
            Some(key),
            "prompt_cache_key must be stable across turns of one conversation",
        );

        let planning = supported
            .request_body(
                &[system_message("planning persona"), user_message("hi")],
                &[],
            )
            .unwrap();
        let coding = supported
            .request_body(&[system_message("coding persona"), user_message("hi")], &[])
            .unwrap();
        assert_ne!(
            planning.get("prompt_cache_key").and_then(Value::as_str),
            coding.get("prompt_cache_key").and_then(Value::as_str),
            "plan-mode and coding-mode lanes must not share a cache route",
        );
    }

    /// The OpenCode Go gateway 400s a DeepSeek request whose assistant
    /// tool-call messages lack `reasoning_content` — including the ones deeper
    /// in the history, not just the newest — so assert every one of them
    /// carries the field, and that nothing else on the wire grows it.
    #[test]
    fn reasoning_content_is_echoed_on_every_assistant_tool_call_message() {
        let session = provider_session("sk", "http://localhost/v1", "deepseek-v4-flash");
        let target = crate::provider::fallback_run_target(&OPENCODE_METADATA, &session);
        let history = [
            user_message("count the files"),
            tool_call_message("c1", "bash", "{\"cmd\":\"ls\"}"),
            crate::provider::test_utils::tool_result_message("c1", "a.txt"),
            tool_call_message("c2", "bash", "{\"cmd\":\"pwd\"}"),
            crate::provider::test_utils::tool_result_message("c2", "/tmp"),
        ];

        let echoing = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN.with_reasoning_content_echo(),
        );
        let body = echoing.request_body(&history, &[]).unwrap();
        let messages = body["messages"].as_array().expect("messages array");

        let tool_call_messages: Vec<&Value> = messages
            .iter()
            .filter(|message| {
                message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty())
            })
            .collect();
        assert_eq!(
            tool_call_messages.len(),
            2,
            "both rounds replay on the wire"
        );
        for message in tool_call_messages {
            assert_eq!(
                message.get("reasoning_content").and_then(Value::as_str),
                Some(""),
                "every assistant tool-call message needs the field, not just the last",
            );
        }
        assert!(
            messages
                .iter()
                .filter(|message| message["role"] != "assistant")
                .all(|message| message.get("reasoning_content").is_none()),
            "user and tool messages must not grow a reasoning_content field",
        );

        let plain = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN,
        );
        let body = plain.request_body(&history, &[]).unwrap();
        assert!(
            body["messages"]
                .as_array()
                .expect("messages array")
                .iter()
                .all(|message| message.get("reasoning_content").is_none()),
            "the off-schema field stays off backends that have not asked for it",
        );
    }

    #[test]
    fn request_body_dedupes_tool_call_ids_across_assistant_turns() {
        // Regression (observed live): Kimi K2 mints per-message ids
        // like `project_info:1`, so two assistant turns can replay the same id
        // and Moonshot 400s the request ("tool call id ... is duplicated").
        // The wire body must keep the first occurrence and rename later ones,
        // retargeting each turn's tool results to its own calls.
        let session = provider_session("sk", "http://localhost/v1", "kimi-k3");
        let target = crate::provider::fallback_run_target(&OPENCODE_METADATA, &session);
        let provider = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN,
        );

        let body = provider
            .request_body(
                &[
                    user_message("inspect the project"),
                    tool_call_message("project_info:1", "project_info", "{}"),
                    tool_result_message("project_info:1", "rust workspace"),
                    user_message("look again"),
                    tool_call_message("project_info:1", "project_info", "{}"),
                    tool_result_message("project_info:1", "rust workspace (unchanged)"),
                ],
                &[],
            )
            .unwrap();

        let messages = body["messages"].as_array().expect("messages array");
        let call_ids: Vec<&str> = messages
            .iter()
            .filter_map(|message| message.get("tool_calls"))
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(|call| call.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(
            call_ids,
            vec!["project_info:1", "project_info:1-dup2"],
            "first occurrence keeps its wire id; the duplicate is renamed",
        );

        let result_ids: Vec<&str> = messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .filter_map(|message| message.get("tool_call_id").and_then(Value::as_str))
            .collect();
        assert_eq!(
            result_ids,
            vec!["project_info:1", "project_info:1-dup2"],
            "each tool result follows its own assistant turn's rename",
        );
    }

    #[test]
    fn request_body_dedupes_tool_call_ids_within_one_assistant_message() {
        // The shape Moonshot actually 400s on (verified live): TWO parallel
        // calls sharing one id inside a single assistant message. Both results
        // reference the same original id, so pairing must be positional —
        // result 1 keeps the kept id, result 2 follows the rename.
        let session = provider_session("sk", "http://localhost/v1", "kimi-k3");
        let target = crate::provider::fallback_run_target(&OPENCODE_METADATA, &session);
        let provider = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN,
        );

        let two_calls_one_id = ChatCompletionRequestMessage::Assistant(
            async_openai::types::chat::ChatCompletionRequestAssistantMessageArgs::default()
                .content("")
                .tool_calls(vec![
                    async_openai::types::chat::ChatCompletionMessageToolCalls::Function(
                        async_openai::types::chat::ChatCompletionMessageToolCall {
                            id: "project_info:1".to_string(),
                            function: async_openai::types::chat::FunctionCall {
                                name: "project_info".to_string(),
                                arguments: "{}".to_string(),
                            },
                        },
                    ),
                    async_openai::types::chat::ChatCompletionMessageToolCalls::Function(
                        async_openai::types::chat::ChatCompletionMessageToolCall {
                            id: "project_info:1".to_string(),
                            function: async_openai::types::chat::FunctionCall {
                                name: "project_info".to_string(),
                                arguments: "{}".to_string(),
                            },
                        },
                    ),
                ])
                .build()
                .unwrap(),
        );

        let body = provider
            .request_body(
                &[
                    user_message("inspect the project"),
                    two_calls_one_id,
                    tool_result_message("project_info:1", "first result"),
                    tool_result_message("project_info:1", "second result"),
                ],
                &[],
            )
            .unwrap();

        let messages = body["messages"].as_array().expect("messages array");
        let call_ids: Vec<&str> = messages
            .iter()
            .filter_map(|message| message.get("tool_calls"))
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(|call| call.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(call_ids, vec!["project_info:1", "project_info:1-dup2"]);

        let results: Vec<(&str, &str)> = messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .filter_map(|message| {
                Some((
                    message.get("tool_call_id").and_then(Value::as_str)?,
                    message.get("content").and_then(Value::as_str)?,
                ))
            })
            .collect();
        assert_eq!(
            results,
            vec![
                ("project_info:1", "first result"),
                ("project_info:1-dup2", "second result"),
            ],
            "result N pairs with call N, not last-rename-wins",
        );
    }

    /// The capability must survive the path production actually takes: the
    /// registry is built from the catalog, so a connection's provider is minted
    /// by `CatalogProviderFactory` from `connections.toml` — NOT from the
    /// builtin `OPENCODE_METADATA`. A flag declared only on the builtin metadata
    /// is inert at runtime, and a test that constructs the provider directly
    /// cannot see that.
    #[test]
    fn catalog_built_opencode_provider_echoes_reasoning_content() {
        let catalog =
            crate::model_catalog::ModelCatalog::load_builtin().expect("built-in catalog loads");
        let registry = crate::provider::ProviderRegistry::from_catalog(&catalog);
        let factory = registry.get("opencode").expect("opencode connection");
        let session = provider_session(
            "sk",
            OPENCODE_METADATA.default_base_url.as_ref(),
            "opencode/deepseek-v4-flash",
        );
        let connection_id = "opencode"
            .parse::<crate::model_catalog::ConnectionId>()
            .expect("connection id");
        let resolved = catalog
            .resolve_connection_model(&connection_id, &session.model)
            .expect("deepseek target resolves");
        let target = resolved.run_target(
            OPENCODE_METADATA.default_base_url.as_ref().into(),
            session.reasoning,
        );
        let provider = factory.build_target(&session, &target);

        let preview = provider
            .preview_request(
                &[
                    user_message("count the files"),
                    tool_call_message("c1", "bash", "{\"cmd\":\"ls\"}"),
                    crate::provider::test_utils::tool_result_message("c1", "a.txt"),
                ],
                &[],
            )
            .expect("preview builds");
        let assistant = preview.body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find(|message| message["role"] == "assistant")
            .expect("assistant tool-call message replays");
        assert_eq!(
            assistant.get("reasoning_content").and_then(Value::as_str),
            Some(""),
            "the DeepSeek echo must reach the provider the registry actually builds",
        );
    }

    #[test]
    fn official_openai_base_url_enables_prompt_cache_key() {
        // A generic endpoint-style metadata (no seeds, optional key) standing
        // in for a custom OpenAI-compatible catalog connection.
        let endpoint_metadata = crate::provider::ProviderMetadata::new(
            "test-openai-compatible",
            "Test OpenAI Compatible",
            "",
            "",
            None,
            None,
            None,
            &[],
            crate::provider::Protocol::OpenAiChat,
            crate::provider::ProviderCapabilities::new(
                crate::provider::NO_REASONING,
                crate::provider::NO_PARAMETERS,
            ),
            "chat/completions",
        );
        let session = provider_session(
            "sk-openai",
            "https://api.openai.com/v1/chat/completions",
            "gpt-5.5",
        );
        let target = crate::provider::fallback_run_target(&endpoint_metadata, &session);
        let provider = OpenAiCompatibleProvider::with_api_key_policy(
            endpoint_metadata.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Optional,
            PLAIN,
        );

        let first = provider.request_body(&[user_message("hi")], &[]).unwrap();
        let second = provider
            .request_body(&[user_message("again")], &[])
            .unwrap();
        let key = first
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .expect("official OpenAI requests should carry a prompt_cache_key");
        assert!(!key.is_empty());
        assert_eq!(
            second.get("prompt_cache_key").and_then(Value::as_str),
            Some(key)
        );
    }

    #[test]
    fn catalog_openai_request_uses_the_public_chat_completions_dialect() {
        let catalog =
            crate::model_catalog::ModelCatalog::load_builtin().expect("built-in catalog loads");
        let registry = crate::provider::ProviderRegistry::from_catalog(&catalog);
        let factory = registry.get("openai").expect("OpenAI API connection");
        let metadata = factory.metadata();
        let mut session =
            provider_session("sk-openai", &metadata.default_base_url, "openai/gpt-5.6");
        session.reasoning = crate::provider::ReasoningSelection::High;
        let connection_id = "openai"
            .parse::<crate::model_catalog::ConnectionId>()
            .expect("connection id");
        let resolved = catalog
            .resolve_connection_model(&connection_id, &session.model)
            .expect("OpenAI default target resolves");
        let target =
            resolved.run_target(metadata.default_base_url.as_ref().into(), session.reasoning);
        let provider = OpenAiCompatibleProvider::with_api_key_policy(
            metadata.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            metadata.capabilities,
        );

        let body = provider
            .request_body(&[user_message("inspect this repository")], &[sample_tool()])
            .expect("request body builds");

        assert_eq!(body["reasoning_effort"], serde_json::json!("high"));
        assert!(body.get("reasoning").is_none());
        assert_eq!(body["max_completion_tokens"], serde_json::json!(128_000));
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["parallel_tool_calls"], serde_json::json!(true));
        assert!(
            body.get("prompt_cache_key")
                .and_then(Value::as_str)
                .is_some_and(|key| !key.is_empty())
        );
    }

    #[test]
    fn qwen_cache_layout_remains_identical_with_durable_project_state() {
        let session = provider_session("sk", "http://localhost/v1", "qwen3.7-max");
        let target = crate::provider::fallback_run_target(&OPENCODE_METADATA, &session);
        let provider = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            CACHING,
        );
        let volatile = format!("{}\n- dirty files", crate::context::VOLATILE_STATE_HEADING);
        let legacy = provider
            .request_body(
                &[
                    system_message(&format!("stable instructions\n\n{volatile}")),
                    user_message("hello"),
                ],
                &[],
            )
            .unwrap();
        let durable = provider
            .request_body(
                &[
                    system_message("stable instructions"),
                    user_message("hello"),
                    named_user_message(
                        crate::context::PROJECT_STATE_MESSAGE_NAME,
                        &transform::volatile_context_user_text(&volatile),
                    ),
                ],
                &[],
            )
            .unwrap();
        let messages = durable["messages"]
            .as_array()
            .expect("messages should serialize");

        assert_eq!(
            durable["messages"], legacy["messages"],
            "IMPORTANT: do not change the proven Qwen/GLM cache wire layout"
        );
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "stable instructions");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "hello");
        assert_eq!(messages[2]["role"], "user");
        assert!(messages[2].get("name").is_none());
        assert_eq!(
            messages[2]["content"],
            transform::volatile_context_user_text(&volatile)
        );
    }

    #[test]
    fn request_body_normalizes_malformed_tool_call_arguments() {
        let session = provider_session("sk", "http://localhost/v1", "qwen3.7-max");
        let target = crate::provider::fallback_run_target(&OPENCODE_METADATA, &session);
        let provider = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN,
        );

        let body = provider
            .request_body(&[tool_call_message("call-1", "plan_set_title", "")], &[])
            .unwrap();

        assert_eq!(
            body.pointer("/messages/0/tool_calls/0/function/arguments")
                .and_then(Value::as_str),
            Some("{}")
        );
    }

    #[test]
    fn official_openai_host_detection_is_strict() {
        assert!(is_official_openai_api_base_url("https://api.openai.com/v1"));
        assert!(!is_official_openai_api_base_url("http://api.openai.com/v1"));
        assert!(!is_official_openai_api_base_url(
            "https://api.openai.com.example/v1"
        ));
        assert!(!is_official_openai_api_base_url("http://localhost/v1"));
    }

    #[test]
    fn metadata_is_opencode() {
        assert_eq!(OPENCODE_METADATA.id.as_ref(), "opencode");
        assert_eq!(OPENCODE_METADATA.default_model.as_ref(), "qwen3.7-max");
        assert_eq!(
            OPENCODE_METADATA.seed_model_list(),
            ["qwen3.7-max".to_string(), "glm-5.2".to_string()]
        );
        assert_eq!(
            OPENCODE_METADATA.env_var_api_key.as_deref(),
            Some("OPENCODE_API_KEY")
        );
        assert_eq!(
            OPENCODE_METADATA.auth_requirement,
            AuthRequirement::ApiKeyRequired
        );
        assert_eq!(OPENCODE_METADATA.protocol, Protocol::OpenAiChat);
    }

    #[test]
    fn live_model_parser_returns_remote_ids_only() {
        let models = opencode_catalog_from_response(&serde_json::json!({
            "data": [
                {"id": "glm-5.2", "object": "model", "owned_by": "opencode"},
                {"object": "model"}
            ]
        }));

        assert_eq!(models, vec!["glm-5.2"]);
    }

    #[test]
    fn openai_reasoning_payload_emits_max_effort_for_default_codec() {
        let mut session = provider_session(
            "sk-oc",
            OPENCODE_METADATA.default_base_url.as_ref(),
            "deepseek-v4-flash",
        );
        session.reasoning = crate::provider::ReasoningSelection::Max;
        let provider = resolved_opencode_provider(&session);

        assert_eq!(
            provider.reasoning_payload(),
            Some(serde_json::json!({"effort": "max"}))
        );
    }

    #[test]
    fn request_local_reasoning_override_does_not_mutate_provider_default() {
        let mut session = provider_session(
            "sk-oc",
            OPENCODE_METADATA.default_base_url.as_ref(),
            "deepseek-v4-flash",
        );
        session.reasoning = crate::provider::ReasoningSelection::High;
        let provider = resolved_opencode_provider(&session);

        let body = provider
            .request_body_with_reasoning(
                &[user_message("hello")],
                &[],
                crate::provider::ReasoningSelection::Medium,
            )
            .unwrap();

        assert_eq!(body["reasoning"]["effort"], serde_json::json!("medium"));
        assert_eq!(
            provider.reasoning,
            crate::provider::ReasoningSelection::High
        );
    }

    #[test]
    fn openai_reasoning_payload_omits_off_for_default_codec() {
        let mut session = provider_session(
            "sk-oc",
            OPENCODE_METADATA.default_base_url.as_ref(),
            "deepseek-v4-flash",
        );
        session.reasoning = crate::provider::ReasoningSelection::Off;
        let provider = resolved_opencode_provider(&session);

        assert_eq!(provider.reasoning_payload(), None);
    }

    #[test]
    fn glm_5_2_reasoning_body_sends_reasoning_effort_only() {
        // GLM 5.2 rejects a request carrying both `thinking` and
        // `reasoning_effort` ("cannot specify both"), so an effort selection
        // must send only `reasoning_effort`.
        let mut session = provider_session(
            "sk-oc",
            OPENCODE_METADATA.default_base_url.as_ref(),
            "glm-5.2",
        );
        session.reasoning = crate::provider::ReasoningSelection::Max;
        let provider = resolved_opencode_provider(&session);
        let body = provider.request_body(&[user_message("hi")], &[]).unwrap();

        assert_eq!(body.get("reasoning"), None);
        assert_eq!(
            body.get("thinking"),
            None,
            "GLM 5.2 must not receive `thinking` alongside `reasoning_effort`"
        );
        assert_eq!(
            body.get("reasoning_effort"),
            Some(&serde_json::json!("max"))
        );
    }

    #[test]
    fn glm_reasoning_body_can_disable_zai_thinking() {
        let mut session = provider_session(
            "sk-oc",
            OPENCODE_METADATA.default_base_url.as_ref(),
            "glm-5.2",
        );
        session.reasoning = crate::provider::ReasoningSelection::Off;
        let provider = resolved_opencode_provider(&session);
        let body = provider.request_body(&[user_message("hi")], &[]).unwrap();

        assert_eq!(body.get("reasoning"), None);
        assert_eq!(
            body.get("thinking"),
            Some(&serde_json::json!({ "type": "disabled" }))
        );
        assert_eq!(body.get("reasoning_effort"), None);
    }

    #[tokio::test]
    async fn list_available_models_reads_live_ids() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer sk-oc-test"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"{"data":[{"id":"deepseek-v4-flash","context_window":131072},{"id":"qwen3.7-max"}]}"#,
                ),
            )
            .mount(&server)
            .await;

        let session = provider_session("sk-oc-test", &server.uri(), "qwen3.7-max");
        let availability = OpenCodeFactory
            .list_available_models(&session)
            .await
            .unwrap();
        assert_eq!(
            availability.remote_model_ids(),
            vec!["deepseek-v4-flash".to_string(), "qwen3.7-max".to_string()]
        );
    }

    #[tokio::test]
    async fn chat_stream_splits_inline_think_tags_into_reasoning() {
        use std::sync::Mutex;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        #[derive(Default)]
        struct RecordingSink {
            assistant: Mutex<String>,
            reasoning: Mutex<String>,
        }
        impl crate::output::OutputSink for RecordingSink {
            fn assistant_delta(&self, text: &str) {
                self.assistant.lock().unwrap().push_str(text);
            }
            fn reasoning_delta(&self, text: &str) {
                self.reasoning.lock().unwrap().push_str(text);
            }
        }

        // Reasoning emitted inline as <think>…</think> in the content channel,
        // split across stream frames, then the visible answer. The final frame
        // carries finish_reason, as every real server's does — a stream
        // without one is treated as truncated.
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"<think>weigh\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ing</think>Done\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let session = provider_session("sk-oc-test", &server.uri(), "qwen3.7-max");
        let catalog =
            crate::model_catalog::ModelCatalog::load_builtin().expect("built-in catalog loads");
        let connection_id = OPENCODE_METADATA
            .id
            .parse::<crate::model_catalog::ConnectionId>()
            .expect("metadata id is a connection id");
        let resolved = catalog
            .resolve_connection_model(&connection_id, &session.model)
            .expect("opencode test model resolves");
        let target = resolved.run_target(server.uri().into(), session.reasoning);
        let provider = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN,
        );

        let sink = std::sync::Arc::new(RecordingSink::default());
        let response = provider
            .chat_stream(&[], &[], CancellationToken::new(), sink.clone())
            .await
            .expect("chat stream succeeds");

        assert_eq!(response.content, "Done");
        assert_eq!(
            response.finish_reason(),
            Some(&crate::provider::FinishReason::Stop)
        );
        assert_eq!(response.reasoning_chars, "weighing".chars().count());
        assert_eq!(*sink.assistant.lock().unwrap(), "Done");
        assert_eq!(*sink.reasoning.lock().unwrap(), "weighing");
        let diagnostics = provider.take_last_request_diagnostics().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&diagnostics.serialized_body).unwrap(),
            provider.request_body(&[], &[]).unwrap()
        );
        assert_eq!(provider.take_last_request_diagnostics(), None);
    }

    #[tokio::test]
    async fn chat_stream_reassembles_fragmented_tool_call_fields() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"re\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"ad\",\"arguments\":\"\\\"Cargo.toml\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let session = provider_session("sk-oc-test", &server.uri(), "qwen3.7-max");
        let catalog = crate::model_catalog::ModelCatalog::load_builtin().unwrap();
        let connection_id = OPENCODE_METADATA
            .id
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();
        let resolved = catalog
            .resolve_connection_model(&connection_id, &session.model)
            .unwrap();
        let target = resolved.run_target(server.uri().into(), session.reasoning);
        let provider = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN,
        );

        let response = provider
            .chat_stream(
                &[],
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
    async fn native_reasoning_is_replayed_with_its_tool_call_turn() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"inspect \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"the file\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"Cargo.toml\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let session = provider_session("sk-test", &server.uri(), "qwen3.7-max");
        let catalog = crate::model_catalog::ModelCatalog::load_builtin().unwrap();
        let connection_id = OPENCODE_METADATA.id.parse().unwrap();
        let resolved = catalog
            .resolve_connection_model(&connection_id, &session.model)
            .unwrap();
        let target = resolved.run_target(server.uri().into(), session.reasoning);
        let provider = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN.with_reasoning_content_echo(),
        );

        let response = provider
            .chat_stream(
                &[],
                &[],
                CancellationToken::new(),
                std::sync::Arc::new(crate::output::StdoutSink),
            )
            .await
            .unwrap();
        assert_eq!(response.tool_calls[0].id, "call_1");

        let history = [
            tool_call_message("call_1", "read", "{\"path\":\"Cargo.toml\"}"),
            crate::provider::test_utils::tool_result_message("call_1", "contents"),
        ];
        let body = provider.request_body(&history, &[]).unwrap();
        assert_eq!(
            body["messages"][0]["reasoning_content"],
            serde_json::json!("inspect the file")
        );
    }

    #[tokio::test]
    async fn benign_error_and_non_json_frames_do_not_fail_the_stream() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // A non-JSON keepalive line, a normal chunk that also carries a benign
        // `"error": false`, and a `[DONE]` with a stray trailing space — none of
        // these may be mistaken for a malformed/error frame and fail the turn.
        let sse = concat!(
            "data: keep-alive\n\n",
            "data: {\"error\":false,\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE] \n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let session = provider_session("sk-oc-test", &server.uri(), "qwen3.7-max");
        let catalog =
            crate::model_catalog::ModelCatalog::load_builtin().expect("built-in catalog loads");
        let connection_id = OPENCODE_METADATA
            .id
            .parse::<crate::model_catalog::ConnectionId>()
            .expect("metadata id is a connection id");
        let resolved = catalog
            .resolve_connection_model(&connection_id, &session.model)
            .expect("opencode test model resolves");
        let target = resolved.run_target(server.uri().into(), session.reasoning);
        let provider = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN,
        );

        let sink: crate::output::SharedSink = std::sync::Arc::new(crate::output::StdoutSink);
        let response = provider
            .chat_stream(&[], &[], CancellationToken::new(), sink)
            .await
            .expect("benign non-JSON / falsy-error frames must not fail the stream");
        assert_eq!(response.content, "Hi there");
    }

    /// GLM streamed ~53k tokens of reasoning, the connection
    /// dropped at a frame boundary, and the empty remainder was returned as a
    /// clean success — the agent run then "completed" with nothing. A stream
    /// that ends without a finish_reason or usage frame must surface as a
    /// retryable transport error, never as a successful empty turn.
    #[tokio::test]
    async fn stream_without_finish_reason_or_usage_is_a_retryable_error() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking hard\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"still thinking\"}}]}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let session = provider_session("sk-oc-test", &server.uri(), "qwen3.7-max");
        let catalog =
            crate::model_catalog::ModelCatalog::load_builtin().expect("built-in catalog loads");
        let connection_id = OPENCODE_METADATA
            .id
            .parse::<crate::model_catalog::ConnectionId>()
            .expect("metadata id is a connection id");
        let resolved = catalog
            .resolve_connection_model(&connection_id, &session.model)
            .expect("opencode test model resolves");
        let target = resolved.run_target(server.uri().into(), session.reasoning);
        let provider = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN,
        );

        let sink: crate::output::SharedSink = std::sync::Arc::new(crate::output::StdoutSink);
        let error = provider
            .chat_stream(&[], &[], CancellationToken::new(), sink)
            .await
            .expect_err("a stream that never completes must not return a clean response");
        assert!(
            error.is_retryable(),
            "truncated stream must be retryable: {error}"
        );
    }

    /// The mirror image of `usage_frame_without_finish_reason_is_a_retryable_error`:
    /// on a backend that never sends `finish_reason` (OpenCode Zen's gpt-5.x and
    /// claude-opus-4-8 close on a usage frame plus a cost frame), the usage frame
    /// IS the terminal signal, and the turn — tool calls included — must survive.
    #[tokio::test]
    async fn usage_frame_ends_the_stream_when_the_backend_declares_it() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Reading it.\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\
             \"function\":{\"name\":\"bash\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}\n\n",
            "data: {\"choices\":[],\"cost\":\"0.00014700\"}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let session = provider_session("sk-oc-test", &server.uri(), "qwen3.7-max");
        let target = crate::provider::fallback_run_target(&OPENCODE_METADATA, &session);
        let provider = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN.with_usage_frame_stream_terminal(),
        );

        let sink: crate::output::SharedSink = std::sync::Arc::new(crate::output::StdoutSink);
        let response = provider
            .chat_stream(&[], &[], CancellationToken::new(), sink)
            .await
            .expect("a usage-terminated stream is complete on backends that end that way");
        assert_eq!(response.content, "Reading it.");
        assert_eq!(response.tool_calls.len(), 1, "tool calls survive the turn");
        assert_eq!(
            response.terminal,
            crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::ToolCalls),
            "with no finish_reason on the wire, infer it from the streamed tool calls",
        );
    }

    #[tokio::test]
    async fn usage_frame_without_finish_reason_is_a_retryable_error() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let session = provider_session("sk-oc-test", &server.uri(), "qwen3.7-max");
        let catalog =
            crate::model_catalog::ModelCatalog::load_builtin().expect("built-in catalog loads");
        let connection_id = OPENCODE_METADATA
            .id
            .parse::<crate::model_catalog::ConnectionId>()
            .expect("metadata id is a connection id");
        let resolved = catalog
            .resolve_connection_model(&connection_id, &session.model)
            .expect("opencode test model resolves");
        let target = resolved.run_target(server.uri().into(), session.reasoning);
        let provider = OpenAiCompatibleProvider::with_api_key_policy(
            OPENCODE_METADATA.id.as_ref(),
            &session,
            &target,
            ApiKeyPolicy::Required,
            PLAIN,
        );

        let output: crate::output::SharedSink = std::sync::Arc::new(crate::output::StdoutSink);
        let error = provider
            .chat_stream(&[], &[], CancellationToken::new(), output)
            .await
            .expect_err("usage is accounting data, not a terminal event");
        assert!(error.is_retryable());
    }

    #[tokio::test]
    async fn authorize_from_env_reads_key() {
        unsafe {
            std::env::set_var("OPENCODE_API_KEY", "sk-oc-env");
        }
        let outcome = OpenCodeFactory.authorize(AuthInput::FromEnv).await.unwrap();
        assert_eq!(outcome.api_key, "sk-oc-env");
        unsafe {
            std::env::remove_var("OPENCODE_API_KEY");
        }
    }

    #[tokio::test]
    async fn authorize_accepts_pasted_key() {
        let outcome = OpenCodeFactory
            .authorize(AuthInput::ApiKey {
                api_key: "sk-oc-pasted".to_string(),
                persistence: crate::session::CredentialPersistence::File,
            })
            .await
            .unwrap();
        assert_eq!(outcome.api_key, "sk-oc-pasted");
    }

    #[tokio::test]
    async fn authorize_rejects_empty_pasted_key() {
        let result = OpenCodeFactory
            .authorize(AuthInput::ApiKey {
                api_key: "   ".to_string(),
                persistence: crate::session::CredentialPersistence::File,
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn authorize_rejects_codex_cache_input() {
        let result = OpenCodeFactory.authorize(AuthInput::FromCodexCache).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn is_authorized_checks_api_key_presence() {
        let factory = OpenCodeFactory;
        let session = provider_session(
            "",
            OPENCODE_METADATA.default_base_url.as_ref(),
            "qwen3.7-max",
        );
        assert!(!factory.is_authorized(&session));
        let session = provider_session(
            "sk-oc",
            OPENCODE_METADATA.default_base_url.as_ref(),
            "qwen3.7-max",
        );
        assert!(factory.is_authorized(&session));
    }

    #[tokio::test]
    async fn clear_authorization_wipes_api_key() {
        let factory = OpenCodeFactory;
        let mut session = provider_session(
            "sk-oc",
            OPENCODE_METADATA.default_base_url.as_ref(),
            "qwen3.7-max",
        );
        factory.clear_authorization(&mut session);
        assert!(session.api_key.is_empty());
    }

    #[tokio::test]
    async fn ensure_authorized_bails_on_empty_credentials() {
        let session = provider_session(
            "",
            OPENCODE_METADATA.default_base_url.as_ref(),
            "deepseek-v4-flash",
        );
        let provider = resolved_opencode_provider(&session);
        let result = provider.list_models().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn chat_stream_classifies_missing_credentials_as_configuration_failure() {
        let session = provider_session(
            "",
            OPENCODE_METADATA.default_base_url.as_ref(),
            "deepseek-v4-flash",
        );
        let provider = resolved_opencode_provider(&session);
        let output: crate::output::SharedSink = std::sync::Arc::new(crate::output::StdoutSink);

        let error = provider
            .chat_stream(&[], &[], CancellationToken::new(), output)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            crate::provider::ProviderFailure::Configuration { .. }
        ));
        assert!(!error.is_retryable());
    }
}
